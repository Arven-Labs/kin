//! CLI de desenvolvimento: sobe um nó Kin e conversa 1:1 pelo terminal.
//!
//! ```text
//! kin [--data-dir DIR] [--listen ADDR] [--dial ADDR] [--accept LINK] [--relay ADDR]...
//!     [--relay-count N] [--external-addr ADDR]... [--nostr-relay URL]... [--no-nostr] [--no-mdns]
//! kin --serve-relay [--listen ADDR] [--external-addr ADDR]... [--relay-max-bytes N]
//!     [--relay-max-circuits N]
//! ```
//!
//! Atrás de NAT, `--relay ADDR` (que termina em `/p2p/<relay>`) reserva um lugar no relay e imprime o
//! endereço de circuito que o outro lado deve passar a `--dial`. `--serve-relay` sobe só um relay
//! (sem chat). `/invite` imprime o seu link de convite (e um QR) e `--accept LINK` / `/accept LINK`
//! abre o de outra pessoa: o link prova quem é e por onde alcançá-la. Na LAN, duas instâncias se acham sozinhas via mDNS.
//! A caixa postal Nostr fica sempre ligada: mensagens para quem está offline são depositadas nos relays e
//! buscadas ao voltar (só com conversa já fechada). Os relays vêm de `--nostr-relay URL` (`wss://...`;
//! `ws://` só para o próprio computador; repetível), senão de `<data-dir>/relays.txt` (uma URL por linha,
//! `#` comenta), senão do padrão embutido. `--no-nostr` desliga a caixa postal.
//! `/trust [prefixo]` aceita o contato da conversa atual (ou a identidade vista que começa com o
//! prefixo): sem isso quem convidou não processa a lista de devices dele. A escolha é gravada.
//! `/untrust [prefixo]` desfaz: o contato deixa de receber o meu anúncio e a sua lista de devices
//! deixa de valer (um irmão que o repasse não o faz voltar); a conversa e as mensagens seguem.
//! Segundo device da mesma identidade: `/pair` (depois `/pair confirmo`) mostra a chave mestra como link
//! `kin://pair/...`; no device novo, `--pair ARQUIVO` (ou `-` para stdin) a importa na primeira execução
//! e recusa se já houver identidade. É o segredo inteiro, sem cifra: use só em terminal privado.
//! Linhas digitadas vão à identidade da conversa atual (a todos os devices dela) e são espelhadas
//! nos meus outros devices (`eu → ...`);
//! `/r <id> texto` responde à mensagem `<id>` (thread); `/quit` sai.

use std::path::{Path, PathBuf};

use kin_chat::nostr::{NostrMailbox, RelaySource, resolve_relays};
use kin_chat::{Chat, ChatEvent, Error, HistoryEntry, Invite, Message, MessageId};
use kin_identity::{DeviceKey, IdentityId, IdentityProvider, StandaloneIdentity};
use kin_transport::{Multiaddr, Node, NodeConfig, NodeEvent, PeerId, RelayLimits};
use tokio::io::{AsyncBufReadExt, BufReader};

struct Args {
    data_dir: PathBuf,
    listen: Option<Multiaddr>,
    dial: Option<Multiaddr>,
    accept: Option<String>,
    pair: Option<String>,
    relays: Vec<Multiaddr>,
    relay_count: Option<usize>,
    external_addrs: Vec<Multiaddr>,
    nostr_relays: Vec<String>,
    no_nostr: bool,
    serve_relay: bool,
    relay_limits: RelayLimits,
    mdns: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        data_dir: PathBuf::from(".kin"),
        listen: None,
        dial: None,
        accept: None,
        pair: None,
        relays: Vec::new(),
        relay_count: None,
        external_addrs: Vec::new(),
        nostr_relays: Vec::new(),
        no_nostr: false,
        serve_relay: false,
        relay_limits: RelayLimits::default(),
        mdns: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} precisa de um valor"));
        match flag.as_str() {
            "--data-dir" => args.data_dir = value()?.into(),
            "--listen" => args.listen = Some(value()?.parse().map_err(|e| format!("{e}"))?),
            "--dial" => args.dial = Some(value()?.parse().map_err(|e| format!("{e}"))?),
            "--accept" => args.accept = Some(value()?),
            "--pair" => args.pair = Some(value()?),
            "--relay-count" => {
                args.relay_count = Some(value()?.parse().map_err(|e| format!("{e}"))?);
            }
            "--relay" => args
                .relays
                .push(value()?.parse().map_err(|e| format!("{e}"))?),
            "--external-addr" => args
                .external_addrs
                .push(value()?.parse().map_err(|e| format!("{e}"))?),
            "--nostr-relay" => args.nostr_relays.push(value()?),
            "--no-nostr" => args.no_nostr = true,
            "--serve-relay" => args.serve_relay = true,
            "--relay-max-bytes" => {
                args.relay_limits.max_circuit_bytes =
                    value()?.parse().map_err(|e| format!("{e}"))?;
            }
            "--relay-max-circuits" => {
                args.relay_limits.max_circuits = value()?.parse().map_err(|e| format!("{e}"))?;
            }
            "--no-mdns" => args.mdns = false,
            other => return Err(format!("argumento desconhecido: {other}")),
        }
    }
    Ok(args)
}

