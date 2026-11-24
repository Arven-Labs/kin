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
//! O link do `/invite` é de **uso único** (P18, ADR 0039): `/invites` lista os vivos (id e tempo restante) e
//! `/revoke-invite ID` cancela um antes do uso (`/revoke` é outra coisa: revoga um device).
//! O link leva **só endereços de circuito de relay** (P18 b): não revela o seu IP. `/invite --direct`
//! inclui também o IP direto (`--external-addr`, escuta global).
//! **Portão de admissão, fechado por padrão** (P18 PR 3b): quem não é conhecido só abre conversa se
//! apresentar o token de um convite seu. Conhecido é quem já tem conversa, quem eu aceitei por link, quem
//! eu disquei com `--dial` **com o Peer ID** (`/p2p/<id>`; sem ele ninguém é admitido) e os devices de uma
//! identidade minha ou aceita por `/trust`. `--open-admission` abre mão do portão. Dois devices que só se
//! acharam por mDNS não passam sozinhos: usam `--dial` com Peer ID ou um link.
//! `--require-approval` (P18 d, só com o portão fechado) segura quem chega com um link vivo como pedido
//! pendente: `/allow PEER` abre a conversa e `/deny PEER` recusa **sem revogar o link** (para isso,
//! `/revoke-invite`). PEER é o Peer ID inteiro ou um prefixo único de 12+ caracteres; sem ele, lista os pedidos e não age.
//! `/trust [prefixo]` aceita o contato da conversa atual (ou a identidade vista que começa com o
//! prefixo): sem isso quem convidou não processa a lista de devices dele. A escolha é gravada.
//! `/untrust [prefixo]` desfaz: o contato deixa de receber o meu anúncio e a sua lista de devices
//! deixa de valer (um irmão que o repasse não o faz voltar); a conversa e as mensagens seguem.
//! No primário (diretório com `authority.key`): `kin pair answer` registra cada certificado emitido em
//! `issued/`; `/devices` lista esses devices (ativo, revogado, conectado) e `/revoke PREFIXO` mostra qual
//! é, e `/revoke PREFIXO confirmo` o revoga (tombstone assinado pela autoridade, enviado a quem aceita a
//! identidade; ADR 0034). Não revoga o device do próprio primário.
//! Device pareado sem copiar a mestra: `kin pair request/answer/accept` (ADR 0020) e depois `kin --data-dir DIR`
//! normal; o diretório com `primary.id` abre como device da identidade do primário (ADR 0021).
//! O `/pair` antigo (copiar a chave mestra por `kin://pair/...` e `--pair`) foi **removido** (ADR 0035): a CLI no modo
//! `Master` (mestra neste device, v3) avisa e mostra o roteiro de migração para a identidade nova; `/pair` imprime o mesmo
//! caminho (`kin cold init` + `kin pair`). A trava de envio continua barrando um `kin://pair/` colado por engano.
//! Linhas digitadas vão à identidade da conversa atual (a todos os devices dela) e são espelhadas
//! nos meus outros devices (`eu → ...`);
//! `/history [prefixo] [N]` mostra as últimas N linhas (padrão 20, teto 200) do histórico local com a
//! conversa atual (ou a identidade que começa com o prefixo), inclusive o que um irmão sincronizou;
//! `/r <id> texto` responde à mensagem `<id>` (thread); `/quit` sai.

use std::path::{Path, PathBuf};

mod cold;
mod devices;
mod keys;
mod pair;
mod passwd;

use kin_chat::nostr::{NostrMailbox, RelaySource, resolve_relays};
use kin_chat::{
    Chat, ChatEvent, DEFAULT_INVITE_TTL, Error, HistoryEntry, Invite, LiveInvite, Message,
    MessageId,
};
use kin_identity::{DeviceKey, IdentityId};
use kin_transport::{Multiaddr, Node, NodeConfig, NodeEvent, PeerId, RelayLimits};
use tokio::io::{AsyncBufReadExt, BufReader};

struct Args {
    keys: keys::KeyOptions,
    data_dir: PathBuf,
    listen: Option<Multiaddr>,
    dial: Option<Multiaddr>,
    accept: Option<String>,
    relays: Vec<Multiaddr>,
    relay_count: Option<usize>,
    external_addrs: Vec<Multiaddr>,
    nostr_relays: Vec<String>,
    no_nostr: bool,
    serve_relay: bool,
    relay_limits: RelayLimits,
    mdns: bool,
    /// `--open-admission`: abre mão do portão (P18 PR 3b, ADR 0039). Sem ela, só abre conversa quem é
    /// conhecido (conversa, convite aceito, `--dial` com Peer ID, identidade minha ou aceita) ou apresenta
    /// o token de um convite meu.
    open_admission: bool,
    /// `--require-approval` (P18 d, ADR 0039): quem chega com um token vivo vira pedido pendente até `/allow`.
    /// Só vale com o portão fechado: com `--open-admission` é erro de uso.
    require_approval: bool,
}

/// O aviso de conversa refeita (P16, auditoria do PR #254, médio 3). O sinal "o outro lado perdeu o estado"
/// não é verificável (quem tem as chaves de identidade o produz igual), então o aviso diz **quem** e pede
/// a confirmação por outro canal.
fn reset_notice(peer: &str, identity: &str) -> String {
    format!(
        "(conversa com {peer} refeita: a identidade {identity} disse que perdeu o estado da conversa; \
         se você não esperava isso, confirme por outro canal que é ela mesma; \
         mensagens que enviei sem ela perceber podem ter sido perdidas)"
    )
}

/// P34: o portão fechado recusou `peer`. Só local (nada vai ao peer): diz **quem** e os caminhos para admitir.
fn not_admitted_notice(peer: &str) -> String {
    format!(
        "(peer {peer} conectou e não admitido: o portão está fechado; para abrir conversa mande a ele um /invite \
         meu, use /accept com o link dele, ou --dial com o Peer ID (/p2p/<id>); --open-admission abre o portão)"
    )
}

/// Em que modo de admissão o kin subiu (auditoria do PR 3b, baixo 1): o portão fechado recusa estranhos
/// em silêncio, então o usuário precisa saber que ele está ligado, ou desligado.
fn admission_notice(open_admission: bool) -> &'static str {
    if open_admission {
        "aviso: admissão aberta (--open-admission): qualquer peer que conectar abre conversa"
    } else {
        "admissão: fechada; abre conversa quem tem conversa, convite seu, --dial com Peer ID ou identidade aceita (--open-admission abre)"
    }
}

// ---- aprovação do convidante (P18 d, ADR 0039) ----

/// Menor prefixo de Peer ID aceito por `/allow` e `/deny`. Todo Peer ID ed25519 começa por `12D3KooW`
/// (8 caracteres), então um prefixo menor que isto não distingue ninguém e abriria espaço para um
/// Peer ID forçado a casar (auditoria do PR #278, M1).
const MIN_REQUEST_PREFIX: usize = 12;

/// Qual pedido pendente o `/allow` ou `/deny` atinge: `Ok(peer)` ou o texto a imprimir. **Sempre exige o
/// Peer ID** (inteiro ou prefixo de pelo menos [`MIN_REQUEST_PREFIX`] caracteres que só um pedido tem):
/// agir "no único pedido" aprovaria quem estivesse pendente na hora de digitar, e não quem o dono viu no
/// aviso (o pedido some se a conexão cai, e outro portador do link pode ocupar o lugar). Sem pedido, sem
/// argumento, prefixo curto, ambíguo ou desconhecido: lista os pedidos e não age.
fn pick_request(prefix: &str, pending: &[(PeerId, IdentityId)]) -> Result<PeerId, String> {
    if pending.is_empty() {
        return Err("(nenhum pedido de entrada pendente)".to_string());
    }
    if prefix.len() >= MIN_REQUEST_PREFIX {
        let mut matching = pending
            .iter()
            .filter(|(peer, _)| peer.to_string().starts_with(prefix));
        if let (Some((peer, _)), None) = (matching.next(), matching.next()) {
            return Ok(*peer);
        }
    }
    let listing = pending
        .iter()
        .map(|(peer, identity)| format!("  {peer} (identidade {})", identity.to_base58()))
        .collect::<Vec<_>>()
        .join("\n");
    Err(format!(
        "(pedidos de entrada pendentes; informe o Peer ID inteiro ou um prefixo único de pelo menos {MIN_REQUEST_PREFIX} caracteres)\n{listing}"
    ))
}

/// Resposta de `/allow` a partir do resultado de `Chat::approve`.
fn allow_reply(peer: &str, result: Result<bool, Error>) -> String {
    match result {
        Ok(true) => format!("{peer} aprovado: retomando a conversa"),
        Ok(false) => format!(
            "(sem pedido pendente de {peer}, ou o convite dele já não vale (gasto ou revogado): nada foi aprovado)"
        ),
        Err(e) => format!("(não deu para aprovar {peer}: {e})"),
    }
}

/// Resposta de `/deny` a partir do resultado de `Chat::deny`. O link segue vivo: quem quer encerrá-lo usa
/// `/revoke-invite` (decisão do dono, 2026-11-21).
fn deny_reply(peer: &str, denied: bool) -> String {
    if denied {
        format!(
            "{peer} negado: sem conversa; o link segue vivo (/revoke-invite ID o cancela, veja os ids com /invites)"
        )
    } else {
        format!("(nenhum pedido pendente de {peer})")
    }
}

/// Aviso de partida quando a aprovação está ligada.
fn approval_notice() -> &'static str {
    "aprovação: ligada (--require-approval); quem chegar com um convite meu espera o meu /allow ou /deny"
}

/// Aviso de um pedido de entrada novo (`ChatEvent::AdmissionRequested`): quem (peer e identidade que ele
/// apresentou) e como responder.
fn admission_requested_notice(peer: &str, identity: &str) -> String {
    format!(
        "(pedido de entrada de {peer}, identidade {identity} (apresentada pelo próprio peer, não verificada: \
         quem tiver o link gera uma; só aprove se você espera esta pessoa agora): /allow {peer} abre a conversa, \
         /deny {peer} recusa; o link segue vivo se você negar)"
    )
}

fn parse_args() -> Result<Args, String> {
    parse_from(
        std::env::args().skip(1),
        std::env::var("KIN_PASSPHRASE_FILE").ok(),
    )
}

/// `env_passphrase_file`: valor de `KIN_PASSPHRASE_FILE` (vazio conta como ausente). A flag
/// `--passphrase-file` vence a variável, e `--no-passphrase` a ignora.
fn parse_from(
    argv: impl IntoIterator<Item = String>,
    env_passphrase_file: Option<String>,
) -> Result<Args, String> {
    let mut args = Args {
        keys: keys::KeyOptions::default(),
        data_dir: PathBuf::from(".kin"),
        listen: None,
        dial: None,
        accept: None,
        relays: Vec::new(),
        relay_count: None,
        external_addrs: Vec::new(),
        nostr_relays: Vec::new(),
        no_nostr: false,
        serve_relay: false,
        relay_limits: RelayLimits::default(),
        mdns: true,
        open_admission: false,
        require_approval: false,
    };
    let mut it = argv.into_iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} precisa de um valor"));
        match flag.as_str() {
            "--data-dir" => args.data_dir = value()?.into(),
            "--listen" => args.listen = Some(value()?.parse().map_err(|e| format!("{e}"))?),
            "--dial" => args.dial = Some(value()?.parse().map_err(|e| format!("{e}"))?),
            "--accept" => args.accept = Some(value()?),
            "--passphrase-file" => args.keys.passphrase_file = Some(value()?.into()),
            "--protect" => args.keys.protect = true,
            "--no-passphrase" => args.keys.no_passphrase = true,
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
            "--open-admission" => args.open_admission = true,
            "--require-approval" => args.require_approval = true,
            other => return Err(format!("argumento desconhecido: {other}")),
        }
    }
    // A biblioteca ignora a aprovação com o portão aberto; aqui seria falsa sensação de proteção.
    if args.require_approval && args.open_admission {
        return Err(
            "--require-approval não vale com --open-admission (a aprovação só existe com o portão fechado)"
                .to_string(),
        );
    }
    if args.keys.passphrase_file.is_none() && !args.keys.no_passphrase {
        args.keys.passphrase_file = env_passphrase_file
            .filter(|path| !path.is_empty())
            .map(PathBuf::from);
    }
    Ok(args)
}

/// Carrega a identidade e a chave de device do diretório, criando-as na primeira execução (P13,
/// ADR 0012). Por padrão ficam **cifradas** com uma senha (prompt, `--passphrase-file` ou
/// `KIN_PASSPHRASE_FILE`); `--no-passphrase` grava em claro; `--protect` cifra no lugar um diretório
/// antigo em claro. Com senha, também cria e abre o `db.key` dos bancos; `--protect` cifra os bancos
/// em claro (P13 fatia 3, ADR 0045). Com `--serve-relay` isso não acontece: o relay não abre banco, e o
/// terceiro valor devolvido é sempre `None`.
///
/// `params` é o custo do Argon2 de tudo o que for criado ou re-selado aqui (`DEFAULT` em produção; os
/// testes passam `MIN`).
fn open_keys(
    args: &Args,
    params: kin_identity::KdfParams,
) -> Result<(keys::LocalIdentity, DeviceKey, Option<kin_identity::DbKey>), Box<dyn std::error::Error>>
{
    let dir = &args.data_dir;
    std::fs::create_dir_all(dir)?;
    let state = keys::files_state(dir)?;
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let access = keys::decide(&args.keys, state, tty)?;
    let (passphrase, protect) = match &access {
        keys::Access::Plain(keys::PlainReason::Explicit) => {
            eprintln!("aviso: --no-passphrase: as chaves ficam em claro no disco (só 0600)");
            (None, false)
        }
        keys::Access::Plain(keys::PlainReason::Legacy) => {
            eprintln!(
                "aviso: as chaves deste --data-dir estão em claro; rode uma vez com --protect para cifrá-las"
            );
            (None, false)
        }
        keys::Access::Sealed { source, protect } => {
            let pass = match source {
                keys::Source::File(path) => keys::read_passphrase_file(path)?,
                keys::Source::Prompt { confirm } => prompt_passphrase(*confirm)?,
            };
            (Some(pass), *protect)
        }
    };
    if protect {
        eprintln!("{}", protect_warning(args.serve_relay));
    }
    if passphrase.is_some() {
        eprintln!("{}", kdf_notice(protect));
    }
    let loaded = keys::load(
        dir,
        passphrase.as_ref().map(|p| p.as_slice()),
        protect,
        params,
    )?;
    // O relay não abre banco: nem cria, nem lê, nem migra o `db.key` (um `db.key` ausente ou ilegível
    // não pode impedi-lo de subir). `None` aqui só quer dizer "não aplicável"; `main` não o usa no relay.
    let db_key = if args.serve_relay {
        None
    } else {
        keys::open_db_key(
            dir,
            passphrase.as_ref().map(|p| p.as_slice()),
            protect,
            params,
        )?
    };
    // O pareado só se anuncia depois de o `Chat::open_paired` e o `ensure_current_issuer` passarem (em
    // `main`): antes disso ainda pode ser um pareamento pendente ou um banco v3.
    if matches!(loaded.0, keys::LocalIdentity::Primary(_)) {
        println!(
            "primário da identidade {} (chave de autoridade; a mestra não está neste diretório)",
            loaded.0.id().to_base58()
        );
        eprintln!(
            "aviso: o certificado deste device é v4; builds antigos do kin não o leem (atualize os outros devices)"
        );
    }
    Ok((loaded.0, loaded.1, db_key))
}

/// Aviso do `--protect`: o arquivo em claro antigo pode continuar no disco. O relay só cifra as chaves
/// (não toca os bancos), então o texto dele não fala de bancos nem de histórico.
fn protect_warning(relay: bool) -> String {
    let (what, exposed) = if relay {
        ("chaves", "a chave como exposta")
    } else {
        ("chaves e bancos", "a chave e o histórico como expostos")
    };
    format!(
        "aviso: o arquivo em claro antigo ({what}) pode continuar recuperável no disco (SSD, snapshot, backup); se o disco ou o backup não for confiável, trate {exposed}"
    )
}

/// Aviso antes do Argon2id (P13): a derivação leva segundos de CPU sem nenhuma saída, e quem aperta
/// Ctrl-C achando que travou, com `--protect`, deixa as chaves seladas e os bancos ainda em claro.
fn kdf_notice(protect: bool) -> String {
    let base = "derivando a chave da senha (Argon2id); pode levar alguns segundos...";
    if protect {
        format!(
            "{base} não interrompa: um Ctrl-C no meio deixa as chaves cifradas e os bancos em claro; se acontecer, rode --protect de novo"
        )
    } else {
        base.to_string()
    }
}