/// Carrega a identidade e a chave de device do diretório, criando-as na primeira execução.
/// Sem cifra em repouso (P13): só para desenvolvimento.
///
/// `pair` (de `--pair`, ADR 0006): o link `kin://pair/...` de outro device desta identidade, lido de
/// um arquivo (`-` = stdin; nunca do argv, que aparece em `ps`). Só vale na primeira execução: com
/// `identity.key` já existente falha, em vez de sobrescrever uma identidade. O device continua com
/// `device.key` próprio, gerado aqui.
fn load_keys(
    dir: &Path,
    pair: Option<&str>,
) -> Result<(StandaloneIdentity, DeviceKey), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    let identity_path = dir.join("identity.key");
    let device_path = dir.join("device.key");
    if let Some(source) = pair {
        if identity_path.exists() {
            return Err(
                "já existe identity.key neste --data-dir: --pair não sobrescreve uma identidade"
                    .into(),
            );
        }
        // Estado MLS e outbox pertencem à identidade que os criou; com outra identidade, o `Chat`
        // abriria conversas que não são dela.
        if dir.join("mls.sqlite").exists() {
            return Err(
                "este --data-dir já tem estado (mls.sqlite) de outra identidade: use um diretório novo"
                    .into(),
            );
        }
        let text = read_pair_link(source)?;
        let imported = StandaloneIdentity::from_pair_link(&text)?;
        write_private(&identity_path, &imported.to_bytes()?)?;
        println!("identidade importada: {}", imported.id().to_base58());
        if source != "-" {
            println!("(apague o arquivo do link: ele guarda a chave mestra em claro)");
        }
    }
    let identity = if identity_path.exists() {
        StandaloneIdentity::load(&identity_path)?
    } else {
        let identity = StandaloneIdentity::generate();
        identity.save(&identity_path)?;
        identity
    };
    let device = if device_path.exists() {
        DeviceKey::from_bytes(&std::fs::read(&device_path)?)?
    } else {
        let device = DeviceKey::generate();
        write_private(&device_path, &device.to_bytes()?)?;
        device
    };
    Ok((identity, device))
}

/// Lê o link de pareamento de um arquivo ou do stdin (`-`, até o EOF: o chat do mesmo processo não
/// recebe mais linhas). Teto de 4 KiB (o link tem ~100 caracteres) contra arquivo gigante ou
/// `/dev/zero`.
fn read_pair_link(source: &str) -> Result<String, Box<dyn std::error::Error>> {
    use std::io::Read as _;
    const LIMIT: u64 = 4096;
    let mut text = String::new();
    let read = if source == "-" {
        std::io::stdin().take(LIMIT).read_to_string(&mut text)?
    } else {
        std::fs::File::open(source)?
            .take(LIMIT)
            .read_to_string(&mut text)?
    };
    if read as u64 >= LIMIT {
        return Err("link de pareamento grande demais".into());
    }
    Ok(text)
}

/// Cria `path` (falha se existir) com 0600 e grava `bytes`. Se a escrita falhar, apaga o arquivo
/// parcial: um segredo truncado travaria o próximo `--pair` ("já existe") e a abertura normal.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}

fn short(id: &MessageId) -> String {
    id.to_string()[..8].to_string()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args().map_err(|e| {
        eprintln!(
            "{e}\nuso: kin [--data-dir DIR] [--listen ADDR] [--dial ADDR] [--relay ADDR]... [--nostr-relay URL]... [--no-nostr] [--no-mdns]\n     kin --serve-relay [--listen ADDR] [--relay-max-bytes N] [--relay-max-circuits N]"
        );
        e
    })?;
    let (identity, device) = load_keys(&args.data_dir, args.pair.as_deref())?;
    if args.serve_relay {
        return serve_relay(&args, &device).await;
    }
    let config = NodeConfig {
        mdns: args.mdns,
        relays: args.relays.clone(),
        relay_count: args
            .relay_count
            .unwrap_or_else(|| NodeConfig::default().relay_count),
        external_addrs: args.external_addrs.clone(),
        ..Default::default()
    };
    let mut chat = Chat::open(
        &identity,
        &device,
        config,
        &args.data_dir.join("mls.sqlite"),
    )?;
    println!(
        "kin {} — peer id: {}",
        env!("CARGO_PKG_VERSION"),
        chat.peer_id()
    );
    if !args.no_nostr {
        let choice = resolve_relays(&args.nostr_relays, &args.data_dir.join("relays.txt"))?;
        let origin = match choice.source {
            RelaySource::Flag => "--nostr-relay",
            RelaySource::File => "relays.txt",
            RelaySource::Default => "padrão embutido",
        };
        // Dentro do runtime tokio: os workers dos relays sobem no `attach` de `set_mailbox`.
        chat.set_mailbox(NostrMailbox::new(&device, &choice.relays)?);
        println!(
            "caixa postal Nostr em {} relay(s) ({origin}): {}",
            choice.relays.len(),
            choice.relays.join(", ")
        );
    }

    match args.listen {
        Some(addr) => chat.listen_on(addr)?,
        None => chat.listen()?,
    }
    for relay in &args.relays {
        println!("alcançável por relay em {}", chat.circuit_address(relay));
    }
    if let Some(addr) = args.dial {
        chat.dial(addr)?;
    }
    if let Some(link) = &args.accept {
        accept(&mut chat, link);
    }

    let mut seen: Vec<MessageId> = Vec::new();
    let mut current: Option<PeerId> = None;
    // Identidades vistas em `ConversationReady` nesta execução: `/trust` só aceita uma destas.
    let mut met: Vec<(PeerId, IdentityId)> = Vec::new();
    let own = identity.id();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();

    loop {
        tokio::select! {
            event = chat.next_event() => on_event(event, &mut current, &mut seen, &mut met, &own),
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                let line = line.trim();
                if line == "/quit" {
                    break;
                }
                if line.is_empty() {
                    continue;
                }
                if line == "/invite" {
                    print_invite(&chat);
                    continue;
                }
                if line == "/pair" || line == "/pair confirmo" {
                    print_pair(&identity, line == "/pair confirmo");
                    continue;
                }
                if let Some(link) = line.strip_prefix("/accept ") {
                    accept(&mut chat, link);
                    continue;
                }
                if line == "/trust" || line.starts_with("/trust ") {
                    trust(&mut chat, line.strip_prefix("/trust").unwrap_or("").trim(), current, &met);
                    continue;
                }
                if line == "/untrust" || line.starts_with("/untrust ") {
                    untrust(&mut chat, line.strip_prefix("/untrust").unwrap_or("").trim(), current, &met);
                    continue;
                }
                // A chave mestra nunca vai a um contato por engano (ex.: Ctrl+V do link de pareamento).
                if line.contains("kin://pair/") {
                    println!("(não enviado: isto é um link de pareamento, a sua chave mestra)");
                    continue;
                }
                let Some(peer) = current else {
                    println!("(sem conversa pronta ainda)");
                    continue;
                };
                let (parent, text) = match line.strip_prefix("/r ") {
                    Some(rest) => {
                        let (prefix, text) = rest.split_once(' ').unwrap_or((rest, ""));
                        match seen.iter().find(|id| id.to_string().starts_with(prefix)) {
                            Some(id) => (Some(*id), text),
                            None => {
                                println!("(mensagem {prefix} desconhecida)");
                                continue;
                            }
                        }
                    }
                    None => (None, line),
                };
                let Some(target) = identity_of(&met, peer) else {
                    println!("(identidade desconhecida: a conversa não completou o handshake)");
                    continue;
                };
                match chat.send_to(&target, text, parent) {
                    Ok(id) => {
                        seen.push(id);
                        println!("[{}] você: {text}", short(&id));
                    }
                    Err(e) => println!("(falha ao enviar: {e})"),
                }
            }
        }
    }
    Ok(())
}