/// Pede a senha no terminal, sem eco; `confirm` pede de novo e exige que conferam. Normalizada em NFKC.
fn prompt_passphrase(
    confirm: bool,
) -> Result<zeroize::Zeroizing<Vec<u8>>, Box<dyn std::error::Error>> {
    let first = zeroize::Zeroizing::new(rpassword::prompt_password("senha: ")?);
    if first.is_empty() {
        return Err("a senha não pode ser vazia".into());
    }
    if confirm {
        let second = zeroize::Zeroizing::new(rpassword::prompt_password("repita a senha: ")?);
        if *first != *second {
            return Err("as senhas não conferem".into());
        }
    }
    Ok(kin_identity::normalize_passphrase(&first))
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
/// parcial: um segredo truncado travaria a próxima criação ("já existe") e a abertura normal.
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

/// P35: "entregue" é o ack de transporte de **um** device; sem o Peer ID não dá para saber qual (o fan-out
/// manda uma cópia por device). O prefixo `(entregue ` fica, o lab e o dono já o conhecem.
fn delivered_notice(message: &str, peer: &str) -> String {
    format!("(entregue {message} a {peer})")
}

/// P35: a cópia para `peer` ficou na fila. O prefixo `(na fila, sai quando o peer conectar` é procurado
/// pelo lab (`run.sh`) e fica.
fn queued_notice(message: &str, peer: &str) -> String {
    format!("(na fila, sai quando o peer conectar: {message} para {peer})")
}

/// P35: o envio de `message` (ou do handshake) a `peer` falhou.
fn send_failed_notice(message: &str, peer: &str, reason: &str) -> String {
    format!("(falha no envio {message} a {peer}: {reason})")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `kin cold ...` (P30 fatia 1b): custódia offline da mestra, sem rede nem chat.
    if std::env::args().nth(1).as_deref() == Some("cold") {
        let command = cold::parse(std::env::args().skip(2)).map_err(|e| {
            eprintln!(
                "{e}\nuso: kin cold init --master-out ARQUIVO --authority-dir DIR [--master-passphrase-file ARQ] [--passphrase-file ARQ]\n     kin cold rotate-authority --master ARQUIVO --authority-dir DIR [--master-passphrase-file ARQ] [--passphrase-file ARQ]"
            );
            e
        })?;
        return cold::run(command).map_err(Into::into);
    }
    // `kin pair ...` (P30 fatia 2d-ii-a2): pareamento autenticado por links de texto, sem rede nem chat.
    if std::env::args().nth(1).as_deref() == Some("pair") {
        let command = pair::parse(std::env::args().skip(2)).map_err(|e| {
            eprintln!(
                "{e}\nuso: kin pair request --data-dir DIR --identity BASE58 [--passphrase-file ARQ]\n     kin pair answer --data-dir DIR_DO_PRIMARIO --request ARQUIVO [--passphrase-file ARQ]\n     kin pair accept --data-dir DIR --certificate ARQUIVO|- [--passphrase-file ARQ]"
            );
            e
        })?;
        return pair::run(command).map_err(Into::into);
    }
    // `kin passwd ...` (P13): troca a senha dos arquivos de chave, sem rede nem chat.
    if std::env::args().nth(1).as_deref() == Some("passwd") {
        let command = passwd::parse(std::env::args().skip(2)).map_err(|e| {
            eprintln!(
                "{e}\nuso: kin passwd [--data-dir DIR] [--passphrase-file ARQ_DA_SENHA_ATUAL] [--new-passphrase-file ARQ_DA_SENHA_NOVA]"
            );
            e
        })?;
        return passwd::run(command).map_err(Into::into);
    }
    let args = parse_args().map_err(|e| {
        eprintln!(
            "{e}\nuso: kin [--data-dir DIR] [--listen ADDR] [--dial ADDR] [--relay ADDR]... [--nostr-relay URL]... [--no-nostr] [--no-mdns] [--open-admission | --require-approval]\n     kin --serve-relay [--listen ADDR] [--relay-max-bytes N] [--relay-max-circuits N]"
        );
        e
    })?;
    let (identity, device, db_key) = open_keys(&args, kin_identity::KdfParams::DEFAULT)?;
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
    let db = args.data_dir.join("mls.sqlite");
    let mut chat = match identity.provider() {
        Some(provider) => Chat::open(provider, &device, config, &db, db_key.as_ref())?,
        // Device pareado (ADR 0021): sem provider; o certificado v4 já está no banco e é conferido
        // contra a identidade de `primary.id` a cada abertura.
        None => Chat::open_paired(&identity.id(), &device, config, &db, db_key.as_ref())?,
    };
    chat.set_require_invite(!args.open_admission);
    chat.set_require_approval(args.require_approval);
    keys::ensure_current_issuer(&identity, chat.certificate())?;
    if let Some(notice) = keys::notice_for(&identity) {
        eprintln!("{notice}");
    }
    if matches!(identity, keys::LocalIdentity::Paired(_)) {
        println!(
            "device pareado da identidade {} (certificado v4 do primário; sem mestra nem autoridade aqui)",
            identity.id().to_base58()
        );
        eprintln!(
            "aviso: o certificado deste device é v4; builds antigos do kin não o leem (atualize os outros devices)"
        );
    }
    println!(
        "kin {} — peer id: {}",
        env!("CARGO_PKG_VERSION"),
        chat.peer_id()
    );
    println!("{}", admission_notice(args.open_admission));
    if args.require_approval {
        println!("{}", approval_notice());
    }
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
                if let Some(scope) = parse_invite(line) {
                    match scope {
                        Ok(scope) => print_invite(&mut chat, &scope),
                        Err(usage) => println!("({usage})"),
                    }
                    continue;
                }
                if line == "/invites" {
                    println!("{}", invites_reply(&chat.invites(), now_secs()));
                    continue;
                }
                if line == "/revoke-invite" || line.starts_with("/revoke-invite ") {
                    let prefix = line.strip_prefix("/revoke-invite").unwrap_or("").trim();
                    println!("{}", revoke_invite_reply(prefix, chat.revoke_invite(prefix)));
                    continue;
                }
                if line == "/allow" || line.starts_with("/allow ") {
                    let prefix = line.strip_prefix("/allow").unwrap_or("").trim();
                    match pick_request(prefix, &chat.pending_approvals()) {
                        Ok(peer) => println!("{}", allow_reply(&peer.to_string(), chat.approve(peer))),
                        Err(listing) => println!("{listing}"),
                    }
                    continue;
                }
                if line == "/deny" || line.starts_with("/deny ") {
                    let prefix = line.strip_prefix("/deny").unwrap_or("").trim();
                    match pick_request(prefix, &chat.pending_approvals()) {
                        Ok(peer) => println!("{}", deny_reply(&peer.to_string(), chat.deny(peer))),
                        Err(listing) => println!("{listing}"),
                    }
                    continue;
                }
                if line == "/pair" || line == "/pair confirmo" {
                    println!("{}", pair_reply(&identity));
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
                if line == "/history" || line.starts_with("/history ") {
                    history(&chat, line.strip_prefix("/history").unwrap_or("").trim(), current, &met);
                    continue;
                }
                if line == "/devices" {
                    show_devices(&chat, &identity, &args.data_dir);
                    continue;
                }
                if line == "/revoke" || line.starts_with("/revoke ") {
                    revoke_device(
                        &mut chat,
                        &identity,
                        &args.data_dir,
                        line.strip_prefix("/revoke").unwrap_or("").trim(),
                    );
                    continue;
                }
                if line == "/untrust" || line.starts_with("/untrust ") {
                    untrust(&mut chat, line.strip_prefix("/untrust").unwrap_or("").trim(), current, &met);
                    continue;
                }
                // A chave mestra nunca vai a um contato por engano (ex.: Ctrl+V do link de pareamento).
                if is_pair_link(line) {
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

/// Quais endereços o link de `/invite` leva (P18 b).
#[derive(Debug, PartialEq)]
enum InviteScope {
    /// Só circuito de relay: o link não revela o IP de quem o gera (padrão).
    ViaRelay,
    /// Também o IP direto (`--external-addr`, escuta global): `/invite --direct`.
    Direct,
}

/// Lê `/invite` e `/invite --direct`. `None` se a linha não é o comando `/invite` (`/invites` não é);
/// `Some(Err(uso))` se tem argumento que não conheço.
fn parse_invite(line: &str) -> Option<Result<InviteScope, String>> {
    let args = line.strip_prefix("/invite")?;
    if args.is_empty() {
        return Some(Ok(InviteScope::ViaRelay));
    }
    // Só há comando se o nome acaba ali: `/invites` e `/invite-x` são outros comandos.
    let args = args.strip_prefix(' ')?;
    Some(match args.trim() {
        "--direct" => Ok(InviteScope::Direct),
        _ => Err("uso: /invite [--direct] (--direct põe o IP direto no link)".to_string()),
    })
}

/// Imprime o convite deste nó (link e QR). Sem endereço compartilhável ainda, avisa.
fn print_invite(chat: &mut Chat, scope: &InviteScope) {
    let invite = match scope {
        InviteScope::ViaRelay => chat.shareable_invite_via_relay(),
        InviteScope::Direct => chat.shareable_invite(),
    };
    if invite.addrs().is_empty() {
        println!(
            "(sem endereço compartilhável ainda: espere a reserva no relay; para pôr o IP direto no \
             link use /invite --direct)"
        );
        return;
    }
    let link = match chat.invite_link(invite.addrs().to_vec(), DEFAULT_INVITE_TTL) {
        Ok(link) => link,
        Err(e) => {
            println!("(não deu para gerar o convite: {e})");
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
    println!(
        "convite (válido por {} h, uso único): {link}",
        DEFAULT_INVITE_TTL.as_secs() / 3600
    );
}

/// Segundos desde a época Unix (0 se o relógio do sistema falhar).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Resposta de `/invites`: um convite vivo por linha, com o `id` que `/revoke-invite` aceita e quanto
/// falta para vencer; ou o aviso de que não há nenhum.
fn invites_reply(invites: &[LiveInvite], now: u64) -> String {
    if invites.is_empty() {
        return "(nenhum convite vivo)".to_string();
    }
    invites
        .iter()
        .map(|invite| {
            let left = time_left(invite.expires_at.saturating_sub(now));
            format!("convite {}: vence em {left}", invite.id)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tempo restante em linguagem humana: `23 h 59 min`, `5 min` ou `menos de 1 min`.
fn time_left(secs: u64) -> String {
    match (secs / 3600, secs % 3600 / 60) {
        (0, 0) => "menos de 1 min".to_string(),
        (0, minutes) => format!("{minutes} min"),
        (hours, minutes) => format!("{hours} h {minutes} min"),
    }
}

/// Resposta de `/revoke-invite PREFIXO` a partir do resultado de `Chat::revoke_invite`.
fn revoke_invite_reply(prefix: &str, result: Result<bool, Error>) -> String {
    match result {
        Ok(true) => format!("convite {prefix} revogado"),
        Ok(false) => format!("(nenhum convite vivo começa por {prefix})"),
        // Curto demais (mínimo de 4 caracteres) ou que bate em mais de um: a biblioteca não distingue.
        Err(Error::InvalidInvite) => "(prefixo curto demais (mínimo de 4 caracteres) ou que bate em mais de um convite: veja os ids com /invites)".to_string(),
        Err(e) => format!("(não deu para revogar: {e})"),
    }
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

/// Resposta a `/pair` depois que a cópia da mestra foi removida (P30 fatia 6, ADR 0035).
const PAIR_REMOVED_MASTER: &str = "(o /pair foi removido: ele copiava a chave mestra para outro device, e copiar o identity.key à mão é a mesma \
coisa: essa mestra nunca revoga nada. Para ter vários devices é preciso uma identidade NOVA (outro ID): `kin cold init`, \
depois `kin pair request`, `kin pair answer` e `kin pair accept`, e reconfirmar com cada contato. Veja o aviso de migração \
da abertura.)";
const PAIR_REMOVED_PRIMARY: &str = "(o /pair foi removido. Para adicionar um device a este primário: no device novo, `kin pair request`; \
aqui, `kin pair answer`; e de volta no device novo, `kin pair accept`. Veja também /devices e /revoke.)";
const PAIR_REMOVED_PAIRED: &str = "(o /pair foi removido. Este é um device pareado: quem adiciona e revoga devices é o primário, \
com `kin pair answer`.)";

/// A resposta a `/pair` para o modo em que a CLI está: cada um tem um caminho diferente, e mandar o
/// primário rodar `kin cold init` criaria uma segunda identidade por engano (auditoria da fatia 6, I1).
fn pair_reply(identity: &keys::LocalIdentity) -> &'static str {
    match identity {
        keys::LocalIdentity::Master(_) => PAIR_REMOVED_MASTER,
        keys::LocalIdentity::Primary(_) => PAIR_REMOVED_PRIMARY,
        keys::LocalIdentity::Paired(_) => PAIR_REMOVED_PAIRED,
    }
}

/// A linha parece um link de pareamento antigo (`kin://pair/`, que carregava a chave mestra)? A trava de
/// envio a barra mesmo sem o `/pair`: links velhos ainda podem circular (ADR 0035).
fn is_pair_link(line: &str) -> bool {
    line.contains("kin://pair/")
}

const NOT_PRIMARY: &str = "(só o primário lista e revoga devices: rode a CLI no diretório com authority.key; no modo antigo da mestra, revogar não adianta, pois cada device tem a mestra)";

/// `/devices`: os devices que este primário emitiu (`issued/`), com estado e se estão conectados.
fn show_devices(chat: &Chat, identity: &keys::LocalIdentity, data_dir: &Path) {
    if !matches!(identity, keys::LocalIdentity::Primary(_)) {
        println!("{NOT_PRIMARY}");
        return;
    }
    let issued = devices::list(data_dir, &identity.id());
    if issued.is_empty() {
        println!("(nenhum device emitido por este primário; veja `kin pair answer`)");
        return;
    }
    for certificate in &issued {
        match chat.is_revoked(certificate) {
            Ok(revoked) => println!(
                "{}",
                devices::describe(
                    certificate,
                    revoked,
                    chat.is_ready(&certificate.network_peer_id())
                )
            ),
            Err(e) => {
                println!("(não consegui ler as revogações: {e})");
                return;
            }
        }
    }
}

/// `/revoke PREFIXO [confirmo]`: o primário assina o tombstone do device e o `Chat` o grava e envia.
/// Irreversível, por isso só com `confirmo`.
fn revoke_device(chat: &mut Chat, identity: &keys::LocalIdentity, data_dir: &Path, args: &str) {
    let keys::LocalIdentity::Primary(primary) = identity else {
        println!("{NOT_PRIMARY}");
        return;
    };
    let issued = devices::list(data_dir, &identity.id());
    match devices::decide_revoke(args, &issued, &chat.peer_id()) {
        Err(e) => println!("({e})"),
        Ok(devices::RevokeAction::Ask(certificate)) => println!(
            "revogar o device {peer} (emitido em {})? Não dá para desfazer. Para confirmar: /revoke {peer} confirmo",
            certificate.created_at,
            peer = certificate.network_peer_id(),
        ),
        Ok(devices::RevokeAction::Revoke(certificate)) => {
            let result = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .map_err(|e| format!("relógio do sistema inválido: {e}"))
                .and_then(|now| {
                    primary
                        .revoke_device(certificate, now)
                        .map_err(|e| e.to_string())
                })
                .and_then(|revocation| {
                    chat.broadcast_revocation(&revocation)
                        .map_err(|e| e.to_string())
                });
            match result {
                Ok(()) => println!(
                    "(device {} revogado: tombstone gravado e enviado a quem está conectado; quem estiver offline só saberá quando reconectar)",
                    certificate.network_peer_id()
                ),
                Err(e) => println!("(não revoguei: {e})"),
            }
        }
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
const DEFAULT_HISTORY: usize = 20;
const MAX_HISTORY: usize = 200;
/// Linhas lidas por vez de `Chat::history` (a API pagina do mais antigo ao mais novo).
const HISTORY_PAGE: usize = 256;

/// Argumentos do `/history`: `[prefixo] [N]`. Um único argumento só de dígitos é `N`; para usar um
/// prefixo que seja só dígitos, passe também o `N`. `N` acima de `MAX_HISTORY` vira o teto; `N = 0`
/// ou lixo é erro.
fn parse_history_args(rest: &str) -> Result<(String, usize), String> {
    let count = |text: &str| match text.parse::<usize>() {
        Ok(0) | Err(_) => Err(format!("N inválido: {text}")),
        Ok(n) => Ok(n.min(MAX_HISTORY)),
    };
    let all_digits = |text: &str| text.chars().all(|c| c.is_ascii_digit());
    match rest.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] => Ok((String::new(), DEFAULT_HISTORY)),
        [one] if all_digits(one) => Ok((String::new(), count(one)?)),
        [prefix] => Ok(((*prefix).to_string(), DEFAULT_HISTORY)),
        [prefix, n] => Ok(((*prefix).to_string(), count(n)?)),
        _ => Err("uso: /history [prefixo] [N]".to_string()),
    }
}

/// Linha do `/history`: `[id8] você: texto` / `[id8] peer: texto`, com `↳ pai8` entre o autor e o
/// texto quando a mensagem responde a outra.
fn format_entry(entry: &HistoryEntry) -> String {
    let author = if entry.outgoing { "você" } else { "peer" };
    let message = &entry.message;
    match &message.parent {
        Some(parent) => format!(
            "[{}] {author} ↳ {}: {}",
            short(&message.id),
            short(parent),
            message.text
        ),
        None => format!("[{}] {author}: {}", short(&message.id), message.text),
    }
}

/// As últimas `n` linhas, na ordem original.
fn tail(mut entries: Vec<HistoryEntry>, n: usize) -> Vec<HistoryEntry> {
    let skip = entries.len().saturating_sub(n);
    entries.drain(..skip);
    entries
}

/// `/history`: as últimas linhas da conversa com a identidade escolhida (como em `/trust`). Lê o
/// histórico inteiro por cursor, mantendo só a cauda, porque a API pagina do começo.
fn history(chat: &Chat, rest: &str, current: Option<PeerId>, met: &[(PeerId, IdentityId)]) {
    let (prefix, n) = match parse_history_args(rest) {
        Ok(parsed) => parsed,
        Err(e) => {
            println!("({e})");
            return;
        }
    };
    let identity = match resolve_identity(&prefix, current, met) {
        Resolved::One(identity) => identity,
        Resolved::Unknown => {
            println!("(nenhuma identidade conhecida assim; veja `conversa cifrada pronta`)");
            return;
        }
        Resolved::Ambiguous => {
            println!("(prefixo ambíguo: digite mais caracteres)");
            return;
        }
    };
    let mut last = Vec::new();
    let mut after = None;
    loop {
        let page = match chat.history(&identity, after, HISTORY_PAGE) {
            Ok(page) => page,
            Err(e) => {
                println!("(não consegui ler o histórico: {e})");
                return;
            }
        };
        let Some(newest) = page.last().map(|entry| entry.seq) else {
            break;
        };
        after = Some(newest);
        last.extend(page);
        last = tail(last, n);
    }
    println!(
        "(histórico com {}: {} linhas)",
        identity.to_base58(),
        last.len()
    );
    for entry in &last {
        println!("{}", format_entry(entry));
    }
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
        ChatEvent::ConversationReset { peer, identity } => {
            println!("{}", reset_notice(&peer.to_string(), &identity.to_base58()));
        }
        ChatEvent::NotAdmitted { peer } => {
            println!("{}", not_admitted_notice(&peer.to_string()));
        }
        // O evento só existe com `--require-approval` (`set_require_approval`).
        ChatEvent::AdmissionRequested { peer, identity } => {
            println!(
                "{}",
                admission_requested_notice(&peer.to_string(), &identity.to_base58())
            );
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
        ChatEvent::Delivered { peer, message } => {
            println!("{}", delivered_notice(&short(&message), &peer.to_string()));
        }
        ChatEvent::SendFailed {
            peer,
            message,
            reason,
        } => {
            let id = message.map_or_else(|| "handshake".into(), |m| short(&m));
            println!("{}", send_failed_notice(&id, &peer.to_string(), &reason));
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
        ChatEvent::Queued { peer, message } => {
            println!("{}", queued_notice(&short(&message), &peer.to_string()));
        }
        ChatEvent::Stale { peer, recovering } => {
            if recovering {
                println!(
                    "(conversa com {peer} divergiu: faltam commits que não chegaram; a conversa foi abandonada e o kin reconecta para refazê-la com a mesma identidade; o que estava em voo pode se perder e a fila segue; no máximo uma vez a cada 10 min por contato)"
                );
            } else {
                println!(
                    "(conversa com {peer} divergiu: faltam commits que não chegaram; só aviso, a conversa foi mantida como estava)"
                );
            }
        }
        ChatEvent::PeerRelays { peer, relays } => {
            println!("(relays de {peer}: {})", relays.join(", "));
        }
        ChatEvent::PeerRevoked { peer, identity } => {
            println!(
                "(device {peer} da identidade {} foi revogado: conversa e fila apagadas)",
                identity.to_base58()
            );
        }
        ChatEvent::PeerDowngraded { peer, identity } => {
            println!(
                "(device {peer} da identidade {} usa certificado antigo (v3) e a identidade já está no regime novo: conversa e fila apagadas)",
                identity.to_base58()
            );
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
    use kin_identity::{IdentityProvider, StandaloneIdentity};

    fn parse(argv: &[&str], env: Option<&str>) -> Result<Args, String> {
        parse_from(argv.iter().map(|s| s.to_string()), env.map(str::to_string))
    }

    // ---- convite com token de uso único (P18, ADR 0039) ----

    /// P18 b: `/invite` leva só circuito de relay; `--direct` é opt-in; outro argumento é erro de uso;
    /// `/invites` e outros comandos não são `/invite`.
    #[test]
    fn invite_command_defaults_to_relay_only_and_direct_is_opt_in() {
        assert_eq!(parse_invite("/invite"), Some(Ok(InviteScope::ViaRelay)));
        assert_eq!(
            parse_invite("/invite --direct"),
            Some(Ok(InviteScope::Direct))
        );
        for bad in [
            "/invite xyz",
            "/invite --direct extra",
            "/invite --direct --direct",
        ] {
            let reply = parse_invite(bad);
            let Some(Err(usage)) = reply else {
                panic!("{bad}: esperava erro de uso, veio {reply:?}");
            };
            assert!(usage.contains("--direct"), "{usage}");
        }
        for other in ["/invites", "/invite-x", "invite", "/accept kin://x", ""] {
            assert_eq!(parse_invite(other), None, "{other}");
        }
    }

    fn live(id: &str, expires_at: u64) -> LiveInvite {
        LiveInvite {
            id: id.to_string(),
            expires_at,
        }
    }

    #[test]
    fn admission_is_closed_by_default_and_open_admission_opens_it() {
        assert!(!parse(&[], None).unwrap().open_admission);
        assert!(
            parse(&["--open-admission"], None).unwrap().open_admission,
            "a flag foi aceita mas não guardada"
        );
        // convive com as outras flags, em qualquer ordem
        let args = parse(&["--no-mdns", "--open-admission", "--no-nostr"], None).unwrap();
        assert!(args.open_admission && !args.mdns && args.no_nostr);
    }

    // ---- aprovação do convidante na CLI (P18 d PR 2, ADR 0039) ----

    #[test]
    fn approval_is_off_by_default_and_require_approval_turns_it_on() {
        assert!(!parse(&[], None).unwrap().require_approval);
        let args = parse(&["--no-mdns", "--require-approval", "--no-nostr"], None).unwrap();
        assert!(args.require_approval && !args.open_admission && !args.mdns);
    }

    /// A biblioteca ignora a aprovação com o portão aberto; na CLI isso seria uma falsa sensação de
    /// proteção, então a combinação é erro de uso, em qualquer ordem.
    #[test]
    fn require_approval_with_open_admission_is_a_usage_error() {
        for argv in [
            ["--require-approval", "--open-admission"],
            ["--open-admission", "--require-approval"],
        ] {
            let Err(error) = parse(&argv, None) else {
                panic!("{argv:?}: esperava erro de uso");
            };
            assert!(
                error.contains("--require-approval") && error.contains("--open-admission"),
                "{error}"
            );
        }
    }

    fn request() -> (PeerId, IdentityId) {
        (PeerId::random(), StandaloneIdentity::generate().id())
    }

    /// Sem pedido nenhum, diz isso. Com um pedido só, mas sem Peer ID (ou com prefixo curto ou que não casa),
    /// **não age**: lista (auditoria do #278, M1 e B7: o pedido único de agora pode não ser o que o dono viu).
    #[test]
    fn pick_request_never_acts_without_a_real_peer_id() {
        let Err(none) = pick_request("", &[]) else {
            panic!("sem pedidos devia ser erro");
        };
        assert!(none.contains("nenhum pedido"), "{none}");

        let only = request();
        let full = only.0.to_string();
        for prefix in ["", "1", "12D3KooW", &full[..11], "naoexiste00000"] {
            let Err(listing) = pick_request(prefix, std::slice::from_ref(&only)) else {
                panic!("{prefix:?}: devia listar, não agir");
            };
            assert!(listing.contains(&full), "{prefix:?}: {listing}");
        }
        assert_eq!(
            pick_request(&full[..12], std::slice::from_ref(&only)),
            Ok(only.0)
        );
    }

    /// Com vários pedidos e sem prefixo que decida, nada é aprovado: a resposta lista peer e identidade.
    #[test]
    fn pick_request_with_several_requests_lists_them_and_does_not_act() {
        let (a, b) = (request(), request());
        let pending = [a.clone(), b.clone()];
        for prefix in ["", "12D3KooW", "naoexiste"] {
            let Err(listing) = pick_request(prefix, &pending) else {
                panic!("{prefix:?}: devia listar, não agir");
            };
            for (peer, identity) in &pending {
                assert!(listing.contains(&peer.to_string()), "{prefix:?}: {listing}");
                assert!(
                    listing.contains(&identity.to_base58()),
                    "{prefix:?}: {listing}"
                );
            }
        }
    }

    /// O Peer ID inteiro, ou um prefixo que só um pedido tem, escolhe o pedido.
    #[test]
    fn pick_request_by_full_id_or_unique_prefix() {
        let (a, b) = (request(), request());
        let pending = [a.clone(), b.clone()];
        assert_eq!(pick_request(&a.0.to_string(), &pending), Ok(a.0));
        assert_eq!(pick_request(&b.0.to_string(), &pending), Ok(b.0));
        let unique = &b.0.to_string()[..16];
        assert_eq!(pick_request(unique, &pending), Ok(b.0));
    }

    #[test]
    fn allow_and_deny_replies_say_what_happened() {
        let approved = allow_reply("12D3KooWPeer", Ok(true));
        assert!(
            approved.contains("12D3KooWPeer") && approved.contains("aprovado"),
            "{approved}"
        );
        let stale = allow_reply("12D3KooWPeer", Ok(false));
        assert!(stale.contains("nada foi aprovado"), "{stale}");
        assert!(allow_reply("12D3KooWPeer", Err(Error::InvalidInvite)).contains("não deu"));

        let denied = deny_reply("12D3KooWPeer", true);
        assert!(
            denied.contains("12D3KooWPeer") && denied.contains("negado"),
            "{denied}"
        );
        // o `deny` não revoga o link (decisão do dono, 2026-11-21): a resposta diz como encerrá-lo
        assert!(
            denied.contains("segue vivo") && denied.contains("/revoke-invite"),
            "{denied}"
        );
        assert!(deny_reply("12D3KooWPeer", false).contains("nenhum pedido"));
    }

    /// O aviso do pedido diz **quem** (peer e identidade apresentada) e os dois comandos; o `/deny` não
    /// revoga. O prefixo "(pedido de entrada de" é o que o lab procura.
    #[test]
    fn the_admission_request_notice_names_who_and_the_commands() {
        let notice = admission_requested_notice("12D3KooWPeer", "IdentidadeBase58");
        assert!(
            notice.starts_with("(pedido de entrada de 12D3KooWPeer"),
            "{notice}"
        );
        assert!(notice.contains("IdentidadeBase58"), "{notice}");
        assert!(
            notice.contains("/allow") && notice.contains("/deny"),
            "{notice}"
        );
        assert!(notice.contains("segue vivo"), "{notice}");
        // a identidade é autodeclarada (auditoria do #278, M2): o aviso não a vende como "quem é"
        assert!(notice.contains("não verificada"), "{notice}");
    }

    #[test]
    fn the_approval_mode_is_announced_at_startup() {
        let notice = approval_notice();
        assert!(
            notice.contains("--require-approval") && notice.contains("/allow"),
            "{notice}"
        );
    }

    /// Conversa refeita (P16): o aviso diz **quem** (a identidade), que o sinal não é verificável e que, se não
    /// era esperado, deve ser confirmado por outro canal; e que mensagens podem ter se perdido.
    #[test]
    fn the_reset_notice_names_the_identity_and_asks_for_confirmation() {
        let notice = reset_notice("12D3KooWPeer", "IdentidadeBase58");
        assert!(notice.contains("12D3KooWPeer"), "{notice}");
        assert!(notice.contains("IdentidadeBase58"), "{notice}");
        assert!(notice.contains("confirme por outro canal"), "{notice}");
        assert!(notice.contains("perdid"), "{notice}");
    }

    /// P35: as linhas de entrega dizem de qual device vêm e mantêm os prefixos que o lab procura.
    #[test]
    fn delivery_lines_name_the_peer_and_keep_the_old_prefixes() {
        let delivered = delivered_notice("4a327655", "12D3KooWPeer");
        assert!(delivered.starts_with("(entregue 4a327655"), "{delivered}");
        assert!(delivered.contains("12D3KooWPeer"), "{delivered}");

        let queued = queued_notice("4a327655", "12D3KooWPeer");
        assert!(
            queued.starts_with("(na fila, sai quando o peer conectar"),
            "{queued}"
        );
        assert!(queued.contains("4a327655"), "{queued}");
        assert!(queued.contains("12D3KooWPeer"), "{queued}");

        let failed = send_failed_notice("4a327655", "12D3KooWPeer", "timeout");
        assert!(failed.starts_with("(falha no envio 4a327655"), "{failed}");
        assert!(failed.contains("12D3KooWPeer"), "{failed}");
        assert!(failed.contains("timeout"), "{failed}");
    }

    /// P34: o aviso de peer não admitido diz quem e os caminhos para admitir.
    #[test]
    fn the_not_admitted_notice_names_the_peer_and_the_ways_in() {
        let notice = not_admitted_notice("12D3KooWPeer");
        assert!(notice.contains("12D3KooWPeer"), "{notice}");
        assert!(notice.contains("não admitido"), "{notice}");
        assert!(notice.contains("/invite"), "{notice}");
        assert!(notice.contains("/accept"), "{notice}");
        assert!(notice.contains("--dial"), "{notice}");
        // `/trust` não abre o portão para estranho (o `Hello` dele não traz certificado): fora do aviso.
        assert!(!notice.contains("/trust"), "{notice}");
    }

    /// O modo de admissão aparece na partida: aberto como aviso, fechado como informação.
    #[test]
    fn the_admission_mode_is_announced_at_startup() {
        let open = admission_notice(true);
        assert!(open.starts_with("aviso:") && open.contains("aberta"));
        assert!(admission_notice(false).contains("fechada"));
    }

    /// `--require-invite` virou o padrão (PR 3b): a flag antiga some, e quem a passava vê um erro claro.
    #[test]
    fn the_old_require_invite_flag_is_gone() {
        assert!(parse(&["--require-invite"], None).is_err());
    }

    #[test]
    fn slash_invites_without_any_invite_says_so() {
        let text = invites_reply(&[], 1_000);
        assert!(text.contains("nenhum convite"), "{text:?}");
    }

    #[test]
    fn slash_invites_lists_each_id_with_the_time_left() {
        let now = 1_000_000;
        let text = invites_reply(
            &[
                live("a1b2c3d4", now + 24 * 3600 - 60),
                live("0f0f0f0f", now + 90 * 60),
                live("deadbeef", now + 5 * 60),
                live("cafe0001", now + 20),
            ],
            now,
        );
        assert_eq!(text.lines().count(), 4, "uma linha por convite: {text}");
        for (id, left) in [
            ("a1b2c3d4", "23 h 59 min"),
            ("0f0f0f0f", "1 h 30 min"),
            ("deadbeef", "5 min"),
            ("cafe0001", "menos de 1 min"),
        ] {
            let line = text.lines().find(|l| l.contains(id)).expect(id);
            assert!(line.contains(left), "{id} deveria mostrar {left:?}: {line}");
        }
    }

    #[test]
    fn slash_invites_never_prints_a_link_or_more_than_the_id() {
        let text = invites_reply(&[live("a1b2c3d4", 2_000)], 1_000);
        assert!(!text.contains("kin://"), "{text}");
        assert!(text.contains("a1b2c3d4"), "{text}");
    }

    #[test]
    fn slash_revoke_invite_replies_for_each_outcome() {
        let revoked = revoke_invite_reply("a1b2", Ok(true));
        assert!(
            revoked.contains("revogado") && revoked.contains("a1b2"),
            "{revoked}"
        );

        let none = revoke_invite_reply("zzzz", Ok(false));
        assert!(
            none.contains("nenhum convite") && none.contains("zzzz"),
            "{none}"
        );

        // Prefixo curto ou ambíguo: o erro da biblioteca vira uma dica que diz o mínimo.
        let invalid = revoke_invite_reply("a1", Err(Error::InvalidInvite));
        assert!(
            invalid.contains('4') && invalid.contains("mais de um"),
            "{invalid}"
        );

        let other = revoke_invite_reply("a1b2", Err(Error::Malformed));
        assert!(other.contains("não deu para revogar"), "{other}");
    }

    // ---- remoção da cópia da mestra (P30 fatia 6, ADR 0035) ----

    #[test]
    fn the_pair_flag_no_longer_exists() {
        // `--pair ARQUIVO` importava a mestra de outro device: agora é uma flag desconhecida.
        assert!(parse(&["--pair", "arquivo"], None).is_err());
        assert!(parse(&["--pair", "-"], None).is_err());
        // controle: uma flag válida segue sendo aceita
        assert!(parse(&["--no-nostr"], None).is_ok());
    }

    #[test]
    fn the_slash_pair_reply_depends_on_the_mode_and_carries_no_link() {
        let master = StandaloneIdentity::generate();
        let key = kin_identity::AuthorityKey::generate();
        let chain = master.certify_authority(&key.public_key(), 1).unwrap();
        let primary = kin_identity::AuthorityIdentity::new(key, chain).unwrap();
        let paired = keys::LocalIdentity::Paired(master.id());
        let master = keys::LocalIdentity::Master(Box::new(master));
        let primary = keys::LocalIdentity::Primary(Box::new(primary));

        // Master: o caminho é uma identidade NOVA, e o texto tem de dizer isso.
        let text = pair_reply(&master);
        for needle in [
            "kin cold init",
            "kin pair request",
            "identidade NOVA",
            "identity.key",
        ] {
            assert!(text.contains(needle), "o Master não ouve {needle}: {text}");
        }
        // Primário: só `kin pair answer`; mandá-lo rodar `kin cold init` criaria uma segunda identidade.
        let text = pair_reply(&primary);
        assert!(text.contains("kin pair answer"), "{text}");
        assert!(!text.contains("kin cold init"), "{text}");
        // Pareado: quem adiciona devices é o primário.
        let text = pair_reply(&paired);
        assert!(text.contains("primário"), "{text}");
        assert!(!text.contains("kin cold init"), "{text}");
        // Nenhuma resposta pode disparar a própria trava nem sugerir o link antigo.
        for mode in [&master, &primary, &paired] {
            assert!(!is_pair_link(pair_reply(mode)));
        }
    }

    #[test]
    fn the_send_guard_still_blocks_an_old_pair_link_but_not_other_links() {
        assert!(is_pair_link("kin://pair/AAAA"));
        assert!(is_pair_link("olha isso kin://pair/AAAA ok?"));
        // pedido e certificado do pareamento novo não carregam a mestra
        assert!(!is_pair_link("kin://pair-request/AAAA"));
        assert!(!is_pair_link("kin://pair-cert/AAAA"));
        assert!(!is_pair_link("kin://invite/AAAA"));
        assert!(!is_pair_link("oi, tudo bem?"));
    }

    #[test]
    fn key_flags_are_parsed() {
        let args = parse(&["--passphrase-file", "/s/p", "--protect"], None).unwrap();
        assert_eq!(args.keys.passphrase_file, Some(PathBuf::from("/s/p")));
        assert!(args.keys.protect);
        assert!(!args.keys.no_passphrase);
        let args = parse(&["--no-passphrase"], None).unwrap();
        assert!(args.keys.no_passphrase);
        assert_eq!(args.keys.passphrase_file, None);
    }

    #[test]
    fn key_options_default_to_nothing_requested() {
        let args = parse(&[], None).unwrap();
        assert_eq!(args.keys, keys::KeyOptions::default());
    }

    #[test]
    fn the_environment_variable_supplies_the_passphrase_file() {
        let args = parse(&[], Some("/env/p")).unwrap();
        assert_eq!(args.keys.passphrase_file, Some(PathBuf::from("/env/p")));
    }

    #[test]
    fn the_flag_beats_the_environment_variable() {
        let args = parse(&["--passphrase-file", "/flag/p"], Some("/env/p")).unwrap();
        assert_eq!(args.keys.passphrase_file, Some(PathBuf::from("/flag/p")));
    }

    #[test]
    fn an_empty_environment_variable_counts_as_unset() {
        let args = parse(&[], Some("")).unwrap();
        assert_eq!(args.keys.passphrase_file, None);
    }

    #[test]
    fn no_passphrase_ignores_the_environment_variable() {
        let args = parse(&["--no-passphrase"], Some("/env/p")).unwrap();
        assert!(args.keys.no_passphrase);
        assert_eq!(args.keys.passphrase_file, None);
    }

    #[test]
    fn passphrase_file_without_a_value_is_an_error() {
        assert!(parse(&["--passphrase-file"], None).is_err());
    }

    #[test]
    fn unknown_flags_are_still_refused() {
        assert!(parse(&["--nao-existe"], None).is_err());
    }

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

    // ---- `--serve-relay` não toca no `db.key` nem nos bancos (P13, ADR 0045) ----

    /// Custo mínimo do Argon2 (o `open_keys` de produção recebe `DEFAULT`).
    const FAST: kin_identity::KdfParams = kin_identity::KdfParams::MIN;

    /// Diretório de dados novo + arquivo de senha (0600, como o `read_passphrase_file` exige).
    fn data_dir_and_passphrase() -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let pass = dir.path().join("senha.txt");
        std::fs::write(&pass, "senha-de-teste-p13\n").unwrap();
        std::fs::set_permissions(&pass, std::fs::Permissions::from_mode(0o600)).unwrap();
        (dir, pass)
    }

    /// `kin --data-dir <dir> --passphrase-file <pass> [extra...]`; `relay` acrescenta `--serve-relay`.
    fn key_args(dir: &Path, pass: &Path, relay: bool, extra: &[&str]) -> Args {
        let mut argv = vec![
            "--data-dir",
            dir.to_str().unwrap(),
            "--passphrase-file",
            pass.to_str().unwrap(),
        ];
        argv.extend_from_slice(extra);
        if relay {
            argv.push("--serve-relay");
        }
        parse(&argv, None).unwrap()
    }

    fn plain_database(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (v TEXT NOT NULL); INSERT INTO t (v) VALUES ('p13'); PRAGMA user_version = 3;",
        )
        .unwrap();
    }

    /// O relay não abre banco: com chaves seladas e sem `db.key`, não cria um (o caminho normal cria,
    /// como no teste seguinte), mas a identidade e o `device.key` continuam sendo criados e devolvidos.
    #[test]
    fn serve_relay_with_sealed_keys_does_not_create_a_db_key() {
        let (dir, pass) = data_dir_and_passphrase();
        let args = key_args(dir.path(), &pass, true, &[]);
        let (_, _, db_key) = open_keys(&args, FAST).unwrap();
        assert!(db_key.is_none(), "o relay não devolve chave de banco");
        assert!(
            !dir.path().join(keys::DB_KEY_FILE).exists(),
            "o relay criou o db.key"
        );
        for name in ["identity.key", "device.key"] {
            let path = dir.path().join(name);
            assert!(
                kin_identity::key_file_is_sealed(&path).unwrap(),
                "{name} devia existir e estar selado"
            );
        }
    }

    /// A outra metade: o caminho do chat, com as mesmas chaves seladas, segue criando o `db.key`. Sem isto
    /// a correção do relay poderia ter removido a criação no caminho errado.
    #[test]
    fn the_chat_path_with_sealed_keys_still_creates_the_db_key() {
        let (dir, pass) = data_dir_and_passphrase();
        let args = key_args(dir.path(), &pass, false, &[]);
        let (_, _, db_key) = open_keys(&args, FAST).unwrap();
        assert!(db_key.is_some(), "o chat precisa da chave dos bancos");
        let path = dir.path().join(keys::DB_KEY_FILE);
        assert!(kin_identity::key_file_is_sealed(&path).unwrap());
    }

    /// Fora do caso feliz (dir novo): o relay num diretório do chat com `db.key` ilegível não falha por
    /// ele. O chat falha neste mesmo diretório (premissa: é o `db.key` que o derruba), o relay não, e o
    /// arquivo fica byte a byte igual.
    #[test]
    fn serve_relay_ignores_a_db_key_it_could_not_open() {
        let (dir, pass) = data_dir_and_passphrase();
        open_keys(&key_args(dir.path(), &pass, true, &[]), FAST).unwrap();
        let db_key = dir.path().join(keys::DB_KEY_FILE);
        std::fs::write(&db_key, b"isto nao e um db.key").unwrap();

        let chat = key_args(dir.path(), &pass, false, &[]);
        assert!(open_keys(&chat, FAST).is_err(), "o chat devia recusar");

        let relay = key_args(dir.path(), &pass, true, &[]);
        let (_, _, opened) = open_keys(&relay, FAST).unwrap();
        assert!(opened.is_none());
        assert_eq!(std::fs::read(&db_key).unwrap(), b"isto nao e um db.key");
    }

    /// Com `--protect` o caminho do chat cifra no lugar o banco em claro; o relay não pode migrar banco
    /// nenhum (nem ler: o arquivo e o `db.key` ficam byte a byte iguais).
    #[test]
    fn serve_relay_with_protect_leaves_a_plain_database_and_the_db_key_untouched() {
        let (dir, pass) = data_dir_and_passphrase();
        open_keys(&key_args(dir.path(), &pass, false, &[]), FAST).unwrap();
        let database = dir.path().join("mls.sqlite");
        plain_database(&database);
        let (database_before, key_before) = (
            std::fs::read(&database).unwrap(),
            std::fs::read(dir.path().join(keys::DB_KEY_FILE)).unwrap(),
        );

        // Controle: o chat, numa cópia do mesmo estado, migra o banco (é o que o relay não pode fazer).
        let copy = tempfile::tempdir().unwrap();
        for name in [
            "identity.key",
            "device.key",
            keys::DB_KEY_FILE,
            "mls.sqlite",
        ] {
            std::fs::copy(dir.path().join(name), copy.path().join(name)).unwrap();
        }
        let chat = key_args(copy.path(), &pass, false, &["--protect"]);
        open_keys(&chat, FAST).unwrap();
        assert_eq!(
            kin_crypto::db_kind(&copy.path().join("mls.sqlite")).unwrap(),
            kin_crypto::DbKind::Encrypted,
            "o chat com --protect devia migrar o banco"
        );

        let relay = key_args(dir.path(), &pass, true, &["--protect"]);
        open_keys(&relay, FAST).unwrap();

        assert_eq!(
            kin_crypto::db_kind(&database).unwrap(),
            kin_crypto::DbKind::Plain,
            "o relay migrou o banco"
        );
        assert_eq!(std::fs::read(&database).unwrap(), database_before);
        assert_eq!(
            std::fs::read(dir.path().join(keys::DB_KEY_FILE)).unwrap(),
            key_before
        );
    }

    /// Sem `--protect`, o chat recusa banco em claro com chaves seladas; o relay não olha o banco: abre,
    /// não cria `db.key` e deixa o banco byte a byte igual.
    #[test]
    fn serve_relay_ignores_a_plain_database_without_protect() {
        let (dir, pass) = data_dir_and_passphrase();
        let relay = key_args(dir.path(), &pass, true, &[]);
        open_keys(&relay, FAST).unwrap();
        let database = dir.path().join("mls.sqlite");
        plain_database(&database);
        let before = std::fs::read(&database).unwrap();

        let chat = key_args(dir.path(), &pass, false, &[]);
        assert!(open_keys(&chat, FAST).is_err(), "o chat devia recusar");
        assert!(!dir.path().join(keys::DB_KEY_FILE).exists());

        let (_, _, db_key) = open_keys(&relay, FAST).unwrap();
        assert!(db_key.is_none());
        assert!(!dir.path().join(keys::DB_KEY_FILE).exists());
        assert_eq!(
            kin_crypto::db_kind(&database).unwrap(),
            kin_crypto::DbKind::Plain
        );
        assert_eq!(std::fs::read(&database).unwrap(), before);
    }

    /// Banco cifrado sem `db.key` é erro para o chat (nunca "sem chave"); o relay não abre banco, então
    /// sobe, e não inventa um `db.key` novo (que não abriria o banco).
    #[test]
    fn serve_relay_ignores_an_encrypted_database_with_no_db_key() {
        let (dir, pass) = data_dir_and_passphrase();
        open_keys(&key_args(dir.path(), &pass, false, &[]), FAST).unwrap();
        let database = dir.path().join("mls.sqlite");
        plain_database(&database);
        open_keys(&key_args(dir.path(), &pass, false, &["--protect"]), FAST).unwrap();
        assert_eq!(
            kin_crypto::db_kind(&database).unwrap(),
            kin_crypto::DbKind::Encrypted
        );
        std::fs::remove_file(dir.path().join(keys::DB_KEY_FILE)).unwrap();
        let before = std::fs::read(&database).unwrap();

        let chat = key_args(dir.path(), &pass, false, &[]);
        assert!(open_keys(&chat, FAST).is_err(), "o chat devia recusar");

        let relay = key_args(dir.path(), &pass, true, &[]);
        let (_, _, db_key) = open_keys(&relay, FAST).unwrap();
        assert!(db_key.is_none());
        assert!(
            !dir.path().join(keys::DB_KEY_FILE).exists(),
            "o relay criou um db.key"
        );
        assert_eq!(std::fs::read(&database).unwrap(), before);
    }

    /// O `--protect` das chaves segue valendo no relay: identidade e `device.key` em claro terminam
    /// seladas, e ainda assim nenhum `db.key` é criado.
    #[test]
    fn serve_relay_with_protect_still_seals_the_plain_keys() {
        let (dir, pass) = data_dir_and_passphrase();
        let plain = parse(
            &[
                "--data-dir",
                dir.path().to_str().unwrap(),
                "--no-passphrase",
                "--serve-relay",
            ],
            None,
        )
        .unwrap();
        open_keys(&plain, FAST).unwrap();
        for name in ["identity.key", "device.key"] {
            let path = dir.path().join(name);
            assert!(
                !kin_identity::key_file_is_sealed(&path).unwrap(),
                "{name} devia começar em claro"
            );
        }

        let relay = key_args(dir.path(), &pass, true, &["--protect"]);
        let (_, _, db_key) = open_keys(&relay, FAST).unwrap();
        assert!(db_key.is_none());
        for name in ["identity.key", "device.key"] {
            let path = dir.path().join(name);
            assert!(
                kin_identity::key_file_is_sealed(&path).unwrap(),
                "{name} devia ter sido selado pelo --protect"
            );
        }
        assert!(!dir.path().join(keys::DB_KEY_FILE).exists());
    }

    /// O relay só fala de chaves no aviso do `--protect`; o chat segue falando de bancos e histórico.
    #[test]
    fn the_protect_warning_mentions_databases_only_for_the_chat() {
        let relay = protect_warning(true);
        assert!(relay.contains("(chaves)"), "{relay}");
        assert!(
            !relay.contains("bancos") && !relay.contains("histórico"),
            "{relay}"
        );
        let chat = protect_warning(false);
        assert!(
            chat.contains("(chaves e bancos)") && chat.contains("histórico"),
            "{chat}"
        );
    }

    /// Antes do Argon2id (segundos de CPU, sem saída nenhuma) o kin avisa que está derivando a chave.
    #[test]
    fn the_kdf_notice_says_the_key_is_being_derived() {
        for protect in [false, true] {
            let notice = kdf_notice(protect);
            assert!(
                notice.contains("Argon2id") && notice.contains("derivando"),
                "protect={protect}: {notice}"
            );
        }
    }

    /// Só o `--protect` deixa um estado pela metade se interrompido, e só ele avisa disso e diz o conserto.
    #[test]
    fn only_the_protect_notice_warns_about_interrupting() {
        let protect = kdf_notice(true);
        assert!(
            protect.contains("Ctrl-C") && protect.contains("--protect de novo"),
            "{protect}"
        );
        let plain = kdf_notice(false);
        assert!(
            !plain.contains("Ctrl-C") && !plain.contains("--protect"),
            "{plain}"
        );
    }
}