/// Imprime o convite deste nó (link e QR). Sem endereço compartilhável ainda, avisa.
fn print_invite(chat: &Chat) {
    let invite = chat.shareable_invite();
    if invite.addrs().is_empty() {
        println!(
            "(sem endereço compartilhável ainda: espere a reserva no relay ou use --external-addr)"
        );
        return;
    }
    let link = invite.to_link();
    match qrcode::QrCode::new(link.as_bytes()) {
        Ok(code) => println!(
            "{}",
            code.render::<qrcode::render::unicode::Dense1x2>()
                .quiet_zone(true)
                .build()
        ),
        Err(e) => println!("(não deu para gerar o QR: {e})"),
    }
    println!("convite: {link}");
}

/// `/pair`: mostra o link de pareamento (a chave mestra!) e o QR, só depois de `/pair confirmo`. O
/// segredo vai só para a tela: nada de log nem de arquivo.
fn print_pair(identity: &StandaloneIdentity, confirmed: bool) {
    if !confirmed {
        println!(
            "(isto mostra a CHAVE MESTRA: quem vir o link ou o QR é esta identidade e não há revogação.\n \
             Use só em terminal privado, passe o link ao outro device por arquivo ou stdin e limpe a tela.\n \
             Para continuar: /pair confirmo)"
        );
        return;
    }
    // Saída redirecionada (`> log`, `| tee`, supervisor) gravaria a chave mestra em disco.
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        println!("(recusado: a saída não é um terminal, o link iria para um arquivo ou pipe)");
        return;
    }
    let link = match identity.to_pair_link() {
        Ok(link) => link,
        Err(e) => {
            println!("(não deu para gerar o link: {e})");
            return;
        }
    };
    match qrcode::QrCode::new(link.as_bytes()) {
        Ok(code) => println!(
            "{}",
            code.render::<qrcode::render::unicode::Dense1x2>()
                .quiet_zone(true)
                .build()
        ),
        Err(e) => println!("(não deu para gerar o QR: {e})"),
    }
    println!("pareamento: {link}");
    println!(
        "(no outro device: kin --data-dir NOVO --pair ARQUIVO, com o link no arquivo ou `-` para stdin)"
    );
}

/// Abre um link de convite: disca o dono e exige a identidade do convite.
fn accept(chat: &mut Chat, link: &str) {
    match Invite::from_link(link).and_then(|invite| {
        println!(
            "convite de {} ({} endereço(s))",
            invite.identity().to_base58(),
            invite.addrs().len()
        );
        chat.accept(&invite)
    }) {
        Ok(()) => {}
        Err(e) => println!("(convite não aceito: {e})"),
    }
}

/// Que identidade um argumento de `/trust` ou `/untrust` designa.
#[derive(Debug, PartialEq, Eq)]
enum Resolved {
    One(IdentityId),
    Unknown,
    Ambiguous,
}

/// Sem prefixo, a identidade da conversa atual; com prefixo, a única cujo base58 começa com ele
/// (vários devices da mesma identidade contam como uma). Só vale para identidades vistas em
/// `ConversationReady` nesta execução (`met`).
fn resolve_identity(
    prefix: &str,
    current: Option<PeerId>,
    met: &[(PeerId, IdentityId)],
) -> Resolved {
    let matching = met.iter().filter(|(peer, identity)| {
        if prefix.is_empty() {
            Some(*peer) == current
        } else {
            identity.to_base58().starts_with(prefix)
        }
    });
    let mut chosen: Vec<&IdentityId> = Vec::new();
    for (_, identity) in matching {
        if !chosen.contains(&identity) {
            chosen.push(identity);
        }
    }
    match chosen.as_slice() {
        [identity] => Resolved::One((*identity).clone()),
        [] => Resolved::Unknown,
        _ => Resolved::Ambiguous,
    }
}

/// `/trust`: passa a confiar numa identidade que já completou o handshake nesta execução.
fn trust(chat: &mut Chat, prefix: &str, current: Option<PeerId>, met: &[(PeerId, IdentityId)]) {
    match resolve_identity(prefix, current, met) {
        Resolved::One(identity) => match chat.trust(identity.clone()) {
            Ok(()) => println!("(confio em {})", identity.to_base58()),
            Err(e) => println!("(não consegui gravar a confiança: {e})"),
        },
        Resolved::Unknown => {
            println!("(nenhuma identidade conhecida assim; veja `conversa cifrada pronta`)");
        }
        Resolved::Ambiguous => println!("(prefixo ambíguo: digite mais caracteres)"),
    }
}

/// `/untrust`: deixa de confiar numa identidade (ver `Chat::untrust`). A conversa já aberta continua e
/// as mensagens dela ainda chegam; só deixam de valer a lista de devices dela e o meu anúncio a ela.
fn untrust(chat: &mut Chat, prefix: &str, current: Option<PeerId>, met: &[(PeerId, IdentityId)]) {
    match resolve_identity(prefix, current, met) {
        Resolved::One(identity) => match chat.untrust(&identity) {
            Ok(()) => println!("(desconfio de {})", identity.to_base58()),
            Err(Error::OwnIdentity) => {
                println!("(essa é a sua identidade: os seus devices são família, não contatos)");
            }
            Err(e) => println!("(não consegui gravar a desconfiança: {e})"),
        },
        Resolved::Unknown => {
            println!("(nenhuma identidade conhecida assim; veja `conversa cifrada pronta`)");
        }
        Resolved::Ambiguous => println!("(prefixo ambíguo: digite mais caracteres)"),
    }
}

/// Modo relay dedicado: só encaminha circuitos para os outros, sem chat.
async fn serve_relay(args: &Args, device: &DeviceKey) -> Result<(), Box<dyn std::error::Error>> {
    let config = NodeConfig {
        mdns: false,
        relay_server: Some(args.relay_limits.clone()),
        external_addrs: args.external_addrs.clone(),
        ..Default::default()
    };
    let mut node = Node::new(device.keypair().clone(), config)?;
    println!(
        "kin relay {} — peer id: {}",
        env!("CARGO_PKG_VERSION"),
        node.peer_id()
    );
    match &args.listen {
        Some(addr) => node.listen_on(addr.clone())?,
        None => node.listen()?,
    }
    loop {
        match node.next_event().await {
            NodeEvent::Listening(addr) => println!("relay em {addr}/p2p/{}", node.peer_id()),
            NodeEvent::PeerConnected(peer) => println!("cliente conectado: {peer}"),
            NodeEvent::PeerDisconnected(peer) => println!("cliente saiu: {peer}"),
            _ => {}
        }
    }
}

/// A dica de `/trust` só vale para outra pessoa: um irmão (mesma identidade que a minha) já é
/// confiável por construção.
fn should_hint_trust(peer_identity: &IdentityId, own: &IdentityId) -> bool {
    peer_identity != own
}

/// Quantas linhas o `/history` mostra sem `N`, e o teto de `N`.
#[allow(dead_code)] // stub (testes primeiro)
const DEFAULT_HISTORY: usize = 20;
#[allow(dead_code)] // stub (testes primeiro)
const MAX_HISTORY: usize = 200;

/// Argumentos do `/history`: `[prefixo] [N]`. Um único argumento só de dígitos é `N`; para usar um
/// prefixo que seja só dígitos, passe também o `N`. `N` acima de `MAX_HISTORY` vira o teto; `N = 0`
/// ou lixo é erro.
#[allow(dead_code)] // stub (testes primeiro)
fn parse_history_args(_rest: &str) -> Result<(String, usize), String> {
    Ok((String::new(), 0))
}

/// Linha do `/history`: `[id8] você: texto` / `[id8] peer: texto`, com `↳ pai8` entre o autor e o
/// texto quando a mensagem responde a outra.
#[allow(dead_code)] // stub (testes primeiro)
fn format_entry(_entry: &HistoryEntry) -> String {
    String::new()
}

/// As últimas `n` linhas, na ordem original.
#[allow(dead_code)] // stub (testes primeiro)
fn tail(_entries: Vec<HistoryEntry>, _n: usize) -> Vec<HistoryEntry> {
    Vec::new()
}

/// A identidade vista em `ConversationReady` para `peer`: a linha digitada vai a ela (a todos os
/// devices dela, e espelhada nos meus outros devices), não só ao device da conversa atual.
fn identity_of(met: &[(PeerId, IdentityId)], peer: PeerId) -> Option<IdentityId> {
    met.iter()
        .find(|(known, _)| *known == peer)
        .map(|(_, identity)| identity.clone())
}

fn on_event(
    event: ChatEvent,
    current: &mut Option<PeerId>,
    seen: &mut Vec<MessageId>,
    met: &mut Vec<(PeerId, IdentityId)>,
    own: &IdentityId,
) {
    match event {
        ChatEvent::Listening(addr) => println!("escutando em {addr}"),
        ChatEvent::PeerConnected(peer) => println!("conectado a {peer}"),
        // Mantém `current`: com a caixa postal, enviar a quem saiu é válido (fica na fila/depósito).
        ChatEvent::PeerDisconnected(peer) => println!("desconectado de {peer}"),
        ChatEvent::ConversationReady { peer, identity } => {
            println!(
                "conversa cifrada pronta com {} (identidade {})",
                peer,
                identity.to_base58()
            );
            if should_hint_trust(&identity, own) {
                println!("(para aceitar este contato: /trust)");
            }
            met.retain(|(known, _)| *known != peer);
            met.push((peer, identity));
            *current = Some(peer);
        }
        ChatEvent::Received { message, .. } => {
            let Message {
                id, parent, text, ..
            } = message;
            seen.push(id);
            match parent {
                Some(parent) => println!("[{}] ↳ {}: {text}", short(&id), short(&parent)),
                None => println!("[{}] peer: {text}", short(&id)),
            }
        }
        // Espelho multi-device: mensagem que EU enviei de outro device (3.5).
        ChatEvent::SentElsewhere { to, message, .. } => {
            seen.push(message.id);
            println!(
                "[{}] eu → {}: {}",
                short(&message.id),
                to.to_base58(),
                message.text
            );
        }
        ChatEvent::DialFailed { peer, reason } => {
            let who = peer.map_or_else(|| "?".into(), |p| p.to_string());
            println!("falha ao conectar a {who}: {reason}");
        }
        ChatEvent::Route { peer, relayed } => {
            let route = if relayed { "via relay" } else { "direta" };
            println!("rota com {peer}: {route}");
        }
        ChatEvent::Nat(status) => println!("NAT: {status:?}"),
        ChatEvent::RelayReserved { relay } => println!("reserva aceita no relay {relay}"),
        ChatEvent::RelayLost { relay } => println!("perdi a reserva no relay {relay}"),
        ChatEvent::HolePunch { peer, result } => match result {
            Ok(()) => println!("hole punching com {peer}: ok"),
            Err(e) => println!("hole punching com {peer} falhou: {e}"),
        },
        ChatEvent::Delivered { message, .. } => println!("(entregue {})", short(&message)),
        ChatEvent::SendFailed {
            message, reason, ..
        } => {
            let id = message.map_or_else(|| "handshake".into(), |m| short(&m));
            println!("(falha no envio {id}: {reason})");
        }
        ChatEvent::Deposited { message, .. } => {
            println!("(depositada na caixa postal: {})", short(&message));
        }
        ChatEvent::MailboxRelay {
            relay,
            accepted,
            reason,
        } => {
            if accepted {
                println!("(relay {relay}: aceitou)");
            } else {
                println!("(relay {relay}: recusou: {reason})");
            }
        }
        ChatEvent::MailboxRelaySlow { relay } => {
            println!("(relay {relay}: demorando a responder, ainda esperando)");
        }
        ChatEvent::DepositFailed {
            message, reason, ..
        } => println!("(depósito recusado {}: {reason})", short(&message)),
        ChatEvent::Queued { message, .. } => {
            println!("(na fila, sai quando o peer conectar: {})", short(&message));
        }
        ChatEvent::Stale { peer } => {
            println!("(conversa com {peer} divergiu: faltam commits que não chegaram)");
        }
        ChatEvent::PeerRelays { peer, relays } => {
            println!("(relays de {peer}: {})", relays.join(", "));
        }
        ChatEvent::Dropped { peer, reason } => println!("(descartado de {peer}: {reason})"),
        ChatEvent::HistorySynced {
            peer,
            imported,
            done,
        } => {
            let end = if done { ", completo" } else { "" };
            println!("(histórico de {peer}: {imported} novas{end})");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, outgoing: bool, id: u8, parent: Option<u8>, text: &str) -> HistoryEntry {
        HistoryEntry {
            seq,
            outgoing,
            message: Message {
                id: MessageId::from_bytes([id; 16]),
                parent: parent.map(|p| MessageId::from_bytes([p; 16])),
                sent_at_ms: 0,
                text: text.to_string(),
            },
        }
    }

    #[test]
    fn history_without_arguments_uses_the_current_conversation_and_the_default_count() {
        assert_eq!(parse_history_args(""), Ok((String::new(), DEFAULT_HISTORY)));
    }

    #[test]
    fn history_with_only_digits_is_a_count() {
        assert_eq!(parse_history_args("5"), Ok((String::new(), 5)));
    }

    #[test]
    fn history_with_a_prefix_and_a_count() {
        assert_eq!(parse_history_args("3xK 7"), Ok(("3xK".to_string(), 7)));
    }

    #[test]
    fn history_with_only_a_prefix_uses_the_default_count() {
        assert_eq!(
            parse_history_args("3xK"),
            Ok(("3xK".to_string(), DEFAULT_HISTORY))
        );
    }

    #[test]
    fn history_with_a_numeric_prefix_needs_the_count_too() {
        assert_eq!(parse_history_args("12 4"), Ok(("12".to_string(), 4)));
    }

    #[test]
    fn history_count_above_the_cap_becomes_the_cap() {
        assert_eq!(parse_history_args("999"), Ok((String::new(), MAX_HISTORY)));
    }

    #[test]
    fn history_rejects_a_zero_or_non_numeric_count_and_extra_arguments() {
        assert!(parse_history_args("0").is_err());
        assert!(parse_history_args("3xK 0").is_err());
        assert!(parse_history_args("3xK abc").is_err());
        assert!(parse_history_args("3xK 5 9").is_err());
    }

    #[test]
    fn a_sent_entry_reads_voce_and_a_received_one_reads_peer() {
        assert_eq!(
            format_entry(&entry(1, true, 0xab, None, "oi")),
            "[abababab] você: oi"
        );
        assert_eq!(
            format_entry(&entry(2, false, 0xcd, None, "olá")),
            "[cdcdcdcd] peer: olá"
        );
    }

    #[test]
    fn a_reply_shows_the_short_id_of_its_parent() {
        assert_eq!(
            format_entry(&entry(3, false, 0xcd, Some(0xab), "sim")),
            "[cdcdcdcd] peer ↳ abababab: sim"
        );
        assert_eq!(
            format_entry(&entry(4, true, 0xef, Some(0xcd), "ok")),
            "[efefefef] você ↳ cdcdcdcd: ok"
        );
    }

    #[test]
    fn tail_keeps_the_last_n_in_the_original_order() {
        let all: Vec<_> = (1..=5)
            .map(|i| entry(i, true, i as u8, None, "x"))
            .collect();
        let last: Vec<u64> = tail(all, 2).iter().map(|e| e.seq).collect();
        assert_eq!(last, vec![4, 5]);
    }

    #[test]
    fn tail_of_a_short_or_empty_history_returns_all_of_it() {
        let all: Vec<_> = (1..=3)
            .map(|i| entry(i, true, i as u8, None, "x"))
            .collect();
        assert_eq!(tail(all, 10).len(), 3);
        assert!(tail(Vec::new(), 10).is_empty());
    }

    #[test]
    fn hints_trust_for_another_person() {
        let me = StandaloneIdentity::generate().id();
        let other = StandaloneIdentity::generate().id();
        assert!(should_hint_trust(&other, &me));
    }

    #[test]
    fn no_trust_hint_for_a_sibling() {
        let me = StandaloneIdentity::generate().id();
        assert!(!should_hint_trust(&me.clone(), &me));
    }

    #[test]
    fn identity_of_finds_the_identity_seen_for_that_peer() {
        let (a, b) = (PeerId::random(), PeerId::random());
        let (x, y) = (
            StandaloneIdentity::generate().id(),
            StandaloneIdentity::generate().id(),
        );
        let met = vec![(a, x.clone()), (b, y.clone())];
        assert_eq!(identity_of(&met, a), Some(x));
        assert_eq!(identity_of(&met, b), Some(y));
    }

    #[test]
    fn without_a_prefix_the_current_conversations_identity_is_chosen() {
        let (a, b) = (PeerId::random(), PeerId::random());
        let (x, y) = (
            StandaloneIdentity::generate().id(),
            StandaloneIdentity::generate().id(),
        );
        let met = vec![(a, x.clone()), (b, y.clone())];
        assert_eq!(resolve_identity("", Some(b), &met), Resolved::One(y));
        assert_eq!(resolve_identity("", Some(a), &met), Resolved::One(x));
    }

    #[test]
    fn without_a_prefix_and_without_a_conversation_nothing_is_chosen() {
        let met = vec![(PeerId::random(), StandaloneIdentity::generate().id())];
        assert_eq!(resolve_identity("", None, &met), Resolved::Unknown);
        assert_eq!(
            resolve_identity("", Some(PeerId::random()), &met),
            Resolved::Unknown
        );
    }

    #[test]
    fn a_unique_prefix_chooses_that_identity() {
        let (a, b) = (PeerId::random(), PeerId::random());
        let (x, y) = (
            StandaloneIdentity::generate().id(),
            StandaloneIdentity::generate().id(),
        );
        let met = vec![(a, x.clone()), (b, y)];
        // O base58 inteiro de `x` é um prefixo que só `x` tem.
        assert_eq!(
            resolve_identity(&x.to_base58(), None, &met),
            Resolved::One(x)
        );
    }

    #[test]
    fn a_prefix_shared_by_two_identities_is_ambiguous_and_one_matching_nothing_is_unknown() {
        let met = vec![
            (PeerId::random(), StandaloneIdentity::generate().id()),
            (PeerId::random(), StandaloneIdentity::generate().id()),
        ];
        let shared = &met[0].1.to_base58()[..4];
        assert!(met[1].1.to_base58().starts_with(shared));
        assert_eq!(resolve_identity(shared, None, &met), Resolved::Ambiguous);
        assert_eq!(resolve_identity("zzzzzz", None, &met), Resolved::Unknown);
    }

    #[test]
    fn two_devices_of_the_same_identity_are_one_choice() {
        let (a, b, c) = (PeerId::random(), PeerId::random(), PeerId::random());
        let (x, y) = (
            StandaloneIdentity::generate().id(),
            StandaloneIdentity::generate().id(),
        );
        // Não consecutivos: `dedup` sozinho não os juntaria.
        let met = vec![(a, x.clone()), (b, y), (c, x.clone())];
        assert_eq!(
            resolve_identity(&x.to_base58(), None, &met),
            Resolved::One(x)
        );
    }

    #[test]
    fn identity_of_is_none_for_a_peer_never_ready() {
        let met = vec![(PeerId::random(), StandaloneIdentity::generate().id())];
        assert_eq!(identity_of(&met, PeerId::random()), None);
        assert_eq!(identity_of(&[], PeerId::random()), None);
    }

    #[test]
    fn two_devices_of_one_identity_resolve_to_the_same_identity() {
        let (a, b) = (PeerId::random(), PeerId::random());
        let x = StandaloneIdentity::generate().id();
        let met = vec![(a, x.clone()), (b, x.clone())];
        assert_eq!(identity_of(&met, a), identity_of(&met, b));
        assert_eq!(identity_of(&met, a), Some(x));
    }
}
