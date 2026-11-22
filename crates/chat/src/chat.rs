use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kin_crypto::{Conversation, CryptoDevice, Decrypted, Error as CryptoError, Expected};
use kin_identity::{
    DeviceCertificate, DeviceKey, DeviceRevocation, IdentityId, IdentityProvider, InviteSigner,
};
use kin_transport::{Multiaddr, NatStatus, Node, NodeConfig, NodeEvent, PeerId, SendId};

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use futures::future::{self, Either};
use futures::pin_mut;

use crate::invite::MAX_INVITE_TTL;
use crate::nostr::validate_peer_relays;
use crate::outbox::{EntryKind, Outbox, SeenKind};
use crate::payload::{HISTORY_CHUNK_BUDGET, HistoryItem, MAX_DECODED_HISTORY, Payload};
use crate::wire::{Envelope, Presence};
use crate::{DepositId, Mailbox, MailboxEvent};
use crate::{Error, HistoryEntry, Invite, LiveInvite, Message, MessageId};

/// O que o [`Chat`] avisa à aplicação.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    Listening(Multiaddr),
    /// Conexão de rede aberta; o handshake E2EE começa sozinho.
    PeerConnected(PeerId),
    PeerDisconnected(PeerId),
    /// Uma tentativa de conexão falhou (endereço fora do ar, Peer ID diferente do esperado, relay
    /// sem reserva do destino, etc.).
    DialFailed {
        peer: Option<PeerId>,
        reason: String,
    },
    /// Por onde a conexão com o peer passa: direta ou via relay. Chega de novo quando o hole punching
    /// troca o relay por uma conexão direta (a conversa não precisa de novo handshake).
    Route {
        peer: PeerId,
        relayed: bool,
    },
    /// O AutoNAT mudou a avaliação de alcançabilidade deste nó.
    Nat(NatStatus),
    /// O relay aceitou a reserva: este nó é alcançável pelo endereço de circuito.
    RelayReserved {
        relay: PeerId,
    },
    /// A reserva num relay acabou; o nó já tenta o próximo candidato.
    RelayLost {
        relay: PeerId,
    },
    /// Resultado de uma tentativa de hole punching com o peer.
    HolePunch {
        peer: PeerId,
        result: Result<(), String>,
    },
    /// Chegou uma página do histórico de um irmão (P10a): `imported` linhas novas gravadas (as que já
    /// existiam não contam); `done` = não há mais nada a pedir a ele. Leia com `Chat::history`.
    HistorySynced {
        peer: PeerId,
        imported: usize,
        done: bool,
    },
    /// Conversa E2EE pronta: já dá para enviar mensagens a este peer.
    ConversationReady {
        peer: PeerId,
        identity: IdentityId,
    },
    /// O outro lado perdeu o estado da conversa (P16) e ela foi **refeita** com ele: o grupo antigo foi
    /// descartado e um novo, com a mesma identidade, o substituiu. Chega antes do `ConversationReady` da
    /// conversa nova. O que eu enviei e deu `Delivered` antes de eu perceber a perda ficou perdido.
    ConversationReset {
        peer: PeerId,
        /// A identidade do outro lado (a mesma da conversa antiga): quem a aplicação mostra ao usuário.
        identity: IdentityId,
    },
    /// P34: `peer` conectou, se apresentou (`Hello`) e o portão fechado ([`Chat::set_require_invite`]) **não
    /// o admitiu**: sem conversa, sem convite meu, sem `dial` com Peer ID, sem identidade aceita. Uma vez por
    /// conexão. É só local: nada é dito ao peer recusado (não vira oráculo do estado dos convites).
    NotAdmitted {
        peer: PeerId,
    },
    /// P18 (d): `peer` chegou com um token vivo de um convite meu e a aprovação
    /// ([`Chat::set_require_approval`]) está ligada: a conversa **não** abre até [`Chat::approve`] (ou
    /// morre em [`Chat::deny`]). `identity` é a do certificado que o peer apresentou no `Hello`. Uma vez
    /// por conexão; só local (nada é dito ao peer).
    AdmissionRequested {
        peer: PeerId,
        identity: IdentityId,
    },
    /// Mensagem recebida e decifrada.
    Received {
        peer: PeerId,
        sender: IdentityId,
        message: Message,
    },
    /// Outro device meu enviou `message` a `to` (espelho multi-device, 3.5). Não é uma mensagem
    /// recebida: é uma mensagem **minha**, para mostrar na conversa com `to`. `peer` é o device irmão.
    SentElsewhere {
        peer: PeerId,
        to: IdentityId,
        message: Message,
    },
    /// A mensagem está na outbox esperando o peer: ele está offline ou o envio falhou. Sai sozinha
    /// quando ele conectar.
    Queued {
        peer: PeerId,
        message: MessageId,
    },
    /// A caixa postal aceitou a mensagem para o peer buscar depois. Não é entrega: a mensagem segue
    /// na outbox e também sai direto quando o peer conectar (ele deduplica).
    Deposited {
        peer: PeerId,
        message: MessageId,
    },
    /// Diagnóstico: um relay da caixa postal aceitou ou recusou um depósito (e por quê). Vem além de
    /// `Deposited`/`DepositFailed`, um por relay, e pode chegar depois do `Deposited`.
    MailboxRelay {
        relay: String,
        accepted: bool,
        reason: String,
    },
    /// Diagnóstico: um relay da caixa postal demorou a responder ao depósito. Não é falha: uma
    /// resposta tardia ainda conta, e o resultado final vem em `MailboxRelay`.
    MailboxRelaySlow {
        relay: String,
    },
    /// A caixa postal recusou o depósito; a mensagem continua na outbox e será depositada de novo
    /// no próximo envio ou conexão.
    DepositFailed {
        peer: PeerId,
        message: MessageId,
        reason: String,
    },
    /// O peer confirmou o recebimento.
    Delivered {
        peer: PeerId,
        message: MessageId,
    },
    /// O envio falhou; a aplicação decide se reenvia.
    SendFailed {
        peer: PeerId,
        message: Option<MessageId>,
        reason: String,
    },
    /// Chegaram mensagens demais de uma época que este device não alcança: o commit que falta
    /// provavelmente não virá e a conversa divergiu. O buffer foi descartado e, fora da janela de
    /// [`STALE_RECOVERY_WINDOW`], este lado abandonou a conversa e reconecta para o P16 refazê-la (P9 c).
    /// `recovering` diz o que este lado fez: `true` = abandonou a conversa e reconecta; `false` = só
    /// avisou (janela, caixa postal, irmão, sem conexão) e a conversa segue como estava.
    Stale {
        peer: PeerId,
        recovering: bool,
    },
    /// O peer anunciou onde recebe (lista já validada); os depósitos para ele passam a ir para lá.
    PeerRelays {
        peer: PeerId,
        relays: Vec<String>,
    },
    /// O device `peer` (da `identity`) foi revogado por um tombstone e a conversa com ele foi apagada, com
    /// a fila e o que a caixa postal sabia dele (P30, 3d-ii-b2). Chega uma vez por device derrubado; cada
    /// mensagem que estava na fila para ele chega antes como [`ChatEvent::SendFailed`].
    PeerRevoked {
        peer: PeerId,
        identity: IdentityId,
    },
    /// O device `peer` usa certificado v3 (da mestra), mas a `identity` dele já foi aceita em regime v4: a
    /// conversa foi apagada, com a fila e o que a caixa postal sabia dele (P30, 3e-ii-b, ADR 0032). Não há
    /// tombstone: é a regra local do trinco por identidade. Cada mensagem que estava na fila para ele chega
    /// antes como [`ChatEvent::SendFailed`].
    PeerDowngraded {
        peer: PeerId,
        identity: IdentityId,
    },
    /// Algo do peer foi descartado (malformado, de outra conversa, etc.).
    Dropped {
        peer: PeerId,
        reason: String,
    },
}

/// Por que uma conversa é derrubada (P30 3e-ii-b, ADR 0032): decide o texto do `SendFailed` e o evento.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    /// O device tem tombstone gravado (3d).
    Revoked,
    /// O device é v3 e a identidade dele já foi aceita em v4 (trinco por identidade, 3e).
    Downgraded,
}

/// O resultado de consultar o store (tombstones e trinco) para cada conversa (`Chat::scan_revoked`).
struct Scan {
    /// Peers a derrubar (certificado revogado, ou v3 de identidade já em v4), com a identidade e o motivo.
    to_drop: Vec<(PeerId, IdentityId, Reason)>,
    /// Peers que não consegui consultar, com o erro.
    failures: Vec<(PeerId, Error)>,
}

/// Quantas mensagens de época futura se guardam por peer à espera do commit (P9).
const FUTURE_LIMIT: usize = 32;
/// P9 (c, ADR 0041): intervalo mínimo entre duas recuperações automáticas da conversa com o mesmo peer
/// depois de `Stale`. Dentro dele o `Stale` só avisa (quem força 32 mensagens de época futura não faz
/// o nó refazer a conversa em laço).
const STALE_RECOVERY_WINDOW: Duration = Duration::from_secs(600);

/// Quanto tempo um peer fica "em recuperação" (conhecido, com o endereço guardado e rediscado) esperando a
/// conversa voltar. Passado isso a marca e a identidade esperada saem: um peer que nunca volta não fica
/// conhecido para sempre.
const RECOVERING_TTL: Duration = Duration::from_secs(24 * 3600);

/// Quantas mensagens da outbox podem estar em voo (sem ack) por peer. É 1 de propósito: envios em
/// paralelo seguem por substreams diferentes e a rede não garante qual chega primeiro, então o
/// destinatário veria "dois" antes de "um". Um por vez garante a ordem (custo: uma ida e volta por
/// mensagem ao esvaziar a fila). Pode subir quando a mensagem tiver número de sequência. Qualquer
/// valor deve ficar abaixo das 10 gerações fora de ordem que o MLS tolera, ou a rajada embaralhada
/// viraria `TooOld`, ignorado em silêncio.
const SEND_WINDOW: usize = 1;

/// Quantas mensagens por peer podem estar depositadas na caixa postal sem terem sido confirmadas
/// (ainda na outbox). Sem ack do destinatário, o backend pode devolvê-las fora de ordem; o MLS só
/// tolera 10 gerações fora de ordem (e o `chat` ignora `TooOld` em silêncio), então fica abaixo disso.
const MAILBOX_WINDOW: usize = 8;

/// Quantos endereços de cada device listado por um contato se tentam. Irmão (a minha identidade) não
/// tem este limite. Limita a amplificação de uma lista hostil (auditoria I1).
const CONTACT_DIAL_ADDRS: usize = 4;
/// O mesmo limite para um irmão, bem mais folgado: ele é a minha identidade, mas o `identify` aceita dezenas
/// de endereços e a rediscagem por timer os multiplicaria (auditoria P32, I4).
const SIBLING_DIAL_ADDRS: usize = 16;

/// Quantos devices conhecidos se guardam em cada mapa só-memória (`household`, `contact_devices`,
/// `expected`). É o mesmo teto da lista do codec: a lista que sai já corta nesse número, então guardar
/// mais não serviria a nada. `household` e `contact_devices` despejam ao encher ([`insert_bounded`]);
/// em `expected`, cheio, o Peer ID novo é ignorado (o já conhecido ainda se atualiza).
const MAX_KNOWN_DEVICES: usize = crate::payload::MAX_DECODED_DEVICES;

/// Quantas vezes se reenvia cada mensagem de handshake (`Hello`, `KeyPackage`, `Welcome`) a um peer
/// por conexão quando o envio falha (P25). A conta é por tipo.
const MAX_HANDSHAKE_RETRIES: u8 = 3;

/// Intervalo mínimo entre dois re-anúncios da lista de devices ao mesmo irmão. Um irmão que alterna o
/// endereço de um Peer ID sem parar não vira uma torrente de mensagens cifradas.
const SIBLING_ANNOUNCE_MIN: Duration = Duration::from_secs(5);
/// Espera antes do primeiro reenvio direto de uma mensagem de chat que falhou com o peer conectado (P19). Cada
/// falha seguida dobra a espera, até [`RETRY_CAP`]; um ack ou a desconexão zera.
const RETRY_BASE: Duration = Duration::from_secs(2);
/// Falhas seguidas de envio direto a um peer depois das quais o timer de reenvio para de armar (P19, auditoria
/// I1). Cada falha equivale a uma geração de chave MLS gasta, por qualquer gatilho (timer, `send`, ack), e o
/// receptor tolera um salto de 2000 (`MAX_FORWARD_DISTANCE`): 500 deixa folga para o `Hello` e os anúncios. A
/// mensagem segue na outbox e sai quando o peer reconectar (que zera a conta) ou com um ack.
const RETRY_BUDGET: u32 = 500;
/// Rodadas de varredura por idade por peer depois das quais ela para (P21, auditoria I1). Cada rodada pode
/// re-cifrar até [`MAILBOX_WINDOW`] mensagens (uma geração MLS cada): 8 x 8 = 64 gerações por peer, longe do
/// salto de 2000 que o receptor tolera. Com retenção de 7 dias e varredura a cada quarto dela, são ~2 retenções.
const SWEEP_BUDGET: u32 = 8;
/// Espera antes da primeira rediscagem a um peer que caiu com mensagem na fila (P32). Cada falha seguida dobra
/// a espera, até [`REDIAL_CAP`]; conectar zera.
const REDIAL_BASE: Duration = Duration::from_secs(2);
/// Teto da espera entre rediscagens (P32).
const REDIAL_CAP: Duration = Duration::from_secs(300);
/// Rediscagens seguidas, sem conectar, depois das quais o timer para de armar (P32). Não gasta geração MLS
/// (o dial só abre transporte): o orçamento protege rede e bateria. Com o teto de 5 min são ~8 h.
const REDIAL_BUDGET: u32 = 100;
/// Quanto esperar o resultado de um dial pedido pela rediscagem antes de contá-lo como falha (P32,auditoria
/// I2): cobre o evento perdido e o dial que o nó engole (`DialPeerConditionFalse`) sem avisar.
const REDIAL_INFLIGHT: Duration = Duration::from_secs(60);
/// Teto da espera entre reenvios diretos (P19). Cada tentativa re-cifra e gasta uma geração de chave MLS (ADR
/// 0002); quem limita o total é [`RETRY_BUDGET`], não este teto (outros gatilhos também reenviam na hora).
const RETRY_CAP: Duration = Duration::from_secs(60);

/// Em que pé está a rediscagem de um peer (P32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedialState {
    /// Esperando o prazo (`due`) para discar.
    Waiting,
    /// O dial foi pedido ao nó; `due` é o prazo para contá-lo como falha se não vier `DialFailed` nem conexão
    /// ([`REDIAL_INFLIGHT`]).
    InFlight,
    /// O orçamento acabou: sem timer. Só uma mensagem nova rearma uma tentativa.
    Exhausted,
}

/// Rediscagem agendada para um peer que caiu com mensagem na fila (P32).
struct Redial {
    /// Tentativas seguidas sem conectar; decide a espera ([`Chat::redial_backoff`]).
    attempts: u32,
    /// Prazo do estado atual (discar, ou dar o dial por perdido). Sem efeito em `Exhausted`.
    due: Instant,
    state: RedialState,
}

/// O peer vale uma rediscagem agora? (ver [`Chat::redial_want`])
enum Want {
    Yes,
    No,
    /// A outbox não respondeu: não dá para dizer que a fila está vazia.
    Unknown,
}

/// Reenvio direto agendado para um peer depois de um `SendFailed` de mensagem de chat (P19).
struct Retry {
    /// Falhas seguidas sem ack; decide a espera ([`Chat::retry_delay`]).
    attempts: u32,
    /// Quando o reenvio pode sair.
    due: Instant,
    /// Há um reenvio esperando o prazo. Depois que ele dispara, o registro fica (para contar a próxima
    /// falha seguida), mas sem acordar o relógio: senão o prazo vencido manteria o laço girando à toa.
    armed: bool,
}

/// Folga abaixo do teto de validade ao emitir um link, para o leitor com relógio um pouco atrasado.
const INVITE_CLOCK_SLACK: Duration = Duration::from_secs(300);

/// Tombstones aceitos por peer em cada janela (P30, 3c; ADR 0024). Conta toda tentativa recebida, não só as
/// gravadas: conferir a assinatura custa CPU, e repetir um tombstone já gravado também é inundar.
const REVOCATIONS_PER_WINDOW: u32 = 16;
/// Teto de todos os peers somados na mesma janela (auditoria 3c-i: só o limite por peer não segura quem
/// abre várias conexões).
const REVOCATIONS_GLOBAL_PER_WINDOW: u32 = 64;
/// Teto de devoluções de cota por janela (auditoria 3c-ii, I2): com o armazenamento falhando para sempre, a
/// devolução ilimitada deixaria o limite sem efeito justamente quando cada tentativa já pagou a assinatura.
const REVOCATION_REFUNDS_PER_WINDOW: u32 = 16;
/// Tombstones reenviados a um peer por conexão (3c-ii-b): abaixo da cota de 16 por peer do receptor, para
/// sobrar espaço para repasses novos e para outros remetentes. Quem tem mais recebe só os mais recentes.
const REVOCATIONS_RESEND_MAX: usize = 4;
/// Intervalo mínimo entre reenvios ao mesmo peer: maior que `REVOCATION_WINDOW` (a janela de cota do
/// receptor), para que dois lotes nunca caiam na mesma janela dele (auditoria 3c-ii-b, I2).
const REVOCATION_RESEND_GAP: Duration = Duration::from_secs(120);
const REVOCATION_WINDOW: Duration = Duration::from_secs(60);

/// Teto de linhas de histórico importadas de um irmão por sessão de sincronização (P10a). Um irmão
/// comprometido (chave mestra vazada) não pode encher o disco sem limite.
const MAX_SYNC_ENTRIES: usize = 100_000;

/// Maior `KeyPackage` guardado à espera do token de convite (P18): um MLS KeyPackage real tem poucos KiB.
const MAX_HELD_KEY_PACKAGE: usize = 16 * 1024;
/// Quantos peers podem ter um `KeyPackage` guardado ao mesmo tempo (no pior caso, 1 MiB). Maior que o dos
/// devices conhecidos de propósito: quem enche as vagas precisa de uma conexão por vaga (auditoria, médio 2).
const MAX_HELD_KEY_PACKAGES: usize = 64;
/// Quantos peers recusados podem estar avisados ao mesmo tempo (P34). Acima disso, recusa sem aviso: o teto
/// só limita a visibilidade, nunca a admissão (auditoria do PR #257, baixo 2).
const MAX_NOT_ADMITTED_NOTIFIED: usize = 64;

/// Quantos pedidos de entrada (P18 d) podem esperar `approve`/`deny` ao mesmo tempo. Acima disso o peer cai
/// no `NotAdmitted` de sempre: o teto limita a fila de decisões, não a admissão (que segue fechada).
const MAX_PENDING_APPROVALS: usize = 16;

/// Cabe `key` em `map` sem passar de [`MAX_KNOWN_DEVICES`]? Quem já está sempre cabe (atualiza).
/// O Peer ID fixado num endereço discado: só vale o `/p2p/<id>` que **termina** o endereço (em circuito,
/// o destino depois de `/p2p-circuit`). Circuito sem destino ou Peer ID no meio não fixam ninguém, e a
/// conexão que o libp2p fizer não é a esse peer (auditoria, baixo 4).
fn pinned_peer(addr: &Multiaddr) -> Option<PeerId> {
    match addr.iter().last() {
        Some(kin_transport::Protocol::P2p(peer)) => Some(peer),
        _ => None,
    }
}

fn has_room<V>(map: &HashMap<PeerId, V>, key: &PeerId) -> bool {
    map.contains_key(key) || map.len() < MAX_KNOWN_DEVICES
}

/// Resultado de [`insert_bounded`].
#[derive(Debug, PartialEq, Eq)]
enum Inserted {
    /// Peer ID novo com o mapa cheio e nada a despejar: nem guardado, nem discado.
    Ignored,
    /// Já estava com o mesmo convite.
    Unchanged,
    /// Entrou, ou o convite mudou (um despejo também conta).
    Changed,
}

/// Guarda `invite` em `map` respeitando [`MAX_KNOWN_DEVICES`]. Quem já está só se atualiza. Cheio, sai
/// a entrada de **menor Peer ID** entre as não protegidas (`protected`: com conversa ou conexão, ou
/// seja, alcançáveis), se ela for menor que o Peer ID novo ou se o novo for protegido; senão o novo é
/// ignorado. Sem relógio não há "o mais velho", e uma ordem total faz todos os devices
/// convergirem para o mesmo conjunto: irmãos que trocam listas não despejam as entradas uns dos outros
/// sem parar.
fn insert_bounded(
    map: &mut HashMap<PeerId, Invite>,
    protected: &std::collections::HashSet<PeerId>,
    key: PeerId,
    invite: Invite,
) -> Inserted {
    if let Some(known) = map.get_mut(&key) {
        if *known == invite {
            return Inserted::Unchanged;
        }
        *known = invite;
        return Inserted::Changed;
    }
    if map.len() < MAX_KNOWN_DEVICES {
        map.insert(key, invite);
        return Inserted::Changed;
    }
    let victim = map.keys().filter(|k| !protected.contains(k)).min().copied();
    match victim {
        Some(victim) if protected.contains(&key) || victim < key => {
            map.remove(&victim);
            map.insert(key, invite);
            Inserted::Changed
        }
        _ => Inserted::Ignored,
    }
}

/// Endereços a tentar para um device listado: só os que passam em [`dialable`], todos se o remetente
/// é irmão e poucos se é contato.
fn dial_addrs(addrs: &[Multiaddr], from_sibling: bool) -> Vec<Multiaddr> {
    let limit = if from_sibling {
        SIBLING_DIAL_ADDRS
    } else {
        CONTACT_DIAL_ADDRS
    };
    addrs
        .iter()
        .filter(|addr| dialable(addr, from_sibling))
        .take(limit)
        .cloned()
        .collect()
}

/// IPv4 público? Nega as faixas que não são destino de um contato: loopback, "esta rede" (`0/8`),
/// privadas, link-local, CGNAT, multicast, reservadas (`240/4`), broadcast, protocolo IETF (`192.0.0.0/24`)
/// e teste de desempenho (`198.18.0.0/15`). As de documentação (`203.0.113.0/24`) passam: os testes as usam.
fn global_ipv4(ip: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && b & 0xc0 == 64)
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && b & 0xfe == 18))
}

/// IPv6 unicast global? Lista de permissão, não de negação: só `2000::/3`. Isso barra de uma vez o
/// IPv4 embutido (`::ffff:10.0.0.1`, `::10.0.0.1`), NAT64 (`64:ff9b::/96`), loopback, `fc00::/7`, `fe80::/10`,
/// site-local e multicast, que um contato usaria para me fazer discar a minha rede (auditoria P32, I1). Dentro
/// de `2000::/3` ainda nega 6to4 (`2002::/16`, embute IPv4 arbitrário) e Teredo (`2001:0::/32`).
fn global_ipv6(ip: std::net::Ipv6Addr) -> bool {
    let [first, second, ..] = ip.segments();
    first & 0xe000 == 0x2000 && first != 0x2002 && !(first == 0x2001 && second == 0)
}

/// Um endereço vindo de um terceiro vale discar? `/dnsaddr` nunca (resolve TXT escolhido por ele, vaza o
/// resolvedor e permite rebinding). De irmão (a minha identidade) o resto vale. De contato, só IP global
/// por tcp ou quic, ou circuito de relay de IP global: recusa loopback, rede privada, link-local, CGNAT e
/// nomes DNS, para a lista de um contato não me fazer varrer a minha rede nem revelar o meu IP (auditoria I2).
fn dialable(addr: &Multiaddr, from_sibling: bool) -> bool {
    use kin_transport::Protocol::{Dnsaddr, Ip4, Ip6, P2p, P2pCircuit, QuicV1, Tcp, Udp};
    if addr.iter().any(|part| matches!(part, Dnsaddr(_))) {
        return false;
    }
    if from_sibling {
        // Irmão é a minha identidade, então o resto vale (inclusive loopback, nos testes e no lab), mas
        // "qualquer endereço" e multicast nunca são um destino (auditoria P32, I4).
        return match addr.iter().next() {
            Some(Ip4(ip)) => !(ip.is_unspecified() || ip.is_multicast()),
            Some(Ip6(ip)) => !(ip.is_unspecified() || ip.is_multicast()),
            _ => true,
        };
    }
    let mut parts = addr.iter();
    let global = match parts.next() {
        Some(Ip4(ip)) => global_ipv4(ip),
        Some(Ip6(ip)) => global_ipv6(ip),
        _ => false,
    };
    global && parts.all(|part| matches!(part, Tcp(_) | Udp(_) | QuicV1 | P2p(_) | P2pCircuit))
}

/// De onde veio o próximo evento do laço de [`Chat::next_event`].
enum Next {
    Node(NodeEvent),
    Mailbox(Option<MailboxEvent>),
    /// O prazo de um re-anúncio pendente a irmão chegou (sem evento de rede).
    Tick,
}

/// Chat 1:1: liga o transporte (`kin-transport`) à conversa cifrada (`kin-crypto`).
///
/// Uma conversa por peer; persistente com [`Chat::open`], em memória com [`Chat::new`]. O
/// certificado do device cobre a chave MLS e a chave de rede (P15): o KeyPackage e o Welcome só são
/// aceitos se o Peer ID da conexão for o certificado nele, então `identity` é de quem está na linha.
pub struct Chat {
    node: Node,
    device: CryptoDevice,
    /// Só a chave de rede do device, para assinar links de convite (`invite_link`); sem a chave Nostr.
    signer: InviteSigner,
    conversations: HashMap<PeerId, Conversation>,
    pending: HashMap<SendId, Option<MessageId>>,
    /// Envios em voo que são handshake (`Hello`, `KeyPackage`, `Welcome`), com o envelope: só estes são
    /// reenviados quando falham, com os mesmos bytes (P25). Em memória: reiniciar refaz pelo `Hello`.
    handshake: HashMap<SendId, Envelope>,
    /// Quantas vezes reenviei cada tipo de handshake a cada peer nesta conexão (zera ao desconectar).
    handshake_retries: HashMap<(PeerId, std::mem::Discriminant<Envelope>), u8>,
    /// Identidade exigida de cada peer discado por convite (`accept`), até a conversa ficar pronta.
    expected: HashMap<PeerId, IdentityId>,
    /// Com `true`, quem não é conhecido só abre conversa se apresentar um token vivo de convite meu
    /// (P18, ADR 0039). Padrão `false`: a biblioteca segue aberta.
    require_invite: bool,
    /// Token do link v3 que apresento no `Hello` a cada peer discado por `accept`, até a conversa ficar pronta.
    hello_tokens: HashMap<PeerId, [u8; 16]>,
    /// Peers que apresentaram, no `Hello`, um token vivo de convite meu. O token só é gasto em `ready`.
    /// Em memória: cai com a conexão, e o `Hello` seguinte o apresenta de novo.
    admitted: HashMap<PeerId, [u8; 16]>,
    /// Identidade de quem se apresentou no `Hello` com um certificado cuja cadeia confere e cujo Peer ID de
    /// rede é o da conexão (P18 PR 3, ADR 0039). É por ela que o portão admite a minha identidade e as
    /// aceitas por `trust`, sem token. Em memória: cai com a conexão e a conversa pronta.
    hello_identity: HashMap<PeerId, IdentityId>,
    /// Peers que disseram no `Hello` que **não** têm a conversa que eu tenho com eles (P16): o `KeyPackage`
    /// ou o `Welcome` deles pode, então, substituir a conversa antiga (só com a mesma identidade). Em
    /// memória: cai com a conexão e a conversa pronta.
    lost_state: HashSet<PeerId>,
    /// Peers com conversa que acabaram de conectar e cujo `Hello` ainda não chegou (P16, auditoria, médio 1):
    /// a fila deles espera, porque o `Hello` pode dizer que eles perderam a conversa e o que eu despejasse
    /// no grupo antigo seria confirmado pelo transporte (`Delivered`) e descartado por eles.
    hello_pending: HashSet<PeerId>,
    /// Peers com quem já refiz a conversa nesta conexão: um reset por conexão (auditoria, médio 2).
    reset_done: HashSet<PeerId>,
    /// `state_id` (P36, ADR 0043) que o `Hello` de um peer apresentou e que ainda **não** está gravado: o de antes
    /// de a conversa existir, ou o do peer que acabou de provar que perdeu o banco. O `ready` o grava como a
    /// referência da conversa nova. Em memória, com teto, e some com a conexão.
    pending_state_ids: HashMap<PeerId, [u8; 16]>,
    /// Peers que o portão recusou e que já avisei nesta conexão (P34): um `NotAdmitted` por conexão, com
    /// teto. Em memória; some com a conexão.
    not_admitted_notified: HashSet<PeerId>,
    /// P18 (d): com a aprovação ligada, o token vivo não basta: o convidante confirma cada entrada.
    require_approval: bool,
    /// Pedidos aguardando `approve`/`deny` (peer e identidade do `Hello`), com teto. Em memória; some com a
    /// conexão.
    pending_approval: HashMap<PeerId, IdentityId>,
    /// Peers cujo pedido eu aprovei nesta conexão: o token deles passa pelo portão.
    approved: HashSet<PeerId>,
    /// Peers cujo pedido eu neguei nesta conexão: não geram outro pedido nem `NotAdmitted`.
    denied: HashSet<PeerId>,
    /// Peers que eu disquei por `dial` com o Peer ID fixado no endereço (`/p2p/<id>`): discar é um ato meu,
    /// então o portão os admite sem token. Em memória, com teto; some quando a conversa fica pronta.
    dialed: HashSet<PeerId>,
    /// `KeyPackage` recusado só por falta de token: o `Hello` com o token e o `KeyPackage` vão por
    /// substreams diferentes, sem ordem garantida. Guardado até o token chegar (ou a conexão cair).
    held_key_packages: HashMap<PeerId, Vec<u8>>,
    /// Mensagens MLS de época futura por peer, à espera do commit que falta (P9). Só em memória.
    future: HashMap<PeerId, Vec<Vec<u8>>>,
    /// P9 (c, ADR 0041): quando (segundos Unix) a conversa com o peer foi abandonada por `Stale` pela última
    /// vez. Dentro de [`STALE_RECOVERY_WINDOW`] o `Stale` só avisa. Só em memória.
    stale_recovered: HashMap<PeerId, u64>,
    /// `true` só enquanto uma mensagem da caixa postal é processada: o `Stale` vindo dela não recupera (entrega
    /// fora de ordem; o commit atrasado ainda salvaria a conversa).
    from_mailbox: bool,
    /// P9 (c, ADR 0041): peers cuja conversa foi abandonada por `Stale` e ainda não voltou, com a
    /// **identidade da conversa antiga** e o instante do abandono. A identidade também vai para `expected`
    /// (o portão admite o peer, o `Hello` leva o certificado, o endereço fica e o `invite`/`join` exigem
    /// essa identidade); aqui ela serve ao `ConversationReset` e ao `redial_want`, que quer rediscar o peer
    /// mesmo sem conversa e sem fila. Sai em `ready`, ao vencer [`RECOVERING_TTL`] ou no `untrust`. Só em memória.
    recovering: HashMap<PeerId, (IdentityId, u64)>,
    /// Fila de saída e ids já vistos (3.2); persistente em `open`, em memória em `new`.
    outbox: Outbox,
    /// Peers com ao menos uma conexão de rede aberta (`PeerConnected` até `PeerDisconnected`).
    connected: std::collections::HashSet<PeerId>,
    /// Peers a quem já anunciamos nossos relays nesta conexão (zera ao desconectar).
    announced: std::collections::HashSet<PeerId>,
    /// Peers a quem já anunciamos nossa lista de devices nesta conexão (3.5; zera ao desconectar).
    devices_announced: std::collections::HashSet<PeerId>,
    /// Contatos cuja lista de devices já disparou discagens nesta conexão (zera ao desconectar).
    lists_acted: std::collections::HashSet<PeerId>,
    /// Convites dos meus outros devices, como eles mesmos anunciaram (3.5). Só em memória: o endereço
    /// envelhece, e cada irmão reanuncia ao conectar. É o que repasso aos irmãos na `DeviceList`.
    household: HashMap<PeerId, Invite>,
    /// Identidades que aceitei (convite aceito ou `trust`), além da minha. Só destas se processa a lista
    /// de devices e a elas se anuncia o meu convite. Só em memória. Pequena: busca linear.
    trusted: Vec<IdentityId>,
    /// Identidades de que desconfio (`untrust`): a confiança que um irmão repassa não as aceita de volta;
    /// só `trust`/`accept` explícitos as tiram daqui. Gravadas no banco (`kin_distrusted`).
    distrusted: Vec<IdentityId>,
    /// Convites de devices de contatos aceitos, como eles mesmos anunciaram ou como um irmão repassou.
    /// Só em memória. Vão para os meus irmãos, para que eles também achem o contato.
    contact_devices: HashMap<PeerId, Invite>,
    /// Quando anunciei a lista de devices a cada irmão por causa de algo aprendido (limite de taxa).
    last_sibling_announce: HashMap<PeerId, Instant>,
    /// Limite de taxa dos tombstones (P30, 3c): por peer, o início da janela e quantos chegaram nela.
    /// Só em memória. **Não** zera ao desconectar (senão reconectar devolveria a cota); expira pela janela.
    revocations_seen: HashMap<PeerId, (Instant, u32)>,
    /// Teto global dos tombstones, de todos os peers somados: início da janela e contagem.
    revocations_global: (Instant, u32),
    /// Duração da janela do limite de tombstones (campo, para o teste não esperar 60 s).
    revocation_window: Duration,
    /// Devoluções de cota já feitas na janela (teto `REVOCATION_REFUNDS_PER_WINDOW`): início e contagem.
    revocation_refunds: (Instant, u32),
    /// Quando reenviei tombstones a cada peer (3c-ii-b): no máximo um reenvio por janela por peer.
    revocations_sent: HashMap<PeerId, Instant>,
    /// Intervalo mínimo entre reenvios ao mesmo peer (campo, para o teste não esperar 120 s). **Maior** que
    /// `revocation_window` do receptor, para dois lotes nunca caírem na mesma janela de cota dele.
    revocation_resend_gap: Duration,
    /// Peers cujo reenvio já foi avaliado **nesta conexão** (mesmo vazio ou com erro): a consulta ao store
    /// roda uma vez por conexão, não a cada mensagem. Zera ao desconectar. O que entrar no store depois sai
    /// pelo repasse de `on_revocation`.
    revocations_checked: std::collections::HashSet<PeerId>,
    /// Irmãos cujo re-anúncio caiu dentro do intervalo mínimo; saem no próximo evento depois do prazo.
    sibling_pending: std::collections::HashSet<PeerId>,
    /// Intervalo mínimo entre re-anúncios ao mesmo irmão (campo, para o teste não esperar 5 s).
    sibling_announce_gap: Duration,
    /// Reenvio direto agendado por peer depois de um `SendFailed` de mensagem de chat (P19). Só existe para
    /// peer conectado; sai no ack, ao desconectar ou quando o reenvio é tentado de novo.
    retry: HashMap<PeerId, Retry>,
    /// Espera inicial e teto do reenvio (campos, para o teste não esperar 2 s a 60 s).
    retry_base: Duration,
    retry_cap: Duration,
    /// Falhas seguidas depois das quais o timer para de armar (campo, para o teste não falhar 500 vezes).
    retry_budget: u32,
    /// Novo depósito agendado por peer depois de um `DepositFailed` (P21). Mesma estrutura e mesma espera
    /// do reenvio direto, mas conta à parte: sai no `Deposited`, ao reconectar ou ao esquecer o peer.
    deposit_retry: HashMap<PeerId, Retry>,
    /// Falhas seguidas de depósito depois das quais o timer para de armar (campo, para o teste não falhar 500 vezes).
    deposit_budget: u32,
    /// Quando a varredura por idade dos depósitos roda (P21): metade da retenção do backend depois do último
    /// depósito confirmado. `None` = nada a varrer (sem retenção declarada ou sem depósito pendente).
    sweep_due: Option<Instant>,
    /// Recusas de depósito por peer desde o último aceite (P21, auditoria I4): uma geração MLS gasta cada, e
    /// é isto que o `deposit_budget` limita. O backoff (`deposit_retry`) conta por rodada, não por recusa.
    deposit_spent: HashMap<PeerId, u32>,
    /// Rodadas de varredura por idade já feitas por peer (P21, auditoria I1): cada rodada pode re-cifrar até
    /// `MAILBOX_WINDOW` mensagens, e `sweep_budget` impõe o teto. Zera ao conectar, no ack e ao esquecer o peer.
    sweep_rounds: HashMap<PeerId, u32>,
    /// Teto de rodadas da varredura por peer (campo, para o teste não rodar 8 vezes).
    sweep_budget: u32,
    /// Endereços de escuta que cada peer informou ao conectar (`PeerIdentified`) (P32). Só memória; peer sem
    /// conversa só fica enquanto conectado (sai em `PeerDisconnected`), então o mapa é limitado pelo teto
    /// [`MAX_KNOWN_DEVICES`] e, fora de conexões vivas, pelo número de contatos.
    peer_addrs: HashMap<PeerId, Vec<Multiaddr>>,
    /// Rediscagem agendada por peer que caiu com mensagem na fila (P32). Só existe com o peer desconectado e
    /// a fila não vazia; sai ao conectar, ao esvaziar a fila e ao esquecer o peer.
    redial: HashMap<PeerId, Redial>,
    /// Endereços do convite que eu aceitei (`accept`), já filtrados: último recurso da rediscagem, antes de o
    /// peer ter informado os dele. Só memória, limitado por [`MAX_KNOWN_DEVICES`], esquecido com o peer.
    invite_addrs: HashMap<PeerId, Vec<Multiaddr>>,
    /// Espera inicial, teto e orçamento da rediscagem (campos, para o teste não esperar 2 s a 5 min nem 100 vezes).
    redial_base: Duration,
    redial_cap: Duration,
    redial_budget: u32,
    /// Só nos testes: cada rediscagem pedida ao nó, para o teste ver para quem e com quais endereços.
    #[cfg(test)]
    dial_log: Vec<(PeerId, Vec<Multiaddr>)>,
    /// Teto de linhas importadas por sessão de sincronização de histórico (campo, para o teste não
    /// alimentar 100 000 linhas).
    history_cap: usize,
    /// Irmãos a quem já pedi o histórico nesta conexão (zera ao desconectar).
    history_requested: std::collections::HashSet<PeerId>,
    /// Sessões de sincronização em curso (um pedido em voo por irmão): quantas linhas já recebi nela.
    history_sessions: HashMap<PeerId, usize>,
    /// Envios de `HistoryRequest` em voo (id → peer); falhar um deles libera o pedido (P27a).
    history_requests: HashMap<SendId, PeerId>,
    /// Endereços que este device anuncia; vazio = os do nó (`Node::shareable_addresses`).
    advertised: Vec<Multiaddr>,
    /// Mensagens da outbox enviadas e ainda sem ack, por peer (limitadas por [`SEND_WINDOW`]).
    in_flight: HashMap<PeerId, std::collections::HashSet<MessageId>>,
    /// Caixa postal opcional (3.1) e o canal por onde ela avisa o que acontece.
    mailbox: Option<Box<dyn Mailbox>>,
    mailbox_events: Option<UnboundedReceiver<MailboxEvent>>,
    /// Depósitos pedidos e ainda sem resposta da caixa postal.
    depositing: HashMap<DepositId, (PeerId, MessageId)>,
    /// Eventos já produzidos mas ainda não entregues, em ordem.
    queue: std::collections::VecDeque<ChatEvent>,
}

impl Chat {
    /// Chat só em memória: `device_key` fornece o Peer ID da rede; a chave MLS do device é gerada
    /// e certificada por `identity`. Nada sobrevive ao processo.
    pub fn new(
        identity: &dyn IdentityProvider,
        device_key: &DeviceKey,
        config: NodeConfig,
    ) -> Result<Self, Error> {
        let device = CryptoDevice::new(
            identity,
            &device_key.public_key(),
            &device_key.nostr_public_key(),
            now_secs(),
        )?;
        Self::with_device(device, device_key, config, Outbox::memory()?)
    }

    /// Chat persistente: o estado MLS e a lista de conversas ficam no banco SQLite `db`. Ao
    /// reabrir, as conversas voltam sozinhas e já aceitam mensagens, sem novo handshake.
    pub fn open(
        identity: &dyn IdentityProvider,
        device_key: &DeviceKey,
        config: NodeConfig,
        db: &Path,
    ) -> Result<Self, Error> {
        let device = CryptoDevice::open(
            identity,
            &device_key.public_key(),
            &device_key.nostr_public_key(),
            db,
            now_secs(),
        )?;
        Self::open_with_device(device, device_key, config, db)
    }

    /// Chat persistente de um device **pareado** (P30 fatia 2d-i): sem mestra nem autoridade, só com o
    /// certificado v4 que o primário emitiu e que já está no banco `db` (ver `PendingDevice`).
    /// `expected` é a identidade do primário, recebida fora de banda e guardada **fora** do banco: o
    /// certificado é conferido contra ela a cada abertura (ADR 0017). O emissor tem de ser uma autoridade:
    /// um banco criado pela mestra (certificado v3) é recusado (trinco v4, P30 3e-i).
    pub fn open_paired(
        expected: &IdentityId,
        device_key: &DeviceKey,
        config: NodeConfig,
        db: &Path,
    ) -> Result<Self, Error> {
        let device = CryptoDevice::open_paired(
            expected,
            &device_key.public_key(),
            &device_key.nostr_public_key(),
            db,
        )?;
        Self::open_with_device(device, device_key, config, db)
    }

    /// O que `open` e `open_paired` têm em comum: outbox, confiança e conversas recarregadas do banco.
    fn open_with_device(
        device: CryptoDevice,
        device_key: &DeviceKey,
        config: NodeConfig,
        db: &Path,
    ) -> Result<Self, Error> {
        // A outbox fica num arquivo ao lado do banco do `crypto` (`a.db` → `a.outbox.db`).
        let outbox = Outbox::open(&db.with_extension("outbox.db"))?;
        let mut chat = Self::with_device(device, device_key, config, outbox)?;
        chat.trusted = chat.outbox.trusted_identities()?;
        chat.distrusted = chat.outbox.distrusted_identities()?;
        // A desconfiança vence: se um `untrust` caiu no meio (gravou a lista negra e não apagou a
        // confiança), a identidade não volta a ser confiada ao reabrir.
        chat.trusted.retain(|t| !chat.distrusted.contains(t));
        for (label, id) in chat.device.remembered()? {
            // Rótulo ou conversa que não carregam mais são ignorados: o handshake refaz.
            let Ok(peer) = label.parse::<PeerId>() else {
                continue;
            };
            if let Some(conversation) = Conversation::load(&chat.device, &id)? {
                // O rótulo salvo tem de bater com o Peer ID certificado do outro membro.
                if conversation.peer_ids()? == [peer] {
                    chat.conversations.insert(peer, conversation);
                }
            }
        }
        // Backfill do trinco por identidade (P30 3e-ii-b, ADR 0032): quem aceitou um v4 antes de o trinco
        // existir não tem a marca; as conversas v4 já gravadas a refazem. **Antes** da varredura: sem marcar
        // primeiro, um banco antigo responderia "não" e a conversa v3 sobreviveria. Erro falha a abertura.
        for conversation in chat.conversations.values() {
            for certificate in conversation.peer_certificates()? {
                chat.device.learn_regime(&certificate)?;
            }
        }
        // Um device revogado com o processo fechado (ou cuja derrubada parou no meio) não volta vivo (P30,
        // 3d-ii-c). Antes de qualquer `set_mailbox`: a caixa postal nunca aprende o revogado. Se não consigo
        // consultar o store, não sei quem está revogado: a abertura falha (falha fechada), sem abrir "vivo".
        let Scan {
            to_drop,
            mut failures,
        } = chat.scan_revoked();
        if !failures.is_empty() {
            return Err(failures.swap_remove(0).1);
        }
        for (peer, identity, reason) in to_drop {
            chat.drop_peer(peer, identity, reason)?;
        }
        // Depois da varredura de revogados: quem foi derrubado não tem mais conversa e não é rediscado.
        chat.restore_peer_addrs();
        Ok(chat)
    }

    fn with_device(
        device: CryptoDevice,
        device_key: &DeviceKey,
        config: NodeConfig,
        outbox: Outbox,
    ) -> Result<Self, Error> {
        Ok(Self {
            node: Node::new(device_key.keypair().clone(), config)?,
            device,
            signer: device_key.invite_signer(),
            conversations: HashMap::new(),
            pending: HashMap::new(),
            handshake: HashMap::new(),
            handshake_retries: HashMap::new(),
            expected: HashMap::new(),
            require_invite: false,
            hello_tokens: HashMap::new(),
            admitted: HashMap::new(),
            hello_identity: HashMap::new(),
            lost_state: HashSet::new(),
            hello_pending: HashSet::new(),
            reset_done: HashSet::new(),
            pending_state_ids: HashMap::new(),
            not_admitted_notified: HashSet::new(),
            require_approval: false,
            pending_approval: HashMap::new(),
            approved: HashSet::new(),
            denied: HashSet::new(),
            dialed: HashSet::new(),
            held_key_packages: HashMap::new(),
            future: HashMap::new(),
            stale_recovered: HashMap::new(),
            from_mailbox: false,
            recovering: HashMap::new(),
            outbox,
            connected: Default::default(),
            announced: Default::default(),
            devices_announced: Default::default(),
            lists_acted: Default::default(),
            household: HashMap::new(),
            trusted: Vec::new(),
            distrusted: Vec::new(),
            contact_devices: HashMap::new(),
            last_sibling_announce: HashMap::new(),
            revocations_seen: HashMap::new(),
            revocations_global: (Instant::now(), 0),
            revocation_window: REVOCATION_WINDOW,
            revocation_refunds: (Instant::now(), 0),
            revocations_sent: HashMap::new(),
            revocation_resend_gap: REVOCATION_RESEND_GAP,
            revocations_checked: Default::default(),
            sibling_pending: Default::default(),
            sibling_announce_gap: SIBLING_ANNOUNCE_MIN,
            retry: HashMap::new(),
            retry_base: RETRY_BASE,
            retry_cap: RETRY_CAP,
            retry_budget: RETRY_BUDGET,
            deposit_retry: HashMap::new(),
            deposit_budget: RETRY_BUDGET,
            sweep_due: None,
            deposit_spent: HashMap::new(),
            sweep_rounds: HashMap::new(),
            sweep_budget: SWEEP_BUDGET,
            peer_addrs: HashMap::new(),
            redial: HashMap::new(),
            invite_addrs: HashMap::new(),
            redial_base: REDIAL_BASE,
            redial_cap: REDIAL_CAP,
            redial_budget: REDIAL_BUDGET,
            #[cfg(test)]
            dial_log: Vec::new(),
            history_cap: MAX_SYNC_ENTRIES,
            history_requested: Default::default(),
            history_requests: HashMap::new(),
            history_sessions: HashMap::new(),
            advertised: Vec::new(),
            in_flight: HashMap::new(),
            mailbox: None,
            mailbox_events: None,
            depositing: HashMap::new(),
            queue: Default::default(),
        })
    }

    /// Peers com conversa pronta (inclusive as recarregadas do banco).
    pub fn ready_peers(&self) -> Vec<PeerId> {
        self.conversations.keys().copied().collect()
    }

    pub fn peer_id(&self) -> PeerId {
        self.node.peer_id()
    }

    /// O certificado em uso por este device (o guardado no banco, se já existia).
    pub fn certificate(&self) -> &kin_identity::DeviceCertificate {
        self.device.certificate()
    }

    pub fn listen(&mut self) -> Result<(), Error> {
        Ok(self.node.listen()?)
    }

    pub fn listen_on(&mut self, addr: Multiaddr) -> Result<(), Error> {
        Ok(self.node.listen_on(addr)?)
    }

    /// Conecta direto ou, se `addr` for um endereço de circuito (`.../p2p-circuit/p2p/<peer>`), por
    /// um relay.
    ///
    /// Com o portão ([`Chat::set_require_invite`]) ligado, discar com o Peer ID fixado (`/p2p/<id>`, o
    /// último do endereço) admite esse peer sem token: o ato é seu, e o outro lado prova o Peer ID no
    /// handshake. Sem `/p2p/<id>` não há quem admitir e o portão segue valendo (ADR 0039).
    pub fn dial(&mut self, addr: Multiaddr) -> Result<(), Error> {
        let pinned = pinned_peer(&addr);
        self.node.dial(addr)?;
        if let Some(peer) = pinned
            && (self.dialed.contains(&peer) || self.dialed.len() < MAX_KNOWN_DEVICES)
        {
            self.dialed.insert(peer);
        }
        Ok(())
    }

    /// Endereço pelo qual outros discam este nó através do relay `relay_addr`.
    pub fn circuit_address(&self, relay_addr: &Multiaddr) -> Multiaddr {
        self.node.circuit_address(relay_addr)
    }

    /// Convite para este device com os endereços `addrs`. Quem o aceitar só conversa se do outro lado
    /// estiver exatamente esta identidade, neste Peer ID.
    pub fn invite(&self, addrs: Vec<Multiaddr>) -> Invite {
        Invite::new(self.device.certificate().clone(), addrs)
    }

    /// Link de convite deste device com os endereços `addrs`, válido por `ttl` a partir de agora e
    /// assinado pela chave de rede do device (P18). `ttl` passa de [`MAX_INVITE_TTL`] menos 5 minutos
    /// vira esse teto (folga para relógios desencontrados): quem lê recusa validade além de 7 dias.
    ///
    /// O link é o v3 e leva um **token de uso único** (16 bytes aleatórios, na parte assinada): fica no
    /// banco até vencer, ser usado (conversa pronta) ou revogado ([`Chat::revoke_invite`]). Com
    /// [`Chat::set_require_invite`] ligado, só quem o apresenta abre conversa. Falha com
    /// `Error::TooManyInvites` se já há 16 convites vivos (ADR 0039).
    pub fn invite_link(&mut self, addrs: Vec<Multiaddr>, ttl: Duration) -> Result<String, Error> {
        let ttl = ttl.min(MAX_INVITE_TTL - INVITE_CLOCK_SLACK);
        let now = now_secs();
        let expires_at = now.saturating_add(ttl.as_secs());
        let token: [u8; 16] = rand::random();
        self.outbox.add_invite_token(&token, expires_at, now)?;
        Ok(self
            .invite(addrs)
            .to_link_with_token(&self.signer, expires_at, token))
    }

    /// Convites emitidos por este device que ainda não foram usados nem venceram (P18, ADR 0039). Um erro
    /// de leitura do banco aparece como lista vazia.
    pub fn invites(&self) -> Vec<LiveInvite> {
        self.outbox
            .live_invite_tokens(now_secs())
            .unwrap_or_default()
            .into_iter()
            .map(|(token, expires_at)| LiveInvite {
                id: invite_id(&token),
                expires_at,
            })
            .collect()
    }

    /// Revoga o convite vivo cujo `id` começa por `prefix` (maiúsculas valem). `Ok(false)` se nenhum bate;
    /// prefixo com menos de 4 caracteres ou que bate em mais de um é `Error::InvalidInvite` (nada é
    /// revogado por engano).
    pub fn revoke_invite(&mut self, prefix: &str) -> Result<bool, Error> {
        // Mínimo de 4 caracteres: um prefixo curto demais revoga "o único que bate" por engano.
        if prefix.len() < 4 {
            return Err(Error::InvalidInvite);
        }
        let prefix = prefix.to_ascii_lowercase();
        let prefix = prefix.as_str();
        let now = now_secs();
        let mut matches = self
            .outbox
            .live_invite_tokens(now)?
            .into_iter()
            .filter(|(token, _)| invite_id(token).starts_with(prefix));
        let Some((token, _)) = matches.next() else {
            return Ok(false);
        };
        if matches.next().is_some() {
            return Err(Error::InvalidInvite);
        }
        // Quem já apresentou o token no `Hello`, mas ainda não virou conversa, perde a vaga também.
        self.admitted.retain(|_, admitted| *admitted != token);
        // O mesmo vale para os pedidos de entrada (P18 d): sem token apresentado não há o que aprovar.
        let admitted = &self.admitted;
        self.pending_approval
            .retain(|peer, _| admitted.contains_key(peer));
        self.approved.retain(|peer| admitted.contains_key(peer));
        self.outbox.take_invite_token(&token, now)
    }

    /// Com `true`, quem não é conhecido só abre conversa se apresentar um token vivo de um convite meu
    /// (P18, ADR 0039). Conhecido é quem já tem conversa, quem eu discei por convite (`accept`) e os
    /// devices de uma identidade confiável. Padrão `false`: a biblioteca segue aberta; o aplicativo liga
    /// a exigência.
    pub fn set_require_invite(&mut self, require: bool) {
        self.require_invite = require;
    }

    /// P18 (d): com `true` (e o portão de [`Chat::set_require_invite`] ligado), quem chega com um token vivo
    /// **não** abre conversa sozinho: vira pedido pendente (`ChatEvent::AdmissionRequested`, com a identidade
    /// que ele apresentou) até [`Chat::approve`]; [`Chat::deny`] o dispensa. O token só é gasto quando a
    /// conversa nasce, então um pedido pendente ou negado não queima o link. Padrão `false`.
    pub fn set_require_approval(&mut self, require: bool) {
        self.require_approval = require;
    }

    /// Aprova o pedido pendente de `peer` e retoma o handshake: com o `KeyPackage` dele guardado (eu de menor
    /// Peer ID) a conversa nasce agora; sendo eu o de maior Peer ID, mando o meu `KeyPackage`, que não saiu
    /// enquanto o pedido esperava. `Ok(false)` se `peer` não tem pedido pendente ou o token dele já não vale
    /// (gasto por outro, revogado): nada é aprovado.
    pub fn approve(&mut self, peer: PeerId) -> Result<bool, Error> {
        if !self.pending_approval.contains_key(&peer) {
            return Ok(false);
        }
        let live = match self.admitted.get(&peer) {
            Some(token) => self.outbox.invite_token_is_live(token, now_secs())?,
            None => false,
        };
        self.pending_approval.remove(&peer);
        if !live {
            return Ok(false);
        }
        self.approved.insert(peer);
        if let Some(key_package) = self.held_key_packages.remove(&peer) {
            self.on_key_package(peer, &key_package)?;
        } else if self.connected.contains(&peer)
            && self.peer_id() > peer
            && !self.conversations.contains_key(&peer)
        {
            let key_package = self.device.key_package()?;
            self.transmit(peer, &Envelope::KeyPackage(key_package));
        }
        Ok(true)
    }

    /// Nega o pedido pendente de `peer`: descarta o `KeyPackage` guardado e não pergunta de novo nesta conexão.
    /// O token **segue vivo** (revogá-lo é [`Chat::revoke_invite`]). `false` se não havia pedido.
    pub fn deny(&mut self, peer: PeerId) -> bool {
        if self.pending_approval.remove(&peer).is_none() {
            return false;
        }
        self.held_key_packages.remove(&peer);
        self.denied.insert(peer);
        true
    }

    /// Pedidos de entrada esperando `approve` ou `deny`: peer e identidade apresentada.
    pub fn pending_approvals(&self) -> Vec<(PeerId, IdentityId)> {
        self.pending_approval
            .iter()
            .map(|(peer, identity)| (*peer, identity.clone()))
            .collect()
    }

    /// Convite com os endereços que o nó considera compartilháveis (circuito de relay, externos e
    /// escuta de IP global; ver `Node::shareable_addresses`). Vazio se ainda não há nenhum.
    pub fn shareable_invite(&self) -> Invite {
        self.invite(self.node.shareable_addresses())
    }

    /// Como [`Chat::shareable_invite`], mas só com endereços de circuito de relay: o link não revela o IP
    /// de quem o gera (P18 b). Vazio se ainda não há reserva num relay.
    pub fn shareable_invite_via_relay(&self) -> Invite {
        self.invite(self.node.shareable_circuit_addresses())
    }

    /// Fixa os endereços que este device anuncia na lista de devices (IP fixo, port-forward, testes).
    /// Vazio (o padrão) usa os do nó, os mesmos de [`Chat::shareable_invite`].
    pub fn set_advertised_addrs(&mut self, addrs: Vec<Multiaddr>) {
        self.advertised = addrs;
    }

    /// O convite deste device como vai na `DeviceList`.
    fn own_invite(&self) -> Invite {
        let addrs = if self.advertised.is_empty() {
            self.node.shareable_addresses()
        } else {
            self.advertised.clone()
        };
        self.invite(addrs)
    }

    /// Passa a confiar em `identity`: processo a lista de devices dela e anuncio a ela o meu convite
    /// (inclusive aos devices dela que já estão conectados: a lista só saía quando a conversa ficava
    /// pronta). A confiança é gravada no banco e sobrevive a reiniciar. `accept` já faz isto para o
    /// dono do convite aceito. Quem só conectou a mim não é aceito: o aplicativo chama isto quando o
    /// usuário confirma o contato (ex.: ao ver `ConversationReady`).
    pub fn trust(&mut self, identity: IdentityId) -> Result<(), Error> {
        if self.is_trusted(&identity) {
            return Ok(());
        }
        // Confiar de propósito tira a identidade da lista negra do `untrust`.
        if self.is_distrusted(&identity) {
            self.outbox.remove_distrusted(&identity)?;
            self.distrusted.retain(|distrusted| *distrusted != identity);
        }
        self.outbox.add_trusted(&identity)?;
        self.trusted.push(identity.clone());
        let to_announce: Vec<PeerId> = self
            .connected
            .iter()
            .copied()
            .filter(|peer| {
                self.conversations
                    .get(peer)
                    .and_then(|conversation| conversation.peers().ok())
                    .is_some_and(|peers| peers.as_slice() == std::slice::from_ref(&identity))
            })
            .collect();
        for peer in to_announce {
            self.announce_devices(peer);
        }
        Ok(())
    }

    /// Deixa de confiar em `identity`: para de processar a lista de devices dela e de anunciar-lhe o meu
    /// convite, esquece os devices dela em `contact_devices` e `expected`, e a põe numa lista negra
    /// gravada no banco, para que a confiança que um irmão repassa não a aceite de volta. Só `trust`
    /// ou `accept` explícitos a tiram dela. **Não** bloqueia mensagens: a conversa existente continua, e
    /// o que a identidade já sabe de mim (endereços anunciados antes) continua sabido. A minha própria
    /// identidade é recusada: os meus devices são família, não contatos.
    pub fn untrust(&mut self, identity: &IdentityId) -> Result<(), Error> {
        if identity == self.device.identity() {
            return Err(Error::OwnIdentity);
        }
        // Lista negra primeiro: se o passo seguinte falhar, o lado seguro (desconfiar) já está gravado.
        self.outbox.add_distrusted(identity)?;
        self.outbox.remove_trusted(identity)?;
        if !self.distrusted.contains(identity) {
            self.distrusted.push(identity.clone());
        }
        self.trusted.retain(|trusted| trusted != identity);
        self.contact_devices
            .retain(|_, invite| invite.identity() != identity);
        self.expected.retain(|_, owner| owner != identity);
        self.recovering.retain(|_, (owner, _)| owner != identity);
        Ok(())
    }

    /// Desconfio de `identity` (`untrust`)?
    fn is_distrusted(&self, identity: &IdentityId) -> bool {
        self.distrusted.contains(identity)
    }

    /// `identity` é a minha ou foi aceita?
    fn is_trusted(&self, identity: &IdentityId) -> bool {
        identity == self.device.identity() || self.trusted.contains(identity)
    }

    /// Lista completa, só para irmãos: o meu convite, os dos irmãos e os dos devices de contatos que
    /// conheço (cada grupo em ordem de Peer ID), até o teto do codec.
    fn device_list(&self) -> Vec<Invite> {
        let sorted = |map: &HashMap<PeerId, Invite>| {
            let mut invites: Vec<Invite> = map.values().cloned().collect();
            invites.sort_by_key(|invite| invite.peer_id());
            invites
        };
        std::iter::once(self.own_invite())
            .chain(sorted(&self.household))
            .chain(sorted(&self.contact_devices))
            .take(crate::payload::MAX_DECODED_DEVICES)
            .collect()
    }

    /// O que anunciar a `peer`: irmão recebe a lista completa; contato aceito, só o meu convite (os
    /// meus outros devices o acham pelos irmãos, sem eu entregar o endereço de todos a ele); quem não
    /// aceitei, nada (privacidade, auditoria I4).
    fn device_list_for(&self, peer: &PeerId) -> Option<Vec<Invite>> {
        let identity = match self.conversations.get(peer)?.peers().ok()?.as_slice() {
            [identity] => identity.clone(),
            _ => return None,
        };
        if identity == *self.device.identity() {
            Some(self.device_list())
        } else if self.is_trusted(&identity) {
            Some(vec![self.own_invite()])
        } else {
            None
        }
    }

    fn is_sibling(&self, peer: &PeerId) -> bool {
        self.conversations
            .get(peer)
            .and_then(|c| c.peers().ok())
            .is_some_and(|peers| peers.as_slice() == std::slice::from_ref(self.device.identity()))
    }

    /// Aceita um convite: disca o dono dele e, no handshake, **exige** a identidade do convite.
    /// Identidade errada vira `ChatEvent::Dropped` e nunca uma conversa.
    ///
    /// **Não confere validade**: ela vive só no link, e quem confere é `Invite::from_link`. Quem
    /// guardar um `Invite` e chamar `accept` depois aceita um convite que já pode ter vencido (ADR 0033).
    pub fn accept(&mut self, invite: &Invite) -> Result<(), Error> {
        let peer = invite.peer_id();
        if peer == self.peer_id() {
            return Err(Error::InvalidInvite);
        }
        self.trust(invite.identity().clone())?;
        self.expected.insert(peer, invite.identity().clone());
        // O token do link v3 vai no `Hello` a este peer (e só a ele) até a conversa ficar pronta.
        match invite.token() {
            Some(token) => {
                // Cheio, sai uma entrada qualquer: quem enche isto é o usuário aceitando links, e uma
                // aceitação nova não pode perder o token em silêncio (auditoria, médio 2).
                if !has_room(&self.hello_tokens, &peer) {
                    let evict = self.hello_tokens.keys().next().copied();
                    if let Some(evict) = evict {
                        self.hello_tokens.remove(&evict);
                    }
                }
                self.hello_tokens.insert(peer, token);
            }
            None => {
                self.hello_tokens.remove(&peer);
            }
        }
        // Guarda onde o convite diz que o peer está, para a rediscagem ter por onde começar (P32,I3).
        let addrs = dial_addrs(invite.addrs(), false);
        if !addrs.is_empty() && has_room(&self.invite_addrs, &peer) {
            self.invite_addrs.insert(peer, addrs);
        }
        // Já conectado e sem conversa: o `Hello` desta conexão saiu antes do link, sem o token (e o do outro
        // lado foi recusado pelo portão), e o `dial_peer` abaixo não faz nada com a conexão existente.
        // Reapresento o `Hello` com o token; o de maior Peer ID só manda o `KeyPackage` ao receber o `Hello`
        // do outro, que já passou, então o mando junto (achado do lab, PR 3b).
        if self.connected.contains(&peer) && !self.conversations.contains_key(&peer) {
            self.send_hello(peer);
            if self.peer_id() > peer {
                // Sem `?`: o estado do aceite já foi gravado acima, e um erro de storage aqui não pode
                // devolver `Err` com o convite meio aceito (auditoria do PR 3b, baixo 2).
                match self.device.key_package() {
                    Ok(key_package) => {
                        self.transmit(peer, &Envelope::KeyPackage(key_package));
                    }
                    Err(e) => self.drop_message(peer, e.into()),
                }
            }
        }
        Ok(self.node.dial_peer(peer, invite.addrs().to_vec())?)
    }

    /// Liga uma caixa postal (3.1): com o peer offline, as mensagens da outbox são cifradas e
    /// depositadas nela, e o que ela entregar a este device entra como se tivesse vindo direto.
    pub fn set_mailbox(&mut self, mailbox: impl Mailbox + 'static) {
        let (events, receiver) = futures::channel::mpsc::unbounded();
        let mut mailbox = Box::new(mailbox);
        // Ensinar ANTES de ligar: um backend assíncrono começa a entregar assim que `attach` roda, e o
        // que chegar de um par ainda desconhecido seria descartado. Conversas recarregadas do banco
        // entram aqui; as novas são ensinadas em `ready`.
        for (peer, conversation) in &self.conversations {
            if let Some(nostr) = conversation
                .peer_nostr_keys()
                .ok()
                .and_then(|keys| keys.into_iter().next())
            {
                mailbox.learn_peer(*peer, nostr);
            }
            // Onde o contato recebe, guardado de uma sessão anterior.
            if let Ok(Some(relays)) = self.outbox.peer_relays(peer) {
                mailbox.learn_relays(*peer, relays);
            }
        }
        mailbox.attach(events);
        self.mailbox = Some(mailbox);
        self.mailbox_events = Some(receiver);
        // Varredura inicial (P21): o que já estava na fila sai agora, não no próximo envio ou queda. Peer
        // conectado fica de fora: a fila dele sai pelo caminho direto.
        let offline: Vec<PeerId> = self
            .conversations
            .keys()
            .filter(|peer| !self.connected.contains(peer))
            .copied()
            .collect();
        for peer in offline {
            self.deposit(peer);
        }
        // Depois de reiniciar, o que já estava depositado envelhece sem ninguém para re-depositar: só o
        // `Deposited` armava a varredura, e talvez nenhum aconteça (auditoria I2).
        if self.conversations.keys().any(|peer| self.queued(peer) > 0) {
            self.arm_sweep();
        }
    }

    /// Diz à caixa postal a chave Nostr do `peer`, lida do certificado dele (já validado pelo MLS).
    fn teach_mailbox(&mut self, peer: PeerId) {
        let Some(mailbox) = self.mailbox.as_mut() else {
            return;
        };
        let nostr = self
            .conversations
            .get(&peer)
            .and_then(|c| c.peer_nostr_keys().ok())
            .and_then(|keys| keys.into_iter().next());
        if let Some(nostr) = nostr {
            mailbox.learn_peer(peer, nostr);
        }
    }

    /// Quantas mensagens esperam na outbox por este peer (ainda sem confirmação de entrega).
    pub fn queued(&self, peer: &PeerId) -> usize {
        self.outbox.entries(peer).map_or(0, |entries| entries.len())
    }

    /// Há conversa E2EE pronta com o peer?
    pub fn is_ready(&self, peer: &PeerId) -> bool {
        self.conversations.contains_key(peer)
    }

    /// **Só para testes de integração** (P9 c, ADR 0041): gira a chave da conversa com `peer` e **descarta o
    /// commit** em vez de enviá-lo. Daí em diante o que este lado cifra está numa época que o outro não
    /// alcança, que é o que faz o `Stale` nascer. Em produção o commit sempre sai; não use fora de teste.
    #[doc(hidden)]
    pub fn rotate_keys_dropping_commit(&mut self, peer: PeerId) -> Result<(), Error> {
        let conversation = self
            .conversations
            .get_mut(&peer)
            .ok_or(Error::NoConversation(peer))?;
        conversation.rotate_keys()?;
        Ok(())
    }

    /// Põe `text` na outbox do peer e tenta enviar; `parent` faz a mensagem responder a outra
    /// (thread). A mensagem só é cifrada na hora de sair. Se o peer está offline, a janela está cheia
    /// ou o envio falha, chega [`ChatEvent::Queued`] e ela sai sozinha quando der; a entrega chega
    /// depois como [`ChatEvent::Delivered`].
    pub fn send(
        &mut self,
        peer: PeerId,
        text: impl Into<String>,
        parent: Option<MessageId>,
    ) -> Result<MessageId, Error> {
        if !self.conversations.contains_key(&peer) {
            return Err(Error::NoConversation(peer));
        }
        let message = Message::new(text.into(), parent);
        // Histórico (P10a), antes de enfileirar: uma queda no meio não perde a linha. Só se a conversa
        // tem exatamente uma identidade do outro lado (a mesma regra de `device_list_for`).
        if let Some(contact) = self.sole_identity(&peer) {
            self.outbox.record_history(&contact, true, &message)?;
        }
        self.queue_and_flush(&[(peer, None)], &message)?;
        Ok(message.id)
    }

    /// A identidade do outro lado da conversa com `peer`, se for exatamente uma.
    fn sole_identity(&self, peer: &PeerId) -> Option<IdentityId> {
        match self.conversations.get(peer)?.peers().ok()?.as_slice() {
            [identity] => Some(identity.clone()),
            _ => None,
        }
    }

    /// Histórico local da conversa com `contact` (a minha própria identidade para "notas para mim"),
    /// do mais antigo ao mais novo, até `limit` linhas depois do cursor `after` (o `seq` da última
    /// linha já lida; `None` = do começo). Cada mensagem lógica aparece uma vez, venha de contato,
    /// de mim ou do espelho de um irmão.
    pub fn history(
        &self,
        contact: &IdentityId,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>, Error> {
        self.outbox.history(contact, after, limit)
    }

    /// Teto de linhas do histórico **por contato** (padrão 10 000; `None` = sem teto). Passando do
    /// teto, as linhas mais antigas daquela conversa são apagadas, sem volta, na próxima gravação;
    /// baixar o teto não apaga na hora.
    pub fn set_history_limit(&mut self, limit: Option<std::num::NonZeroUsize>) {
        self.outbox.set_history_limit(limit);
    }

    /// Devices da `identity` com conversa pronta, em ordem de Peer ID. Sai dos certificados das
    /// conversas (a identidade do outro membro), então não há lista separada para ficar velha.
    pub fn devices_of(&self, identity: &IdentityId) -> Vec<PeerId> {
        let mut devices: Vec<PeerId> = self
            .conversations
            .iter()
            .filter(|(_, conversation)| {
                // Exatamente um outro membro. Um commit de membro pode ter levado outros ao grupo
                // (o `crypto` aceita `Add` de credencial válida), e esse grupo não é "um device de
                // `identity`": mandar texto em claro para ele vazaria para quem entrou.
                conversation
                    .peers()
                    .is_ok_and(|peers| peers.as_slice() == std::slice::from_ref(identity))
            })
            .map(|(peer, _)| *peer)
            .collect();
        devices.sort();
        devices
    }

    /// Como [`Chat::send`], mas para uma identidade: a MESMA mensagem (mesmo `MessageId`, para
    /// `parent` e deduplicação valerem entre devices) entra na fila de cada device dela que tem
    /// conversa pronta, cada um cifrado na sua própria conversa. Cada device tem a sua entrega
    /// (`Delivered`, `Queued`, depósito). Se gravar na outbox falhar no meio, as cópias já gravadas
    /// continuam e saem no próximo gatilho; o erro é devolvido.
    pub fn send_to(
        &mut self,
        identity: &IdentityId,
        text: impl Into<String>,
        parent: Option<MessageId>,
    ) -> Result<MessageId, Error> {
        let devices = self.devices_of(identity);
        if devices.is_empty() {
            return Err(Error::UnknownIdentity(identity.to_base58()));
        }
        let message = Message::new(text.into(), parent);
        // Histórico (P10a): a mensagem lógica, uma vez; as cópias por device são detalhe de entrega.
        self.outbox.record_history(identity, true, &message)?;
        let mut targets: Vec<(PeerId, Option<IdentityId>)> =
            devices.into_iter().map(|peer| (peer, None)).collect();
        // Espelho: meus outros devices recebem "enviei isto a `identity`" para mostrar na conversa.
        // Para mim mesma não: o irmão já recebe a mensagem comum, e o espelho a duplicaria.
        let own = self.device.identity().clone();
        if *identity != own {
            targets.extend(
                self.devices_of(&own)
                    .into_iter()
                    .map(|peer| (peer, Some(identity.clone()))),
            );
        }
        self.queue_and_flush(&targets, &message)?;
        Ok(message.id)
    }

    /// Grava `message` na outbox de cada alvo e só então tenta enviar; quem não saiu direto avisa
    /// `Queued`. O alvo com `Some(to)` é um espelho (ver [`Outbox::enqueue_mirror`]).
    fn queue_and_flush(
        &mut self,
        targets: &[(PeerId, Option<IdentityId>)],
        message: &Message,
    ) -> Result<(), Error> {
        for (peer, mirror_to) in targets {
            match mirror_to {
                Some(to) => self.outbox.enqueue_mirror(peer, message, to)?,
                None => self.outbox.enqueue(peer, message)?,
            }
        }
        for &(peer, _) in targets {
            self.flush(peer);
            if !self.is_in_flight(&peer, &message.id) {
                self.queue.push_back(ChatEvent::Queued {
                    peer,
                    message: message.id,
                });
            }
        }
        Ok(())
    }

    fn leave_flight(&mut self, peer: PeerId, message: &MessageId) {
        if let Some(flying) = self.in_flight.get_mut(&peer) {
            flying.remove(message);
        }
    }

    fn is_in_flight(&self, peer: &PeerId, message: &MessageId) -> bool {
        self.in_flight
            .get(peer)
            .is_some_and(|flying| flying.contains(message))
    }

    /// Com o peer conectado, envia o que está na outbox direto; sem conexão, deposita na caixa
    /// postal (se houver uma ligada).
    fn flush(&mut self, peer: PeerId) {
        if self.connected.contains(&peer) {
            self.send_direct(peer);
        } else {
            self.deposit(peer);
            self.start_redial(peer);
        }
    }

    /// Deposita na caixa postal as mensagens da outbox do peer que ainda não foram depositadas, da
    /// mais antiga à mais nova, até [`MAILBOX_WINDOW`] (contando as depositadas e as pedidas). A
    /// mensagem continua na outbox: só o ack do envio direto a tira.
    fn deposit(&mut self, peer: PeerId) {
        let Some(mailbox) = self.mailbox.as_ref() else {
            return;
        };
        // O relay guarda por `retention`: o que foi depositado há mais da metade disso e segue sem ack
        // volta para a fila de depósito, antes de o relay esquecê-lo.
        if let Some(retention) = mailbox.retention()
            && let Err(e) = self
                .outbox
                .expire_deposits(&peer, now_secs().saturating_sub(retention.as_secs() / 2))
        {
            return self.drop_message(peer, e);
        }
        let waiting = match self.outbox.undeposited(&peer) {
            Ok(waiting) => waiting,
            Err(e) => return self.drop_message(peer, e),
        };
        for message in waiting {
            if self
                .depositing
                .values()
                .any(|(p, m)| *p == peer && *m == message.id)
            {
                continue;
            }
            let asked = self.depositing.values().filter(|(p, _)| *p == peer).count();
            let done = match self.outbox.deposited_count(&peer) {
                Ok(done) => done,
                Err(e) => return self.drop_message(peer, e),
            };
            if asked + done >= MAILBOX_WINDOW {
                break;
            }
            let plaintext = match self.outgoing_plaintext(&peer, &message) {
                Ok(Some(plaintext)) => plaintext,
                Ok(None) => continue,
                Err(e) => return self.drop_message(peer, e),
            };
            let Some(conversation) = self.conversations.get_mut(&peer) else {
                return;
            };
            let wire = match conversation.encrypt(&plaintext) {
                Ok(wire) => wire,
                Err(e) => return self.drop_message(peer, e.into()),
            };
            if let Some(mailbox) = self.mailbox.as_mut() {
                let id = mailbox.deposit(peer, Envelope::Mls(wire).encode());
                self.depositing.insert(id, (peer, message.id));
            }
        }
    }

    /// O que se cifra para `peer` por esta entrada da outbox: a mensagem comum ou, se a entrada for
    /// um espelho (3.5), o `Payload::Sent` com a identidade de destino. `Ok(None)` = não há o que
    /// enviar e quem chama segue para a próxima entrada: a entrada já sumiu, ou o destino gravado está
    /// corrompido (nunca será enviável: é apagada e avisada com `Dropped`, para não travar a fila
    /// inteira do peer). `Err` só para falha do banco.
    fn outgoing_plaintext(
        &mut self,
        peer: &PeerId,
        message: &Message,
    ) -> Result<Option<Vec<u8>>, Error> {
        match self.outbox.entry_kind(peer, &message.id) {
            Ok(EntryKind::Plain) => Ok(Some(message.encode())),
            Ok(EntryKind::Mirror(to)) => Ok(Some(
                Payload::Sent {
                    to,
                    message: message.clone(),
                }
                .encode(),
            )),
            Ok(EntryKind::Missing) => Ok(None),
            Err(Error::Malformed) => {
                self.outbox.remove(peer, &message.id)?;
                self.drop_message(*peer, Error::Malformed);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Envia o que está na outbox do peer, do mais antigo ao mais novo, até encher a janela
    /// ([`SEND_WINDOW`]). Cada mensagem é cifrada agora, então uma nova tentativa sai com bytes
    /// diferentes; quem recebe deduplica pelo `MessageId`.
    fn send_direct(&mut self, peer: PeerId) {
        // A fila espera o `Hello` do peer (e a troca da conversa, se ele a perdeu): ver `hello_pending`.
        if self.hello_pending.contains(&peer) || self.lost_state.contains(&peer) {
            return;
        }
        let entries = match self.outbox.entries(&peer) {
            Ok(entries) => entries,
            Err(e) => return self.drop_message(peer, e),
        };
        for message in entries {
            let flying = self.in_flight.entry(peer).or_default();
            if flying.len() >= SEND_WINDOW {
                break;
            }
            if flying.contains(&message.id) {
                continue;
            }
            let plaintext = match self.outgoing_plaintext(&peer, &message) {
                Ok(Some(plaintext)) => plaintext,
                Ok(None) => continue,
                Err(e) => return self.drop_message(peer, e),
            };
            let Some(conversation) = self.conversations.get_mut(&peer) else {
                return;
            };
            let wire = match conversation.encrypt(&plaintext) {
                Ok(wire) => wire,
                Err(e) => return self.drop_message(peer, e.into()),
            };
            let id = self.node.send(peer, Envelope::Mls(wire).encode());
            self.pending.insert(id, Some(message.id));
            self.in_flight.entry(peer).or_default().insert(message.id);
        }
    }

    /// Avança o chat até o próximo evento relevante para a aplicação.
    pub async fn next_event(&mut self) -> ChatEvent {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return event;
            }
            // O que a caixa postal já avisou vem antes de esperar a rede, para um nó ocupado não
            // atrasá-la indefinidamente.
            if let Some(Ok(event)) = self.mailbox_events.as_mut().map(|rx| rx.try_recv()) {
                self.handle_mailbox(event);
                continue;
            }
            // Com re-anúncio pendente (P24) ou reenvio agendado (P19), acorda no prazo mesmo sem tráfego:
            // sem isto a lista ficaria parada numa rede quieta e a mensagem que falhou esperaria a próxima
            // conexão.
            let wake = self.wake_in();
            let node = &mut self.node;
            let mailbox_events = self.mailbox_events.as_mut();
            let waiting = async {
                let node = node.next_event();
                match mailbox_events {
                    Some(events) => {
                        let mail = events.next();
                        pin_mut!(node, mail);
                        match future::select(node, mail).await {
                            Either::Left((event, _)) => Next::Node(event),
                            Either::Right((event, _)) => Next::Mailbox(event),
                        }
                    }
                    None => Next::Node(node.await),
                }
            };
            let next = match wake {
                Some(delay) => match tokio::time::timeout(delay, waiting).await {
                    Ok(next) => next,
                    Err(_) => Next::Tick,
                },
                None => waiting.await,
            };
            match next {
                Next::Tick => {
                    self.flush_sibling_pending();
                    self.flush_due_retries();
                    self.flush_due_deposits();
                    self.expire_recovering();
                    self.flush_due_redials();
                    self.sweep_stale_deposits();
                }
                Next::Node(event) => self.handle(event),
                Next::Mailbox(Some(event)) => self.handle_mailbox(event),
                // O backend largou o canal: sem ele não há mais o que escutar.
                Next::Mailbox(None) => self.mailbox_events = None,
            }
        }
    }

    fn handle_mailbox(&mut self, event: MailboxEvent) {
        match event {
            MailboxEvent::Deposited { id } => {
                if let Some((peer, message)) = self.depositing.remove(&id) {
                    // Aceito: garante que a varredura por idade está armada.
                    self.arm_sweep();
                    if let Err(e) = self.outbox.mark_deposited(&peer, &message, now_secs()) {
                        self.drop_message(peer, e);
                    }
                    // E zera a conta de recusas, mas só se nada mais do peer ficou sem depósito (auditoria I3):
                    // o aceite de uma mensagem não pode apagar o agendamento de outra, recusada.
                    if self
                        .outbox
                        .undeposited(&peer)
                        .is_ok_and(|waiting| waiting.is_empty())
                    {
                        self.forget_deposit_retry(&peer);
                    }
                    self.queue.push_back(ChatEvent::Deposited { peer, message });
                }
            }
            MailboxEvent::RelayOutcome {
                relay,
                accepted,
                reason,
                ..
            } => self.queue.push_back(ChatEvent::MailboxRelay {
                relay,
                accepted,
                reason,
            }),
            MailboxEvent::RelaySlow { relay, .. } => {
                self.queue.push_back(ChatEvent::MailboxRelaySlow { relay })
            }
            MailboxEvent::DepositFailed { id, reason } => {
                if let Some((peer, message)) = self.depositing.remove(&id) {
                    self.schedule_deposit_retry(peer);
                    self.queue.push_back(ChatEvent::DepositFailed {
                        peer,
                        message,
                        reason,
                    });
                }
            }
            // Só mensagem MLS vem pela caixa postal. O handshake exige o Peer ID da conexão (P15),
            // que aqui seria apenas o que o backend afirma, então nunca entra por este caminho.
            MailboxEvent::Retrieved { from, blob } => match Envelope::decode(&blob) {
                Ok(Envelope::Mls(wire)) => {
                    self.from_mailbox = true;
                    self.on_envelope(from, Envelope::Mls(wire));
                    self.from_mailbox = false;
                }
                Ok(_) => self.drop_message(from, Error::Malformed),
                Err(e) => self.drop_message(from, e),
            },
        }
    }

    fn handle(&mut self, event: NodeEvent) {
        // O que venceu o prazo sai no primeiro evento depois dele (o `next_event` também acorda no prazo, sem
        // tráfego): o re-anúncio a irmão que caiu no intervalo mínimo e o reenvio da outbox.
        self.flush_sibling_pending();
        self.flush_due_retries();
        self.flush_due_deposits();
        self.expire_recovering();
        self.flush_due_redials();
        self.sweep_stale_deposits();
        match event {
            NodeEvent::Listening(addr) => self.queue.push_back(ChatEvent::Listening(addr)),
            NodeEvent::PeerConnected(peer) => {
                self.connected.insert(peer);
                // Conectado, a fila sai direto (`flush` abaixo): o novo depósito agendado não faz mais sentido.
                self.forget_deposit_retry(&peer);
                self.forget_redial(&peer);
                self.sweep_rounds.remove(&peer);
                self.queue.push_back(ChatEvent::PeerConnected(peer));
                self.announce_if_ready(peer);
                // Conversa recarregada do banco: os dois lados já a têm, então dá para anunciar já.
                self.announce_relays(peer);
                self.announce_devices(peer);
                self.announce_revocations(peer);
                // Com conversa, a fila só sai depois do `Hello` do peer (P16): ele pode ter perdido o estado.
                if self.conversations.contains_key(&peer) {
                    self.hello_pending.insert(peer);
                }
                self.send_hello(peer);
                self.flush(peer);
            }
            NodeEvent::PeerDisconnected(peer) => {
                self.connected.remove(&peer);
                self.hello_pending.remove(&peer);
                self.reset_done.remove(&peer);
                self.pending_state_ids.remove(&peer);
                self.not_admitted_notified.remove(&peer);
                self.forget_approval(&peer);
                self.retry.remove(&peer);
                // O token apresentado vale por conexão: o `Hello` seguinte o apresenta de novo.
                self.admitted.remove(&peer);
                self.hello_identity.remove(&peer);
                self.lost_state.remove(&peer);
                // Discar vale para esta conexão: se não virou conversa, a permissão não fica aberta
                // para o peer me discar depois (auditoria, baixo 3). Com conversa, `admits` já passa.
                self.dialed.remove(&peer);
                self.held_key_packages.remove(&peer);
                self.handshake_retries.retain(|(p, _), _| *p != peer);
                self.announced.remove(&peer);
                self.devices_announced.remove(&peer);
                self.lists_acted.remove(&peer);
                self.last_sibling_announce.remove(&peer);
                self.revocations_checked.remove(&peer);
                self.sibling_pending.remove(&peer);
                self.history_requested.remove(&peer);
                self.history_sessions.remove(&peer);
                self.history_requests
                    .retain(|_, requested| *requested != peer);
                // Sem conversa nem handshake esperado, o endereço informado não tem dono a rediscar: não
                // ocupa vaga do teto.
                // Em recuperação (P9 c) o peer está em `expected`: o endereço fica, é por ele que a conversa volta.
                if !self.conversations.contains_key(&peer) && !self.expected.contains_key(&peer) {
                    self.peer_addrs.remove(&peer);
                }
                self.queue.push_back(ChatEvent::PeerDisconnected(peer));
                // O que ficou na fila sem poder sair direto vai para a caixa postal.
                self.flush(peer);
            }
            NodeEvent::MessageReceived { peer, data } => match Envelope::decode(&data) {
                Ok(envelope) => self.on_envelope(peer, envelope),
                Err(e) => self.drop_message(peer, e),
            },
            NodeEvent::MessageDelivered { peer, id } => {
                self.handshake.remove(&id);
                self.history_requests.remove(&id);
                if let Some(Some(message)) = self.pending.remove(&id) {
                    // O ack fecha o ciclo: sai da fila e libera uma vaga na janela. Só o ack de um
                    // device do destinatário é "entregue": o de um irmão (espelho) só libera a fila.
                    // O tipo tem de ser lido ANTES do `remove`, que apaga a linha.
                    let mirror = matches!(
                        self.outbox.entry_kind(&peer, &message),
                        Ok(EntryKind::Mirror(_))
                    );
                    self.leave_flight(peer, &message);
                    // O ack prova que o caminho voltou: o próximo reenvio recomeça da espera base (P19).
                    self.retry.remove(&peer);
                    // E à varredura por idade: com ack o peer está de volta, as rodadas gastas não valem mais.
                    self.sweep_rounds.remove(&peer);
                    if let Err(e) = self.outbox.remove(&peer, &message) {
                        self.drop_message(peer, e);
                    }
                    if !mirror {
                        self.queue.push_back(ChatEvent::Delivered { peer, message });
                    }
                    self.flush(peer);
                }
            }
            NodeEvent::SendFailed { peer, id, reason } => {
                let handshake = self.handshake.remove(&id);
                // O pedido de histórico se perdeu com a conexão de pé: libera o estado dele, e o próximo
                // `announce_devices` (já liberado abaixo, e com o seu limite de taxa) pede de novo do
                // cursor salvo. Sem reenvio aqui, para não virar laço (P27a).
                if let Some(requested) = self.history_requests.remove(&id) {
                    self.history_requested.remove(&requested);
                    self.history_sessions.remove(&requested);
                }
                match self.pending.remove(&id).flatten() {
                    // Mensagem de chat: continua na outbox. Com o peer conectado, o reenvio é agendado com
                    // espera crescente (P19) e só a primeira falha seguida avisa; sem conexão, sai ao
                    // reconectar.
                    Some(message) => {
                        self.leave_flight(peer, &message);
                        let first = !self.connected.contains(&peer) || self.schedule_retry(peer);
                        if first {
                            self.queue.push_back(ChatEvent::Queued { peer, message });
                        }
                    }
                    // Handshake ou anúncio (controle): sem outbox, quem decide é a aplicação. Os
                    // anúncios podem ter se perdido, então liberam o próximo gatilho (`on_mls`, algo
                    // aprendido). Sem reenvio imediato: se o envio seguir falhando, não vira laço.
                    None => {
                        if let Some(envelope) = handshake {
                            self.resend_handshake(peer, envelope);
                        }
                        self.announced.remove(&peer);
                        self.devices_announced.remove(&peer);
                        self.queue.push_back(ChatEvent::SendFailed {
                            peer,
                            message: None,
                            reason,
                        });
                    }
                }
            }
            NodeEvent::PeerRoute { peer, relayed } => {
                self.queue.push_back(ChatEvent::Route { peer, relayed });
            }
            NodeEvent::NatStatus(status) => self.queue.push_back(ChatEvent::Nat(status)),
            NodeEvent::RelayReserved { relay } => {
                self.queue.push_back(ChatEvent::RelayReserved { relay });
            }
            NodeEvent::RelayLost { relay } => {
                self.queue.push_back(ChatEvent::RelayLost { relay });
            }
            NodeEvent::HolePunch { peer, result } => {
                self.queue.push_back(ChatEvent::HolePunch { peer, result });
            }
            NodeEvent::DialFailed { peer, reason } => {
                // A discagem morreu: libera a vaga em `expected`. Uma lista que repetir o dial refaz.
                if let Some(peer) = peer {
                    // Em recuperação (P9 c) a identidade fixada em `expected` fica: é ela que o handshake exige.
                    self.release_expected(&peer);
                    self.dialed.remove(&peer);
                    self.lost_state.remove(&peer);
                    self.forget_unreachable(&peer);
                    // Falha da rediscagem de fundo é do registro, não da aplicação: um evento por tentativa
                    // (até ~100 por peer e episódio, com os endereços no texto) seria ruído (auditoria S2).
                    if self.redial_failed(peer) {
                        return;
                    }
                }
                self.queue.push_back(ChatEvent::DialFailed { peer, reason });
            }
            NodeEvent::PeerIdentified {
                peer, listen_addrs, ..
            } => self.learn_peer_addrs(peer, listen_addrs),
            NodeEvent::PeerDiscovered(..) => {}
        }
    }

    /// A discagem a `peer` falhou: esquece o convite dele em `household` e `contact_devices`, o que
    /// libera a vaga do teto e o tira da lista que repasso. Só se não há conversa nem conexão com ele:
    /// com qualquer das duas ele é alcançável e a falha é só de um endereço velho. Se um irmão
    /// reanunciar o item, ele volta.
    fn forget_unreachable(&mut self, peer: &PeerId) {
        if self.conversations.contains_key(peer) || self.connected.contains(peer) {
            return;
        }
        self.household.remove(peer);
        self.contact_devices.remove(peer);
    }

    fn on_envelope(&mut self, peer: PeerId, envelope: Envelope) {
        let result = match envelope {
            Envelope::Hello(token, certificate, presence) => {
                self.on_hello(peer, token, certificate, presence)
            }
            Envelope::KeyPackage(kp) => self.on_key_package(peer, &kp),
            Envelope::Welcome(welcome) => self.on_welcome(peer, &welcome),
            Envelope::Mls(wire) => self.on_mls(peer, &wire),
        };
        if let Err(e) = result {
            self.drop_message(peer, e);
        }
    }

    /// Só o de maior Peer ID responde com KeyPackage; o de menor convida (evita convite duplo).
    fn on_hello(
        &mut self,
        peer: PeerId,
        token: Option<[u8; 16]>,
        certificate: Option<Vec<u8>>,
        presence: Option<Presence>,
    ) -> Result<(), Error> {
        let has_conversation = presence.map(|p| p.has_conversation);
        // P36: o `state_id` do peer difere do que gravei ao formar a conversa?
        let new_instance = presence
            .and_then(|p| p.state_id)
            .is_some_and(|state_id| self.note_state_id(peer, state_id));
        // P16: `peer` diz que não tem a conversa que eu tenho com ele (perdeu o banco). Só marca: a conversa
        // antiga continua valendo até um `KeyPackage`/`Welcome` novo, da mesma identidade, a substituir.
        // `None` (formatos antigos do `Hello`) não diz nada.
        //
        // P35: só vale se a conversa já existia quando a conexão abriu (`hello_pending`). Numa conversa formada
        // nesta conexão, um `Hello` "não tenho conversa" é velho (montado antes dela, chegou depois: os
        // substreams não têm ordem) e marcar `lost_state` travaria a fila do peer até a conexão cair.
        //
        // P36 (ADR 0043): o `hello_pending` só existe na 1ª conexão. Numa 2ª (a antiga ainda de pé), o `Hello`
        // "não tenho conversa" vale se traz um `state_id` **diferente** do gravado: o velho do P35 vem da mesma
        // instância que formou a conversa, então traz o mesmo.
        let was_pending = self.hello_pending.remove(&peer);
        if has_conversation.is_some()
            && (was_pending || new_instance)
            && self.lost_state_candidate(&peer)
        {
            let newly_marked = self.lost_state.insert(peer);
            // Quem tem o menor Peer ID não responde com `KeyPackage` e, na 2ª conexão, não mandou `Hello` (só sai
            // em `PeerConnected`): sem ele o `b` (maior) nunca manda o `KeyPackage`.
            if newly_marked && new_instance && !was_pending && self.peer_id() < peer {
                self.send_hello(peer);
            }
        }
        // O `Hello` chegou: a fila que esperava sai, a não ser que o peer tenha dito que perdeu a conversa
        // (então ela sai pela conversa nova, depois da troca, em `ready`).
        if self.conversations.contains_key(&peer) && !self.lost_state.contains(&peer) {
            self.flush(peer);
        }
        let mut introduced = token.is_some();
        // Um certificado inválido, ou de outro Peer ID, é recusado inteiro (nem o token vale): quem o manda
        // está forjando ou com defeito.
        if let Some(certificate) = certificate {
            self.note_hello_identity(peer, &certificate)?;
            introduced = true;
        }
        if let Some(token) = token {
            self.note_invite_token(peer, token)?;
        }
        // Só aqui a recusa é definitiva para quem ainda não tem conversa: o `Hello` já trouxe o que o peer
        // tinha a apresentar (token, certificado). Antes disso (só o `KeyPackage`), ele ainda pode se apresentar.
        if !self.request_approval(peer) {
            self.notify_not_admitted(peer);
        }
        // O `KeyPackage` pode ter chegado antes deste `Hello` (substreams sem ordem): agora que o token ou
        // a identidade chegou, ele é processado (e guardado de novo se ainda não for admitido).
        if (introduced || self.lost_state.contains(&peer))
            && let Some(key_package) = self.held_key_packages.remove(&peer)
        {
            return self.on_key_package(peer, &key_package);
        }
        let needs_handshake =
            !self.conversations.contains_key(&peer) || self.lost_state.contains(&peer);
        if needs_handshake && self.peer_id() > peer {
            if !self.admits(&peer)? {
                return Ok(());
            }
            let kp = self.device.key_package()?;
            self.transmit(peer, &Envelope::KeyPackage(kp));
        }
        Ok(())
    }

    /// P36 (ADR 0043): registra o `state_id` que o `Hello` de `peer` apresentou e diz se ele **difere** do que
    /// gravei quando a conversa com `peer` foi formada (`true` = outra instância de estado: o banco foi trocado).
    ///
    /// - Com conversa e referência gravada: só compara; a referência **não** muda aqui (quem a troca é o `ready`
    ///   da conversa nova, com o id que ficou em `pending_state_ids`).
    /// - Com conversa e **sem** referência (conversa de antes do P36, ou linha ilegível): grava este id e devolve
    ///   `false`. Não confia no 1º `Hello` para decidir nada, só passa a tê-lo como referência.
    /// - Sem conversa: guarda em memória, com teto, para o `ready` gravar (o `Hello` do `b` sai antes de a
    ///   conversa existir, e é assim que o `a` aprende a referência).
    ///
    /// Erro de leitura do banco vale como "sem referência": nunca marca `lost_state` por falha de disco.
    fn note_state_id(&mut self, peer: PeerId, state_id: [u8; 16]) -> bool {
        if self.conversations.contains_key(&peer) {
            match self.outbox.peer_state(&peer) {
                Ok(Some(known)) if known != state_id => {
                    self.pending_state_ids.insert(peer, state_id);
                    return true;
                }
                Ok(None) => {
                    let _ = self.outbox.set_peer_state(&peer, state_id);
                }
                Ok(Some(_)) | Err(_) => {}
            }
            return false;
        }
        if self.pending_state_ids.contains_key(&peer)
            || self.pending_state_ids.len() < MAX_HELD_KEY_PACKAGES
        {
            self.pending_state_ids.insert(peer, state_id);
        }
        false
    }

    /// Posso marcar `peer` como "perdeu o estado" (P16)? Só com uma conversa **1:1** com ele (a identidade
    /// da conversa antiga é o que o `KeyPackage` novo tem de repetir) e com vaga (teto de `held`).
    fn lost_state_candidate(&self, peer: &PeerId) -> bool {
        !self.reset_done.contains(peer)
            && (self.lost_state.contains(peer) || self.lost_state.len() < MAX_HELD_KEY_PACKAGES)
            && self.old_identity(peer).is_some()
    }

    /// A identidade de uma conversa 1:1 (do outro lado), se for 1:1.
    fn old_identity_of(&self, conversation: &Conversation) -> Option<IdentityId> {
        match conversation.peers().ok()?.as_slice() {
            [identity] => Some(identity.clone()),
            _ => None,
        }
    }

    /// A identidade da conversa 1:1 que tenho com `peer`, se há uma.
    fn old_identity(&self, peer: &PeerId) -> Option<IdentityId> {
        let peers = self.conversations.get(peer)?.peers().ok()?;
        match peers.as_slice() {
            [identity] => Some(identity.clone()),
            _ => None,
        }
    }

    /// O `Hello` de `peer` trouxe `certificate`. Só vale se a cadeia confere (`verify`) e o Peer ID de rede
    /// certificado é o da conexão, que o libp2p já autenticou: sem isso, qualquer um repetiria o certificado
    /// de uma identidade confiável. Guarda a identidade para o portão (`admits`). Não confere revogação:
    /// isso é do `Conversation::invite`/`join`, que vêm depois do portão.
    fn note_hello_identity(&mut self, peer: PeerId, certificate: &[u8]) -> Result<(), Error> {
        let certificate = DeviceCertificate::from_bytes(certificate).map_err(|_| Error::Malformed)?;
        if certificate.verify().is_err() || certificate.network_peer_id() != peer {
            return Err(Error::Malformed);
        }
        // Com pedido pendente ou aprovado, a identidade que o dono viu é a que vale: um 2º `Hello` com outro
        // certificado do mesmo Peer ID não a troca (P18 d, auditoria, baixo 1).
        if self.pending_approval.contains_key(&peer) || self.approved.contains(&peer) {
            return Ok(());
        }
        if self.hello_identity.contains_key(&peer) || self.hello_identity.len() < MAX_HELD_KEY_PACKAGES {
            self.hello_identity
                .insert(peer, certificate.identity().clone());
        }
        Ok(())
    }

    /// O `Hello` de `peer` trouxe `token`. Se é de um convite meu ainda vivo: quem **já é contato** (já tem
    /// conversa) gasta o token na hora, porque `ready` não vai rodar de novo para ele e o link ficaria
    /// vivo (auditoria, baixo 1); quem não é fica admitido até a conversa nascer, quando o token é gasto
    /// (`ready`). Vale com o portão ligado ou não: o uso único não depende dele. Token que não é meu,
    /// vencido, usado ou revogado é ignorado em silêncio (nada de estado, nada de sinal a quem sonda).
    fn note_invite_token(&mut self, peer: PeerId, token: [u8; 16]) -> Result<(), Error> {
        let now = now_secs();
        if !self.outbox.invite_token_is_live(&token, now)? {
            return Ok(());
        }
        if self.conversations.contains_key(&peer) {
            self.outbox.take_invite_token(&token, now)?;
        } else if has_room(&self.admitted, &peer)
            // Pedido pendente ou aprovado fica preso ao token com que foi feito (P18 d, auditoria, baixo 2).
            && !self.pending_approval.contains_key(&peer)
            && !self.approved.contains(&peer)
        {
            self.admitted.insert(peer, token);
        }
        Ok(())
    }

    /// `peer` pode abrir conversa comigo? Sem a exigência, sempre. Com ela, só quem já tem conversa, quem
    /// eu discei (por convite ou por `dial` com Peer ID fixado), os devices de uma identidade confiável
    /// (por lista ou pelo certificado autenticado do `Hello`: a minha identidade e as aceitas por `trust`),
    /// ou quem apresentou um token que **ainda** está vivo (outro peer pode tê-lo gastado ou eu o ter
    /// revogado desde o `Hello`).
    fn admits(&self, peer: &PeerId) -> Result<bool, Error> {
        if !self.require_invite
            || self.knows(peer)
            || self
                .hello_identity
                .get(peer)
                .is_some_and(|identity| self.is_trusted(identity))
        {
            return Ok(true);
        }
        match self.admitted.get(peer) {
            // Com a aprovação ligada, o token vivo só vale para quem eu aprovei (P18 d).
            Some(token) => Ok((!self.require_approval || self.approved.contains(peer))
                && self.outbox.invite_token_is_live(token, now_secs())?),
            None => Ok(false),
        }
    }

    /// P18 (d): `peer` apresentou um token vivo e a aprovação está ligada: vira pedido pendente e avisa
    /// (`AdmissionRequested`, uma vez por conexão). Devolve `true` se o peer está (ou esteve, nesta conexão)
    /// em pedido, e então o `NotAdmitted` não deve sair por cima. `false` para quem o portão admite, quem
    /// não tem token vivo ou identidade (segue o `NotAdmitted` de sempre) e quando o teto de pendentes
    /// acabou.
    fn request_approval(&mut self, peer: PeerId) -> bool {
        if !self.require_invite
            || !self.require_approval
            || !matches!(self.admits(&peer), Ok(false))
        {
            return false;
        }
        if self.denied.contains(&peer) || self.pending_approval.contains_key(&peer) {
            return true;
        }
        let (Some(token), Some(identity)) = (
            self.admitted.get(&peer).copied(),
            self.hello_identity.get(&peer).cloned(),
        ) else {
            return false;
        };
        if !matches!(
            self.outbox.invite_token_is_live(&token, now_secs()),
            Ok(true)
        ) || self.pending_approval.len() >= MAX_PENDING_APPROVALS
        {
            return false;
        }
        self.pending_approval.insert(peer, identity.clone());
        self.queue
            .push_back(ChatEvent::AdmissionRequested { peer, identity });
        true
    }

    /// Esquece o que a aprovação guardava sobre `peer` (a conexão caiu ou a conversa nasceu).
    fn forget_approval(&mut self, peer: &PeerId) {
        self.pending_approval.remove(peer);
        self.approved.remove(peer);
        self.denied.remove(peer);
    }

    /// P34: avisa (`NotAdmitted`) que o portão recusou `peer`, uma vez por conexão e com teto. Só quando
    /// `admits` devolveu `Ok(false)`: erro de banco não é recusa. Nada vai ao peer.
    fn notify_not_admitted(&mut self, peer: PeerId) {
        // `contains` antes de `admits`: um `Hello` repetido de quem já avisei não consulta o banco.
        if self.not_admitted_notified.contains(&peer) || !matches!(self.admits(&peer), Ok(false)) {
            return;
        }
        if self.not_admitted_notified.len() < MAX_NOT_ADMITTED_NOTIFIED {
            self.not_admitted_notified.insert(peer);
            self.queue.push_back(ChatEvent::NotAdmitted { peer });
        }
    }

    /// A identidade que o `KeyPackage`/`Welcome` de `peer` tem de ter: a do convite que eu aceitei, ou, na
    /// falta dela, a que o certificado do `Hello` autenticou. Sem esta amarração, quem tem as chaves de um
    /// device de uma identidade confiável passaria pelo portão com o `Hello` dela e abriria a conversa com
    /// um certificado de outra identidade (auditoria, médio 1). A revogação é conferida no `credential::verify`
    /// que o `invite`/`join` fazem sobre esse mesmo certificado.
    fn expected_identity(&self, peer: &PeerId) -> Option<&IdentityId> {
        self.expected
            .get(peer)
            .or_else(|| self.hello_identity.get(peer))
    }

    /// Guarda (com teto) o `KeyPackage` de `peer` até chegar o `Hello` que o torna utilizável: o `Hello` e o
    /// `KeyPackage` vão por substreams sem ordem. Serve ao token (P18), à identidade (PR 3a) e ao "não tenho
    /// conversa" (P16).
    fn hold_key_package(&mut self, peer: PeerId, key_package: &[u8]) {
        if key_package.len() <= MAX_HELD_KEY_PACKAGE
            && (self.held_key_packages.contains_key(&peer)
                || self.held_key_packages.len() < MAX_HELD_KEY_PACKAGES)
        {
            self.held_key_packages.insert(peer, key_package.to_vec());
        }
    }

    fn on_key_package(&mut self, peer: PeerId, key_package: &[u8]) -> Result<(), Error> {
        if self.peer_id() > peer {
            return Ok(());
        }
        // P16: com a conversa antiga e `peer` dizendo que a perdeu, este `KeyPackage` pode substituí-la.
        let replacing = self.lost_state.contains(&peer) && self.conversations.contains_key(&peer);
        if self.conversations.contains_key(&peer) && !replacing {
            // O `Hello` "não tenho conversa" pode ainda não ter chegado: guarda em vez de perder.
            self.hold_key_package(peer, key_package);
            return Ok(());
        }
        // Erro de leitura do banco conta como "não admitido" (falha fechada) e guarda igual (auditoria, baixo 2).
        if !self.admits(&peer).unwrap_or(false) {
            // Pode ser o `Hello` com o token que ainda não chegou: guarda (com teto) em vez de perder.
            self.hold_key_package(peer, key_package);
            return Ok(());
        }
        // Substituir exige a identidade da conversa antiga; sem ela (a conversa mudou), nada é refeito.
        let old_identity = if replacing {
            let Some(identity) = self.old_identity(&peer) else {
                self.lost_state.remove(&peer);
                return Ok(());
            };
            Some(identity)
        } else {
            None
        };
        let mut conversation = Conversation::create(&self.device)?;
        let invite = match conversation.invite(
            key_package,
            &Expected {
                identity: old_identity
                    .as_ref()
                    .or_else(|| self.expected_identity(&peer)),
                peer: Some(&peer),
            },
        ) {
            Ok(invite) => invite,
            Err(e) => {
                // O grupo já está no banco: sem isto, um `KeyPackage` recusado (revogado, identidade errada)
                // deixaria um grupo órfão por tentativa (P31 am). Sem teste no `chat`: não há como ver grupos.
                if let Err(delete) = conversation.delete() {
                    self.drop_message(peer, delete.into());
                }
                // Recusado, a conversa antiga fica intacta e a suspeita de perda acaba aqui (uma por vez).
                self.lost_state.remove(&peer);
                return Err(e.into());
            }
        };
        self.transmit(peer, &Envelope::Welcome(invite.welcome));
        if replacing {
            self.replace_conversation(peer, conversation)
        } else {
            self.ready(peer, conversation)
        }
    }

    /// P16: troca a conversa antiga com `peer` pela nova, **já verificada** (identidade e Peer ID da antiga,
    /// revogação). Descarta o que valia só para o grupo antigo (anúncios, sessões de histórico, mensagens de
    /// época futura), avisa com `ConversationReset` antes do `ConversationReady`, e só no fim apaga o grupo
    /// antigo: se o apagamento falhar a conversa nova já funciona e o erro vira `Dropped`. A fila (`outbox`)
    /// **não** é tocada: o que estava para sair sai pela conversa nova.
    fn replace_conversation(
        &mut self,
        peer: PeerId,
        conversation: Conversation,
    ) -> Result<(), Error> {
        self.lost_state.remove(&peer);
        self.reset_done.insert(peer);
        let identity = self.old_identity(&peer);
        let old = self.conversations.remove(&peer);
        self.forget_conversation_state(peer);
        if let Some(identity) = identity {
            self.queue
                .push_back(ChatEvent::ConversationReset { peer, identity });
        }
        if let Err(e) = self.ready(peer, conversation) {
            // Se a nova não entrou, a antiga volta: sem conversa nenhuma não há cura até a próxima conexão
            // (auditoria, baixo 3). Se entrou e algo depois falhou, a nova fica.
            if let Some(old) = old
                && !self.conversations.contains_key(&peer)
            {
                self.conversations.insert(peer, old);
            }
            return Err(e);
        }
        if let Some(old) = old
            && let Err(e) = old.delete()
        {
            self.drop_message(peer, e.into());
        }
        Ok(())
    }

    fn on_welcome(&mut self, peer: PeerId, welcome: &[u8]) -> Result<(), Error> {
        if self.peer_id() < peer {
            return Ok(());
        }
        let replacing = self.lost_state.contains(&peer) && self.conversations.contains_key(&peer);
        if self.conversations.contains_key(&peer) && !replacing {
            return Ok(());
        }
        // Repetido aqui de propósito: o token pode ter sido gasto por outro peer ou revogado entre o
        // `Hello` e este `Welcome` (dois convidados com o mesmo link).
        if !self.admits(&peer)? {
            return Ok(());
        }
        let old_identity = if replacing {
            let Some(identity) = self.old_identity(&peer) else {
                self.lost_state.remove(&peer);
                return Ok(());
            };
            Some(identity)
        } else {
            None
        };
        let conversation = match Conversation::join(
            &self.device,
            welcome,
            &Expected {
                identity: old_identity
                    .as_ref()
                    .or_else(|| self.expected_identity(&peer)),
                peer: Some(&peer),
            },
        ) {
            Ok(conversation) => conversation,
            Err(e) => {
                // Recusado, a conversa antiga fica intacta e a suspeita de perda acaba aqui.
                self.lost_state.remove(&peer);
                return Err(e.into());
            }
        };
        if replacing {
            self.replace_conversation(peer, conversation)?;
        } else {
            self.ready(peer, conversation)?;
        }
        // Quem entra por Welcome anuncia primeiro: o convidador já tem a conversa. O convidador só
        // anuncia ao receber algo do outro (ver `on_mls`), porque o Welcome e esta mensagem seguem
        // por substreams diferentes e a ordem de chegada não é garantida.
        self.announce_relays(peer);
        self.announce_devices(peer);
        self.announce_revocations(peer);
        Ok(())
    }

    /// Anuncia ao `peer` os relays em que este device recebe, uma vez por conexão. Mensagem de
    /// controle dentro do MLS, fora da outbox: se se perder, a próxima conexão repete.
    fn announce_relays(&mut self, peer: PeerId) {
        if !self.connected.contains(&peer) || self.announced.contains(&peer) {
            return;
        }
        let Some(relays) = self.mailbox.as_ref().map(|m| m.relays()) else {
            return;
        };
        if relays.is_empty() {
            return;
        }
        let Some(conversation) = self.conversations.get_mut(&peer) else {
            return;
        };
        match conversation.encrypt(&Payload::RelayList(relays).encode()) {
            Ok(wire) => {
                self.announced.insert(peer);
                self.transmit(peer, &Envelope::Mls(wire));
            }
            Err(e) => self.drop_message(peer, e.into()),
        }
    }

    /// Anuncia ao `peer` os devices desta identidade (o meu e os dos meus irmãos), uma vez por
    /// conexão, como mensagem de controle dentro do MLS, fora da outbox: se se perder, a próxima
    /// conexão repete. Contato e irmão recebem a mesma lista.
    fn announce_devices(&mut self, peer: PeerId) {
        if !self.connected.contains(&peer) || self.devices_announced.contains(&peer) {
            return;
        }
        let Some(list) = self.device_list_for(&peer) else {
            return;
        };
        let Some(conversation) = self.conversations.get_mut(&peer) else {
            return;
        };
        match conversation.encrypt(&Payload::DeviceList(list).encode()) {
            Ok(wire) => {
                self.devices_announced.insert(peer);
                self.transmit(peer, &Envelope::Mls(wire));
                // O outro lado já tem a conversa (mesmo ponto do anúncio): se for irmão, pede o histórico.
                self.request_history(peer);
            }
            Err(e) => self.drop_message(peer, e.into()),
        }
    }

    /// Um device (já em conversa) listou devices. As regras:
    /// - **Consentimento**: de quem não aceitei (nem é irmão), a lista é ignorada em silêncio.
    /// - **Contato aceito**: todo item tem de ser da MESMA identidade de quem enviou (o `sender`
    ///   autenticado pelo MLS), ou a lista inteira é recusada; só a primeira lista por conexão age.
    ///   O item que é o convite do próprio remetente fica em `contact_devices`.
    /// - **Irmão** (a minha identidade): itens da minha identidade viram a "família"; itens de outra
    ///   identidade são devices de contatos que o irmão repassa, e eu passo a confiar nessa identidade.
    ///   O certificado de cada item já foi conferido ao ler, então só devices genuínos passam.
    /// - O que ainda não tem conversa é discado (só endereços que passam em [`dialable`]), exigindo
    ///   no handshake a identidade dona do item.
    fn on_device_list(
        &mut self,
        peer: PeerId,
        sender: &IdentityId,
        invites: Vec<Invite>,
    ) -> Result<(), Error> {
        let from_sibling = sender == self.device.identity();
        if !from_sibling && !self.is_trusted(sender) {
            return Ok(());
        }
        if !from_sibling && invites.iter().any(|invite| invite.identity() != sender) {
            return Err(Error::InvalidDeviceList(
                "item de outra identidade".to_string(),
            ));
        }
        // De um contato, só a primeira lista por conexão age: cada lista pode trazer Peer IDs novos,
        // então sem isto uma conexão geraria discagens sem fim (auditoria I1).
        if !from_sibling && !self.lists_acted.insert(peer) {
            return Ok(());
        }
        let me = self.peer_id();
        let mut learned = false;
        // Quem tem conversa ou conexão é alcançável e nunca é despejado de `household`/`contact_devices`.
        let protected: std::collections::HashSet<PeerId> = self
            .conversations
            .keys()
            .chain(self.connected.iter())
            .copied()
            .collect();
        for invite in invites {
            let device = invite.peer_id();
            if device == me {
                continue;
            }
            let owner = invite.identity().clone();
            // Teto de cada mapa (P24): Peer ID novo com o mapa cheio é ignorado por inteiro (nem
            // guardado, nem discado); quem já está segue sendo atualizado.
            if from_sibling && owner == *self.device.identity() {
                match insert_bounded(&mut self.household, &protected, device, invite.clone()) {
                    Inserted::Ignored => continue,
                    outcome => learned |= outcome == Inserted::Changed,
                }
            } else if from_sibling || device == peer {
                // Device de contato repassado por um irmão, ou o convite que o contato anuncia de si.
                // Identidade que desconfio (`untrust`) não volta pela confiança herdada do irmão.
                if from_sibling && self.is_distrusted(&owner) {
                    continue;
                }
                match insert_bounded(
                    &mut self.contact_devices,
                    &protected,
                    device,
                    invite.clone(),
                ) {
                    Inserted::Ignored => continue,
                    outcome => learned |= outcome == Inserted::Changed,
                }
                if from_sibling {
                    self.trust(owner.clone())?;
                }
            }
            if self.conversations.contains_key(&device) {
                continue;
            }
            let addrs = dial_addrs(invite.addrs(), from_sibling);
            if addrs.is_empty() {
                continue;
            }
            // Não troca uma identidade que eu já exigi para esse Peer ID (por `accept`, por exemplo): o
            // certificado não prova posse do device, então outra identidade poderia reivindicar o Peer
            // ID alheio e travar o handshake dele (auditoria I3).
            if self
                .expected
                .get(&device)
                .is_some_and(|known| *known != owner)
            {
                continue;
            }
            if !has_room(&self.expected, &device) {
                continue;
            }
            // Repetir o dial é inofensivo: o libp2p ignora quando já está discando ou conectado.
            self.expected.insert(device, owner);
            if let Err(e) = self.node.dial_peer(device, addrs) {
                self.release_expected(&device);
                self.forget_unreachable(&device);
                self.queue.push_back(ChatEvent::DialFailed {
                    peer: Some(device),
                    reason: e.to_string(),
                });
            }
        }
        if learned {
            // Algo novo (irmão, ou device de contato): os irmãos conectados precisam saber. O contato
            // só recebe o meu próprio convite, que não mudou.
            let siblings: Vec<PeerId> = self
                .connected
                .iter()
                .copied()
                .filter(|p| self.is_sibling(p))
                .collect();
            for target in siblings {
                self.re_announce_to_sibling(target);
            }
        }
        Ok(())
    }

    /// Re-anuncia a lista a um irmão, no máximo uma vez por `sibling_announce_gap`. Dentro do
    /// intervalo, marca o irmão como pendente (o que se aprendeu já está guardado, só o anúncio espera).
    fn re_announce_to_sibling(&mut self, target: PeerId) {
        let due = self
            .last_sibling_announce
            .get(&target)
            .is_none_or(|last| last.elapsed() >= self.sibling_announce_gap);
        if due {
            self.sibling_pending.remove(&target);
            self.last_sibling_announce.insert(target, Instant::now());
            self.devices_announced.remove(&target);
            self.announce_devices(target);
        } else {
            self.sibling_pending.insert(target);
        }
    }

    /// Quanto falta para o primeiro re-anúncio pendente vencer (`None` sem pendência).
    fn sibling_wake_in(&self) -> Option<Duration> {
        self.sibling_pending
            .iter()
            .filter_map(|target| self.last_sibling_announce.get(target))
            .map(|last| self.sibling_announce_gap.saturating_sub(last.elapsed()))
            .min()
    }

    /// Espera antes do reenvio depois de `attempts` falhas seguidas: a base, dobrando a cada falha, até o
    /// teto. O deslocamento é limitado, então muitas falhas não estouram a conta.
    fn retry_delay(&self, attempts: u32) -> Duration {
        let doublings = attempts.saturating_sub(1).min(16);
        self.retry_base
            .saturating_mul(1 << doublings)
            .min(self.retry_cap)
    }

    /// Registra uma falha de envio direto a `peer` e agenda o reenvio, a não ser que o orçamento de falhas
    /// tenha acabado (`retry_budget`): aí o registro fica, sem armar o timer, e a mensagem espera a próxima
    /// conexão ou um ack. Devolve `true` se é a primeira falha seguida (a única que avisa com `Queued`).
    fn schedule_retry(&mut self, peer: PeerId) -> bool {
        let attempts = self
            .retry
            .get(&peer)
            .map_or(0, |retry| retry.attempts)
            .saturating_add(1);
        let due = Instant::now() + self.retry_delay(attempts);
        self.retry.insert(
            peer,
            Retry {
                attempts,
                due,
                armed: attempts < self.retry_budget,
            },
        );
        attempts == 1
    }

    /// Reenvia aos peers cujo prazo venceu (P19). Peer que desconectou no meio do caminho perde o
    /// agendamento: a fila sai sozinha ao reconectar. O registro de quem disparou fica, desarmado.
    fn flush_due_retries(&mut self) {
        let now = Instant::now();
        let due: Vec<PeerId> = self
            .retry
            .iter()
            .filter(|(_, retry)| retry.armed && retry.due <= now)
            .map(|(peer, _)| *peer)
            .collect();
        for peer in due {
            if !self.connected.contains(&peer) {
                self.retry.remove(&peer);
                continue;
            }
            if let Some(retry) = self.retry.get_mut(&peer) {
                retry.armed = false;
            }
            self.send_direct(peer);
            // Nada saiu (a entrada sumiu da outbox, ou a conversa caiu): o registro velho faria a próxima
            // mensagem herdar a contagem, esperar mais e não avisar com `Queued`.
            if self
                .in_flight
                .get(&peer)
                .is_none_or(|flying| flying.is_empty())
            {
                self.retry.remove(&peer);
            }
        }
    }

    /// Guarda os endereços de escuta que `peer` informou ao conectar (P32), passados pelo mesmo filtro dos
    /// endereços de terceiros ([`dial_addrs`]: sem loopback, rede privada nem DNS; no máximo
    /// [`CONTACT_DIAL_ADDRS`] para contato). O que o peer diz é dado dele, não verdade: a conexão só vale se
    /// ele provar o Peer ID, mas o filtro impede que um contato aponte a minha rediscagem para a minha rede.
    /// Só entra peer com conversa, com handshake esperado (`expected`) ou conectado: o `identify` chega com a
    /// conexão, antes de a conversa existir, e quem disca com `dial` não tem `expected`. O conectado sem
    /// conversa sai de `peer_addrs` quando a conexão cai (`PeerDisconnected`), então um estranho só ocupa
    /// vaga enquanto estiver conectado, e com o mapa cheio quem tem conversa ou `expected` despeja um estranho
    /// ([`Self::evict_stranger_for`]). O mapa tem o teto de [`MAX_KNOWN_DEVICES`]. A informação nova vai na frente e a velha
    /// completa até o limite (auditoria I3: mudança de endereço não apaga o que ainda pode valer); uma lista
    /// que o filtro esvazia (um peer na LAN que só anuncia IP privado) não apaga a velha.
    fn learn_peer_addrs(&mut self, peer: PeerId, addrs: Vec<Multiaddr>) {
        if !(self.conversations.contains_key(&peer)
            || self.expected.contains_key(&peer)
            || self.connected.contains(&peer))
        {
            return;
        }
        let sibling = self.is_sibling(&peer);
        let mut merged = dial_addrs(&addrs, sibling);
        if merged.is_empty() {
            return;
        }
        if !has_room(&self.peer_addrs, &peer) && !self.evict_stranger_for(&peer) {
            return;
        }
        for old in self.peer_addrs.get(&peer).into_iter().flatten() {
            if !merged.contains(old) {
                merged.push(old.clone());
            }
        }
        merged.truncate(if sibling {
            SIBLING_DIAL_ADDRS
        } else {
            CONTACT_DIAL_ADDRS
        });
        self.peer_addrs.insert(peer, merged);
        // Sem comparar com o que já estava: a gravação anterior pode ter falhado, e o `identify` (um por
        // conexão) é a chance de refazê-la (auditoria #1).
        self.persist_peer_addrs(&peer);
    }

    /// Grava no banco os endereços de `peer` (P32 a), só se ele tem conversa: estranho conectado fica só em
    /// memória e nunca escreve no disco. Melhor esforço: se a gravação falhar a memória segue valendo e o
    /// próximo `identify` tenta de novo; o custo é perder o endereço num reinício, que é o comportamento de
    /// antes da persistência.
    fn persist_peer_addrs(&self, peer: &PeerId) {
        if !self.conversations.contains_key(peer) {
            return;
        }
        if let Some(addrs) = self.peer_addrs.get(peer) {
            let _ = self.outbox.set_peer_addrs(peer, addrs);
        }
    }

    /// Na abertura (P32 a): devolve à memória os endereços gravados de cada contato com conversa e agenda a
    /// rediscagem de quem tem fila. Sem isto, depois de um reinício nenhum evento (envio, queda, conversa
    /// pronta) chamaria `start_redial` para quem só tem fila no banco. O que está no disco é entrada não
    /// confiável (arquivo adulterado, regra de filtro mais nova): passa de novo pelo [`dial_addrs`] e pelos
    /// mesmos tetos de quando foi aprendido. Erro de leitura de um contato não derruba a abertura: ele só fica
    /// sem endereço, como antes.
    fn restore_peer_addrs(&mut self) {
        let peers: Vec<PeerId> = self.conversations.keys().copied().collect();
        for peer in peers {
            if let Ok(stored) = self.outbox.peer_addrs(&peer) {
                let sibling = self.is_sibling(&peer);
                let mut addrs = dial_addrs(&stored, sibling);
                addrs.truncate(if sibling {
                    SIBLING_DIAL_ADDRS
                } else {
                    CONTACT_DIAL_ADDRS
                });
                if !addrs.is_empty() && has_room(&self.peer_addrs, &peer) {
                    self.peer_addrs.insert(peer, addrs);
                }
            }
            self.start_redial(peer);
        }
    }

    /// Mapa `peer_addrs` cheio: abre vaga para `peer` despejando um peer **sem conversa e sem handshake
    /// esperado** (estranho conectado). Só quem tem conversa ou `expected` despeja; um estranho nunca tira a
    /// vaga de um contato, e o contrário impede que estranhos conectados bloqueiem o endereço de contato real
    /// (auditoria #1). `false` se `peer` não tem prioridade ou não há estranho a despejar.
    fn evict_stranger_for(&mut self, peer: &PeerId) -> bool {
        let known = |chat: &Self, p: &PeerId| {
            chat.conversations.contains_key(p) || chat.expected.contains_key(p)
        };
        if !known(self, peer) {
            return false;
        }
        let victim = self.peer_addrs.keys().find(|p| !known(self, p)).copied();
        victim.is_some_and(|victim| self.peer_addrs.remove(&victim).is_some())
    }

    /// Endereços a discar para rediscar `peer` (P32): os que ele informou; sem eles, os do convite que eu
    /// aceitei (`accept`), os de irmão em `household` e os de contato em `contact_devices`, com o mesmo filtro.
    fn redial_targets(&self, peer: &PeerId) -> Vec<Multiaddr> {
        if let Some(addrs) = self.peer_addrs.get(peer) {
            return addrs.clone();
        }
        if let Some(addrs) = self.invite_addrs.get(peer) {
            return addrs.clone();
        }
        if let Some(invite) = self.household.get(peer) {
            return dial_addrs(invite.addrs(), true);
        }
        self.contact_devices
            .get(peer)
            .map(|invite| dial_addrs(invite.addrs(), false))
            .unwrap_or_default()
    }

    /// Espera depois de `attempts` rediscagens seguidas, sem jitter: a base, dobrando a cada uma, até o teto.
    /// O deslocamento é limitado, então muitas falhas não estouram a conta.
    fn redial_backoff(&self, attempts: u32) -> Duration {
        let doublings = attempts.saturating_sub(1).min(16);
        self.redial_base
            .saturating_mul(1 << doublings)
            .min(self.redial_cap)
    }

    /// Prazo da rediscagem depois de `attempts` tentativas: a espera com jitter de ±20%, para dois peers
    /// que se rediscam não ficarem em sincronia.
    fn redial_due(&self, attempts: u32) -> Instant {
        let jitter = 0.8 + rand::random::<f64>() * 0.4;
        Instant::now() + self.redial_backoff(attempts).mul_f64(jitter)
    }

    /// Vale rediscar `peer`? `No`: sem conversa, já conectado, identidade desconfiada (`untrust`: auditoria I6)
    /// ou fila vazia. `Unknown`: a outbox não respondeu (auditoria I5): não é "fila vazia", o registro fica e
    /// reagenda em vez de morrer num erro transitório de SQLite.
    fn redial_want(&self, peer: &PeerId) -> Want {
        // Em recuperação (P9 c) não há conversa nem, talvez, fila: ainda assim é preciso reconectar para o
        // P16 refazê-la. Identidade desconfiada ou marca vencida: nada a rediscar.
        let recovering = match self.recovering.get(peer) {
            Some((identity, since)) if !self.conversations.contains_key(peer) => {
                if self.is_distrusted(identity)
                    || now_secs().saturating_sub(*since) >= RECOVERING_TTL.as_secs()
                {
                    return Want::No;
                }
                true
            }
            _ => false,
        };
        if (!self.conversations.contains_key(peer) && !recovering)
            || self.connected.contains(peer)
            || self
                .sole_identity(peer)
                .is_some_and(|identity| self.is_distrusted(&identity))
        {
            return Want::No;
        }
        if recovering {
            return Want::Yes;
        }
        match self.outbox.entries(peer) {
            Ok(entries) if entries.is_empty() => Want::No,
            Ok(_) => Want::Yes,
            Err(_) => Want::Unknown,
        }
    }

    /// Registra a tentativa de número `attempts` como falha ou como a próxima a esperar: `Waiting` com o prazo
    /// da espera, ou `Exhausted` (sem timer) quando o orçamento acaba.
    fn redial_next(&mut self, peer: PeerId, attempts: u32) {
        let state = if attempts >= self.redial_budget {
            RedialState::Exhausted
        } else {
            RedialState::Waiting
        };
        self.redial.insert(
            peer,
            Redial {
                attempts,
                due: self.redial_due(attempts),
                state,
            },
        );
    }

    /// Agenda a rediscagem de `peer` (P32) se vale rediscar. Sem registro, começa pela espera base. `Waiting` e
    /// `InFlight` seguem como estão. `Exhausted` (orçamento acabou, ~8 h offline) rearma **uma** tentativa
    /// quando chega mensagem nova (auditoria I2): a ação do usuário é o sinal de que vale tentar, e o registro só
    /// volta a esgotado depois dela, então não vira laço. Conectar, esquecer o peer ou a fila vazia (conferida no
    /// prazo) apagam o registro.
    fn start_redial(&mut self, peer: PeerId) {
        let state = self.redial.get(&peer).map(|redial| redial.state);
        if matches!(state, Some(RedialState::Waiting | RedialState::InFlight))
            || !matches!(self.redial_want(&peer), Want::Yes)
        {
            return;
        }
        let attempts = match state {
            Some(_) => self.redial_budget.saturating_sub(1).max(1),
            None => 1,
        };
        self.redial_next(peer, attempts);
    }

    /// O nó avisou que um dial a `peer` falhou (P32). Só conta se há uma rediscagem `InFlight` (a espera
    /// dobra): falha de outro dial (mDNS, `/dial` manual) ou atrasada não mexe no registro (auditoria S1).
    /// Devolve `true` se era da rediscagem de fundo, para o `chat` não emitir um `DialFailed` por tentativa
    /// (auditoria S2).
    fn redial_failed(&mut self, peer: PeerId) -> bool {
        let Some(redial) = self
            .redial
            .get(&peer)
            .filter(|redial| redial.state == RedialState::InFlight)
        else {
            return false;
        };
        let attempts = redial.attempts.saturating_add(1);
        if matches!(self.redial_want(&peer), Want::No) {
            self.redial.remove(&peer);
        } else {
            self.redial_next(peer, attempts);
        }
        true
    }

    /// Trata os peers cujo prazo venceu (P32). `Waiting`: se ainda vale, disca (e passa a `InFlight` com prazo
    /// de [`REDIAL_INFLIGHT`]); sem endereço nenhum, ou com a outbox sem responder, conta como tentativa e
    /// reagenda sem discar (rearma sozinho quando um endereço for aprendido: auditoria I2). `InFlight` que
    /// venceu sem `DialFailed` nem conexão (evento perdido, dial que o nó engoliu) conta como falha. O prazo
    /// vencido sempre muda o estado, então não mantém o laço girando.
    fn flush_due_redials(&mut self) {
        let now = Instant::now();
        let due: Vec<PeerId> = self
            .redial
            .iter()
            .filter(|(_, redial)| redial.state != RedialState::Exhausted && redial.due <= now)
            .map(|(peer, _)| *peer)
            .collect();
        for peer in due {
            let Some(&Redial {
                attempts, state, ..
            }) = self.redial.get(&peer)
            else {
                continue;
            };
            match self.redial_want(&peer) {
                Want::No => {
                    self.redial.remove(&peer);
                    continue;
                }
                Want::Unknown => {
                    self.redial_next(peer, attempts.saturating_add(1));
                    continue;
                }
                Want::Yes => {}
            }
            if state == RedialState::InFlight {
                self.redial_next(peer, attempts.saturating_add(1));
                continue;
            }
            let addrs = self.redial_targets(&peer);
            if addrs.is_empty() {
                self.redial_next(peer, attempts.saturating_add(1));
                continue;
            }
            self.redial.insert(
                peer,
                Redial {
                    attempts,
                    due: now + REDIAL_INFLIGHT,
                    state: RedialState::InFlight,
                },
            );
            #[cfg(test)]
            self.dial_log.push((peer, addrs.clone()));
            if self.node.dial_peer(peer, addrs).is_err() {
                self.redial_next(peer, attempts.saturating_add(1));
            }
        }
    }

    /// Esquece a rediscagem agendada de `peer`.
    fn forget_redial(&mut self, peer: &PeerId) {
        self.redial.remove(peer);
    }

    /// Esquece o novo depósito agendado de `peer` e a conta de recusas dele.
    fn forget_deposit_retry(&mut self, peer: &PeerId) {
        self.deposit_retry.remove(peer);
        self.deposit_spent.remove(peer);
    }

    /// Registra uma recusa de depósito para `peer` e agenda o novo depósito (P21), com a mesma espera do
    /// reenvio direto. São duas contas (auditoria I4): o backoff conta por **rodada** (várias recusas de uma
    /// janela cheia, enquanto o prazo já agendado não venceu, são um só passo), e o orçamento conta por
    /// **recusa** (`deposit_spent`: cada uma gastou uma geração MLS). Passado o `deposit_budget` o registro
    /// fica, sem armar o timer: a mensagem segue na outbox e sai ao reconectar, na varredura ou no `send`.
    /// Peer conectado não agenda nada: a recusa é de um depósito antigo e a fila sai direto.
    fn schedule_deposit_retry(&mut self, peer: PeerId) {
        if self.connected.contains(&peer) {
            return;
        }
        let spent = self
            .deposit_spent
            .get(&peer)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.deposit_spent.insert(peer, spent);
        let now = Instant::now();
        let (attempts, due) = match self.deposit_retry.get(&peer) {
            Some(waiting) if waiting.armed && waiting.due > now => (waiting.attempts, waiting.due),
            previous => {
                let attempts = previous.map_or(0, |retry| retry.attempts).saturating_add(1);
                (attempts, now + self.retry_delay(attempts))
            }
        };
        self.deposit_retry.insert(
            peer,
            Retry {
                attempts,
                due,
                armed: spent < self.deposit_budget,
            },
        );
    }

    /// Deposita de novo para os peers cujo prazo venceu (P21). Peer que conectou no meio do caminho perde o
    /// agendamento. O registro de quem disparou fica, desarmado, para contar a próxima recusa seguida.
    fn flush_due_deposits(&mut self) {
        let now = Instant::now();
        let due: Vec<PeerId> = self
            .deposit_retry
            .iter()
            .filter(|(_, retry)| retry.armed && retry.due <= now)
            .map(|(peer, _)| *peer)
            .collect();
        for peer in due {
            if self.connected.contains(&peer) {
                self.forget_deposit_retry(&peer);
                continue;
            }
            if let Some(retry) = self.deposit_retry.get_mut(&peer) {
                retry.armed = false;
            }
            self.deposit(peer);
            // Nada foi pedido (a entrada sumiu, ou não há mais caixa): o registro velho faria a próxima
            // mensagem herdar a contagem.
            if !self.depositing.values().any(|(p, _)| *p == peer) {
                self.forget_deposit_retry(&peer);
            }
        }
    }

    /// Período da varredura por idade: um quarto da retenção do backend, com piso de 1 s para uma retenção
    /// minúscula não manter o laço girando. O `deposit` re-deposita o que passou de metade da retenção; com a
    /// varredura a cada quarto, um depósito é re-depositado com no máximo 3/4 da retenção de idade, e sobra
    /// margem para atraso do relógio (auditoria I2).
    fn sweep_interval(&self) -> Option<Duration> {
        let retention = self.mailbox.as_ref()?.retention()?;
        Some((retention / 4).max(Duration::from_secs(1)))
    }

    /// Arma a varredura por idade se ainda não está armada (P21). Sem retenção declarada não há o que varrer.
    /// Uma retenção que estoura o `Instant` não arma nada, em vez de entrar em pânico.
    fn arm_sweep(&mut self) {
        if self.sweep_due.is_none()
            && let Some(interval) = self.sweep_interval()
        {
            self.sweep_due = Instant::now().checked_add(interval);
        }
    }

    /// No prazo, re-deposita o que ficou velho sem ack nos peers desconectados (P21): o `expire_deposits`
    /// só roda dentro de `deposit`, e sem nenhum evento ninguém o chamaria antes de o relay esquecer. Rearma
    /// enquanto houver mensagem na fila; o que acabou de ser re-depositado rearma no próprio `Deposited`.
    /// Cada peer tem `sweep_budget` rodadas (auditoria I1): uma rodada é uma passada em que algo foi de fato
    /// pedido à caixa (re-cifrou), então uma varredura que só encontra depósitos novos não gasta rodada.
    /// Esgotado, o peer sai da varredura até conectar, receber um ack ou ser esquecido.
    fn sweep_stale_deposits(&mut self) {
        if self.sweep_due.is_none_or(|due| due > Instant::now()) {
            return;
        }
        self.sweep_due = None;
        let peers: Vec<PeerId> = self.conversations.keys().copied().collect();
        let mut pending = false;
        for peer in peers {
            let rounds = self.sweep_rounds.get(&peer).copied().unwrap_or(0);
            if self.queued(&peer) == 0 || rounds >= self.sweep_budget {
                continue;
            }
            pending = true;
            if self.connected.contains(&peer) {
                continue;
            }
            let asked = |chat: &Self| chat.depositing.values().filter(|(p, _)| *p == peer).count();
            let before = asked(self);
            self.deposit(peer);
            if asked(self) > before {
                self.sweep_rounds.insert(peer, rounds.saturating_add(1));
            }
        }
        if pending {
            self.arm_sweep();
        }
    }

    /// Quanto falta para o próximo prazo que pede o relógio: o do re-anúncio dos irmãos (P24) ou o de um
    /// reenvio armado (P19). `None` = só a rede acorda o `next_event`.
    fn wake_in(&self) -> Option<Duration> {
        let now = Instant::now();
        let retries = self
            .retry
            .values()
            .filter(|retry| retry.armed)
            .map(|retry| retry.due.saturating_duration_since(now));
        let deposits = self
            .deposit_retry
            .values()
            .filter(|retry| retry.armed)
            .map(|retry| retry.due.saturating_duration_since(now));
        let redials = self
            .redial
            .values()
            .filter(|redial| redial.state != RedialState::Exhausted)
            .map(|redial| redial.due.saturating_duration_since(now));
        let sweep = self.sweep_due.map(|due| due.saturating_duration_since(now));
        self.sibling_wake_in()
            .into_iter()
            .chain(retries)
            .chain(deposits)
            .chain(redials)
            .chain(sweep)
            .min()
    }

    /// Anuncia aos irmãos pendentes cujo prazo já passou; os que ainda esperam voltam à pendência.
    fn flush_sibling_pending(&mut self) {
        if self.sibling_pending.is_empty() {
            return;
        }
        for target in std::mem::take(&mut self.sibling_pending) {
            if self.connected.contains(&target) && self.is_sibling(&target) {
                self.re_announce_to_sibling(target);
            }
        }
    }

    fn ready(&mut self, peer: PeerId, conversation: Conversation) -> Result<(), Error> {
        let new_identity = self.old_identity_of(&conversation);
        // Só um par v4 de outra identidade vira o trinco (no `crypto`); varrer todas as conversas a cada par
        // novo custaria O(N) por conexão de estranho (auditoria I1 da 3e-ii-b). Erro ao ler: varre.
        let may_turn_latch = match conversation.peer_certificates() {
            Ok(certificates) => certificates.iter().any(|c| {
                matches!(c.issuer, kin_identity::Issuer::Authority(_))
                    && c.identity() != self.device.identity()
            }),
            Err(_) => true,
        };
        self.device
            .remember(&peer.to_string(), &conversation.id())?;
        self.conversations.insert(peer, conversation);
        // P36: a referência da conversa nova é o `state_id` que o peer apresentou (se apresentou).
        if let Some(state_id) = self.pending_state_ids.remove(&peer) {
            let _ = self.outbox.set_peer_state(&peer, state_id);
        }
        self.teach_mailbox(peer);
        // O endereço da janela pré-conversa (só em memória até aqui) agora tem dono: vai para o disco.
        self.persist_peer_addrs(&peer);
        self.expected.remove(&peer);
        // Uso único (P18, ADR 0039): a conversa nasceu, o token do convite é gasto agora, e não no
        // `Hello`, para um handshake que falha no meio não queimar o link de quem convidei.
        self.hello_tokens.remove(&peer);
        self.held_key_packages.remove(&peer);
        self.hello_identity.remove(&peer);
        self.dialed.remove(&peer);
        self.forget_approval(&peer);
        self.lost_state.remove(&peer);
        // Voltou de um `Stale` (P9 c): avisa a troca antes do `ConversationReady`, como o P16 faz no outro lado.
        // Só se a conversa nova é da mesma identidade: outra identidade não é "refeita com a mesma".
        if let Some((identity, _)) = self.recovering.remove(&peer)
            && new_identity.as_ref() == Some(&identity)
        {
            self.queue
                .push_back(ChatEvent::ConversationReset { peer, identity });
        }
        // Sem `?`: a conversa já existe, e um erro aqui não pode abortar o resto do `ready` (anúncio,
        // flush); o token que sobrar vivo é o único custo (auditoria, médio 1).
        if let Some(token) = self.admitted.remove(&peer)
            && let Err(e) = self.outbox.take_invite_token(&token, now_secs())
        {
            self.drop_message(peer, e);
        }
        self.announce_if_ready(peer);
        // Conversa recém-pronta com o peer já conectado: o que estava na fila sai agora.
        self.flush(peer);
        // Aceitar um par v4 vira o trinco da identidade dele (no `crypto`); as conversas v3 que já existiam
        // com ela não são revalidadas por nenhum caminho de mensagem (P30 3e-ii-b, ADR 0032).
        if may_turn_latch {
            self.drop_revoked_peers();
        }
        Ok(())
    }

    /// Avisa a aplicação que a conversa com `peer` está pronta (nova ou recarregada do banco).
    fn announce_if_ready(&mut self, peer: PeerId) {
        let identity = self
            .conversations
            .get(&peer)
            .and_then(|c| c.peers().ok())
            .and_then(|peers| peers.into_iter().next());
        if let Some(identity) = identity {
            self.queue
                .push_back(ChatEvent::ConversationReady { peer, identity });
        }
    }

    fn on_mls(&mut self, peer: PeerId, wire: &[u8]) -> Result<(), Error> {
        let conversation = self
            .conversations
            .get_mut(&peer)
            .ok_or(Error::NoConversation(peer))?;
        match conversation.decrypt(wire) {
            Ok(Decrypted::Message { sender, plaintext }) => {
                let payload = Payload::decode(&plaintext)?;
                // O outro lado já tem a conversa: agora é seguro anunciar os nossos relays e devices.
                self.announce_relays(peer);
                self.announce_devices(peer);
                self.announce_revocations(peer);
                match payload {
                    Payload::Message(message) => {
                        // Nova tentativa do remetente = outros bytes cifrados, o MLS não vê
                        // repetição; quem identifica a mesma mensagem é o `MessageId`.
                        if !self
                            .outbox
                            .mark_seen(&peer, &message.id, SeenKind::Message)?
                        {
                            return Ok(());
                        }
                        // Histórico (P10a): sob a identidade de quem enviou (a minha, se for um irmão).
                        self.outbox.record_history(&sender, false, &message)?;
                        self.queue.push_back(ChatEvent::Received {
                            peer,
                            sender,
                            message,
                        });
                        Ok(())
                    }
                    Payload::RelayList(relays) => self.on_relay_list(peer, &relays),
                    Payload::Sent { to, message } => self.on_sent(peer, &sender, to, message),
                    Payload::DeviceList(invites) => self.on_device_list(peer, &sender, invites),
                    Payload::HistoryRequest { cursor } => {
                        self.on_history_request(peer, &sender, cursor)
                    }
                    Payload::HistoryChunk { items, next, done } => {
                        self.on_history_chunk(peer, &sender, items, next, done)
                    }
                    Payload::Revocation(revocation) => self.on_revocation(peer, &revocation),
                }
            }
            Ok(Decrypted::EpochChanged) => {
                self.replay_future(peer);
                Ok(())
            }
            Ok(Decrypted::ProposalQueued) => Ok(()),
            // Chegou antes do commit que a precede (P9): guarda até o commit chegar.
            Err(CryptoError::UnknownEpoch) => {
                self.hold_future(peer, wire);
                Ok(())
            }
            // Reentrega (at-least-once) ou velha demais: seguro ignorar.
            Err(CryptoError::Duplicate | CryptoError::TooOld) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Um tombstone de device chegou de `peer` (P30, 3c-i; ADR 0024). Ordem: limite de taxa do peer,
    /// admissão pela identidade da **cadeia assinada** do tombstone (não pelo remetente: ele pode ter sido
    /// repassado por um terceiro) e só então a gravação, que confere a assinatura.
    fn on_revocation(&mut self, peer: PeerId, revocation: &DeviceRevocation) -> Result<(), Error> {
        let now = Instant::now();
        let window = self.revocation_window;
        // A cota do peer só expira pela janela (reconectar não a devolve); as vencidas são podadas aqui.
        self.revocations_seen
            .retain(|_, (start, _)| now.duration_since(*start) < window);
        if now.duration_since(self.revocations_global.0) >= window {
            self.revocations_global = (now, 0);
        }
        let seen = self.revocations_seen.entry(peer).or_insert((now, 0));
        if seen.1 >= REVOCATIONS_PER_WINDOW
            || self.revocations_global.1 >= REVOCATIONS_GLOBAL_PER_WINDOW
        {
            return Err(Error::RevocationRateLimited);
        }
        seen.1 += 1;
        self.revocations_global.1 += 1;
        // Só a minha identidade ou uma que aceitei: "ter conversa" não filtra ninguém (um estranho que
        // conecta ganha uma), e `untrust` precisa cortar a escrita no disco (auditoria 3c-i, C1 e I3).
        if !self.is_trusted(revocation.identity()) {
            return Err(Error::UnauthorizedRevocation);
        }
        // A gravação confere a assinatura. Falha de armazenamento sobe como erro, não vira "inválido".
        match self.device.store_revocation(revocation) {
            // Novo: repasso a quem devo avisar, menos a quem me enviou. O repetido não repassa: é o que
            // corta o eco e o loop (cada device grava uma vez, então a cascata termina).
            Ok(true) => {
                // Derrubo antes de repassar: o revogado deixa de ter conversa e sai dos destinos.
                self.drop_revoked_peers();
                for target in self.revocation_targets(revocation.identity(), Some(peer)) {
                    self.send_revocation(target, revocation);
                }
                Ok(())
            }
            // Já estava no store, mas a conversa pode ter sobrado de uma derrubada que falhou no meio.
            Ok(false) => {
                self.drop_revoked_peers();
                Ok(())
            }
            Err(e) => {
                let error = Error::from(e);
                if Self::refunds_quota(&error) {
                    self.refund_revocation_quota(peer);
                }
                Err(error)
            }
        }
    }

    /// Derruba toda conversa com um device que o store marca como revogado (P30, 3d-ii-b2; ADR 0029) ou que é
    /// v3 de uma identidade já aceita em v4 (3e-ii-b, ADR 0032). Um erro de leitura **não** derruba (não sei)
    /// e vira `Dropped`; a reabertura (3d-ii-c) reconsulta, e lá o erro de leitura falha a abertura.
    fn drop_revoked_peers(&mut self) {
        let Scan { to_drop, failures } = self.scan_revoked();
        for (peer, error) in failures {
            self.drop_message(peer, error);
        }
        for (peer, identity, reason) in to_drop {
            if let Err(e) = self.drop_peer(peer, identity, reason) {
                self.drop_message(peer, e);
            }
        }
    }

    /// Consulta o store para cada conversa: os peers com certificado revogado ou v3 de identidade em v4 (com a
    /// identidade e o motivo) e os que não consegui consultar. Revogado vence. Só lê; quem chama decide.
    fn scan_revoked(&self) -> Scan {
        let mut to_drop = Vec::new();
        let mut failures = Vec::new();
        for (peer, conversation) in &self.conversations {
            let certificates = match conversation.peer_certificates() {
                Ok(certificates) => certificates,
                Err(e) => {
                    failures.push((*peer, Error::from(e)));
                    continue;
                }
            };
            let mut found = None;
            for certificate in certificates {
                let identity = certificate.identity().clone();
                match self.device.is_revoked(&certificate) {
                    Ok(true) => {
                        found = Some((identity, Reason::Revoked));
                        break;
                    }
                    Ok(false) => match self.device.is_downgraded(&certificate) {
                        Ok(true) => {
                            found.get_or_insert((identity, Reason::Downgraded));
                        }
                        Ok(false) => {}
                        Err(e) => failures.push((*peer, Error::from(e))),
                    },
                    Err(e) => failures.push((*peer, Error::from(e))),
                }
            }
            if let Some((identity, reason)) = found {
                to_drop.push((*peer, identity, reason));
            }
        }
        Scan { to_drop, failures }
    }

    /// Esquece o device `peer`, nesta ordem (tudo idempotente): a fila (`outbox`), a memória por Peer ID, a
    /// caixa postal (senão um `set_mailbox` reensinaria o que o `outbox` ainda guarda) e só **por último** a
    /// conversa e o grupo no banco. A conexão de rede não é fechada. Devolve o primeiro erro e **para**: em
    /// tempo de execução quem chama o transforma em `Dropped` (a próxima varredura repete); na reabertura ele
    /// falha a abertura (não há "próxima varredura", e abrir com o revogado vivo contradiz a falha fechada).
    fn drop_peer(
        &mut self,
        peer: PeerId,
        identity: IdentityId,
        reason: Reason,
    ) -> Result<(), Error> {
        // A fila primeiro, e se falhar paro com a conversa intacta: ela é o ponto de reparo (a próxima varredura
        // tenta de novo) e nada foi esquecido ainda. É outro arquivo SQLite, então não há atomicidade com o
        // banco do `crypto`: por isso a ordem, e não uma transação.
        let failed = self.outbox.forget_peer(&peer)?;
        self.expected.remove(&peer);
        self.future.remove(&peer);
        self.handshake_retries.retain(|(p, _), _| *p != peer);
        self.announced.remove(&peer);
        self.devices_announced.remove(&peer);
        self.lists_acted.remove(&peer);
        self.last_sibling_announce.remove(&peer);
        self.revocations_checked.remove(&peer);
        self.sibling_pending.remove(&peer);
        self.history_requested.remove(&peer);
        self.history_sessions.remove(&peer);
        self.history_requests.retain(|_, p| *p != peer);
        self.in_flight.remove(&peer);
        self.retry.remove(&peer);
        self.forget_deposit_retry(&peer);
        self.forget_redial(&peer);
        self.peer_addrs.remove(&peer);
        self.invite_addrs.remove(&peer);
        self.sweep_rounds.remove(&peer);
        self.depositing.retain(|_, (p, _)| *p != peer);
        self.household.remove(&peer);
        self.contact_devices.remove(&peer);
        self.pending
            .retain(|_, id| id.is_none_or(|id| !failed.contains(&id)));
        if let Some(mailbox) = self.mailbox.as_mut() {
            mailbox.forget_peer(peer);
        }
        for message in failed {
            self.queue.push_back(ChatEvent::SendFailed {
                peer,
                message: Some(message),
                reason: match reason {
                    Reason::Revoked => "o device foi revogado",
                    Reason::Downgraded => {
                        "a identidade do device já usa certificados v4 e o dele é v3"
                    }
                }
                .into(),
            });
        }
        // A conversa e o grupo por último: `drop_revoked_peers` só reencontra o peer por `conversations`, então
        // enquanto algo acima pudesse falhar ela tinha de continuar lá. Se o `delete` falhar, o label fica e a
        // reabertura recarrega o grupo (3d-ii-c); o evento final só sai com tudo apagado.
        if let Some(conversation) = self.conversations.remove(&peer) {
            conversation.delete()?;
        }
        self.device.forget_conversation(&peer.to_string())?;
        self.queue.push_back(match reason {
            Reason::Revoked => ChatEvent::PeerRevoked { peer, identity },
            Reason::Downgraded => ChatEvent::PeerDowngraded { peer, identity },
        });
        Ok(())
    }

    /// Esta falha de gravação **devolve** a cota do peer? Só a de armazenamento (disco cheio, banco
    /// ocupado): o remetente não teve culpa e o tombstone seria perdido. Assinatura inválida e recusa de
    /// admissão são culpa dele e continuam gastando (ADR 0024, item 4; ADR 0025).
    /// Devolve **uma** unidade de cota do peer e da janela global, até o teto de devoluções por janela.
    fn refund_revocation_quota(&mut self, peer: PeerId) {
        let now = Instant::now();
        if now.duration_since(self.revocation_refunds.0) >= self.revocation_window {
            self.revocation_refunds = (now, 0);
        }
        if self.revocation_refunds.1 >= REVOCATION_REFUNDS_PER_WINDOW {
            return;
        }
        self.revocation_refunds.1 += 1;
        if let Some(seen) = self.revocations_seen.get_mut(&peer) {
            seen.1 = seen.1.saturating_sub(1);
        }
        self.revocations_global.1 = self.revocations_global.1.saturating_sub(1);
    }

    fn refunds_quota(error: &Error) -> bool {
        matches!(error, Error::Crypto(kin_crypto::Error::Storage(_)))
    }

    /// Os peers a quem avisar de um tombstone da identidade `identity`: **conectados**, com conversa pronta
    /// de um único peer, menos `except` (quem me enviou), e só quem **aceita `identity`**, porque o receptor
    /// admite por `is_trusted(identity)` e não pelo remetente (auditoria 3c-ii, I1; ADR 0025):
    /// - irmão (identidade minha), sempre;
    /// - device da própria `identity` (aceita a si mesma);
    /// - contato que eu aceitei, **só se `identity` é a minha** (um contato não aceita uma terceira por
    ///   mim: mandar o tombstone dela revelaria em quem eu confio e gastaria a cota de quem o recusa).
    fn revocation_targets(&self, identity: &IdentityId, except: Option<PeerId>) -> Vec<PeerId> {
        self.connected
            .iter()
            .copied()
            .filter(|peer| {
                Some(*peer) != except
                    && self
                        .conversation_identity(peer)
                        .is_some_and(|who| self.accepts_identity(&who, identity))
            })
            .collect()
    }

    /// Um device da identidade `who` aceita o tombstone da identidade `identity`? A regra de destino do ADR 0025.
    fn accepts_identity(&self, who: &IdentityId, identity: &IdentityId) -> bool {
        let own = self.device.identity();
        who == own || who == identity || (identity == own && self.is_trusted(who))
    }

    /// O inverso de [`Self::accepts_identity`]: as identidades de tombstone que um device de `who` aceita.
    /// `None` = todas (irmão); senão `who` e, se eu aceito `who`, a minha.
    fn revocation_identities_for(&self, who: &IdentityId) -> Option<Vec<IdentityId>> {
        let own = self.device.identity();
        if who == own {
            return None;
        }
        let mut identities = vec![who.clone()];
        if self.is_trusted(who) {
            identities.push(own.clone());
        }
        Some(identities)
    }

    /// Os tombstones a reenviar a um peer da identidade `who`: até [`REVOCATIONS_RESEND_MAX`], **os da minha
    /// identidade primeiro** e só então os das demais (auditoria 3c-ii-b, I1: um contato não pode empurrar
    /// para fora a revogação do meu device roubado), cada grupo do mais recente ao mais antigo.
    fn revocations_to_resend(&self, who: &IdentityId) -> Result<Vec<DeviceRevocation>, Error> {
        let own = self.device.identity();
        // O filtro por identidade é no SQL: os mais recentes do banco podem ser de uma identidade que este
        // peer não aceita, e filtrar depois do limite esconderia o que ele deveria receber.
        let identities = self.revocation_identities_for(who);
        let own_eligible = identities.as_ref().is_none_or(|ids| ids.contains(own));
        let mut found = if own_eligible {
            self.device
                .revocations(Some(std::slice::from_ref(own)), REVOCATIONS_RESEND_MAX)?
        } else {
            Vec::new()
        };
        if found.len() < REVOCATIONS_RESEND_MAX {
            let rest: Option<Vec<IdentityId>> =
                identities.map(|ids| ids.into_iter().filter(|id| id != own).collect());
            let others = match rest.as_deref() {
                Some([]) => Vec::new(),
                other => self.device.revocations(other, REVOCATIONS_RESEND_MAX)?,
            };
            // Sem filtro (irmão) as da minha identidade voltam na consulta: saem aqui. Como `found` tem
            // todas as minhas (são menos que o limite), sobram vagas suficientes entre as demais.
            let room = REVOCATIONS_RESEND_MAX - found.len();
            found.extend(
                others
                    .into_iter()
                    .filter(|r| r.identity() != own)
                    .take(room),
            );
        }
        Ok(found)
    }

    /// Reenvia ao `peer` até [`REVOCATIONS_RESEND_MAX`] tombstones gravados, os mais recentes entre os que ele
    /// aceita (3c-ii-b, ADR 0026), no máximo uma vez por janela por peer: a cota do receptor não zera ao
    /// reconectar, e reenviar a cada conexão a esgotaria. Idempotente no receptor (não repassa o repetido).
    fn announce_revocations(&mut self, peer: PeerId) {
        if !self.connected.contains(&peer) {
            return;
        }
        let Some(who) = self.conversation_identity(&peer) else {
            return;
        };
        // Uma avaliação por conexão, mesmo vazia ou com erro (auditoria 3c-ii-b, I3): sem isso cada mensagem
        // MLS recebida faria uma consulta ao store (e, no erro, um `Dropped`). O que entrar no store depois
        // sai pelo repasse de `on_revocation`.
        if !self.revocations_checked.insert(peer) {
            return;
        }
        let now = Instant::now();
        let gap = self.revocation_resend_gap;
        self.revocations_sent
            .retain(|_, last| now.duration_since(*last) < gap);
        if self.revocations_sent.contains_key(&peer) {
            return;
        }
        let found = match self.revocations_to_resend(&who) {
            Ok(found) => found,
            Err(e) => return self.drop_message(peer, e),
        };
        if found.is_empty() {
            return;
        }
        self.revocations_sent.insert(peer, now);
        for revocation in &found {
            self.send_revocation(peer, revocation);
        }
    }

    /// A identidade do único device do outro lado da conversa com `peer` (conversa com vários membros: `None`;
    /// hoje todas são 1:1, e um grupo futuro ficaria sem tombstone, em silêncio).
    fn conversation_identity(&self, peer: &PeerId) -> Option<IdentityId> {
        match self.conversations.get(peer)?.peers().ok()?.as_slice() {
            [identity] => Some(identity.clone()),
            _ => None,
        }
    }

    /// Cifra o tombstone para `peer` e o transmite (como `announce_relays`: fora da outbox, sem ack, então
    /// uma entrega perdida não volta ao `Chat`; o reenvio ao conectar é a 3c-ii-b).
    fn send_revocation(&mut self, peer: PeerId, revocation: &DeviceRevocation) {
        let payload = Payload::Revocation(Box::new(revocation.clone())).encode();
        let Some(conversation) = self.conversations.get_mut(&peer) else {
            // `revocation_targets` só devolve peers com conversa: se isto rodar, o filtro mudou.
            debug_assert!(false, "destino do tombstone sem conversa");
            return;
        };
        match conversation.encrypt(&payload) {
            Ok(wire) => {
                self.transmit(peer, &Envelope::Mls(wire));
            }
            Err(e) => self.drop_message(peer, e.into()),
        }
    }

    /// Emite um tombstone **já assinado** (a CLI da fatia 5 o monta; o `Chat` nunca tem a mestra): grava
    /// aqui (confere a assinatura) e o envia a todos os destinos, **mesmo se já estava gravado** (um
    /// reenvio explícito). Só de identidade aceita (a minha, ou `trust`), como na recepção.
    pub fn broadcast_revocation(&mut self, revocation: &DeviceRevocation) -> Result<(), Error> {
        if !self.is_trusted(revocation.identity()) {
            return Err(Error::UnauthorizedRevocation);
        }
        self.device.store_revocation(revocation)?;
        // Revogar o meu próprio device roubado passa por aqui: derruba antes de escolher os destinos.
        self.drop_revoked_peers();
        for target in self.revocation_targets(revocation.identity(), None) {
            self.send_revocation(target, revocation);
        }
        Ok(())
    }

    /// O device deste certificado tem tombstone gravado? **Pré-condição:** o certificado já foi verificado.
    /// A consulta que derruba o par é a 3d; isto só lê o store (CLI e testes).
    pub fn is_revoked(&self, certificate: &DeviceCertificate) -> Result<bool, Error> {
        Ok(self.device.is_revoked(certificate)?)
    }

    /// Um device irmão diz que enviou `message` a `to`. Só vale se quem **assinou esta mensagem**
    /// (`sender`, o remetente autenticado pelo MLS) for da minha identidade. Não basta olhar a
    /// composição do grupo: num grupo que cresceu por commit, o "outro membro" pode ser meu irmão
    /// enquanto quem fala é um intruso. A reentrega é deduplicada pelo id.
    fn on_sent(
        &mut self,
        peer: PeerId,
        sender: &IdentityId,
        to: IdentityId,
        message: Message,
    ) -> Result<(), Error> {
        if sender != self.device.identity() {
            return Err(Error::UnauthorizedMirror);
        }
        // `send_to` nunca espelha para a minha própria identidade (o irmão já recebe a mensagem
        // comum): um espelho assim vem de um irmão defeituoso ou hostil.
        if to == *self.device.identity() {
            return Err(Error::Malformed);
        }
        if !self
            .outbox
            .mark_seen(&peer, &message.id, SeenKind::Mirror)?
        {
            return Ok(());
        }
        // Histórico (P10a): um irmão enviou a `to`; fica como enviada na conversa com `to`.
        self.outbox.record_history(&to, true, &message)?;
        self.queue
            .push_back(ChatEvent::SentElsewhere { peer, to, message });
        Ok(())
    }

    /// Pede o histórico ao irmão `peer` (P10a): só a irmão conectado, uma vez por conexão (zera ao
    /// desconectar). Começa do cursor salvo para ele (0 se nunca). O pedido sai pela conversa MLS que os
    /// dois já têm; chamado no mesmo ponto em que se anuncia a lista de devices, quando o outro lado já
    /// tem a conversa.
    fn request_history(&mut self, peer: PeerId) {
        if !self.connected.contains(&peer)
            || !self.is_sibling(&peer)
            || !self.history_requested.insert(peer)
        {
            return;
        }
        match self.outbox.history_cursor(&peer) {
            Ok(cursor) => {
                self.history_sessions.insert(peer, 0);
                self.send_history_request(peer, cursor);
            }
            Err(e) => {
                self.history_requested.remove(&peer);
                self.drop_message(peer, e);
            }
        }
    }

    /// Cifra e envia `HistoryRequest { cursor }` a `peer`; falha de cifra encerra a sessão.
    fn send_history_request(&mut self, peer: PeerId, cursor: u64) {
        let Some(conversation) = self.conversations.get_mut(&peer) else {
            self.history_sessions.remove(&peer);
            return;
        };
        match conversation.encrypt(&Payload::HistoryRequest { cursor }.encode()) {
            Ok(wire) => {
                let id = self.transmit(peer, &Envelope::Mls(wire));
                self.history_requests.insert(id, peer);
            }
            Err(e) => {
                self.history_sessions.remove(&peer);
                self.drop_message(peer, e.into());
            }
        }
    }

    /// A página que respondo a um pedido: todas as conversas, em ordem de `seq`, depois de `cursor`.
    fn serve_history(&self, cursor: u64) -> Result<Payload, Error> {
        let page = self
            .outbox
            .history_page(cursor, HISTORY_CHUNK_BUDGET, MAX_DECODED_HISTORY)?;
        Ok(Payload::HistoryChunk {
            items: page.items,
            next: page.next,
            done: page.done,
        })
    }

    /// Um irmão pede o meu histórico. Só vale se quem **assinou** o pedido (`sender`, autenticado pelo
    /// MLS) for da minha identidade, como no espelho: nunca pela composição do grupo.
    fn on_history_request(
        &mut self,
        peer: PeerId,
        sender: &IdentityId,
        cursor: u64,
    ) -> Result<(), Error> {
        if sender != self.device.identity() {
            return Err(Error::UnauthorizedHistory);
        }
        let page = self.serve_history(cursor)?;
        let conversation = self
            .conversations
            .get_mut(&peer)
            .ok_or(Error::NoConversation(peer))?;
        let wire = conversation.encrypt(&page.encode())?;
        self.transmit(peer, &Envelope::Mls(wire));
        Ok(())
    }

    /// Chegou uma página do histórico de um irmão. Recusada se não é de um device da minha identidade
    /// ou se eu não pedi. Uma página não final sem nenhum item, ou que passa do teto de linhas por
    /// sessão, encerra a sessão (um irmão comprometido não gira o laço sem fim nem enche o disco). As
    /// linhas entram por `record_history` (a primeira gravação vence, então repetir é inofensivo) e o
    /// cursor só é salvo **depois** delas, para uma queda no meio repetir em vez de perder linhas.
    fn on_history_chunk(
        &mut self,
        peer: PeerId,
        sender: &IdentityId,
        items: Vec<HistoryItem>,
        next: u64,
        done: bool,
    ) -> Result<(), Error> {
        if sender != self.device.identity() {
            return Err(Error::UnauthorizedHistory);
        }
        let Some(&received) = self.history_sessions.get(&peer) else {
            return Err(Error::UnauthorizedHistory);
        };
        if !done && items.is_empty() {
            self.history_sessions.remove(&peer);
            return Err(Error::HistoryLimit("página não final sem itens"));
        }
        if received.saturating_add(items.len()) > self.history_cap {
            self.history_sessions.remove(&peer);
            return Err(Error::HistoryLimit("teto de linhas por sessão"));
        }
        let count = items.len();
        let mut imported = 0;
        for item in &items {
            if self
                .outbox
                .record_history(&item.contact, item.outgoing, &item.message)?
            {
                imported += 1;
            }
        }
        self.outbox.set_history_cursor(&peer, next)?;
        self.queue.push_back(ChatEvent::HistorySynced {
            peer,
            imported,
            done,
        });
        if done {
            self.history_sessions.remove(&peer);
        } else {
            self.history_sessions.insert(peer, received + count);
            self.send_history_request(peer, next);
        }
        Ok(())
    }

    /// O contato anunciou onde recebe. Lista inválida é recusada inteira e a anterior fica.
    fn on_relay_list(&mut self, peer: PeerId, relays: &[String]) -> Result<(), Error> {
        let relays =
            validate_peer_relays(relays).map_err(|e| Error::InvalidRelayList(e.to_string()))?;
        self.outbox.set_peer_relays(&peer, &relays)?;
        self.queue.push_back(ChatEvent::PeerRelays {
            peer,
            relays: relays.clone(),
        });
        if let Some(mailbox) = self.mailbox.as_mut() {
            mailbox.learn_relays(peer, relays);
        }
        Ok(())
    }

    /// Guarda uma mensagem de época futura, sem repetir a mesma e dentro do teto [`FUTURE_LIMIT`].
    /// Estourar o teto significa que o commit que falta provavelmente não virá: o buffer é
    /// descartado, a aplicação é avisada com [`ChatEvent::Stale`] e a conversa é abandonada para o P16
    /// refazê-la (P9 c, ADR 0041; ver [`Chat::recover_from_stale`]).
    fn hold_future(&mut self, peer: PeerId, wire: &[u8]) {
        let held = self.future.entry(peer).or_default();
        if held.iter().any(|w| w == wire) {
            return;
        }
        if held.len() >= FUTURE_LIMIT {
            self.future.remove(&peer);
            let recovering = self.recover_from_stale(peer);
            self.queue.push_back(ChatEvent::Stale { peer, recovering });
            return;
        }
        held.push(wire.to_vec());
    }

    /// Esquece o estado por peer que dependia da conversa (época futura, anúncios, sessões de histórico).
    /// Usado quando a conversa é trocada (P16) ou abandonada (P9 c).
    fn forget_conversation_state(&mut self, peer: PeerId) {
        self.future.remove(&peer);
        self.announced.remove(&peer);
        self.devices_announced.remove(&peer);
        self.lists_acted.remove(&peer);
        self.last_sibling_announce.remove(&peer);
        self.revocations_checked.remove(&peer);
        self.sibling_pending.remove(&peer);
        self.history_requested.remove(&peer);
        self.history_sessions.remove(&peer);
        self.history_requests.retain(|_, p| *p != peer);
    }

    /// P9 (c, ADR 0041): a conversa com `peer` divergiu (`Stale`) e este lado é o atrasado. Abandona a
    /// conversa e o grupo, **mantém a outbox**, marca o peer como em recuperação e fecha a conexão: ao
    /// reconectar, o `Hello` diz "não tenho conversa" e o P16 a refaz. A **identidade da conversa antiga** vai
    /// para `expected`, então o `invite`/`join` a exigem (sem isso o próprio par escolheria a identidade). Só
    /// com o peer **conectado** (a recuperação passa pelo `Hello` da conexão direta; pela caixa postal, que
    /// entrega fora de ordem, o `Stale` só avisa), com conversa 1:1 de identidade conhecida e não
    /// desconfiada, e que não seja um irmão (ADR 0040: irmão não se recupera). No máximo uma vez por peer a
    /// cada [`STALE_RECOVERY_WINDOW`]; fora disso o `Stale` só avisa, como antes. Devolve `true` se
    /// abandonou a conversa (vira `recovering` no [`ChatEvent::Stale`]).
    fn recover_from_stale(&mut self, peer: PeerId) -> bool {
        let now = now_secs();
        self.stale_recovered
            .retain(|_, at| now.saturating_sub(*at) < STALE_RECOVERY_WINDOW.as_secs());
        let Some(identity) = self.old_identity(&peer) else {
            return false;
        };
        if self.stale_recovered.contains_key(&peer)
            || self.from_mailbox
            || !self.connected.contains(&peer)
            || self.is_sibling(&peer)
            || self.household.contains_key(&peer)
            || self.is_distrusted(&identity)
        {
            return false;
        }
        let Some(conversation) = self.conversations.remove(&peer) else {
            return false;
        };
        self.stale_recovered.insert(peer, now);
        self.recovering.insert(peer, (identity.clone(), now));
        self.expected.insert(peer, identity);
        self.forget_conversation_state(peer);
        // Se o apagamento falhar, o label fica e a reabertura recarrega o grupo (como na revogação); o
        // peer já está em recuperação e o P16 troca a conversa de qualquer jeito.
        let deleted = conversation
            .delete()
            .and_then(|()| self.device.forget_conversation(&peer.to_string()));
        if let Err(e) = deleted {
            self.drop_message(peer, e.into());
        }
        self.node.disconnect(peer);
        true
    }

    /// Solta a vaga de `expected` de um dial que morreu. Em recuperação (P9 c) a identidade fixada **fica**: é
    /// ela que o handshake seguinte exige (reauditoria do #281, achado 1).
    fn release_expected(&mut self, peer: &PeerId) {
        if !self.recovering.contains_key(peer) {
            self.expected.remove(peer);
        }
    }

    /// Vence as marcas de recuperação mais velhas que [`RECOVERING_TTL`]: o peer deixa de ser esperado.
    fn expire_recovering(&mut self) {
        let now = now_secs();
        let expired: Vec<PeerId> = self
            .recovering
            .iter()
            .filter(|(_, (_, since))| now.saturating_sub(*since) >= RECOVERING_TTL.as_secs())
            .map(|(peer, _)| *peer)
            .collect();
        for peer in expired {
            self.recovering.remove(&peer);
            if !self.conversations.contains_key(&peer) {
                self.expected.remove(&peer);
            }
        }
    }

    /// Reprocessa, na ordem de chegada, o que estava esperando um commit. O que ainda for de época
    /// futura volta para o buffer.
    fn replay_future(&mut self, peer: PeerId) {
        for wire in self.future.remove(&peer).unwrap_or_default() {
            if let Err(e) = self.on_mls(peer, &wire) {
                self.drop_message(peer, e);
            }
        }
    }

    /// Já conheço `peer`: tenho conversa, ele é device de uma identidade confiável, eu o disquei ou o
    /// esperava por convite. É a mesma lista que dispensa o token em [`Chat::admits`], sem a parte do
    /// certificado do `Hello` (que é dele para mim).
    fn knows(&self, peer: &PeerId) -> bool {
        self.conversations.contains_key(peer)
            || self.expected.contains_key(peer)
            || self.household.contains_key(peer)
            || self.contact_devices.contains_key(peer)
            || self.dialed.contains(peer)
    }

    fn send_hello(&mut self, peer: PeerId) {
        let token = self.hello_tokens.get(&peer).copied();
        // Identidade e chave Nostr só a quem já conheço: estranho recebe o `Hello` sem certificado
        // (auditoria, médio 2; decisão do dono).
        let certificate = self
            .knows(&peer)
            .then(|| self.device.certificate().to_bytes());
        // "Tenho conversa com você" (P16): é por este bit que quem tem a conversa percebe que eu a perdi.
        let presence = Presence {
            has_conversation: self.conversations.contains_key(&peer),
            state_id: self.device.state_id().ok(),
        };
        self.transmit(peer, &Envelope::Hello(token, certificate, Some(presence)));
    }

    /// Uma mensagem de handshake falhou (P25): com dois discos simultâneos uma das conexões cai levando
    /// o envio, e o peer segue conectado pela outra, então nada mais dispara o handshake. Reenvia os
    /// mesmos bytes enquanto o peer estiver conectado, no máximo [`MAX_HANDSHAKE_RETRIES`] vezes por
    /// tipo e por conexão (o teto impede laço). `Hello` e `KeyPackage` só enquanto não há conversa:
    /// com ela, o `Hello` é ignorado (`on_hello`) e o `KeyPackage` não serve mais. O `Welcome` é
    /// mandado por quem já tem a conversa (`ready`), então ali a conversa não impede o reenvio; o
    /// outro lado ignora a duplicata (`on_welcome`). Reenviar os mesmos bytes não gasta outro
    /// `KeyPackage` nem muda a época.
    fn resend_handshake(&mut self, peer: PeerId, envelope: Envelope) {
        // `Hello` e `KeyPackage` só voltam enquanto não há conversa; o `Welcome` só se ela existe (derrubado o
        // par, o grupo some e o convite não vale mais).
        let is_welcome = matches!(envelope, Envelope::Welcome(_));
        if !self.connected.contains(&peer) || is_welcome != self.conversations.contains_key(&peer) {
            return;
        }
        let retries = self
            .handshake_retries
            .entry((peer, std::mem::discriminant(&envelope)))
            .or_insert(0);
        if *retries >= MAX_HANDSHAKE_RETRIES {
            return;
        }
        *retries += 1;
        self.transmit(peer, &envelope);
    }

    fn transmit(&mut self, peer: PeerId, envelope: &Envelope) -> SendId {
        let id = self.node.send(peer, envelope.encode());
        self.pending.insert(id, None);
        if matches!(
            envelope,
            Envelope::Hello(..) | Envelope::KeyPackage(_) | Envelope::Welcome(_)
        ) {
            self.handshake.insert(id, envelope.clone());
        }
        id
    }

    fn drop_message(&mut self, peer: PeerId, error: Error) {
        self.queue.push_back(ChatEvent::Dropped {
            peer,
            reason: error.to_string(),
        });
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Os 4 primeiros bytes do token em hexadecimal: o que a CLI lista e o que `revoke_invite` aceita. Não
/// revela o token (sobram 96 bits).
fn invite_id(token: &[u8; 16]) -> String {
    token[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use kin_identity::{
        AuthorityIdentity, AuthorityKey, DeviceCertificate, DeviceRevocation, StandaloneIdentity,
    };

    use super::*;
    use crate::outbox::SEEN_LIMIT;
    use crate::{MemoryMailbox, MemoryRelay};

    fn chat(identity: &StandaloneIdentity, device: &DeviceKey) -> Chat {
        Chat::new(
            identity,
            device,
            NodeConfig {
                mdns: false,
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// Bia (um `Chat`) já conversa com Ana, que aqui é só a `Conversation` crua para fabricar
    /// commits e mensagens à vontade.
    fn bia_and_raw_ana() -> (Chat, Conversation, PeerId) {
        let (id_a, id_b) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        let (dev_a, dev_b) = (DeviceKey::generate(), DeviceKey::generate());
        let ana_device =
            CryptoDevice::new(&id_a, &dev_a.public_key(), &dev_a.nostr_public_key(), 1).unwrap();
        let mut bia = chat(&id_b, &dev_b);
        let mut ana = Conversation::create(&ana_device).unwrap();
        let invite = ana
            .invite(&bia.device.key_package().unwrap(), &Expected::default())
            .unwrap();
        let conversation =
            Conversation::join(&bia.device, &invite.welcome, &Expected::default()).unwrap();
        bia.conversations.insert(dev_a.peer_id(), conversation);
        (bia, ana, dev_a.peer_id())
    }

    fn wire(ana: &mut Conversation, text: &str) -> Vec<u8> {
        ana.encrypt(&Message::new(text.into(), None).encode())
            .unwrap()
    }

    /// Conecta `peer` como o `PeerConnected` real e, se a fila ficou segura à espera do `Hello` (P16), simula
    /// a chegada dele: os testes de envio descrevem uma reconexão normal, em que o `Hello` chega.
    fn connect_peer(chat: &mut Chat, peer: PeerId) {
        chat.handle(NodeEvent::PeerConnected(peer));
        if chat.hello_pending.remove(&peer) {
            chat.flush(peer);
        }
    }

    fn events(chat: &mut Chat) -> Vec<ChatEvent> {
        chat.queue.drain(..).collect()
    }

    fn received_texts(events: &[ChatEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Received { message, .. } => Some(message.text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// P9: mensagem que chega antes do commit que a precede é guardada, não perdida.
    #[tokio::test]
    async fn message_from_a_future_epoch_is_held_until_its_commit_arrives() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        let commit = ana.rotate_keys().unwrap();
        let (m1, m2) = (wire(&mut ana, "um"), wire(&mut ana, "dois"));

        bia.on_mls(ana_peer, &m2).unwrap();
        bia.on_mls(ana_peer, &m1).unwrap();
        assert!(
            events(&mut bia).is_empty(),
            "nada some nem aparece antes do commit"
        );

        bia.on_mls(ana_peer, &commit).unwrap();
        let after = events(&mut bia);
        assert_eq!(
            received_texts(&after),
            ["dois", "um"],
            "na ordem de chegada"
        );
        assert!(!after.iter().any(|e| matches!(e, ChatEvent::Dropped { .. })));
    }

    /// P9 + at-least-once: a mesma mensagem futura reentregue muitas vezes aparece uma vez só e não
    /// gasta vagas do teto (senão reentregas legítimas disparariam `Stale`).
    #[tokio::test]
    async fn a_future_message_delivered_repeatedly_is_shown_once_and_takes_one_slot() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        let commit = ana.rotate_keys().unwrap();
        let early = wire(&mut ana, "uma vez");

        for _ in 0..FUTURE_LIMIT + 5 {
            bia.on_mls(ana_peer, &early).unwrap();
        }
        assert!(events(&mut bia).is_empty(), "repetição não vira Stale");

        bia.on_mls(ana_peer, &commit).unwrap();
        assert_eq!(received_texts(&events(&mut bia)), ["uma vez"]);
    }

    /// P9: o buffer tem teto; ao estourar, avisa `Stale` e esquece o que guardou.
    #[tokio::test]
    async fn the_future_buffer_is_bounded_and_reports_stale() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        let commit = ana.rotate_keys().unwrap();

        for i in 0..FUTURE_LIMIT {
            bia.on_mls(ana_peer, &wire(&mut ana, &i.to_string()))
                .unwrap();
        }
        assert!(events(&mut bia).is_empty());

        bia.on_mls(ana_peer, &wire(&mut ana, "a gota d'água"))
            .unwrap();
        assert_eq!(
            events(&mut bia),
            [ChatEvent::Stale {
                peer: ana_peer,
                recovering: false
            }],
            "sem conexão o Stale só avisa"
        );

        // P9 (c): sem conexão direta o `Stale` só avisa, então o commit atrasado ainda é aplicado (o que o
        // buffer descartado tinha se perdeu, a conversa não).
        bia.on_mls(ana_peer, &commit).unwrap();
        assert!(received_texts(&events(&mut bia)).is_empty());
    }

    /// P9 (d), medição de 2026-11-22 (ADR 0041): um relay Nostr devolve o backlog por `created_at`
    /// decrescente, e o NIP-59 sorteia esse campo (até 2 dias no passado), então a ordem de chegada não tem
    /// relação com a de envio. A ordem aqui vem do `created_at` de gift wraps **reais** (`nostr::wrap`); o relay
    /// em si não entra (é o modelo do NIP-01, não um relay medido, ver P33). Para cada K (mensagens enviadas
    /// depois de um commit) entrega o commit e as K nessa ordem, com o peer desconectado (o `Stale` só avisa).
    ///
    /// Medido (30 execuções por K): K = 8 chega inteiro; K = 32 nunca dá `Stale`, mas perde em média 19 das 32,
    /// em silêncio, porque a tolerância de fora de ordem do MLS é 10 gerações e `on_mls` ignora `TooOld`; K >=
    /// 48 também dá `Stale` falso (teto de 32). O que a outbox reenvia depois (bytes novos, mesmo `MessageId`)
    /// chega sempre, sem repetição.
    ///
    /// **Ressalva (P37)**: K > 8 **não é alcançável** por um remetente `Chat`: `MAILBOX_WINDOW` limita a 8 os
    /// depósitos sem ack, de propósito abaixo das 10 gerações. Este teste caracteriza o limite do `Chat` diante
    /// de uma entrega que não respeite isso (outro cliente, irmão, remetente hostil), não o uso normal; ver
    /// `redeposited_copies_in_random_order_never_lose_a_message` para o cenário alcançável.
    #[tokio::test]
    async fn out_of_order_mailbox_backlog_loses_messages_silently_but_the_resend_recovers_them() {
        const RUNS: usize = 12;
        let sender = DeviceKey::generate();
        let recipient = DeviceKey::generate().nostr_public_key();
        let mut table = String::from("K  runs  stale  mean_held  max_held  mean_lost_first_pass\n");
        for k in [8, FUTURE_LIMIT, 64] {
            let (mut stale, mut held_sum, mut held_max, mut lost_sum) = (0, 0, 0, 0);
            for _ in 0..RUNS {
                let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
                let mut blobs = vec![Envelope::Mls(ana.rotate_keys().unwrap()).encode()];
                let messages: Vec<Message> =
                    (0..k).map(|i| Message::new(i.to_string(), None)).collect();
                for m in &messages {
                    blobs.push(Envelope::Mls(ana.encrypt(&m.encode()).unwrap()).encode());
                }
                let mut seen_ids: Vec<MessageId> = Vec::new();
                // Posição de cada blob na entrega: `created_at` do wrap, do mais novo para o mais velho.
                let mut order: Vec<(u64, usize)> = (0..blobs.len())
                    .map(|i| {
                        let event = crate::nostr::wrap(&sender, &recipient, &[0]).unwrap();
                        (event.created_at.as_secs(), i)
                    })
                    .collect();
                order.sort_by_key(|&(created_at, _)| std::cmp::Reverse(created_at));

                let (mut max, mut texts, mut warned) = (0, 0, false);
                for (_, i) in order {
                    bia.handle_mailbox(MailboxEvent::Retrieved {
                        from: ana_peer,
                        blob: blobs[i].clone(),
                    });
                    max = max.max(bia.future.get(&ana_peer).map_or(0, Vec::len));
                    let evs = events(&mut bia);
                    warned |= evs.iter().any(|e| matches!(e, ChatEvent::Stale { .. }));
                    texts += received_texts(&evs).len();
                    for e in &evs {
                        if let ChatEvent::Received { message, .. } = e {
                            seen_ids.push(message.id);
                        }
                    }
                    assert!(
                        !evs.iter().any(|e| matches!(e, ChatEvent::Dropped { .. })),
                        "a perda é silenciosa: nenhum Dropped"
                    );
                }
                let lost = k - texts;
                // Recuperação: a outbox do remetente reenvia, em ordem, o que não chegou (bytes novos,
                // mesmo `MessageId`); o destinatário deduplica pelo `MessageId`.
                let mut shown = texts;
                for m in &messages {
                    if seen_ids.contains(&m.id) {
                        continue;
                    }
                    let blob = Envelope::Mls(ana.encrypt(&m.encode()).unwrap()).encode();
                    bia.handle_mailbox(MailboxEvent::Retrieved {
                        from: ana_peer,
                        blob,
                    });
                    let evs = events(&mut bia);
                    shown += received_texts(&evs).len();
                    for e in &evs {
                        if let ChatEvent::Received { message, .. } = e {
                            seen_ids.push(message.id);
                        }
                    }
                }
                assert_eq!(seen_ids.len(), k, "K = {k}: o reenvio recupera tudo");
                assert_eq!(shown, k, "K = {k}: nenhuma mensagem aparece duas vezes");
                if k <= FUTURE_LIMIT {
                    assert!(!warned, "K = {k} cabe no teto e não pode dar Stale");
                }
                if k <= 8 {
                    assert_eq!(lost, 0, "K = {k} cabe nas 10 gerações de tolerância");
                }
                stale += usize::from(warned);
                held_sum += max;
                held_max = held_max.max(max);
                lost_sum += lost;
            }
            if k == FUTURE_LIMIT {
                assert!(
                    lost_sum > 0,
                    "fixa o limite de 10 gerações do MLS com K > 8 (inalcançável pelo `Chat`, P37)"
                );
            }
            table += &format!(
                "{k}  {RUNS}  {stale}  {:.1}  {held_max}  {:.1}\n",
                held_sum as f64 / RUNS as f64,
                lost_sum as f64 / RUNS as f64
            );
        }
        eprintln!("{table}");
    }

    /// P37, medição do cenário **alcançável** (a janela de depósito é `MAILBOX_WINDOW` = 8, então K > 8 não
    /// ocorre): 8 mensagens depositadas e re-depositadas pela varredura por idade, cada rodada recifrando as
    /// mesmas 8 com gerações novas, enquanto as cópias antigas ainda estão no relay (retenção de 7 dias,
    /// varredura a cada ~1,75 dia: até ~5 cópias de cada). O relay as serve em ordem aleatória. Uma mensagem se
    /// perde de verdade só se **todas** as cópias caem em `TooOld`.
    ///
    /// Medido no CI (200 execuções por caso, 1, 2, 3, 5 e 8 cópias = até 64 eventos): **0 mensagens perdidas**.
    /// Cada mensagem tem várias cópias e basta uma decifrar. O teste fixa isso (P37, ADR 0041).
    #[tokio::test]
    async fn redeposited_copies_in_random_order_never_lose_a_message() {
        const RUNS: usize = 30;
        let mut table = String::from("copias  eventos  runs  runs_com_perda  perdidas_media  perdidas_max\n");
        for copies in [2usize, 5, 8] {
            let (mut runs_lost, mut lost_sum, mut lost_max) = (0, 0, 0);
            for _ in 0..RUNS {
                let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
                let messages: Vec<Message> =
                    (0..MAILBOX_WINDOW).map(|i| Message::new(i.to_string(), None)).collect();
                let mut blobs = Vec::new();
                for _ in 0..copies {
                    for m in &messages {
                        blobs.push(Envelope::Mls(ana.encrypt(&m.encode()).unwrap()).encode());
                    }
                }
                rand::seq::SliceRandom::shuffle(blobs.as_mut_slice(), &mut rand::rng());
                let mut seen: Vec<MessageId> = Vec::new();
                for blob in blobs {
                    bia.handle_mailbox(MailboxEvent::Retrieved { from: ana_peer, blob });
                    for e in events(&mut bia) {
                        if let ChatEvent::Received { message, .. } = e {
                            seen.push(message.id);
                        }
                    }
                }
                let lost = messages.iter().filter(|m| !seen.contains(&m.id)).count();
                assert_eq!(lost, 0, "{copies} cópias: nenhuma mensagem pode se perder");
                runs_lost += usize::from(lost > 0);
                lost_sum += lost;
                lost_max = lost_max.max(lost);
            }
            table += &format!(
                "{copies}  {}  {RUNS}  {runs_lost}  {:.2}  {lost_max}\n",
                copies * MAILBOX_WINDOW,
                lost_sum as f64 / RUNS as f64
            );
        }
        eprintln!("{table}");
    }

    /// Estoura o teto do buffer com mensagens de época futura (o commit nunca chega): devolve o `Stale`.
    fn force_stale(bia: &mut Chat, ana: &mut Conversation, ana_peer: PeerId) {
        let _commit = ana.rotate_keys().unwrap();
        for i in 0..=FUTURE_LIMIT {
            bia.on_mls(ana_peer, &wire(ana, &i.to_string())).unwrap();
        }
    }

    /// P9 (c, ADR 0041): no `Stale` o lado atrasado abandona a conversa (o P16 a refaz na reconexão) e
    /// **mantém a outbox**: o que estava na fila sai pela conversa nova.
    #[tokio::test]
    async fn stale_drops_the_conversation_and_keeps_the_outbox() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        bia.send(ana_peer, "na fila", None).unwrap();
        connect_peer(&mut bia, ana_peer);
        let old = bia.old_identity(&ana_peer).unwrap();
        events(&mut bia);

        force_stale(&mut bia, &mut ana, ana_peer);

        assert!(events(&mut bia).contains(&ChatEvent::Stale {
            peer: ana_peer,
            recovering: true
        }));
        assert!(
            !bia.conversations.contains_key(&ana_peer),
            "a conversa divergida devia ter sido abandonada"
        );
        assert_eq!(bia.queued(&ana_peer), 1, "a outbox não é tocada");
        assert!(!bia.future.contains_key(&ana_peer));
        assert_eq!(
            bia.recovering.get(&ana_peer).map(|(identity, _)| identity),
            Some(&old),
            "o peer fica marcado, com a identidade da conversa antiga"
        );
        assert_eq!(
            bia.expected.get(&ana_peer),
            Some(&old),
            "e o handshake seguinte exige essa identidade (o par não escolhe a dele)"
        );
    }

    /// Médio 1 da reauditoria do #281: com o peer **conectado**, 33 mensagens de época futura que chegam pela
    /// **caixa postal** (entrega fora de ordem) só avisam: o commit atrasado ainda salvaria a conversa.
    #[tokio::test]
    async fn stale_from_the_mailbox_with_a_connected_peer_only_warns() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        let _commit = ana.rotate_keys().unwrap();

        for i in 0..=FUTURE_LIMIT {
            let blob = Envelope::Mls(wire(&mut ana, &i.to_string())).encode();
            bia.handle_mailbox(MailboxEvent::Retrieved {
                from: ana_peer,
                blob,
            });
        }

        assert!(events(&mut bia).contains(&ChatEvent::Stale {
            peer: ana_peer,
            recovering: false
        }));
        assert!(bia.conversations.contains_key(&ana_peer));
        assert!(bia.recovering.is_empty());
    }

    /// Médio 2: uma discagem que falha (a rediscagem de fundo) não apaga a identidade fixada em `expected`.
    #[tokio::test]
    async fn a_failed_dial_keeps_the_pinned_identity_of_a_recovering_peer() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        let old = bia.old_identity(&ana_peer).unwrap();
        force_stale(&mut bia, &mut ana, ana_peer);

        bia.handle(NodeEvent::DialFailed {
            peer: Some(ana_peer),
            reason: "sem rota".into(),
        });

        assert_eq!(bia.expected.get(&ana_peer), Some(&old));
        assert!(bia.recovering.contains_key(&ana_peer));
    }

    /// Reauditoria do #281, achado 1: soltar a vaga de `expected` depois de um dial que falhou (no
    /// `on_device_list`) não apaga a identidade fixada de um peer em recuperação.
    #[tokio::test]
    async fn releasing_expected_keeps_the_pin_of_a_recovering_peer() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        let old = bia.old_identity(&ana_peer).unwrap();
        force_stale(&mut bia, &mut ana, ana_peer);
        assert_eq!(bia.expected.get(&ana_peer), Some(&old), "pré-condição");

        bia.release_expected(&ana_peer);

        assert_eq!(bia.expected.get(&ana_peer), Some(&old));
    }

    /// Reauditoria do #281, achado 1 (guarda): fora de recuperação, soltar a vaga continua removendo.
    #[tokio::test]
    async fn releasing_expected_frees_the_slot_of_a_peer_not_recovering() {
        let (mut bia, _ana, _ana_peer) = bia_and_raw_ana();
        let stranger = PeerId::random();
        let identity = bia.device.identity().clone();
        bia.expected.insert(stranger, identity);

        bia.release_expected(&stranger);

        assert!(!bia.expected.contains_key(&stranger));
    }

    /// Médio 2 (outra metade): se a conversa nova é de outra identidade, o `ConversationReset` não sai.
    #[tokio::test]
    async fn no_reset_is_reported_when_the_identity_changed() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        force_stale(&mut bia, &mut ana, ana_peer);
        events(&mut bia);
        let (mut other, _, other_peer) = bia_and_raw_ana();
        let fresh = other.conversations.remove(&other_peer).unwrap();

        bia.ready(ana_peer, fresh).unwrap();

        assert!(
            !events(&mut bia)
                .iter()
                .any(|e| matches!(e, ChatEvent::ConversationReset { .. })),
            "a identidade é outra: não é 'refeita com a mesma'"
        );
    }

    /// Sem o peer conectado (a mensagem veio pela caixa postal, que entrega fora de ordem) o `Stale` só
    /// avisa: o commit atrasado ainda poderia salvar a conversa, e o P16 só roda pela conexão direta.
    #[tokio::test]
    async fn stale_without_a_direct_connection_only_warns() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();

        force_stale(&mut bia, &mut ana, ana_peer);

        assert!(events(&mut bia).contains(&ChatEvent::Stale {
            peer: ana_peer,
            recovering: false
        }));
        assert!(bia.conversations.contains_key(&ana_peer));
        assert!(bia.recovering.is_empty() && bia.expected.is_empty());
    }

    /// Irmão (device da mesma identidade) não se recupera (ADR 0040): só avisa.
    #[tokio::test]
    async fn stale_with_a_sibling_only_warns() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        let invite = bia.invite(Vec::new());
        bia.household.insert(ana_peer, invite);

        force_stale(&mut bia, &mut ana, ana_peer);

        assert!(events(&mut bia).contains(&ChatEvent::Stale {
            peer: ana_peer,
            recovering: false
        }));
        assert!(bia.conversations.contains_key(&ana_peer));
        assert!(bia.recovering.is_empty());
    }

    /// Contato desconfiado (`untrust`) não é recuperado: o abandono não pode religar a conversa com ele.
    #[tokio::test]
    async fn stale_with_a_distrusted_contact_only_warns() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        let identity = bia.old_identity(&ana_peer).unwrap();
        bia.untrust(&identity).unwrap();

        force_stale(&mut bia, &mut ana, ana_peer);

        assert!(bia.conversations.contains_key(&ana_peer));
        assert!(bia.recovering.is_empty());
    }

    /// Desconfiar do contato **depois** do abandono desfaz a recuperação: some a marca e a identidade esperada,
    /// e nada mais é rediscado.
    #[tokio::test]
    async fn untrusting_a_recovering_contact_cancels_the_recovery() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        let identity = bia.old_identity(&ana_peer).unwrap();
        force_stale(&mut bia, &mut ana, ana_peer);
        assert!(bia.recovering.contains_key(&ana_peer));

        bia.untrust(&identity).unwrap();

        assert!(bia.recovering.is_empty() && bia.expected.is_empty());
        bia.connected.remove(&ana_peer);
        assert!(matches!(bia.redial_want(&ana_peer), Want::No));
    }

    /// A marca vence: um peer que nunca volta não fica esperado (e conhecido pelo portão) para sempre.
    #[tokio::test]
    async fn the_recovering_mark_expires() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        let identity = bia.old_identity(&ana_peer).unwrap();
        bia.conversations.remove(&ana_peer);
        let since = now_secs() - RECOVERING_TTL.as_secs() - 1;
        bia.recovering.insert(ana_peer, (identity.clone(), since));
        bia.expected.insert(ana_peer, identity);
        assert!(
            matches!(bia.redial_want(&ana_peer), Want::No),
            "vencida, não se redisca"
        );

        bia.expire_recovering();

        assert!(bia.recovering.is_empty() && bia.expected.is_empty());
    }

    /// Com o portão fechado, quem tem a conversa abandonada por `Stale` ainda é conhecido: sem isso o
    /// `Hello` dele viraria `NotAdmitted` e a volta seria barrada. Um estranho continua barrado.
    #[tokio::test]
    async fn a_recovering_peer_still_passes_the_closed_gate() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        bia.set_require_invite(true);
        connect_peer(&mut bia, ana_peer);
        force_stale(&mut bia, &mut ana, ana_peer);
        assert!(bia.recovering.contains_key(&ana_peer), "pré-condição");
        let stranger = PeerId::random();

        assert!(
            bia.admits(&ana_peer).unwrap(),
            "o peer em recuperação passa"
        );
        assert!(bia.knows(&ana_peer), "e recebe o Hello com certificado");
        assert!(!bia.admits(&stranger).unwrap(), "um estranho segue barrado");
    }

    /// Sem conversa e sem fila ninguém rediscaria o peer; em recuperação, sim (senão a conversa só volta se
    /// alguém discar à mão). A queda que o `Stale` provoca já agenda a rediscagem.
    #[tokio::test]
    async fn the_disconnect_after_stale_schedules_a_redial_even_without_a_queue() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        force_stale(&mut bia, &mut ana, ana_peer);
        assert_eq!(bia.queued(&ana_peer), 0, "pré-condição: sem fila");
        assert!(
            matches!(bia.redial_want(&ana_peer), Want::No),
            "ainda conectado, nada a rediscar"
        );

        bia.handle(NodeEvent::PeerDisconnected(ana_peer));

        assert!(matches!(bia.redial_want(&ana_peer), Want::Yes));
        assert!(
            bia.redial.contains_key(&ana_peer),
            "a rediscagem foi agendada"
        );
    }

    /// Cair a conexão (o `disconnect` do `Stale`) não apaga os endereços de quem está em recuperação.
    #[tokio::test]
    async fn disconnecting_keeps_the_addresses_of_a_recovering_peer() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        let address: Multiaddr = "/ip4/203.0.113.7/tcp/4001".parse().unwrap();
        bia.peer_addrs.insert(ana_peer, vec![address.clone()]);
        force_stale(&mut bia, &mut ana, ana_peer);

        bia.handle(NodeEvent::PeerDisconnected(ana_peer));

        assert_eq!(bia.peer_addrs.get(&ana_peer), Some(&vec![address]));
    }

    /// Quando a conversa nova fica pronta, a marca e a identidade esperada saem. (O `ConversationReset` com a
    /// **mesma** identidade não tem teste unitário: o helper só monta conversas de identidades novas; fica para
    /// o teste de integração.)
    #[tokio::test]
    async fn the_new_conversation_clears_the_mark() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        force_stale(&mut bia, &mut ana, ana_peer);
        events(&mut bia);
        let (mut other, _, other_peer) = bia_and_raw_ana();
        let fresh = other.conversations.remove(&other_peer).unwrap();

        bia.ready(ana_peer, fresh).unwrap();

        assert!(bia.recovering.is_empty() && !bia.expected.contains_key(&ana_peer));
    }

    /// Depois do abandono, mensagem velha do par é erro de "sem conversa", não pânico nem buffer novo.
    #[tokio::test]
    async fn after_the_drop_a_late_message_is_a_no_conversation_error() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        force_stale(&mut bia, &mut ana, ana_peer);

        let late = wire(&mut ana, "atrasada");
        assert!(matches!(
            bia.on_mls(ana_peer, &late),
            Err(Error::NoConversation(p)) if p == ana_peer
        ));
        assert!(!bia.future.contains_key(&ana_peer));
    }

    /// Dentro da janela, um 2º `Stale` do mesmo peer só avisa: não apaga a conversa (contra laço de recuperação).
    #[tokio::test]
    async fn a_second_stale_inside_the_window_only_warns() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        bia.stale_recovered.insert(ana_peer, now_secs());

        force_stale(&mut bia, &mut ana, ana_peer);

        assert!(events(&mut bia).contains(&ChatEvent::Stale {
            peer: ana_peer,
            recovering: false
        }));
        assert!(
            bia.conversations.contains_key(&ana_peer),
            "dentro da janela a conversa fica"
        );
    }

    /// Passada a janela, o `Stale` volta a recuperar.
    #[tokio::test]
    async fn a_stale_after_the_window_recovers_again() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        bia.stale_recovered
            .insert(ana_peer, now_secs() - STALE_RECOVERY_WINDOW.as_secs() - 1);

        force_stale(&mut bia, &mut ana, ana_peer);

        assert!(events(&mut bia).contains(&ChatEvent::Stale {
            peer: ana_peer,
            recovering: true
        }));
        assert!(!bia.conversations.contains_key(&ana_peer));
    }

    /// A janela é por peer: um `Stale` recente de outro peer não segura este.
    #[tokio::test]
    async fn the_stale_window_is_per_peer() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        bia.stale_recovered.insert(PeerId::random(), now_secs());

        force_stale(&mut bia, &mut ana, ana_peer);

        assert!(!bia.conversations.contains_key(&ana_peer));
    }

    /// P9: lixo e mensagem de outra conversa continuam sendo erro, nunca vão para o buffer.
    #[tokio::test]
    async fn garbage_is_not_buffered() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        let commit = ana.rotate_keys().unwrap();

        assert!(bia.on_mls(ana_peer, b"lixo").is_err());
        let (_, mut stranger, _) = bia_and_raw_ana();
        assert!(
            bia.on_mls(ana_peer, &wire(&mut stranger, "intruso"))
                .is_err()
        );

        bia.on_mls(ana_peer, &commit).unwrap();
        assert!(received_texts(&events(&mut bia)).is_empty());
    }

    fn in_flight(chat: &Chat) -> usize {
        chat.pending.values().filter(|m| m.is_some()).count()
    }

    fn first_in_flight(chat: &Chat) -> SendId {
        *chat.pending.iter().find(|(_, m)| m.is_some()).unwrap().0
    }

    /// 3.2: mensagem para peer desconectado vai para a fila, não falha.
    #[tokio::test]
    async fn sending_to_a_disconnected_peer_queues_the_message() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        let id = bia.send(ana_peer, "oi", None).unwrap();

        assert_eq!(
            events(&mut bia),
            [ChatEvent::Queued {
                peer: ana_peer,
                message: id
            }]
        );
        assert_eq!(bia.queued(&ana_peer), 1);
        assert_eq!(in_flight(&bia), 0, "nada sai sem conexão");
    }

    /// 3.2: ao conectar, a fila começa a sair (uma por vez, ver `SEND_WINDOW`).
    #[tokio::test]
    async fn queued_messages_go_out_when_the_peer_connects() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        for text in ["um", "dois", "tres"] {
            bia.send(ana_peer, text, None).unwrap();
        }

        connect_peer(&mut bia, ana_peer);
        assert_eq!(in_flight(&bia), SEND_WINDOW);
        assert_eq!(bia.queued(&ana_peer), 3, "só sai da fila com o ack");
    }

    /// 3.2: no máximo `SEND_WINDOW` em voo; o ack de uma libera a próxima.
    #[tokio::test]
    async fn the_send_window_limits_messages_in_flight() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        for i in 0..20 {
            bia.send(ana_peer, i.to_string(), None).unwrap();
        }
        assert_eq!(in_flight(&bia), SEND_WINDOW);
        assert_eq!(bia.queued(&ana_peer), 20);

        let id = first_in_flight(&bia);
        events(&mut bia);
        bia.handle(NodeEvent::MessageDelivered { peer: ana_peer, id });
        assert!(
            events(&mut bia)
                .iter()
                .any(|e| matches!(e, ChatEvent::Delivered { .. }))
        );
        assert_eq!(bia.queued(&ana_peer), 19);
        assert_eq!(in_flight(&bia), SEND_WINDOW, "o ack liberou outra");
    }

    /// 3.2: a ordem da fila é respeitada: o ack de uma libera exatamente a próxima mais antiga.
    #[tokio::test]
    async fn the_queue_drains_oldest_first_one_at_a_time() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        let ids: Vec<_> = ["a", "b", "c"]
            .into_iter()
            .map(|t| bia.send(ana_peer, t, None).unwrap())
            .collect();

        for (i, expected) in ids.iter().enumerate() {
            let flying: Vec<_> = bia.in_flight[&ana_peer].iter().copied().collect();
            assert_eq!(flying, [*expected], "passo {i}");
            let id = first_in_flight(&bia);
            bia.handle(NodeEvent::MessageDelivered { peer: ana_peer, id });
        }
        assert_eq!(bia.queued(&ana_peer), 0);
    }

    /// 3.2: falha de envio não perde a mensagem: ela volta a esperar.
    #[tokio::test]
    async fn a_failed_send_keeps_the_message_queued() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        let message = bia.send(ana_peer, "tentando", None).unwrap();
        events(&mut bia);

        let id = first_in_flight(&bia);
        bia.handle(NodeEvent::SendFailed {
            peer: ana_peer,
            id,
            reason: "sem rota".into(),
        });
        assert_eq!(
            events(&mut bia),
            [ChatEvent::Queued {
                peer: ana_peer,
                message
            }]
        );
        assert_eq!(bia.queued(&ana_peer), 1);
        assert_eq!(in_flight(&bia), 0);
    }

    /// P19: faz o primeiro envio a `peer` falhar com a conexão de pé e devolve o id da mensagem.
    fn send_and_fail(bia: &mut Chat, peer: PeerId, text: &str) -> MessageId {
        let message = bia.send(peer, text, None).unwrap();
        events(bia);
        let id = first_in_flight(bia);
        bia.handle(NodeEvent::SendFailed {
            peer,
            id,
            reason: "timeout".into(),
        });
        events(bia);
        message
    }

    /// P19: vence o prazo do reenvio de `peer` sem esperar.
    fn make_due(bia: &mut Chat, peer: PeerId) {
        bia.retry.get_mut(&peer).expect("sem reenvio agendado").due = Instant::now();
    }

    /// P19: falha de mensagem de chat com o peer conectado agenda o reenvio; antes do prazo nada sai, no
    /// prazo sai (a mensagem continua na fila até o ack).
    #[tokio::test]
    async fn a_failed_chat_send_is_retried_only_after_the_wait() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.retry_base = Duration::from_secs(3600);
        connect_peer(&mut bia, ana_peer);
        send_and_fail(&mut bia, ana_peer, "tentando");

        assert_eq!(bia.retry[&ana_peer].attempts, 1);
        bia.flush_due_retries();
        assert_eq!(in_flight(&bia), 0, "o prazo não venceu: nada sai");

        make_due(&mut bia, ana_peer);
        bia.flush_due_retries();
        assert_eq!(in_flight(&bia), 1, "venceu: reenvia");
        assert_eq!(bia.queued(&ana_peer), 1, "segue na fila até o ack");
    }

    /// P19: falha de novo depois do reenvio dobra a espera (2 falhas seguidas = o dobro da base).
    #[tokio::test]
    async fn a_second_failure_doubles_the_wait() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.retry_base = Duration::from_secs(3600);
        bia.retry_cap = Duration::from_secs(86_400); // acima do dobro da base, senão o teto corta
        connect_peer(&mut bia, ana_peer);
        send_and_fail(&mut bia, ana_peer, "tentando");
        make_due(&mut bia, ana_peer);
        bia.flush_due_retries();

        let id = first_in_flight(&bia);
        bia.handle(NodeEvent::SendFailed {
            peer: ana_peer,
            id,
            reason: "timeout".into(),
        });

        assert_eq!(bia.retry[&ana_peer].attempts, 2);
        assert!(
            bia.retry[&ana_peer].due > Instant::now() + Duration::from_secs(7000),
            "2 falhas: ~2 x 3600 s"
        );
    }

    /// P19: a espera é base, 2x, 4x... e para no teto; muitas falhas não estouram a conta.
    #[tokio::test]
    async fn the_wait_doubles_up_to_the_cap() {
        let (bia, _ana, _peer) = bia_and_raw_ana();
        let secs = |attempts| bia.retry_delay(attempts).as_secs();
        let waits: Vec<u64> = (1..=7).map(secs).collect();
        assert_eq!(waits, [2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(secs(1000), 60, "sem estouro de aritmética");
    }

    /// P19: o ack de uma mensagem zera o reenvio: a próxima falha volta à espera da base.
    #[tokio::test]
    async fn an_ack_resets_the_backoff() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        send_and_fail(&mut bia, ana_peer, "tentando");
        make_due(&mut bia, ana_peer);
        bia.flush_due_retries();

        let id = first_in_flight(&bia);
        bia.handle(NodeEvent::MessageDelivered { peer: ana_peer, id });
        assert!(bia.retry.is_empty(), "o ack zera");

        send_and_fail(&mut bia, ana_peer, "de novo");
        assert_eq!(bia.retry[&ana_peer].attempts, 1, "recomeça da base");
    }

    /// P19: desconectar apaga o reenvio (ao reconectar, o `PeerConnected` já envia a fila).
    #[tokio::test]
    async fn disconnecting_clears_the_retry() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        send_and_fail(&mut bia, ana_peer, "tentando");
        assert!(bia.retry.contains_key(&ana_peer));

        bia.handle(NodeEvent::PeerDisconnected(ana_peer));
        assert!(bia.retry.is_empty());
    }

    /// P19: prazo vencido com o peer já desconectado não envia direto e não deixa o agendamento para trás.
    #[tokio::test]
    async fn a_due_retry_does_not_send_to_a_disconnected_peer() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        send_and_fail(&mut bia, ana_peer, "tentando");
        make_due(&mut bia, ana_peer);
        bia.connected.remove(&ana_peer);

        bia.flush_due_retries();
        assert_eq!(in_flight(&bia), 0);
        assert!(bia.retry.is_empty());
    }

    /// P19: a falha de um peer agenda só o reenvio dele.
    #[tokio::test]
    async fn a_failure_to_one_peer_schedules_only_that_peer() {
        let Household {
            mut bia,
            sibling: (sibling, _),
            ana_device: (ana_peer, _),
            ..
        } = household();
        connect_peer(&mut bia, sibling);
        connect_peer(&mut bia, ana_peer);
        events(&mut bia);
        bia.send(ana_peer, "para a ana", None).unwrap();
        let to_sibling = bia.send(sibling, "para o irmão", None).unwrap();
        let failing = *bia
            .pending
            .iter()
            .find_map(|(id, message)| (*message == Some(to_sibling)).then_some(id))
            .unwrap();
        bia.handle(NodeEvent::SendFailed {
            peer: sibling,
            id: failing,
            reason: "timeout".into(),
        });

        assert_eq!(bia.retry.len(), 1);
        assert!(bia.retry.contains_key(&sibling));
    }

    /// P19: o aviso `Queued` sai na primeira falha da mensagem e não se repete a cada reenvio que falha (a CLI
    /// imprimiria uma linha por minuto).
    #[tokio::test]
    async fn queued_is_announced_on_the_first_failure_only() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        let message = bia.send(ana_peer, "tentando", None).unwrap();
        events(&mut bia);
        let fail = |bia: &mut Chat| {
            let id = first_in_flight(bia);
            bia.handle(NodeEvent::SendFailed {
                peer: ana_peer,
                id,
                reason: "timeout".into(),
            });
            events(bia)
        };

        assert_eq!(
            fail(&mut bia),
            [ChatEvent::Queued {
                peer: ana_peer,
                message
            }]
        );
        make_due(&mut bia, ana_peer);
        bia.flush_due_retries();
        assert_eq!(fail(&mut bia), [], "a segunda falha não avisa de novo");
    }

    /// P19, auditoria I1: depois de `retry_budget` falhas seguidas o timer para de armar (a conta de gerações
    /// de chave é finita), a mensagem segue na outbox e o relógio dorme; reconectar zera e volta a enviar.
    #[tokio::test]
    async fn retries_stop_after_the_budget_and_resume_on_reconnect() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.retry_budget = 3;
        connect_peer(&mut bia, ana_peer);
        send_and_fail(&mut bia, ana_peer, "tentando");
        for _ in 0..2 {
            make_due(&mut bia, ana_peer);
            bia.flush_due_retries();
            let id = first_in_flight(&bia);
            bia.handle(NodeEvent::SendFailed {
                peer: ana_peer,
                id,
                reason: "timeout".into(),
            });
        }

        assert_eq!(bia.retry[&ana_peer].attempts, 3);
        assert!(!bia.retry[&ana_peer].armed, "orçamento esgotado: não arma");
        assert_eq!(bia.wake_in(), None, "o relógio dorme");
        assert_eq!(bia.queued(&ana_peer), 1, "a mensagem segue na outbox");

        bia.handle(NodeEvent::PeerDisconnected(ana_peer));
        connect_peer(&mut bia, ana_peer);
        assert!(bia.retry.is_empty(), "reconectar zera a conta");
        assert_eq!(in_flight(&bia), 1, "e a fila sai de novo");
    }

    /// P19, auditoria I2: depois que o reenvio dispara o relógio volta a dormir. Sem isso o prazo vencido
    /// faria `next_event` girar a 100% de CPU.
    #[tokio::test]
    async fn the_clock_sleeps_again_after_the_retry_fires() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        send_and_fail(&mut bia, ana_peer, "tentando");
        make_due(&mut bia, ana_peer);

        bia.flush_due_retries();
        assert_eq!(in_flight(&bia), 1, "reenviou");
        assert_eq!(bia.wake_in(), None);
    }

    /// P19, auditoria S1: derrubar o peer (revogado) apaga o reenvio dele, como os demais mapas por peer.
    #[tokio::test]
    async fn dropping_a_peer_clears_its_retry() {
        let mut r = revoked_ana();
        r.bia.retry.insert(
            r.ana_peer,
            Retry {
                attempts: 1,
                due: Instant::now(),
                armed: true,
            },
        );

        r.bia.on_mls(r.sibling, &r.wire).unwrap();
        assert!(!r.bia.is_ready(&r.ana_peer), "a conversa caiu");
        assert!(!r.bia.retry.contains_key(&r.ana_peer));
    }

    /// P19, auditoria S2: se o prazo vence e não há o que enviar (a entrada sumiu da outbox), o registro some;
    /// senão a próxima mensagem herdaria a contagem velha, esperaria mais e não avisaria com `Queued`.
    #[tokio::test]
    async fn a_retry_that_finds_nothing_to_send_is_forgotten() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        let first = send_and_fail(&mut bia, ana_peer, "some");
        bia.outbox.remove(&ana_peer, &first).unwrap();
        make_due(&mut bia, ana_peer);

        bia.flush_due_retries();
        assert!(bia.retry.is_empty());

        let next = bia.send(ana_peer, "outra", None).unwrap();
        events(&mut bia);
        let id = first_in_flight(&bia);
        bia.handle(NodeEvent::SendFailed {
            peer: ana_peer,
            id,
            reason: "timeout".into(),
        });
        assert_eq!(bia.retry[&ana_peer].attempts, 1, "recomeça da base");
        assert_eq!(
            events(&mut bia),
            [ChatEvent::Queued {
                peer: ana_peer,
                message: next
            }]
        );
    }

    /// P19: falha de mensagem de controle (o `Hello`) não agenda reenvio da outbox: ela tem o tratamento
    /// próprio do P25.
    #[tokio::test]
    async fn a_failed_control_message_does_not_schedule_a_retry() {
        let Household { mut bia, .. } = household();
        let peer = PeerId::random();
        connect_peer(&mut bia, peer);
        let hello = only_pending(&bia);

        fail_send(&mut bia, peer, hello);
        assert!(bia.retry.is_empty());
    }

    /// P19: falha que chega depois de o peer desconectar não agenda nada (a fila sai ao reconectar).
    #[tokio::test]
    async fn a_failure_after_the_peer_disconnected_schedules_nothing() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        bia.send(ana_peer, "tentando", None).unwrap();
        events(&mut bia);
        let id = first_in_flight(&bia);
        bia.handle(NodeEvent::PeerDisconnected(ana_peer));

        bia.handle(NodeEvent::SendFailed {
            peer: ana_peer,
            id,
            reason: "timeout".into(),
        });
        assert!(bia.retry.is_empty());
    }

    /// P19: o relógio do `next_event` acorda no prazo do reenvio, não só no do re-anúncio dos irmãos.
    #[tokio::test]
    async fn the_clock_wakes_for_the_retry_deadline() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.retry_base = Duration::from_secs(3600);
        bia.retry_cap = Duration::from_secs(86_400);
        connect_peer(&mut bia, ana_peer);
        assert_eq!(bia.wake_in(), None, "sem nada agendado, espera só a rede");

        send_and_fail(&mut bia, ana_peer, "tentando");
        let wake = bia.wake_in().expect("com reenvio agendado, acorda");
        assert!(wake <= Duration::from_secs(3600) && wake > Duration::from_secs(3000));
    }

    /// 3.2: a mesma mensagem re-cifrada (nova tentativa) chega com bytes diferentes e aparece uma vez.
    #[tokio::test]
    async fn the_same_message_encrypted_twice_is_shown_once() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        let message = Message::new("repetida".into(), None);
        let (first, second) = (
            ana.encrypt(&message.encode()).unwrap(),
            ana.encrypt(&message.encode()).unwrap(),
        );
        assert_ne!(first, second, "o MLS não reconheceria como repetição");

        bia.on_mls(ana_peer, &first).unwrap();
        bia.on_mls(ana_peer, &second).unwrap();
        assert_eq!(received_texts(&events(&mut bia)), ["repetida"]);
    }

    /// 3.2: a memória de ids vistos tem teto por peer.
    #[tokio::test]
    async fn seen_ids_are_bounded_per_peer() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        for i in 0..SEEN_LIMIT + 5 {
            bia.on_mls(ana_peer, &wire(&mut ana, &i.to_string()))
                .unwrap();
        }
        assert_eq!(
            bia.outbox.seen_count(&ana_peer, SeenKind::Message).unwrap(),
            SEEN_LIMIT
        );
    }

    /// P24: espelhos não gastam a memória de mensagens comuns do mesmo peer. Antes, `SEEN_LIMIT + 5`
    /// espelhos expulsavam o id de `M`, e `M` reentregue aparecia de novo.
    #[tokio::test]
    async fn many_mirrors_do_not_make_an_old_message_reappear() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut sibling_raw),
            ..
        } = household();
        let old = Message::new("antiga".into(), None);
        let old_wire = sibling_raw.encrypt(&old.encode()).unwrap();
        bia.on_mls(sibling, &old_wire).unwrap();
        for i in 0..SEEN_LIMIT + 5 {
            let mirror = Payload::Sent {
                to: ana.clone(),
                message: Message::new(i.to_string(), None),
            };
            let wire = sibling_raw.encrypt(&mirror.encode()).unwrap();
            bia.on_mls(sibling, &wire).unwrap();
        }
        events(&mut bia);

        let again = sibling_raw.encrypt(&old.encode()).unwrap();
        bia.on_mls(sibling, &again).unwrap();

        assert!(
            received_texts(&events(&mut bia)).is_empty(),
            "a mensagem antiga reapareceu"
        );
        assert_eq!(
            bia.outbox.seen_count(&sibling, SeenKind::Mirror).unwrap(),
            SEEN_LIMIT,
            "o teto vale para os espelhos também"
        );
    }

    /// P24: um espelho e uma mensagem com o MESMO `MessageId` do mesmo peer são coisas diferentes:
    /// os dois aparecem (antes, o segundo era descartado como repetição).
    #[tokio::test]
    async fn a_mirror_and_a_message_sharing_an_id_are_both_shown() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut sibling_raw),
            ..
        } = household();
        let message = Message::new("mesmo id".into(), None);
        let plain = sibling_raw.encrypt(&message.encode()).unwrap();
        let mirror = Payload::Sent {
            to: ana.clone(),
            message: message.clone(),
        };
        let mirrored = sibling_raw.encrypt(&mirror.encode()).unwrap();

        bia.on_mls(sibling, &plain).unwrap();
        bia.on_mls(sibling, &mirrored).unwrap();

        let all = events(&mut bia);
        assert_eq!(received_texts(&all), ["mesmo id"]);
        assert!(
            all.iter().any(
                |e| matches!(e, ChatEvent::SentElsewhere { message: m, .. } if m.id == message.id)
            ),
            "o espelho também aparece: {all:?}"
        );
    }

    /// Espera até 2 s por um evento que o filtro aceita (o mailbox entrega por `next_event`).
    async fn until<T>(chat: &mut Chat, mut pick: impl FnMut(ChatEvent) -> Option<T>) -> Option<T> {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(found) = pick(chat.next_event().await) {
                    return found;
                }
            }
        })
        .await
        .ok()
    }

    fn mls_of(blob: &[u8]) -> Vec<u8> {
        match Envelope::decode(blob).unwrap() {
            Envelope::Mls(wire) => wire,
            other => panic!("esperava Mls, veio {other:?}"),
        }
    }

    fn text_of(ana: &mut Conversation, blob: &[u8]) -> String {
        match ana.decrypt(&mls_of(blob)).unwrap() {
            Decrypted::Message { plaintext, .. } => Message::decode(&plaintext).unwrap().text,
            other => panic!("esperava mensagem, veio {other:?}"),
        }
    }

    /// 3.1: com o peer offline e uma caixa postal ligada, a mensagem é cifrada e depositada.
    #[tokio::test]
    async fn an_offline_send_with_a_mailbox_deposits_the_encrypted_message() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));

        let id = bia.send(ana_peer, "oi", None).unwrap();
        let deposited = until(&mut bia, |e| match e {
            ChatEvent::Deposited { message, .. } => Some(message),
            _ => None,
        })
        .await;
        assert_eq!(deposited, Some(id));

        let deposits = relay.deposits();
        assert_eq!(deposits.len(), 1);
        assert_eq!(
            (deposits[0].from, deposits[0].to),
            (bia.peer_id(), ana_peer)
        );
        assert_eq!(text_of(&mut ana, &deposits[0].blob), "oi");
        assert_eq!(bia.queued(&ana_peer), 1, "depositar não é entregar");
    }

    /// 3.1: com o peer conectado vale o caminho direto; a caixa postal não é usada.
    #[tokio::test]
    async fn a_connected_peer_gets_the_direct_path_not_the_mailbox() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));
        connect_peer(&mut bia, ana_peer);

        bia.send(ana_peer, "direto", None).unwrap();
        assert!(relay.deposits().is_empty());
        assert_eq!(in_flight(&bia), 1);
    }

    /// 3.1: no máximo `MAILBOX_WINDOW` mensagens depositadas e ainda não confirmadas por peer.
    #[tokio::test]
    async fn deposits_stop_at_the_mailbox_window() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));

        for i in 0..MAILBOX_WINDOW + 2 {
            bia.send(ana_peer, i.to_string(), None).unwrap();
        }
        assert_eq!(relay.deposits().len(), MAILBOX_WINDOW);
        assert_eq!(bia.queued(&ana_peer), MAILBOX_WINDOW + 2);
    }

    /// 3.1: o que vem pela caixa postal é mostrado uma vez, mesmo se a mesma mensagem chegar direto.
    #[tokio::test]
    async fn a_retrieved_message_is_shown_once_even_if_it_also_arrives_directly() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        let message = Message::new("duas vias".into(), None);
        let (via_mailbox, direct) = (
            ana.encrypt(&message.encode()).unwrap(),
            ana.encrypt(&message.encode()).unwrap(),
        );
        MemoryMailbox::new(&relay, ana_peer)
            .deposit(bia.peer_id(), Envelope::Mls(via_mailbox).encode());
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));

        let got = until(&mut bia, |e| match e {
            ChatEvent::Received { message, .. } => Some(message.text),
            _ => None,
        })
        .await;
        assert_eq!(got.as_deref(), Some("duas vias"));

        bia.on_mls(ana_peer, &direct).unwrap();
        assert!(received_texts(&events(&mut bia)).is_empty());
    }

    /// 3.1: depósito recusado é avisado, a mensagem continua na fila e é depositada depois.
    #[tokio::test]
    async fn a_refused_deposit_is_reported_and_retried_on_the_next_flush() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));

        relay.fail_deposits(true);
        let first = bia.send(ana_peer, "a", None).unwrap();
        let failed = until(&mut bia, |e| match e {
            ChatEvent::DepositFailed { message, .. } => Some(message),
            _ => None,
        })
        .await;
        assert_eq!(failed, Some(first));
        assert!(relay.deposits().is_empty());
        assert_eq!(bia.queued(&ana_peer), 1);

        relay.fail_deposits(false);
        bia.send(ana_peer, "b", None).unwrap();
        let texts: Vec<_> = relay
            .deposits()
            .iter()
            .map(|d| text_of(&mut ana, &d.blob))
            .collect();
        assert_eq!(texts, ["a", "b"], "a recusada foi depositada primeiro");
    }

    /// 3.1: um `from` que não tem conversa com a gente não vira mensagem de ninguém.
    #[tokio::test]
    async fn a_retrieved_blob_from_an_unknown_sender_is_dropped() {
        let (mut bia, mut ana, _ana_peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        let stranger = DeviceKey::generate().peer_id();
        MemoryMailbox::new(&relay, stranger).deposit(
            bia.peer_id(),
            Envelope::Mls(wire(&mut ana, "intruso")).encode(),
        );
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));

        let dropped = until(&mut bia, |e| match e {
            ChatEvent::Dropped { peer, .. } => Some(peer),
            ChatEvent::Received { .. } => panic!("mensagem atribuída a quem não tem conversa"),
            _ => None,
        })
        .await;
        assert_eq!(dropped, Some(stranger));
    }

    /// 3.1: o handshake nunca entra pela caixa postal (exige o Peer ID da conexão, P15); só MLS.
    #[tokio::test]
    async fn a_handshake_envelope_from_the_mailbox_is_dropped() {
        let (mut bia, _ana, _ana_peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        let stranger = DeviceKey::generate().peer_id();
        MemoryMailbox::new(&relay, stranger)
            .deposit(bia.peer_id(), Envelope::Hello(None, None, None).encode());
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));

        let dropped = until(&mut bia, |e| match e {
            ChatEvent::Dropped { peer, .. } => Some(peer),
            _ => None,
        })
        .await;
        assert_eq!(dropped, Some(stranger));
    }

    /// P15: o KeyPackage legítimo de Bia, entregue por outro Peer ID, não vira conversa.
    #[tokio::test]
    async fn key_package_from_another_peer_is_refused() {
        let (id_a, id_b) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        // `on_key_package` só age no lado de menor Peer ID: sorteia até Ana ser a menor.
        let (dev_a, dev_b, dev_m) = loop {
            let keys = (
                DeviceKey::generate(),
                DeviceKey::generate(),
                DeviceKey::generate(),
            );
            if keys.0.peer_id() < keys.1.peer_id() && keys.0.peer_id() < keys.2.peer_id() {
                break keys;
            }
        };
        let mut ana = chat(&id_a, &dev_a);
        let bia_kp = CryptoDevice::new(&id_b, &dev_b.public_key(), &dev_b.nostr_public_key(), 1)
            .unwrap()
            .key_package()
            .unwrap();

        // M (um peer qualquer, conectado a Ana) repassa o pacote da Bia como se fosse dele.
        let err = ana.on_key_package(dev_m.peer_id(), &bia_kp).unwrap_err();
        assert!(
            matches!(err, Error::Crypto(CryptoError::UnexpectedPeer)),
            "{err:?}"
        );
        assert!(!ana.is_ready(&dev_m.peer_id()) && !ana.is_ready(&dev_b.peer_id()));

        // Vindo da própria Bia, o mesmo pacote é aceito.
        ana.on_key_package(dev_b.peer_id(), &bia_kp).unwrap();
        assert!(ana.is_ready(&dev_b.peer_id()));
    }

    /// P18 PR 3 (auditoria, médio 1): a identidade que o `Hello` autenticou é a que o `KeyPackage` tem de
    /// ter. Sem isso, quem tem as chaves de um device de T (roubado ou revogado) passa pelo portão como T
    /// e abre a conversa com um certificado de outra identidade X para as mesmas chaves.
    #[tokio::test]
    async fn a_key_package_of_another_identity_than_the_hello_is_refused() {
        let (id_a, id_t, id_x) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        let (dev_a, dev_b) = loop {
            let keys = (DeviceKey::generate(), DeviceKey::generate());
            if keys.0.peer_id() < keys.1.peer_id() {
                break keys;
            }
        };
        let device_of = |identity: &StandaloneIdentity| {
            CryptoDevice::new(identity, &dev_b.public_key(), &dev_b.nostr_public_key(), 1).unwrap()
        };
        let mut ana = chat(&id_a, &dev_a);
        ana.set_require_invite(true);
        ana.trust(id_t.id()).unwrap();

        // O `Hello` diz T (aceita); o `KeyPackage` traz X, desconhecida, para as mesmas chaves do device.
        ana.on_hello(
            dev_b.peer_id(),
            None,
            Some(device_of(&id_t).certificate().to_bytes()),
            None,
        )
            .unwrap();
        let err = ana
            .on_key_package(dev_b.peer_id(), &device_of(&id_x).key_package().unwrap())
            .unwrap_err();
        assert!(
            matches!(err, Error::Crypto(CryptoError::UnexpectedIdentity)),
            "{err:?}"
        );
        assert!(!ana.is_ready(&dev_b.peer_id()));

        // O `KeyPackage` da mesma identidade do `Hello` é aceito.
        ana.on_key_package(dev_b.peer_id(), &device_of(&id_t).key_package().unwrap())
            .unwrap();
        assert!(ana.is_ready(&dev_b.peer_id()));
    }

    /// O certificado que o `Hello` leva a `peer` (`None` se foi sem), lido do envelope em voo.
    fn hello_certificate_sent_to(chat: &mut Chat, peer: PeerId) -> Option<Option<Vec<u8>>> {
        chat.handshake.clear();
        chat.send_hello(peer);
        chat.handshake.values().find_map(|envelope| match envelope {
            Envelope::Hello(_, certificate, _) => Some(certificate.clone()),
            _ => None,
        })
    }

    /// P18 PR 3 (auditoria, médio 2; decisão do dono): o certificado (identidade e chave Nostr) só vai no
    /// `Hello` a quem já conheço. Estranho recebe o `Hello` sem identidade.
    #[tokio::test]
    async fn the_hello_certificate_goes_only_to_peers_i_already_know() {
        let Household {
            mut bia,
            ana_device: (ana_peer, _),
            ..
        } = household();
        let stranger = PeerId::random();
        assert_eq!(hello_certificate_sent_to(&mut bia, stranger), Some(None));

        // Com conversa (a Ana do `household`), discado por mim ou esperado por convite: leva.
        let own = Some(bia.device.certificate().to_bytes());
        assert_eq!(hello_certificate_sent_to(&mut bia, ana_peer), Some(own.clone()));
        let (dialed, expected) = (PeerId::random(), PeerId::random());
        bia.dialed.insert(dialed);
        bia.expected.insert(expected, StandaloneIdentity::generate().id());
        assert_eq!(hello_certificate_sent_to(&mut bia, dialed), Some(own.clone()));
        assert_eq!(hello_certificate_sent_to(&mut bia, expected), Some(own));
    }

    /// Auditoria (baixo 3): quem eu disquei e não virou conversa deixa de ser admitido quando a conexão cai.
    #[tokio::test]
    async fn a_dialed_peer_is_forgotten_when_it_disconnects_without_a_conversation() {
        let Household {
            mut bia,
            ana_device: (ana_peer, _),
            ..
        } = household();
        let stranger = PeerId::random();
        bia.dialed.insert(stranger);
        bia.dialed.insert(ana_peer);

        bia.handle(NodeEvent::PeerDisconnected(stranger));
        bia.handle(NodeEvent::PeerDisconnected(ana_peer));

        assert!(!bia.dialed.contains(&stranger));
        assert!(!bia.dialed.contains(&ana_peer));
    }

    /// Auditoria (baixo 4): só vale o `/p2p/<id>` que **termina** o endereço; em circuito, o destino.
    #[test]
    fn only_a_trailing_p2p_component_pins_the_dialed_peer() {
        let (relay, target) = (PeerId::random(), PeerId::random());
        let parse = |s: String| pinned_peer(&s.parse::<Multiaddr>().unwrap());

        assert_eq!(parse(format!("/ip4/10.0.0.1/tcp/1/p2p/{target}")), Some(target));
        assert_eq!(
            parse(format!("/ip4/10.0.0.1/tcp/1/p2p/{relay}/p2p-circuit/p2p/{target}")),
            Some(target)
        );
        assert_eq!(parse("/ip4/10.0.0.1/tcp/1".into()), None);
        assert_eq!(
            parse(format!("/ip4/10.0.0.1/tcp/1/p2p/{relay}/p2p-circuit")),
            None,
            "circuito sem destino: o relay não vira admitido"
        );
        assert_eq!(
            parse(format!("/p2p/{target}/ip4/10.0.0.1/tcp/1")),
            None,
            "o Peer ID não termina o endereço"
        );
    }

    /// `MemoryMailbox` que declara por quanto tempo o backend guarda um depósito (como o Nostr com TTL).
    struct Retaining(MemoryMailbox, Option<std::time::Duration>);

    impl Mailbox for Retaining {
        fn attach(&mut self, events: futures::channel::mpsc::UnboundedSender<MailboxEvent>) {
            self.0.attach(events);
        }

        fn deposit(&mut self, to: PeerId, blob: Vec<u8>) -> DepositId {
            self.0.deposit(to, blob)
        }

        fn retention(&self) -> Option<std::time::Duration> {
            self.1
        }
    }

    /// Bia com Ana offline e uma caixa postal de retenção `keep`; uma mensagem já depositada e confirmada.
    async fn deposited_once(
        keep: Option<std::time::Duration>,
    ) -> (Chat, Conversation, PeerId, MemoryRelay, MessageId) {
        let (mut bia, ana, ana_peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        bia.set_mailbox(Retaining(MemoryMailbox::new(&relay, bia.peer_id()), keep));
        let id = bia.send(ana_peer, "oi", None).unwrap();
        until(&mut bia, |e| {
            matches!(e, ChatEvent::Deposited { .. }).then_some(())
        })
        .await;
        assert_eq!(relay.deposits().len(), 1);
        (bia, ana, ana_peer, relay, id)
    }

    /// 3.4 B: depósito velho demais (sem ack) volta a ser depositado no próximo gatilho.
    #[tokio::test]
    async fn a_stale_unacked_deposit_is_deposited_again_on_the_next_flush() {
        let (mut bia, mut ana, ana_peer, relay, _) =
            deposited_once(Some(std::time::Duration::ZERO)).await;

        bia.flush(ana_peer);

        let deposits = relay.deposits();
        assert_eq!(deposits.len(), 2);
        assert_eq!(text_of(&mut ana, &deposits[0].blob), "oi");
        assert_eq!(text_of(&mut ana, &deposits[1].blob), "oi");
        assert_ne!(
            deposits[0].blob, deposits[1].blob,
            "outro gift wrap, outros bytes"
        );
    }

    /// 3.4 B: depósito novo não é repetido.
    #[tokio::test]
    async fn a_fresh_deposit_is_not_repeated() {
        let (mut bia, _ana, ana_peer, relay, _) =
            deposited_once(Some(std::time::Duration::from_secs(7 * 24 * 3600))).await;

        bia.flush(ana_peer);

        assert_eq!(relay.deposits().len(), 1);
    }

    /// 3.4 B: backend sem retenção declarada (memória) nunca re-deposita.
    #[tokio::test]
    async fn a_backend_without_retention_never_deposits_again() {
        let (mut bia, _ana, ana_peer, relay, _) = deposited_once(None).await;

        bia.flush(ana_peer);

        assert_eq!(relay.deposits().len(), 1);
    }

    /// 3.4 B: mensagem confirmada (ack) sai da fila e não é mais depositada.
    #[tokio::test]
    async fn an_acked_message_is_not_deposited_again() {
        let (mut bia, _ana, ana_peer, relay, id) =
            deposited_once(Some(std::time::Duration::ZERO)).await;
        bia.outbox.remove(&ana_peer, &id).unwrap();

        bia.flush(ana_peer);

        assert_eq!(relay.deposits().len(), 1);
    }

    /// 3.4 B: o re-depósito respeita a janela da caixa postal e a ordem (mais antigas primeiro).
    #[tokio::test]
    async fn redeposits_respect_the_mailbox_window_and_the_order() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        bia.set_mailbox(Retaining(
            MemoryMailbox::new(&relay, bia.peer_id()),
            Some(std::time::Duration::ZERO),
        ));
        for i in 0..MAILBOX_WINDOW + 2 {
            bia.send(ana_peer, i.to_string(), None).unwrap();
        }
        for _ in 0..MAILBOX_WINDOW {
            until(&mut bia, |e| {
                matches!(e, ChatEvent::Deposited { .. }).then_some(())
            })
            .await;
        }
        assert_eq!(relay.deposits().len(), MAILBOX_WINDOW);

        bia.flush(ana_peer);

        let deposits = relay.deposits();
        assert_eq!(
            deposits.len(),
            2 * MAILBOX_WINDOW,
            "no máximo uma janela por rodada"
        );
        let again: Vec<String> = deposits[MAILBOX_WINDOW..]
            .iter()
            .map(|d| text_of(&mut ana, &d.blob))
            .collect();
        let expected: Vec<String> = (0..MAILBOX_WINDOW).map(|i| i.to_string()).collect();
        assert_eq!(again, expected);
    }

    // ---- P21: gatilhos de depósito da caixa postal ----

    /// P21: espera o próximo `DepositFailed` (o relay está recusando).
    async fn until_deposit_fails(bia: &mut Chat) {
        let failed = until(bia, |e| {
            matches!(e, ChatEvent::DepositFailed { .. }).then_some(())
        })
        .await;
        assert!(failed.is_some(), "esperava a recusa do depósito");
    }

    /// P21: Bia com Ana offline e uma caixa postal que recusa tudo; manda uma mensagem e espera a recusa.
    async fn bia_with_a_refused_deposit() -> (Chat, PeerId, MemoryRelay) {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        relay.fail_deposits(true);
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));
        bia.send(ana_peer, "oi", None).unwrap();
        until_deposit_fails(&mut bia).await;
        (bia, ana_peer, relay)
    }

    /// P21: vence o prazo do novo depósito de `peer` sem esperar.
    fn make_deposit_due(bia: &mut Chat, peer: PeerId) {
        bia.deposit_retry
            .get_mut(&peer)
            .expect("sem novo depósito agendado")
            .due = Instant::now();
    }

    /// P21: o que já estava na fila quando a caixa postal foi ligada é depositado na hora, sem esperar
    /// outro `send` nem a queda do peer.
    #[tokio::test]
    async fn turning_the_mailbox_on_deposits_what_was_already_queued() {
        let (mut bia, mut ana, ana_peer) = bia_and_raw_ana();
        let message = bia.send(ana_peer, "esperando", None).unwrap();
        let relay = MemoryRelay::default();

        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));

        let deposited = until(&mut bia, |e| match e {
            ChatEvent::Deposited { message, .. } => Some(message),
            _ => None,
        })
        .await;
        assert_eq!(deposited, Some(message));
        let deposits = relay.deposits();
        assert_eq!(deposits.len(), 1);
        assert_eq!(text_of(&mut ana, &deposits[0].blob), "esperando");
    }

    /// P21: com o peer conectado, ligar a caixa postal não deposita (vale o caminho direto).
    #[tokio::test]
    async fn turning_the_mailbox_on_does_not_deposit_for_a_connected_peer() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        bia.send(ana_peer, "direto", None).unwrap();
        let relay = MemoryRelay::default();

        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));

        assert!(relay.deposits().is_empty());
    }

    /// P21: depósito recusado agenda um novo; antes do prazo nada sai, no prazo sai.
    #[tokio::test]
    async fn a_refused_deposit_is_tried_again_only_after_the_wait() {
        let (mut bia, ana_peer, relay) = {
            let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
            bia.retry_base = Duration::from_secs(3600);
            let relay = MemoryRelay::default();
            relay.fail_deposits(true);
            bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));
            bia.send(ana_peer, "oi", None).unwrap();
            until_deposit_fails(&mut bia).await;
            (bia, ana_peer, relay)
        };
        relay.fail_deposits(false);
        assert_eq!(bia.deposit_retry[&ana_peer].attempts, 1);

        bia.flush_due_deposits();
        assert!(relay.deposits().is_empty(), "o prazo não venceu: nada sai");

        make_deposit_due(&mut bia, ana_peer);
        bia.flush_due_deposits();
        assert_eq!(relay.deposits().len(), 1, "venceu: deposita de novo");
    }

    /// P21: recusar de novo depois do novo depósito dobra a espera (2 falhas = o dobro da base).
    #[tokio::test]
    async fn a_second_refusal_doubles_the_wait() {
        let (mut bia, ana_peer, _relay) = bia_with_a_refused_deposit().await;
        bia.retry_base = Duration::from_secs(3600);
        bia.retry_cap = Duration::from_secs(86_400);
        bia.deposit_retry.clear();
        bia.schedule_deposit_retry(ana_peer);
        make_deposit_due(&mut bia, ana_peer);
        bia.flush_due_deposits();
        until_deposit_fails(&mut bia).await;

        assert_eq!(bia.deposit_retry[&ana_peer].attempts, 2);
        assert!(
            bia.deposit_retry[&ana_peer].due > Instant::now() + Duration::from_secs(7000),
            "2 recusas: ~2 x 3600 s"
        );
    }

    /// P21 (auditoria I1 do P19, mesma conta): depois de `deposit_budget` recusas seguidas o timer para de
    /// armar; o registro fica, desarmado, e o relógio não acorda mais por ele.
    #[tokio::test]
    async fn the_deposit_budget_stops_the_timer() {
        let (mut bia, ana_peer, _relay) = bia_with_a_refused_deposit().await;
        bia.deposit_budget = 3;
        bia.retry_base = Duration::from_secs(3600);
        bia.deposit_retry.clear();
        bia.deposit_spent.clear(); // a recusa do helper já gastou 1
        for _ in 0..2 {
            bia.schedule_deposit_retry(ana_peer);
        }
        assert!(bia.deposit_retry[&ana_peer].armed, "2 de 3: ainda arma");

        bia.schedule_deposit_retry(ana_peer);
        assert_eq!(bia.deposit_spent[&ana_peer], 3, "a conta é por recusa");
        assert!(!bia.deposit_retry[&ana_peer].armed, "3 de 3: esgotou");
        bia.redial.clear(); // o envio offline também agenda rediscagem (P32): aqui só o depósito importa
        assert_eq!(bia.wake_in(), None);
    }

    /// P21: depósito aceito zera a conta: a próxima recusa recomeça da espera base.
    #[tokio::test]
    async fn an_accepted_deposit_resets_the_backoff() {
        let (mut bia, ana_peer, relay) = bia_with_a_refused_deposit().await;
        relay.fail_deposits(false);
        make_deposit_due(&mut bia, ana_peer);
        bia.flush_due_deposits();

        let deposited = until(&mut bia, |e| {
            matches!(e, ChatEvent::Deposited { .. }).then_some(())
        })
        .await;
        assert!(deposited.is_some());
        assert!(bia.deposit_retry.is_empty(), "o aceite zera");
    }

    /// P21: reconectar apaga o novo depósito agendado (o `PeerConnected` já envia direto).
    #[tokio::test]
    async fn reconnecting_clears_the_deposit_retry() {
        let (mut bia, ana_peer, _relay) = bia_with_a_refused_deposit().await;
        assert!(bia.deposit_retry.contains_key(&ana_peer));

        connect_peer(&mut bia, ana_peer);
        assert!(bia.deposit_retry.is_empty());
    }

    /// P21: esquecer o peer (conversa caiu) apaga o novo depósito agendado.
    #[tokio::test]
    async fn dropping_a_peer_clears_its_deposit_retry() {
        let mut r = revoked_ana();
        r.bia.deposit_retry.insert(
            r.ana_peer,
            Retry {
                attempts: 1,
                due: Instant::now(),
                armed: true,
            },
        );
        r.bia.deposit_spent.insert(r.ana_peer, 1);
        r.bia.sweep_rounds.insert(r.ana_peer, 1);

        r.bia.on_mls(r.sibling, &r.wire).unwrap();
        assert!(!r.bia.is_ready(&r.ana_peer), "a conversa caiu");
        assert!(!r.bia.deposit_retry.contains_key(&r.ana_peer));
        assert!(!r.bia.deposit_spent.contains_key(&r.ana_peer));
        assert!(!r.bia.sweep_rounds.contains_key(&r.ana_peer));
    }

    /// P21: o relógio acorda no prazo do novo depósito e dorme de novo depois que ele dispara.
    #[tokio::test]
    async fn the_clock_wakes_for_the_deposit_deadline_and_sleeps_after_it_fires() {
        let (mut bia, ana_peer, relay) = bia_with_a_refused_deposit().await;
        bia.retry_base = Duration::from_secs(3600);
        bia.retry_cap = Duration::from_secs(86_400);
        bia.deposit_retry.clear();
        bia.redial.clear(); // o envio offline também agenda rediscagem (P32): aqui só o depósito importa
        bia.schedule_deposit_retry(ana_peer);
        let wake = bia.wake_in().expect("com novo depósito agendado, acorda");
        assert!(wake <= Duration::from_secs(3600) && wake > Duration::from_secs(3000));

        relay.fail_deposits(false);
        make_deposit_due(&mut bia, ana_peer);
        bia.flush_due_deposits();
        assert_eq!(bia.wake_in(), None, "disparou: não acorda à toa");
    }

    /// P21: o laço do `next_event` de fato acorda no prazo e deposita sozinho, sem tráfego algum.
    #[tokio::test]
    async fn next_event_redeposits_by_itself_after_a_refusal() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.retry_base = Duration::from_millis(10);
        let relay = MemoryRelay::default();
        relay.fail_deposits(true);
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));
        let message = bia.send(ana_peer, "oi", None).unwrap();
        until_deposit_fails(&mut bia).await;
        relay.fail_deposits(false);

        let deposited = until(&mut bia, |e| match e {
            ChatEvent::Deposited { message, .. } => Some(message),
            _ => None,
        })
        .await;
        assert_eq!(deposited, Some(message));
        assert_eq!(relay.deposits().len(), 1);
    }

    /// P21: depósito confirmado arma a varredura por idade para metade da retenção do backend.
    #[tokio::test]
    async fn a_confirmed_deposit_arms_the_age_sweep() {
        let week = std::time::Duration::from_secs(7 * 24 * 3600);
        let (mut bia, _ana, _peer, _relay, _) = deposited_once(Some(week)).await;
        bia.redial.clear(); // o envio offline também agenda rediscagem (P32): aqui só a varredura importa

        assert!(bia.sweep_due.is_some());
        let wake = bia.wake_in().expect("a varredura pede o relógio");
        assert!(
            wake <= week / 4 && wake > week / 4 - Duration::from_secs(60),
            "a varredura roda a cada quarto da retenção (auditoria I2: margem antes de o relay esquecer)"
        );
    }

    /// P21: backend sem retenção declarada não tem o que varrer.
    #[tokio::test]
    async fn no_retention_means_no_age_sweep() {
        let (mut bia, _ana, _peer, _relay, _) = deposited_once(None).await;
        bia.redial.clear(); // o envio offline também agenda rediscagem (P32): aqui só a varredura importa

        assert!(bia.sweep_due.is_none());
        assert_eq!(bia.wake_in(), None);
    }

    /// P21: no prazo, a varredura re-deposita o que ficou velho sem ack, mesmo sem nenhum outro evento.
    #[tokio::test]
    async fn the_age_sweep_redeposits_a_stale_deposit() {
        let (mut bia, mut ana, _peer, relay, _) =
            deposited_once(Some(std::time::Duration::ZERO)).await;
        bia.sweep_due = Some(Instant::now());

        bia.sweep_stale_deposits();

        let deposits = relay.deposits();
        assert_eq!(deposits.len(), 2);
        assert_eq!(text_of(&mut ana, &deposits[1].blob), "oi");
    }

    /// P21: antes do prazo a varredura não faz nada, mesmo com depósito velho.
    #[tokio::test]
    async fn the_age_sweep_waits_for_its_deadline() {
        let (mut bia, _ana, _peer, relay, _) =
            deposited_once(Some(std::time::Duration::ZERO)).await;
        bia.sweep_due = Some(Instant::now() + Duration::from_secs(3600));

        bia.sweep_stale_deposits();

        assert_eq!(relay.deposits().len(), 1);
    }

    /// P21: a varredura não deposita para peer conectado (vale o caminho direto).
    #[tokio::test]
    async fn the_age_sweep_skips_a_connected_peer() {
        let (mut bia, _ana, ana_peer, relay, _) =
            deposited_once(Some(std::time::Duration::ZERO)).await;
        connect_peer(&mut bia, ana_peer);
        bia.sweep_due = Some(Instant::now());

        bia.sweep_stale_deposits();

        assert_eq!(relay.deposits().len(), 1);
    }

    /// P21: enquanto ainda há depósito pendente a varredura se rearma; sem nenhum, ela se desarma.
    #[tokio::test]
    async fn the_age_sweep_rearms_while_deposits_remain_and_stops_after() {
        let week = std::time::Duration::from_secs(7 * 24 * 3600);
        let (mut bia, _ana, ana_peer, _relay, id) = deposited_once(Some(week)).await;
        bia.sweep_due = Some(Instant::now());

        bia.sweep_stale_deposits();
        let again = bia.sweep_due.expect("ainda há depósito: rearma");
        assert!(again > Instant::now() + Duration::from_secs(24 * 3600));

        bia.outbox.remove(&ana_peer, &id).unwrap();
        bia.sweep_due = Some(Instant::now());
        bia.sweep_stale_deposits();
        assert!(bia.sweep_due.is_none(), "nada pendente: desarma");
    }

    // ---- P21, correções da auditoria ----

    /// P21, I2: depois de reiniciar (caixa postal ligada com depósitos já pendentes na outbox), a varredura
    /// é armada; antes só o `Deposited` a armava e o depósito envelhecia sem ninguém re-depositar.
    #[tokio::test]
    async fn turning_the_mailbox_on_arms_the_sweep_when_deposits_are_pending() {
        let week = std::time::Duration::from_secs(7 * 24 * 3600);
        let (mut bia, _ana, _peer, relay, _) = deposited_once(Some(week)).await;
        bia.sweep_due = None;

        bia.set_mailbox(Retaining(
            MemoryMailbox::new(&relay, bia.peer_id()),
            Some(week),
        ));

        assert!(bia.sweep_due.is_some());
    }

    /// P21, I2: sem nada na fila, ligar a caixa postal não arma varredura.
    #[tokio::test]
    async fn turning_the_mailbox_on_with_an_empty_queue_arms_nothing() {
        let (mut bia, _ana, _peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        let week = std::time::Duration::from_secs(7 * 24 * 3600);

        bia.set_mailbox(Retaining(
            MemoryMailbox::new(&relay, bia.peer_id()),
            Some(week),
        ));

        assert!(bia.sweep_due.is_none());
    }

    /// P21: uma retenção minúscula não faz a varredura girar mais que uma vez por segundo.
    #[tokio::test]
    async fn the_sweep_interval_has_a_one_second_floor() {
        let (bia, _ana, _peer, _relay, _) = deposited_once(Some(std::time::Duration::ZERO)).await;

        assert_eq!(bia.sweep_interval(), Some(Duration::from_secs(1)));
    }

    /// P21, S4: uma retenção absurda não estoura a conta do prazo (antes: pânico em `Instant + Duration`).
    #[tokio::test]
    async fn an_absurd_retention_does_not_panic_the_sweep() {
        let (mut bia, _ana, _peer, _relay, _) = deposited_once(Some(Duration::MAX)).await;
        bia.sweep_due = None;

        bia.arm_sweep();
    }

    /// P21, I1: a varredura para depois de `sweep_budget` rodadas por peer (teto de gerações MLS).
    #[tokio::test]
    async fn the_sweep_stops_after_its_round_budget() {
        let (mut bia, _ana, _peer, relay, _) =
            deposited_once(Some(std::time::Duration::ZERO)).await;
        bia.sweep_budget = 2;
        for _ in 0..2 {
            bia.sweep_due = Some(Instant::now());
            bia.sweep_stale_deposits();
            let deposited = until(&mut bia, |e| {
                matches!(e, ChatEvent::Deposited { .. }).then_some(())
            })
            .await;
            assert!(deposited.is_some());
        }
        assert_eq!(relay.deposits().len(), 3, "1 do envio + 2 rodadas");

        bia.sweep_due = Some(Instant::now());
        bia.sweep_stale_deposits();

        assert_eq!(relay.deposits().len(), 3, "esgotou: nada mais sai");
        assert!(bia.sweep_due.is_none(), "e não rearma");
    }

    /// P21, I1: conectar zera as rodadas da varredura.
    #[tokio::test]
    async fn connecting_resets_the_sweep_rounds() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.sweep_rounds.insert(ana_peer, 5);

        connect_peer(&mut bia, ana_peer);

        assert!(bia.sweep_rounds.is_empty());
    }

    /// P21, I1: o ack zera as rodadas da varredura.
    #[tokio::test]
    async fn an_ack_resets_the_sweep_rounds() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        bia.send(ana_peer, "oi", None).unwrap();
        let id = first_in_flight(&bia);
        bia.sweep_rounds.insert(ana_peer, 5);

        bia.handle(NodeEvent::MessageDelivered { peer: ana_peer, id });

        assert!(bia.sweep_rounds.is_empty());
    }

    /// P21, I3: o aceite de uma mensagem não apaga o agendamento da outra, recusada, do mesmo peer.
    #[tokio::test]
    async fn an_accepted_deposit_keeps_the_retry_of_another_refused_message() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));
        let first = bia.send(ana_peer, "um", None).unwrap();
        bia.send(ana_peer, "dois", None).unwrap();
        let id_of = |bia: &Chat, wanted: bool| {
            bia.depositing
                .iter()
                .find(|(_, (_, m))| (*m == first) == wanted)
                .map(|(id, _)| *id)
                .unwrap()
        };
        let (accepted, refused) = (id_of(&bia, true), id_of(&bia, false));

        bia.handle_mailbox(MailboxEvent::DepositFailed {
            id: refused,
            reason: "recusado".into(),
        });
        bia.handle_mailbox(MailboxEvent::Deposited { id: accepted });

        assert!(
            bia.deposit_retry.contains_key(&ana_peer),
            "a segunda ainda precisa de novo depósito"
        );
    }

    /// P21, I4: várias recusas na mesma rodada contam UMA tentativa de backoff (a rampa de 2 s a 60 s vale
    /// com a fila cheia) e UMA geração gasta por recusa (o orçamento).
    #[tokio::test]
    async fn a_whole_refused_round_is_one_backoff_step_and_many_spent_generations() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.retry_base = Duration::from_secs(3600);
        let relay = MemoryRelay::default();
        relay.fail_deposits(true);
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));
        for i in 0..MAILBOX_WINDOW {
            bia.send(ana_peer, i.to_string(), None).unwrap();
        }
        for _ in 0..MAILBOX_WINDOW {
            until_deposit_fails(&mut bia).await;
        }

        assert_eq!(bia.deposit_retry[&ana_peer].attempts, 1);
        assert_eq!(bia.deposit_spent[&ana_peer], MAILBOX_WINDOW as u32);
    }

    /// P21, I4: um aceite zera a conta de gerações gastas.
    #[tokio::test]
    async fn an_accepted_deposit_resets_the_spent_generations() {
        let (mut bia, ana_peer, relay) = bia_with_a_refused_deposit().await;
        assert_eq!(bia.deposit_spent[&ana_peer], 1);
        relay.fail_deposits(false);
        make_deposit_due(&mut bia, ana_peer);
        bia.flush_due_deposits();
        until(&mut bia, |e| {
            matches!(e, ChatEvent::Deposited { .. }).then_some(())
        })
        .await;

        assert!(bia.deposit_spent.is_empty());
    }

    /// P21, S3: uma recusa tardia de um peer que acabou de conectar não cria agendamento.
    #[tokio::test]
    async fn a_late_refusal_for_a_connected_peer_schedules_nothing() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        let relay = MemoryRelay::default();
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));
        bia.send(ana_peer, "oi", None).unwrap();
        let id = *bia.depositing.keys().next().unwrap();
        connect_peer(&mut bia, ana_peer);

        bia.handle_mailbox(MailboxEvent::DepositFailed {
            id,
            reason: "tardia".into(),
        });

        assert!(bia.deposit_retry.is_empty());
        assert!(bia.deposit_spent.is_empty());
    }

    // ---- P32: reconexão automática ----

    fn addr(text: &str) -> Multiaddr {
        text.parse().unwrap()
    }

    /// P32:o peer informa onde escuta (o que o transporte entrega depois do `identify`).
    fn identify(bia: &mut Chat, peer: PeerId, listen: &[&str]) {
        bia.handle(NodeEvent::PeerIdentified {
            peer,
            listen_addrs: listen.iter().map(|a| addr(a)).collect(),
            observed_addr: addr("/ip4/203.0.113.9/tcp/1"),
        });
    }

    /// P32:vence o prazo da rediscagem de `peer` sem esperar.
    fn make_redial_due(bia: &mut Chat, peer: PeerId) {
        bia.redial
            .get_mut(&peer)
            .expect("sem rediscagem agendada")
            .due = Instant::now();
    }

    /// P32:Bia com Ana conectada e uma mensagem na fila (em voo), depois Ana cai.
    fn bia_after_ana_dropped_with_a_queue() -> (Chat, PeerId) {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);
        bia.send(ana_peer, "oi", None).unwrap();
        bia.handle(NodeEvent::PeerDisconnected(ana_peer));
        (bia, ana_peer)
    }

    const GLOBAL_A: &str = "/ip4/203.0.113.10/tcp/4001";
    const GLOBAL_B: &str = "/ip4/203.0.113.11/tcp/4001";

    /// P32:peer que cai com mensagem na fila agenda a primeira rediscagem.
    #[tokio::test]
    async fn a_dropped_peer_with_a_queue_is_scheduled_for_redial() {
        let (bia, ana_peer) = bia_after_ana_dropped_with_a_queue();

        assert_eq!(bia.redial[&ana_peer].attempts, 1);
        assert_eq!(bia.redial[&ana_peer].state, RedialState::Waiting);
    }

    /// P32:sem nada na fila não há por que rediscar (só a caixa postal e o mDNS trazem o peer de volta).
    #[tokio::test]
    async fn a_dropped_peer_without_a_queue_is_not_redialed() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        connect_peer(&mut bia, ana_peer);

        bia.handle(NodeEvent::PeerDisconnected(ana_peer));

        assert!(bia.redial.is_empty());
    }

    /// P32:mensagem enfileirada com o peer que nunca conectou nesta execução também agenda.
    #[tokio::test]
    async fn an_offline_send_schedules_a_redial() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();

        bia.send(ana_peer, "oi", None).unwrap();

        assert_eq!(bia.redial[&ana_peer].attempts, 1);
    }

    /// P32:antes do prazo nada é discado; no prazo disca o peer com os endereços que ele informou.
    #[tokio::test]
    async fn the_redial_waits_for_its_deadline_then_dials_the_learned_addresses() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.redial_base = Duration::from_secs(3600);
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        bia.send(ana_peer, "oi", None).unwrap();

        bia.flush_due_redials();
        assert!(bia.dial_log.is_empty(), "o prazo não venceu");

        make_redial_due(&mut bia, ana_peer);
        bia.flush_due_redials();
        assert_eq!(bia.dial_log, [(ana_peer, vec![addr(GLOBAL_A)])]);
        assert_eq!(
            bia.redial[&ana_peer].state,
            RedialState::InFlight,
            "disparou: espera o resultado do dial"
        );
    }

    /// P32:o que o peer informa passa pelo mesmo filtro dos endereços de terceiros (sem loopback, rede
    /// privada nem DNS), no máximo `CONTACT_DIAL_ADDRS`, e a informação nova substitui a velha.
    #[tokio::test]
    async fn learned_addresses_are_filtered_bounded_and_replaced() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        identify(
            &mut bia,
            ana_peer,
            &[
                "/ip4/127.0.0.1/tcp/4001",
                "/ip4/192.168.0.5/tcp/4001",
                "/dnsaddr/example.com",
                "/ip4/203.0.113.1/tcp/1",
                "/ip4/203.0.113.2/tcp/1",
                "/ip4/203.0.113.3/tcp/1",
                "/ip4/203.0.113.4/tcp/1",
                "/ip4/203.0.113.5/tcp/1",
            ],
        );
        assert_eq!(
            bia.redial_targets(&ana_peer),
            [
                addr("/ip4/203.0.113.1/tcp/1"),
                addr("/ip4/203.0.113.2/tcp/1"),
                addr("/ip4/203.0.113.3/tcp/1"),
                addr("/ip4/203.0.113.4/tcp/1"),
            ]
        );

        identify(&mut bia, ana_peer, &[GLOBAL_B]);
        assert_eq!(
            bia.redial_targets(&ana_peer)[0],
            addr(GLOBAL_B),
            "a informação nova vai na frente"
        );
        assert!(bia.redial_targets(&ana_peer).len() <= CONTACT_DIAL_ADDRS);
    }

    /// P32:peer sem conversa não entra em `peer_addrs` (memória limitada pelo número de contatos).
    #[tokio::test]
    async fn addresses_of_a_peer_without_a_conversation_are_ignored() {
        let (mut bia, _ana, _ana_peer) = bia_and_raw_ana();
        let stranger = PeerId::random();

        identify(&mut bia, stranger, &[GLOBAL_A]);

        assert!(bia.peer_addrs.is_empty());
    }

    /// P32:o `identify` chega com a conexão, antes de a conversa existir (quem disca com `dial` não tem
    /// `expected`). O endereço informado nessa janela tem de valer na rediscagem depois da queda.
    #[tokio::test]
    async fn addresses_told_before_the_conversation_exists_are_used_to_redial() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        let conversation = bia.conversations.remove(&ana_peer).unwrap();
        bia.connected.insert(ana_peer);

        identify(&mut bia, ana_peer, &[GLOBAL_A]);

        bia.conversations.insert(ana_peer, conversation);
        bia.send(ana_peer, "oi", None).unwrap();
        bia.handle(NodeEvent::PeerDisconnected(ana_peer));
        make_redial_due(&mut bia, ana_peer);
        bia.flush_due_redials();

        assert_eq!(bia.dial_log, [(ana_peer, vec![addr(GLOBAL_A)])]);
    }

    /// P32:estranho conectado só ocupa vaga em `peer_addrs` enquanto a conexão durar.
    #[tokio::test]
    async fn addresses_of_a_connected_stranger_are_dropped_when_it_disconnects() {
        let (mut bia, _ana, _ana_peer) = bia_and_raw_ana();
        let stranger = PeerId::random();
        bia.connected.insert(stranger);

        identify(&mut bia, stranger, &[GLOBAL_A]);
        assert!(bia.peer_addrs.contains_key(&stranger), "conectado: guarda");

        bia.handle(NodeEvent::PeerDisconnected(stranger));
        assert!(bia.peer_addrs.is_empty(), "caiu sem conversa: esquece");
    }

    /// P32:com o mapa no teto, estranho conectado não entra (e quem já estava continua).
    #[tokio::test]
    async fn a_connected_stranger_cannot_push_past_the_cap() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        identify(&mut bia, ana_peer, &[GLOBAL_B]);
        for _ in 1..MAX_KNOWN_DEVICES {
            bia.peer_addrs
                .insert(PeerId::random(), vec![addr(GLOBAL_A)]);
        }
        let stranger = PeerId::random();
        bia.connected.insert(stranger);

        identify(&mut bia, stranger, &[GLOBAL_A]);

        assert_eq!(bia.peer_addrs.len(), MAX_KNOWN_DEVICES);
        assert!(!bia.peer_addrs.contains_key(&stranger));
        assert!(bia.peer_addrs.contains_key(&ana_peer));
    }

    /// P32 (auditoria #1): estranhos conectados enchem o mapa primeiro; o contato com conversa que informa o
    /// endereço depois despeja um estranho em vez de ser descartado.
    #[tokio::test]
    async fn a_contact_evicts_a_stranger_when_the_map_is_full() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        let strangers: Vec<PeerId> = (0..MAX_KNOWN_DEVICES).map(|_| PeerId::random()).collect();
        for stranger in &strangers {
            bia.connected.insert(*stranger);
            identify(&mut bia, *stranger, &[GLOBAL_A]);
        }
        assert_eq!(bia.peer_addrs.len(), MAX_KNOWN_DEVICES, "mapa cheio");

        identify(&mut bia, ana_peer, &[GLOBAL_B]);

        assert_eq!(bia.redial_targets(&ana_peer), [addr(GLOBAL_B)]);
        assert_eq!(bia.peer_addrs.len(), MAX_KNOWN_DEVICES, "o teto vale");
        assert_eq!(
            strangers
                .iter()
                .filter(|s| bia.peer_addrs.contains_key(s))
                .count(),
            MAX_KNOWN_DEVICES - 1,
            "um estranho saiu"
        );
    }

    /// P32 (auditoria #3): peer com handshake esperado (`expected`) que cai antes da conversa nascer mantém o
    /// endereço aprendido (como antes de a janela pré-conversa existir).
    #[tokio::test]
    async fn an_expected_peer_keeps_its_addresses_when_it_drops_before_the_handshake() {
        let (mut bia, _ana, _ana_peer) = bia_and_raw_ana();
        let peer = PeerId::random();
        let identity = invite_for(&StandaloneIdentity::generate())
            .identity()
            .clone();
        bia.expected.insert(peer, identity);
        bia.connected.insert(peer);
        identify(&mut bia, peer, &[GLOBAL_A]);

        bia.handle(NodeEvent::PeerDisconnected(peer));

        assert!(bia.peer_addrs.contains_key(&peer));
    }

    /// P32:estranho conectado que só anuncia IP privado ou loopback não deixa nada (mesmo filtro).
    #[tokio::test]
    async fn a_connected_stranger_with_only_private_addresses_leaves_nothing() {
        let (mut bia, _ana, _ana_peer) = bia_and_raw_ana();
        let stranger = PeerId::random();
        bia.connected.insert(stranger);

        identify(
            &mut bia,
            stranger,
            &["/ip4/127.0.0.1/tcp/4001", "/ip4/192.168.0.5/tcp/4001"],
        );

        assert!(bia.peer_addrs.is_empty());
    }

    /// P32 a: o endereço que um contato com conversa informa vai também para o banco.
    #[tokio::test]
    async fn a_learned_address_of_a_contact_is_written_to_the_outbox() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();

        identify(&mut bia, ana_peer, &[GLOBAL_A]);

        assert_eq!(bia.outbox.peer_addrs(&ana_peer).unwrap(), [addr(GLOBAL_A)]);
    }

    /// P32 a (auditoria #1): o mesmo endereço informado de novo é gravado de novo, então uma gravação que
    /// falhou (disco cheio, banco ocupado) é refeita no próximo `identify` em vez de ficar só na memória.
    #[tokio::test]
    async fn an_unchanged_address_is_written_again_on_the_next_identify() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        bia.outbox.forget_peer(&ana_peer).unwrap(); // some com a linha, como uma gravação que falhou
        assert!(bia.outbox.peer_addrs(&ana_peer).unwrap().is_empty());

        identify(&mut bia, ana_peer, &[GLOBAL_A]);

        assert_eq!(bia.outbox.peer_addrs(&ana_peer).unwrap(), [addr(GLOBAL_A)]);
    }

    /// P32 a: o endereço da janela pré-conversa (só em memória) vai para o banco quando a conversa nasce.
    #[tokio::test]
    async fn an_address_told_before_the_conversation_is_written_when_it_is_born() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        let conversation = bia.conversations.remove(&ana_peer).unwrap();
        bia.connected.insert(ana_peer);
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        assert!(
            bia.outbox.peer_addrs(&ana_peer).unwrap().is_empty(),
            "sem conversa, nada no disco"
        );

        bia.ready(ana_peer, conversation).unwrap();

        assert_eq!(bia.outbox.peer_addrs(&ana_peer).unwrap(), [addr(GLOBAL_A)]);
    }

    /// P32 a: estranho conectado (sem conversa) nunca escreve no disco.
    #[tokio::test]
    async fn the_address_of_a_stranger_is_never_written() {
        let (mut bia, _ana, _ana_peer) = bia_and_raw_ana();
        let stranger = PeerId::random();
        bia.connected.insert(stranger);

        identify(&mut bia, stranger, &[GLOBAL_A]);

        assert!(bia.outbox.peer_addrs(&stranger).unwrap().is_empty());
    }

    /// P32 a: depois de reiniciar, o contato com fila é rediscado pelo endereço do disco, sem nenhum envio
    /// novo, e o que o filtro não aceita (rede privada gravada à mão) não é discado.
    #[tokio::test]
    async fn after_a_restart_a_contact_with_a_queue_is_redialed_from_the_stored_address() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.outbox
            .set_peer_addrs(
                &ana_peer,
                &[addr(GLOBAL_A), addr("/ip4/192.168.0.5/tcp/4001")],
            )
            .unwrap();
        bia.outbox
            .enqueue(&ana_peer, &Message::new("oi".into(), None))
            .unwrap();
        assert!(
            bia.peer_addrs.is_empty(),
            "memória vazia, como depois de reiniciar"
        );

        bia.restore_peer_addrs();

        assert_eq!(bia.redial[&ana_peer].state, RedialState::Waiting);
        make_redial_due(&mut bia, ana_peer);
        bia.flush_due_redials();
        assert_eq!(bia.dial_log, [(ana_peer, vec![addr(GLOBAL_A)])]);
    }

    /// P32 a: reaberto com a fila vazia, o endereço volta para a memória mas nada é agendado.
    #[tokio::test]
    async fn after_a_restart_an_empty_queue_schedules_no_redial() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.outbox
            .set_peer_addrs(&ana_peer, &[addr(GLOBAL_A)])
            .unwrap();

        bia.restore_peer_addrs();

        assert_eq!(bia.redial_targets(&ana_peer), [addr(GLOBAL_A)]);
        assert!(bia.redial.is_empty());
    }

    /// P32 a: linha de peer sem conversa (sobra de um peer esquecido) não volta para a memória.
    #[tokio::test]
    async fn after_a_restart_the_stored_address_of_a_peer_without_a_conversation_is_ignored() {
        let (mut bia, _ana, _ana_peer) = bia_and_raw_ana();
        let gone = PeerId::random();
        bia.outbox.set_peer_addrs(&gone, &[addr(GLOBAL_A)]).unwrap();

        bia.restore_peer_addrs();

        assert!(!bia.peer_addrs.contains_key(&gone));
    }

    /// P32:sem endereço informado, valem os do convite do contato.
    #[tokio::test]
    async fn the_contact_invite_addresses_are_the_fallback() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.contact_devices
            .insert(ana_peer, invite_for(&StandaloneIdentity::generate()));

        assert_eq!(
            bia.redial_targets(&ana_peer),
            [addr("/ip4/203.0.113.1/tcp/1")]
        );
    }

    /// P32:a espera é 2 s, 4 s, 8 s... e para no teto de 5 min; muitas falhas não estouram a conta.
    #[tokio::test]
    async fn the_redial_wait_doubles_up_to_the_cap() {
        let (bia, _ana, _peer) = bia_and_raw_ana();
        let secs = |attempts| bia.redial_backoff(attempts).as_secs();
        let waits: Vec<u64> = (1..=10).map(secs).collect();
        assert_eq!(waits, [2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
        assert_eq!(secs(1000), 300, "sem estouro de aritmética");
    }

    /// P32:o prazo agendado tem jitter de no máximo 20% para cada lado (dois peers não rediscam em sincronia).
    #[tokio::test]
    async fn the_scheduled_deadline_has_a_bounded_jitter() {
        for _ in 0..20 {
            let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
            bia.redial_base = Duration::from_secs(3600);
            bia.redial_cap = Duration::from_secs(86_400); // acima da base, senão o teto corta
            let before = Instant::now();
            bia.send(ana_peer, "oi", None).unwrap();
            let after = Instant::now();

            let due = bia.redial[&ana_peer].due;
            assert!(
                due >= before + Duration::from_secs(2880),
                "jitter abaixo de -20%"
            );
            assert!(
                due <= after + Duration::from_secs(4320),
                "jitter acima de +20%"
            );
        }
    }

    /// P32:o dial falhou: agenda o próximo, com espera maior (2 falhas = o dobro da base).
    #[tokio::test]
    async fn a_failed_dial_schedules_the_next_one_with_a_longer_wait() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.redial_base = Duration::from_secs(3600);
        bia.redial_cap = Duration::from_secs(86_400);
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        bia.send(ana_peer, "oi", None).unwrap();
        make_redial_due(&mut bia, ana_peer);
        bia.flush_due_redials();

        bia.handle(NodeEvent::DialFailed {
            peer: Some(ana_peer),
            reason: "sem rota".into(),
        });

        assert_eq!(bia.redial[&ana_peer].attempts, 2);
        assert_eq!(bia.redial[&ana_peer].state, RedialState::Waiting);
        assert!(
            bia.redial[&ana_peer].due > Instant::now() + Duration::from_secs(5700),
            "2 falhas: ~2 x 3600 s, menos o jitter"
        );
    }

    /// P32:uma falha que chega com a rediscagem ainda esperando o prazo (dial antigo) não dobra a espera.
    #[tokio::test]
    async fn a_late_failure_while_waiting_does_not_double_the_wait() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.send(ana_peer, "oi", None).unwrap();

        bia.handle(NodeEvent::DialFailed {
            peer: Some(ana_peer),
            reason: "antiga".into(),
        });

        assert_eq!(bia.redial[&ana_peer].attempts, 1);
    }

    /// P32:depois de `redial_budget` tentativas o registro esgota: sem timer, e o orçamento de 3 dá exatamente
    /// 2 dials (a tentativa 1 é a primeira espera; a 3ª é a que esgota). Falha avulsa de outro dial não conta.
    #[tokio::test]
    async fn the_redial_budget_stops_the_timer_and_the_dials() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.redial_budget = 3;
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        bia.send(ana_peer, "oi", None).unwrap();
        for _ in 1..=2 {
            make_redial_due(&mut bia, ana_peer);
            bia.flush_due_redials();
            bia.handle(NodeEvent::DialFailed {
                peer: Some(ana_peer),
                reason: "sem rota".into(),
            });
        }

        assert_eq!(bia.redial[&ana_peer].state, RedialState::Exhausted);
        assert_eq!(bia.redial[&ana_peer].attempts, 3);
        assert_eq!(bia.dial_log.len(), 2);
        assert_eq!(bia.wake_in(), None);

        make_redial_due(&mut bia, ana_peer);
        bia.flush_due_redials();
        bia.handle(NodeEvent::DialFailed {
            peer: Some(ana_peer),
            reason: "de outro dial".into(),
        });
        assert_eq!(bia.dial_log.len(), 2, "esgotado: nada mais é discado");
        assert_eq!(
            bia.redial[&ana_peer].attempts, 3,
            "e a falha avulsa não conta"
        );
    }

    /// P32,I2: esgotado o orçamento, uma mensagem nova rearma UMA tentativa; depois dela volta a esgotar.
    #[tokio::test]
    async fn a_new_message_rearms_one_attempt_after_the_budget_is_exhausted() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.redial_budget = 2;
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        bia.send(ana_peer, "um", None).unwrap();
        make_redial_due(&mut bia, ana_peer);
        bia.flush_due_redials();
        bia.handle(NodeEvent::DialFailed {
            peer: Some(ana_peer),
            reason: "sem rota".into(),
        });
        assert_eq!(bia.redial[&ana_peer].state, RedialState::Exhausted);

        bia.send(ana_peer, "dois", None).unwrap();
        assert_eq!(bia.redial[&ana_peer].state, RedialState::Waiting, "rearmou");
        assert!(bia.wake_in().is_some());

        make_redial_due(&mut bia, ana_peer);
        bia.flush_due_redials();
        bia.handle(NodeEvent::DialFailed {
            peer: Some(ana_peer),
            reason: "sem rota".into(),
        });
        assert_eq!(bia.dial_log.len(), 2, "uma tentativa a mais, não um laço");
        assert_eq!(bia.redial[&ana_peer].state, RedialState::Exhausted);
    }

    /// P32,I2: o dial que não deu notícia (nem `DialFailed` nem conexão) conta como falha no prazo.
    #[tokio::test]
    async fn an_in_flight_dial_that_never_reports_counts_as_a_failure() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        bia.send(ana_peer, "oi", None).unwrap();
        make_redial_due(&mut bia, ana_peer);
        bia.flush_due_redials();
        assert_eq!(bia.redial[&ana_peer].state, RedialState::InFlight);
        let left = bia.redial[&ana_peer]
            .due
            .saturating_duration_since(Instant::now());
        assert!(left > Duration::from_secs(50) && left <= REDIAL_INFLIGHT);

        make_redial_due(&mut bia, ana_peer);
        bia.flush_due_redials();

        assert_eq!(bia.redial[&ana_peer].attempts, 2);
        assert_eq!(bia.redial[&ana_peer].state, RedialState::Waiting);
        assert_eq!(bia.dial_log.len(), 1, "contou a falha sem discar de novo");
    }

    /// P32,S2: a falha da rediscagem de fundo não vira um `ChatEvent::DialFailed` por tentativa.
    #[tokio::test]
    async fn a_background_redial_failure_is_not_surfaced_as_an_event() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        bia.send(ana_peer, "oi", None).unwrap();
        events(&mut bia);
        make_redial_due(&mut bia, ana_peer);
        bia.flush_due_redials();

        bia.handle(NodeEvent::DialFailed {
            peer: Some(ana_peer),
            reason: "sem rota".into(),
        });

        assert!(
            !events(&mut bia)
                .iter()
                .any(|e| matches!(e, ChatEvent::DialFailed { .. })),
            "a falha é do registro, não da aplicação"
        );
    }

    /// P32,S1: um `DialFailed` de outro dial (mDNS, `/dial` manual) sem rediscagem em andamento segue
    /// visível para a aplicação e não cria registro.
    #[tokio::test]
    async fn a_failure_of_another_dial_is_still_surfaced() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();

        bia.handle(NodeEvent::DialFailed {
            peer: Some(ana_peer),
            reason: "manual".into(),
        });

        assert!(
            events(&mut bia)
                .iter()
                .any(|e| matches!(e, ChatEvent::DialFailed { .. }))
        );
        assert!(bia.redial.is_empty());
    }

    /// P32:um dial que falha para peer sem conversa não cria rediscagem.
    #[tokio::test]
    async fn a_failed_dial_to_a_peer_without_a_conversation_schedules_nothing() {
        let (mut bia, _ana, _ana_peer) = bia_and_raw_ana();

        bia.handle(NodeEvent::DialFailed {
            peer: Some(PeerId::random()),
            reason: "sem rota".into(),
        });

        assert!(bia.redial.is_empty());
    }

    /// P32:conectar apaga a rediscagem agendada.
    #[tokio::test]
    async fn connecting_clears_the_redial() {
        let (mut bia, ana_peer) = bia_after_ana_dropped_with_a_queue();
        assert!(bia.redial.contains_key(&ana_peer));

        connect_peer(&mut bia, ana_peer);

        assert!(bia.redial.is_empty());
    }

    /// P32:prazo vencido com a fila já vazia (a mensagem saiu por outro caminho) não disca e esquece o registro.
    #[tokio::test]
    async fn a_due_redial_with_an_empty_queue_forgets_and_does_not_dial() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        let message = bia.send(ana_peer, "oi", None).unwrap();
        bia.outbox.remove(&ana_peer, &message).unwrap();
        make_redial_due(&mut bia, ana_peer);

        bia.flush_due_redials();

        assert!(bia.dial_log.is_empty());
        assert!(bia.redial.is_empty());
    }

    /// P32:prazo vencido com o peer já reconectado não disca.
    #[tokio::test]
    async fn a_due_redial_for_a_connected_peer_does_not_dial() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        bia.send(ana_peer, "oi", None).unwrap();
        make_redial_due(&mut bia, ana_peer);
        bia.connected.insert(ana_peer);

        bia.flush_due_redials();

        assert!(bia.dial_log.is_empty());
        assert!(bia.redial.is_empty());
    }

    /// P32,I2: sem nenhum endereço (nem informado nem do convite) não há o que discar: conta como tentativa e
    /// reagenda com espera maior, sem discar; ao aprender um endereço, a próxima tentativa disca.
    #[tokio::test]
    async fn a_due_redial_without_any_address_backs_off_and_dials_once_one_is_learned() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.send(ana_peer, "oi", None).unwrap();
        make_redial_due(&mut bia, ana_peer);

        bia.flush_due_redials();

        assert!(bia.dial_log.is_empty());
        assert_eq!(bia.redial[&ana_peer].attempts, 2);
        assert_eq!(bia.redial[&ana_peer].state, RedialState::Waiting);
        assert!(bia.wake_in().is_some(), "não ficou preso: tenta de novo");

        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        make_redial_due(&mut bia, ana_peer);
        bia.flush_due_redials();
        assert_eq!(bia.dial_log, [(ana_peer, vec![addr(GLOBAL_A)])]);
    }

    /// P32,I5: a outbox que não responde não é "fila vazia": o registro fica e reagenda.
    #[tokio::test]
    async fn an_unreadable_queue_keeps_the_redial_and_reschedules() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        bia.send(ana_peer, "oi", None).unwrap();
        make_redial_due(&mut bia, ana_peer);
        bia.outbox
            .run_sql("ALTER TABLE kin_outbox RENAME TO kin_outbox_quebrada");

        bia.flush_due_redials();

        assert_eq!(bia.redial[&ana_peer].attempts, 2);
        assert_eq!(bia.redial[&ana_peer].state, RedialState::Waiting);
        assert!(bia.dial_log.is_empty());
    }

    /// P32,I6: depois de `untrust`, a identidade desconfiada deixa de ser rediscada e o registro some.
    #[tokio::test]
    async fn a_distrusted_identity_is_no_longer_redialed() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        bia.send(ana_peer, "oi", None).unwrap();
        let ana = bia
            .sole_identity(&ana_peer)
            .expect("conversa com 1 identidade");
        bia.untrust(&ana).unwrap();
        make_redial_due(&mut bia, ana_peer);

        bia.flush_due_redials();

        assert!(bia.dial_log.is_empty());
        assert!(bia.redial.is_empty());
    }

    /// P32,I3: o `identify` que chega antes de a conversa existir (handshake esperado por `accept`) já vale.
    #[tokio::test]
    async fn addresses_are_learned_for_a_peer_whose_handshake_is_expected() {
        let (mut bia, _ana, _ana_peer) = bia_and_raw_ana();
        let invite = invite_for(&StandaloneIdentity::generate());
        let peer = invite.peer_id();
        bia.expected.insert(peer, invite.identity().clone());

        identify(&mut bia, peer, &[GLOBAL_A]);

        assert_eq!(bia.redial_targets(&peer), [addr(GLOBAL_A)]);
    }

    /// P32,I3: o convite aceito vira o último recurso de endereço da rediscagem.
    #[tokio::test]
    async fn an_accepted_invite_keeps_its_addresses_for_the_redial() {
        let (mut bia, _ana, _ana_peer) = bia_and_raw_ana();
        let invite = invite_for(&StandaloneIdentity::generate());

        bia.accept(&invite).unwrap();

        assert_eq!(
            bia.redial_targets(&invite.peer_id()),
            [addr("/ip4/203.0.113.1/tcp/1")]
        );
    }

    /// P32,I3: uma lista que o filtro esvazia (peer na LAN que só anuncia IP privado) não apaga a velha, e a
    /// informação nova vai na frente da velha.
    #[tokio::test]
    async fn learned_addresses_merge_and_an_emptied_list_keeps_the_old_ones() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        identify(&mut bia, ana_peer, &[GLOBAL_A]);

        identify(&mut bia, ana_peer, &["/ip4/192.168.0.5/tcp/4001"]);
        assert_eq!(bia.redial_targets(&ana_peer), [addr(GLOBAL_A)]);

        identify(&mut bia, ana_peer, &[GLOBAL_B]);
        assert_eq!(
            bia.redial_targets(&ana_peer),
            [addr(GLOBAL_B), addr(GLOBAL_A)]
        );
    }

    /// P32,I1: o filtro de endereço de contato não deixa passar IPv4 embutido em IPv6, NAT64, 6to4, Teredo,
    /// multicast nem as faixas reservadas do IPv4, que um contato usaria para me fazer discar a minha rede.
    #[test]
    fn contact_filter_rejects_embedded_and_special_addresses() {
        for bad in [
            "/ip6/::ffff:10.0.0.1/tcp/80",
            "/ip6/::ffff:127.0.0.1/tcp/80",
            "/ip6/::ffff:169.254.169.254/tcp/80",
            "/ip6/::10.0.0.1/tcp/80",
            "/ip6/64:ff9b::a00:1/tcp/80",
            "/ip6/2002:a00:1::1/tcp/80",
            "/ip6/2001:0:4136:e378::1/tcp/80",
            "/ip6/fec0::1/tcp/80",
            "/ip6/ff02::1/tcp/80",
            "/ip4/224.0.0.1/tcp/80",
            "/ip4/240.0.0.1/tcp/80",
            "/ip4/0.1.2.3/tcp/80",
            "/ip4/192.0.0.8/tcp/80",
            "/ip4/198.18.0.1/tcp/80",
        ] {
            assert!(
                !dialable(&bad.parse::<Multiaddr>().unwrap(), false),
                "{bad}"
            );
        }
        for good in ["/ip6/2606:4700::1111/tcp/443", "/ip4/8.8.8.8/tcp/443"] {
            assert!(
                dialable(&good.parse::<Multiaddr>().unwrap(), false),
                "{good}"
            );
        }
    }

    /// P32,I4: de irmão vale quase tudo, mas nunca "qualquer endereço" nem multicast, e há teto de quantidade.
    #[test]
    fn sibling_filter_rejects_unspecified_and_multicast_and_is_bounded() {
        let ok = |s: &str| s.parse::<Multiaddr>().unwrap();
        assert!(!dialable(&ok("/ip4/0.0.0.0/tcp/1"), true));
        assert!(!dialable(&ok("/ip6/::/tcp/1"), true));
        assert!(!dialable(&ok("/ip4/224.0.0.1/tcp/1"), true));
        assert!(!dialable(&ok("/ip6/ff02::1/tcp/1"), true));
        let many: Vec<Multiaddr> = (1..=40)
            .map(|n| ok(&format!("/ip4/203.0.113.{n}/tcp/1")))
            .collect();
        assert_eq!(dial_addrs(&many, true).len(), SIBLING_DIAL_ADDRS);
    }

    /// P32:esquecer o peer (conversa caiu) apaga a rediscagem e os endereços dele.
    #[tokio::test]
    async fn dropping_a_peer_clears_its_redial_and_addresses() {
        let mut r = revoked_ana();
        r.bia.redial.insert(
            r.ana_peer,
            Redial {
                attempts: 1,
                due: Instant::now(),
                state: RedialState::Waiting,
            },
        );
        r.bia.peer_addrs.insert(r.ana_peer, vec![addr(GLOBAL_A)]);
        r.bia.invite_addrs.insert(r.ana_peer, vec![addr(GLOBAL_B)]);

        r.bia.on_mls(r.sibling, &r.wire).unwrap();
        assert!(!r.bia.is_ready(&r.ana_peer), "a conversa caiu");
        assert!(!r.bia.redial.contains_key(&r.ana_peer));
        assert!(!r.bia.peer_addrs.contains_key(&r.ana_peer));
        assert!(!r.bia.invite_addrs.contains_key(&r.ana_peer));
    }

    /// P32:o relógio acorda no prazo da rediscagem.
    #[tokio::test]
    async fn the_clock_wakes_for_the_redial_deadline() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.redial_base = Duration::from_secs(3600);
        bia.redial_cap = Duration::from_secs(86_400); // acima da base, senão o teto corta
        assert_eq!(bia.wake_in(), None);

        bia.send(ana_peer, "oi", None).unwrap();

        let wake = bia.wake_in().expect("com rediscagem agendada, acorda");
        assert!(wake <= Duration::from_secs(4320) && wake > Duration::from_secs(2800));
    }

    /// P32:o laço do `next_event` de fato acorda no prazo e rediscaa sozinho, sem tráfego algum.
    #[tokio::test]
    async fn next_event_redials_by_itself() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.redial_base = Duration::from_millis(10);
        identify(&mut bia, ana_peer, &[GLOBAL_A]);
        bia.send(ana_peer, "oi", None).unwrap();
        events(&mut bia); // o `Queued` do envio sairia já, sem o laço esperar o relógio

        let _ = tokio::time::timeout(Duration::from_millis(500), bia.next_event()).await;

        assert!(
            bia.dial_log.iter().any(|(peer, _)| *peer == ana_peer),
            "o Tick não rediscou"
        );
    }

    /// Caixa postal que só anota as listas de relays que o `Chat` lhe ensina.
    type Learned = std::sync::Arc<std::sync::Mutex<Vec<(PeerId, Vec<String>)>>>;

    struct Taught(Learned);

    impl Mailbox for Taught {
        fn attach(&mut self, _events: futures::channel::mpsc::UnboundedSender<MailboxEvent>) {}

        fn deposit(&mut self, _to: PeerId, _blob: Vec<u8>) -> DepositId {
            DepositId(0)
        }

        fn learn_relays(&mut self, peer: PeerId, relays: Vec<String>) {
            self.0.lock().unwrap().push((peer, relays));
        }
    }

    fn taught_bia() -> (Chat, Conversation, PeerId, Learned) {
        let (mut bia, ana, ana_peer) = bia_and_raw_ana();
        let taught = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        bia.set_mailbox(Taught(taught.clone()));
        (bia, ana, ana_peer, taught)
    }

    fn relay_list_wire(ana: &mut Conversation, relays: &[&str]) -> Vec<u8> {
        let relays = relays.iter().map(|s| (*s).to_string()).collect();
        ana.encrypt(&Payload::RelayList(relays).encode()).unwrap()
    }

    /// P23: uma lista válida do contato é guardada, ensinada à caixa postal e não vira mensagem.
    #[tokio::test]
    async fn a_valid_relay_list_is_stored_taught_and_not_shown_as_a_message() {
        let (mut bia, mut ana, ana_peer, taught) = taught_bia();
        let wire = relay_list_wire(&mut ana, &["wss://a.example", "wss://b.example"]);

        bia.on_mls(ana_peer, &wire).unwrap();

        let expected = vec!["wss://a.example".to_string(), "wss://b.example".to_string()];
        assert_eq!(*taught.lock().unwrap(), [(ana_peer, expected.clone())]);
        assert_eq!(
            bia.outbox.peer_relays(&ana_peer).unwrap(),
            Some(expected.clone())
        );
        assert_eq!(
            events(&mut bia),
            [ChatEvent::PeerRelays {
                peer: ana_peer,
                relays: expected
            }]
        );
    }

    /// P23: lista hostil (host local, itens demais, lixo) é recusada e nada muda.
    #[tokio::test]
    async fn a_hostile_relay_list_is_refused_and_changes_nothing() {
        for hostile in [
            vec!["ws://localhost:7777"],
            vec!["wss://127.0.0.1"],
            vec![
                "wss://a.example",
                "wss://b.example",
                "wss://c.example",
                "wss://d.example",
            ],
            vec![],
        ] {
            let (mut bia, mut ana, ana_peer, taught) = taught_bia();
            let wire = relay_list_wire(&mut ana, &hostile);

            assert!(bia.on_mls(ana_peer, &wire).is_err(), "aceitou {hostile:?}");

            assert!(taught.lock().unwrap().is_empty(), "ensinou {hostile:?}");
            assert_eq!(bia.outbox.peer_relays(&ana_peer).unwrap(), None);
        }
    }

    /// P23: uma lista inválida depois de uma válida não apaga a válida.
    #[tokio::test]
    async fn a_bad_list_after_a_good_one_keeps_the_good_one() {
        let (mut bia, mut ana, ana_peer, taught) = taught_bia();
        let good = relay_list_wire(&mut ana, &["wss://a.example"]);
        let bad = relay_list_wire(&mut ana, &["ws://localhost:7777"]);

        bia.on_mls(ana_peer, &good).unwrap();
        assert!(bia.on_mls(ana_peer, &bad).is_err());

        assert_eq!(taught.lock().unwrap().len(), 1);
        assert_eq!(
            bia.outbox.peer_relays(&ana_peer).unwrap(),
            Some(vec!["wss://a.example".to_string()])
        );
    }

    /// P23: a última lista válida vence a anterior.
    #[tokio::test]
    async fn the_latest_valid_list_replaces_the_previous_one() {
        let (mut bia, mut ana, ana_peer, taught) = taught_bia();
        let first = relay_list_wire(&mut ana, &["wss://velho.example"]);
        let second = relay_list_wire(&mut ana, &["wss://novo.example"]);

        bia.on_mls(ana_peer, &first).unwrap();
        bia.on_mls(ana_peer, &second).unwrap();

        assert_eq!(
            taught.lock().unwrap().last().unwrap().1,
            ["wss://novo.example"]
        );
        assert_eq!(
            bia.outbox.peer_relays(&ana_peer).unwrap(),
            Some(vec!["wss://novo.example".to_string()])
        );
    }

    /// P23: o payload de controle truncado é malformado, não pânico.
    #[tokio::test]
    async fn a_truncated_relay_list_payload_is_malformed() {
        let (mut bia, mut ana, ana_peer, taught) = taught_bia();
        let mut plaintext = Payload::RelayList(vec!["wss://a.example".into()]).encode();
        plaintext.truncate(plaintext.len() - 3);
        let wire = ana.encrypt(&plaintext).unwrap();

        let err = bia.on_mls(ana_peer, &wire).unwrap_err();

        assert!(matches!(err, Error::Malformed), "{err:?}");
        assert!(taught.lock().unwrap().is_empty());
    }

    /// Adiciona a `bia` um device novo de `identity` (conversa pronta) e devolve o Peer ID dele e a
    /// ponta crua da conversa, para ler o que a Bia cifrar.
    fn add_device_of(bia: &mut Chat, identity: &StandaloneIdentity) -> (PeerId, Conversation) {
        let (peer, raw, _) = add_device_with_invite(bia, identity);
        (peer, raw)
    }

    /// Como [`add_device_of`], e devolve também o convite que esse device anunciaria de si (com o
    /// certificado real e um endereço global de teste).
    fn add_device_with_invite(
        bia: &mut Chat,
        identity: &StandaloneIdentity,
    ) -> (PeerId, Conversation, Invite) {
        let device_key = DeviceKey::generate();
        let device = CryptoDevice::new(
            identity,
            &device_key.public_key(),
            &device_key.nostr_public_key(),
            1,
        )
        .unwrap();
        let mut raw = Conversation::create(&device).unwrap();
        let invite = raw
            .invite(&bia.device.key_package().unwrap(), &Expected::default())
            .unwrap();
        let joined =
            Conversation::join(&bia.device, &invite.welcome, &Expected::default()).unwrap();
        bia.conversations.insert(device_key.peer_id(), joined);
        let own_invite = Invite::new(
            device.certificate().clone(),
            vec!["/ip4/203.0.113.1/tcp/1".parse().unwrap()],
        );
        (device_key.peer_id(), raw, own_invite)
    }

    fn message_of(raw: &mut Conversation, blob: &[u8]) -> Message {
        match raw.decrypt(&mls_of(blob)).unwrap() {
            Decrypted::Message { plaintext, .. } => Message::decode(&plaintext).unwrap(),
            other => panic!("esperava mensagem, veio {other:?}"),
        }
    }

    /// Bia conversa com 2 devices da Ana e 1 da Cléo; devolve também a identidade da Ana.
    type RawDevice = (PeerId, Conversation);

    fn bia_with_two_ana_devices_and_one_cleo() -> (Chat, IdentityId, [RawDevice; 2], RawDevice) {
        let (ana, cleo) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        let mut bia = chat(&StandaloneIdentity::generate(), &DeviceKey::generate());
        let a1 = add_device_of(&mut bia, &ana);
        let a2 = add_device_of(&mut bia, &ana);
        let c1 = add_device_of(&mut bia, &cleo);
        (bia, ana.id(), [a1, a2], c1)
    }

    /// 3.5: os devices de uma identidade saem das conversas (certificados), sem incluir os de outra.
    #[tokio::test]
    async fn devices_of_lists_exactly_the_devices_of_that_identity() {
        let (bia, ana, [(a1, _), (a2, _)], (c1, _)) = bia_with_two_ana_devices_and_one_cleo();

        let mut devices = bia.devices_of(&ana);
        devices.sort();

        let mut expected = vec![a1, a2];
        expected.sort();
        assert_eq!(devices, expected);
        assert!(!devices.contains(&c1));
    }

    /// 3.5: identidade sem nenhuma conversa não tem devices.
    #[tokio::test]
    async fn devices_of_an_unknown_identity_is_empty() {
        let (bia, _, _, _) = bia_with_two_ana_devices_and_one_cleo();

        assert!(
            bia.devices_of(&StandaloneIdentity::generate().id())
                .is_empty()
        );
    }

    /// 3.5: `send_to` põe a MESMA mensagem na fila de cada device da identidade e de mais ninguém.
    #[tokio::test]
    async fn send_to_queues_the_same_message_for_every_device_of_the_identity() {
        let (mut bia, ana, [(a1, _), (a2, _)], (c1, _)) = bia_with_two_ana_devices_and_one_cleo();

        let id = bia.send_to(&ana, "oi, ana", None).unwrap();

        assert_eq!(
            (bia.queued(&a1), bia.queued(&a2), bia.queued(&c1)),
            (1, 1, 0)
        );
        for device in [a1, a2] {
            let entries = bia.outbox.entries(&device).unwrap();
            assert_eq!(entries[0].id, id, "mesmo MessageId nas duas cópias");
            assert_eq!(entries[0].text, "oi, ana");
        }
    }

    /// 3.5: com os devices offline e uma caixa postal, cada um recebe o próprio depósito, cifrado
    /// na conversa dele, com o mesmo `MessageId` e o mesmo `parent`.
    #[tokio::test]
    async fn send_to_deposits_one_copy_per_device_with_the_same_id_and_parent() {
        let (mut bia, ana, [(a1, mut raw1), (a2, mut raw2)], (c1, _)) =
            bia_with_two_ana_devices_and_one_cleo();
        let relay = MemoryRelay::default();
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));
        let parent = Message::new("pai".into(), None).id;

        let id = bia.send_to(&ana, "oi", Some(parent)).unwrap();
        for _ in 0..2 {
            until(&mut bia, |e| {
                matches!(e, ChatEvent::Deposited { .. }).then_some(())
            })
            .await
            .expect("depósito confirmado");
        }

        let deposits = relay.deposits();
        let mut targets: Vec<_> = deposits.iter().map(|d| d.to).collect();
        targets.sort();
        let mut expected = vec![a1, a2];
        expected.sort();
        assert_eq!(
            targets, expected,
            "um depósito por device, nenhum para a Cléo"
        );
        assert!(!targets.contains(&c1));
        for deposit in &deposits {
            let raw = if deposit.to == a1 {
                &mut raw1
            } else {
                &mut raw2
            };
            let message = message_of(raw, &deposit.blob);
            assert_eq!((message.id, message.parent), (id, Some(parent)));
        }
    }

    /// 3.5: sem conversa com a identidade, `send_to` falha e não enfileira nada.
    #[tokio::test]
    async fn send_to_an_identity_without_conversations_is_an_error() {
        let (mut bia, _, [(a1, _), _], _) = bia_with_two_ana_devices_and_one_cleo();
        let stranger = StandaloneIdentity::generate().id();

        let err = bia.send_to(&stranger, "oi", None).unwrap_err();

        assert!(matches!(err, Error::UnknownIdentity(_)), "{err:?}");
        assert_eq!(bia.queued(&a1), 0);
        assert!(events(&mut bia).is_empty());
    }

    /// Bia com um device irmão (outro device da própria identidade) e um device da Ana.
    struct Household {
        bia: Chat,
        bia_identity: IdentityId,
        ana: IdentityId,
        sibling: RawDevice,
        ana_device: RawDevice,
        bia_key: StandaloneIdentity,
        ana_key: StandaloneIdentity,
    }

    fn household() -> Household {
        let (bia_key, ana_key) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        let mut bia = chat(&bia_key, &DeviceKey::generate());
        let sibling = add_device_of(&mut bia, &bia_key);
        let ana_device = add_device_of(&mut bia, &ana_key);
        Household {
            bia,
            bia_identity: bia_key.id(),
            ana: ana_key.id(),
            sibling,
            ana_device,
            bia_key,
            ana_key,
        }
    }

    /// Convite de um device novo de `identity` (certificado válido) com um endereço de teste.
    fn invite_for(identity: &StandaloneIdentity) -> Invite {
        let device = DeviceKey::generate();
        let certificate = identity
            .authorize_device(
                &DeviceKey::generate().public_key(),
                &device.public_key(),
                &device.nostr_public_key(),
                1,
            )
            .unwrap();
        Invite::new(
            certificate,
            vec!["/ip4/203.0.113.1/tcp/1".parse::<Multiaddr>().unwrap()],
        )
    }

    /// Certificado válido de um device novo de `identity`.
    fn certificate_for(identity: &StandaloneIdentity) -> DeviceCertificate {
        let device = DeviceKey::generate();
        identity
            .authorize_device(
                &DeviceKey::generate().public_key(),
                &device.public_key(),
                &device.nostr_public_key(),
                1,
            )
            .unwrap()
    }

    /// P16: sem `lost_state`, um `KeyPackage` de quem já tem conversa continua ignorado (nenhum reset não
    /// solicitado); com ele, o `KeyPackage` a substitui, mas **só** com a identidade da conversa antiga, e
    /// uma recusa deixa a conversa antiga intacta.
    #[tokio::test]
    async fn a_peer_that_lost_its_state_replaces_the_conversation_only_with_the_same_identity() {
        let (id_a, id_t, id_x) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        let (dev_a, dev_b) = loop {
            let keys = (DeviceKey::generate(), DeviceKey::generate());
            if keys.0.peer_id() < keys.1.peer_id() {
                break keys;
            }
        };
        let b = dev_b.peer_id();
        let key_package_of = |identity: &StandaloneIdentity| {
            CryptoDevice::new(identity, &dev_b.public_key(), &dev_b.nostr_public_key(), 1)
                .unwrap()
                .key_package()
                .unwrap()
        };
        let mut ana = chat(&id_a, &dev_a);
        ana.on_key_package(b, &key_package_of(&id_t)).unwrap();
        assert!(ana.is_ready(&b));
        let old = ana.conversations.get(&b).unwrap().id();
        events(&mut ana);

        // Sem `lost_state`: ignorado, a conversa é a mesma e ninguém é avisado.
        ana.on_key_package(b, &key_package_of(&id_t)).unwrap();
        assert!(ana.conversations.get(&b).unwrap().id() == old);
        assert!(events(&mut ana).is_empty());

        // Com `lost_state`, identidade diferente: recusada, e a conversa antiga fica.
        ana.lost_state.insert(b);
        let err = ana.on_key_package(b, &key_package_of(&id_x)).unwrap_err();
        assert!(
            matches!(err, Error::Crypto(CryptoError::UnexpectedIdentity)),
            "{err:?}"
        );
        assert!(ana.conversations.get(&b).unwrap().id() == old);
        assert!(!events(&mut ana).iter().any(is_reset));
        assert!(!ana.lost_state.contains(&b), "a recusa consome a suspeita");

        // Com `lost_state`, a mesma identidade: troca, avisa antes do `ConversationReady`.
        ana.lost_state.insert(b);
        ana.on_key_package(b, &key_package_of(&id_t)).unwrap();
        assert!(ana.conversations.get(&b).unwrap().id() != old);
        let seen = events(&mut ana);
        let reset = seen.iter().position(is_reset).expect("sem ConversationReset");
        let ready = seen
            .iter()
            .position(|e| matches!(e, ChatEvent::ConversationReady { .. }))
            .expect("sem ConversationReady");
        assert!(reset < ready, "{seen:?}");
        assert!(!ana.lost_state.contains(&b), "consumido pela troca");
    }

    /// P16 (auditoria, médio 2): um reset por conexão. Depois da troca, outro `Hello` "não tenho conversa" na
    /// mesma conexão não marca `lost_state` de novo: sem o limite, um contato repetiria o ciclo (KeyPackage,
    /// invite, remember, delete, eventos) sem parar.
    #[tokio::test]
    async fn a_second_reset_in_the_same_connection_is_not_started() {
        let (id_a, id_t) = (StandaloneIdentity::generate(), StandaloneIdentity::generate());
        let (dev_a, dev_b) = loop {
            let keys = (DeviceKey::generate(), DeviceKey::generate());
            if keys.0.peer_id() < keys.1.peer_id() {
                break keys;
            }
        };
        let b = dev_b.peer_id();
        let key_package = || {
            CryptoDevice::new(&id_t, &dev_b.public_key(), &dev_b.nostr_public_key(), 1)
                .unwrap()
                .key_package()
                .unwrap()
        };
        let mut ana = chat(&id_a, &dev_a);
        ana.on_key_package(b, &key_package()).unwrap();
        ana.lost_state.insert(b);
        ana.on_key_package(b, &key_package()).unwrap();
        assert!(events(&mut ana).iter().any(is_reset), "o 1º reset aconteceu");

        // `hello_pending` de volta (P35: sem ele o `Hello` nem seria candidato): só o `reset_done` barra.
        ana.hello_pending.insert(b);
        ana.on_hello(b, None, None, Some(Presence::of(false)))
            .unwrap();
        assert!(!ana.lost_state.contains(&b), "o 2º reset na mesma conexão");
    }

    /// P35: um `Hello` "não tenho conversa" montado antes de a conversa existir pode chegar depois dela
    /// (substreams sem ordem). Numa conversa formada **nesta** conexão (sem `hello_pending`) ele é velho, não
    /// prova perda de banco: marcar `lost_state` travaria a fila do peer até a conexão cair.
    #[tokio::test]
    async fn a_late_hello_without_conversation_does_not_mark_a_new_conversation() {
        let (id_a, id_t) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        let (dev_a, dev_b) = loop {
            let keys = (DeviceKey::generate(), DeviceKey::generate());
            if keys.0.peer_id() < keys.1.peer_id() {
                break keys;
            }
        };
        let b = dev_b.peer_id();
        let key_package =
            CryptoDevice::new(&id_t, &dev_b.public_key(), &dev_b.nostr_public_key(), 1)
                .unwrap()
                .key_package()
                .unwrap();
        let mut ana = chat(&id_a, &dev_a);
        ana.on_key_package(b, &key_package).unwrap();
        assert!(ana.is_ready(&b));
        assert!(!ana.hello_pending.contains(&b), "conversa criada agora");

        ana.on_hello(b, None, None, Some(Presence::of(false)))
            .unwrap();
        assert!(!ana.lost_state.contains(&b), "o Hello era velho");
    }

    /// P35 (guarda do P16): numa conversa que já existia quando a conexão abriu (`hello_pending`), o `Hello`
    /// "não tenho conversa" continua marcando `lost_state`.
    #[tokio::test]
    async fn a_hello_without_conversation_still_marks_a_loaded_conversation() {
        let (id_a, id_t) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        let (dev_a, dev_b) = loop {
            let keys = (DeviceKey::generate(), DeviceKey::generate());
            if keys.0.peer_id() < keys.1.peer_id() {
                break keys;
            }
        };
        let b = dev_b.peer_id();
        let key_package =
            CryptoDevice::new(&id_t, &dev_b.public_key(), &dev_b.nostr_public_key(), 1)
                .unwrap()
                .key_package()
                .unwrap();
        let mut ana = chat(&id_a, &dev_a);
        ana.on_key_package(b, &key_package).unwrap();
        // Como o `PeerConnected` de uma conversa carregada do banco.
        ana.hello_pending.insert(b);

        ana.on_hello(b, None, None, Some(Presence::of(false)))
            .unwrap();
        assert!(ana.lost_state.contains(&b), "o peer perdeu o banco");
        assert!(!ana.hello_pending.contains(&b));
    }

    const ID_OLD: [u8; 16] = [1; 16];
    const ID_NEW: [u8; 16] = [2; 16];

    fn hello_of(has_conversation: bool, state_id: Option<[u8; 16]>) -> Option<Presence> {
        Some(Presence {
            has_conversation,
            state_id,
        })
    }

    /// `ana` (de menor Peer ID que `b`) com uma conversa 1:1 formada com `b`, **sem** `hello_pending` (como numa
    /// 2ª conexão). Se `first` é `Some`, o `Hello` de `b` com esse `state_id` chegou **antes** da conversa
    /// (é assim que o `a` descobre a referência: o `b` ainda não tinha conversa quando o `Hello` saiu).
    fn ana_with_conversation(first: Option<[u8; 16]>) -> (Chat, PeerId) {
        let (id_a, id_t) = (StandaloneIdentity::generate(), StandaloneIdentity::generate());
        let (dev_a, dev_b) = loop {
            let keys = (DeviceKey::generate(), DeviceKey::generate());
            if keys.0.peer_id() < keys.1.peer_id() {
                break keys;
            }
        };
        let b = dev_b.peer_id();
        let key_package =
            CryptoDevice::new(&id_t, &dev_b.public_key(), &dev_b.nostr_public_key(), 1)
                .unwrap()
                .key_package()
                .unwrap();
        let mut ana = chat(&id_a, &dev_a);
        if first.is_some() {
            ana.on_hello(b, None, None, hello_of(false, first)).unwrap();
        }
        ana.on_key_package(b, &key_package).unwrap();
        assert!(ana.is_ready(&b));
        assert!(!ana.hello_pending.contains(&b), "conversa criada agora");
        (ana, b)
    }

    /// P36 (ADR 0043): o `Hello` "não tenho conversa" com um `state_id` **diferente** do que o `a` gravou ao
    /// formar a conversa prova que o `b` perdeu o banco, **em qualquer conexão** (sem `hello_pending`): é o
    /// caso da conexão antiga ainda de pé.
    #[tokio::test]
    async fn a_hello_with_a_different_state_id_marks_lost_state_without_hello_pending() {
        let (mut ana, b) = ana_with_conversation(Some(ID_OLD));
        ana.on_hello(b, None, None, hello_of(false, Some(ID_NEW)))
            .unwrap();
        assert!(ana.lost_state.contains(&b), "o b trocou de banco");
    }

    /// P36: quem tem o **menor** Peer ID não responde com `KeyPackage`, e na 2ª conexão ele não mandou `Hello`
    /// (só sai em `PeerConnected`): sem um `Hello` dele, o `b` (maior) nunca manda o `KeyPackage`. Ao marcar
    /// `lost_state` o `a` manda o `Hello` (era a lacuna **antiga** do P36, a de menor Peer ID).
    #[tokio::test]
    async fn the_lower_peer_sends_its_hello_when_it_marks_lost_state() {
        let (mut ana, b) = ana_with_conversation(Some(ID_OLD));
        ana.handshake.clear();
        ana.on_hello(b, None, None, hello_of(false, Some(ID_NEW)))
            .unwrap();
        assert!(
            ana.handshake
                .values()
                .any(|e| matches!(e, Envelope::Hello(..))),
            "o a não mandou o Hello"
        );
    }

    /// P35 (guarda de não-regressão): o `Hello` velho (montado antes de a conversa existir, mesma instância)
    /// traz o **mesmo** `state_id` que o `a` gravou: continua ignorado, mesmo sem `hello_pending`.
    #[tokio::test]
    async fn a_late_hello_with_the_same_state_id_does_not_mark_lost_state() {
        let (mut ana, b) = ana_with_conversation(Some(ID_OLD));
        ana.on_hello(b, None, None, hello_of(false, Some(ID_OLD)))
            .unwrap();
        assert!(!ana.lost_state.contains(&b), "mesma instância, Hello velho");
    }

    /// P36: sem referência gravada (conversa de antes do P36), o `Hello` com `state_id` **grava** o id e não
    /// marca; o seguinte, com o mesmo id, também não marca. Não confia no 1º `Hello` para decidir nada.
    #[tokio::test]
    async fn without_a_recorded_state_id_the_first_one_is_recorded_and_marks_nothing() {
        let (mut ana, b) = ana_with_conversation(None);
        assert_eq!(ana.outbox.peer_state(&b).unwrap(), None);
        ana.on_hello(b, None, None, hello_of(false, Some(ID_OLD)))
            .unwrap();
        assert!(!ana.lost_state.contains(&b));
        assert_eq!(ana.outbox.peer_state(&b).unwrap(), Some(ID_OLD));
        ana.on_hello(b, None, None, hello_of(false, Some(ID_OLD)))
            .unwrap();
        assert!(!ana.lost_state.contains(&b));
    }

    /// P36: a referência é gravada em disco quando a conversa nasce (`ready`), não só na memória.
    #[tokio::test]
    async fn the_state_id_seen_before_the_conversation_is_persisted_when_it_is_formed() {
        let (ana, b) = ana_with_conversation(Some(ID_OLD));
        assert_eq!(ana.outbox.peer_state(&b).unwrap(), Some(ID_OLD));
    }

    /// P36: id diferente **com** "tenho conversa" não é perda de banco (um banco novo não tem conversa), e um
    /// `Hello` no formato antigo (sem `state_id`) fora do `hello_pending` segue ignorado como no P35.
    #[tokio::test]
    async fn only_a_hello_without_conversation_and_with_a_different_id_marks_lost_state() {
        let (mut ana, b) = ana_with_conversation(Some(ID_OLD));
        ana.on_hello(b, None, None, hello_of(true, Some(ID_NEW)))
            .unwrap();
        assert!(!ana.lost_state.contains(&b), "tem conversa: não perdeu");
        ana.on_hello(b, None, None, hello_of(false, None)).unwrap();
        assert!(!ana.lost_state.contains(&b), "formato antigo, sem hello_pending");
        ana.on_hello(b, None, None, None).unwrap();
        assert!(!ana.lost_state.contains(&b), "tags 0 e 4 nunca disparam");
    }

    /// P36 (auditoria prevista): o limite de um reset por conexão vale também para o caminho do `state_id`: um
    /// contato que repete `Hello` com ids novos não refaz a conversa a cada um.
    #[tokio::test]
    async fn a_new_state_id_does_not_start_a_second_reset_in_the_same_connection() {
        let (mut ana, b) = ana_with_conversation(Some(ID_OLD));
        ana.reset_done.insert(b);
        ana.on_hello(b, None, None, hello_of(false, Some(ID_NEW)))
            .unwrap();
        assert!(!ana.lost_state.contains(&b));
    }

    fn is_reset(event: &ChatEvent) -> bool {
        matches!(event, ChatEvent::ConversationReset { .. })
    }

    fn not_admitted(events: &[ChatEvent], peer: PeerId) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, ChatEvent::NotAdmitted { peer: p } if *p == peer))
            .count()
    }

    /// P34: estranho sem token com o portão ligado gera um aviso, uma vez por conexão; ao reconectar avisa de
    /// novo. Token que não vive (nunca emitido) também.
    #[tokio::test]
    async fn a_refused_stranger_is_announced_once_per_connection() {
        let mut ana = chat(&StandaloneIdentity::generate(), &DeviceKey::generate());
        ana.set_require_invite(true);
        let stranger = PeerId::random();

        ana.on_hello(stranger, None, None, None).unwrap();
        ana.on_hello(stranger, None, None, None).unwrap();
        assert_eq!(
            not_admitted(&events(&mut ana), stranger),
            1,
            "uma por conexão"
        );

        ana.handle(NodeEvent::PeerDisconnected(stranger));
        ana.on_hello(stranger, Some([7; 16]), None, None).unwrap();
        assert_eq!(
            not_admitted(&events(&mut ana), stranger),
            1,
            "token nunca emitido, nova conexão"
        );
    }

    /// P34: quem o portão admite (aberto, `dial` com Peer ID) não gera aviso.
    #[tokio::test]
    async fn an_admitted_peer_is_not_announced() {
        let mut open = chat(&StandaloneIdentity::generate(), &DeviceKey::generate());
        let peer = PeerId::random();
        open.on_hello(peer, None, None, None).unwrap();
        assert_eq!(not_admitted(&events(&mut open), peer), 0, "portão aberto");

        let mut closed = chat(&StandaloneIdentity::generate(), &DeviceKey::generate());
        closed.set_require_invite(true);
        closed.dialed.insert(peer);
        closed.on_hello(peer, None, None, None).unwrap();
        assert_eq!(
            not_admitted(&events(&mut closed), peer),
            0,
            "dial com Peer ID"
        );
    }

    /// P34: o teto de peers avisados vale; acima dele, recusa sem aviso (como `hello_identity`).
    #[tokio::test]
    async fn the_announced_set_is_capped() {
        let mut ana = chat(&StandaloneIdentity::generate(), &DeviceKey::generate());
        ana.set_require_invite(true);
        let strangers: Vec<PeerId> = (0..MAX_NOT_ADMITTED_NOTIFIED + 10)
            .map(|_| PeerId::random())
            .collect();
        for peer in &strangers {
            ana.on_hello(*peer, None, None, None).unwrap();
        }
        let total = events(&mut ana)
            .iter()
            .filter(|e| matches!(e, ChatEvent::NotAdmitted { .. }))
            .count();
        assert_eq!(total, MAX_NOT_ADMITTED_NOTIFIED);

        // Desconectar um peer avisado libera a vaga: o próximo recusado volta a ser avisado.
        ana.handle(NodeEvent::PeerDisconnected(strangers[0]));
        let late = strangers[MAX_NOT_ADMITTED_NOTIFIED + 5];
        ana.on_hello(late, None, None, None).unwrap();
        assert_eq!(not_admitted(&events(&mut ana), late), 1, "vaga liberada");
    }

    /// P18 PR 3: o certificado do `Hello` só vale para o Peer ID que ele certifica. Repetir o de uma
    /// identidade confiável numa conexão de outro peer é recusado e não grava identidade nenhuma.
    #[tokio::test]
    async fn a_hello_certificate_of_another_peer_is_refused() {
        let Household {
            mut bia, ana_key, ..
        } = household();
        let certificate = certificate_for(&ana_key);
        let owner = certificate.network_peer_id();

        let forged = bia.on_hello(PeerId::random(), None, Some(certificate.to_bytes()), None);
        assert!(matches!(forged, Err(Error::Malformed)), "{forged:?}");
        assert!(bia.hello_identity.is_empty());

        bia.on_hello(owner, None, Some(certificate.to_bytes()), None)
            .unwrap();
        assert_eq!(bia.hello_identity.get(&owner), Some(&ana_key.id()));
    }

    /// P18 PR 3: certificado de bytes inválidos é recusado, sem pânico.
    #[tokio::test]
    async fn a_hello_certificate_with_garbage_bytes_is_refused() {
        let Household { mut bia, .. } = household();
        for bytes in [vec![], vec![0; 8], vec![0xFF; 4096]] {
            let peer = PeerId::random();
            let result = bia.on_hello(peer, None, Some(bytes), None);
            assert!(matches!(result, Err(Error::Malformed)), "{result:?}");
            assert!(bia.hello_identity.is_empty());
        }
    }

    /// Certificado de `identity` para o device `device` (o Peer ID de rede é o dele).
    fn certificate_of(identity: &StandaloneIdentity, device: &DeviceKey) -> DeviceCertificate {
        identity
            .authorize_device(
                &DeviceKey::generate().public_key(),
                &device.public_key(),
                &device.nostr_public_key(),
                1,
            )
            .unwrap()
    }

    /// Uma Bia com portão e aprovação ligados e `n` links vivos; devolve os tokens.
    fn bia_with_links(n: usize) -> (Chat, Vec<[u8; 16]>) {
        let mut bia = household().bia;
        bia.set_require_invite(true);
        bia.set_require_approval(true);
        let tokens = (0..n)
            .map(|_| {
                let link = bia.invite_link(vec![], Duration::from_secs(3600)).unwrap();
                Invite::from_link(&link).unwrap().token().unwrap()
            })
            .collect();
        (bia, tokens)
    }

    /// P18 (d, auditoria, baixo 1): o pedido guarda a identidade que o dono viu. Um 2º `Hello` do mesmo Peer
    /// ID com outro certificado válido não a troca (senão o dono aprovaria X e a conversa nasceria com Y).
    #[tokio::test]
    async fn a_pending_request_keeps_the_identity_it_showed() {
        let (mut bia, tokens) = bia_with_links(1);
        let (x, y) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        let device = DeviceKey::generate();
        let peer = device.peer_id();
        let (cert_x, cert_y) = (certificate_of(&x, &device), certificate_of(&y, &device));

        bia.on_hello(peer, Some(tokens[0]), Some(cert_x.to_bytes()), None)
            .unwrap();
        bia.on_hello(peer, Some(tokens[0]), Some(cert_y.to_bytes()), None)
            .unwrap();

        assert_eq!(bia.pending_approvals(), vec![(peer, x.id())]);
        assert_eq!(bia.hello_identity.get(&peer), Some(&x.id()));
        let asked = events(&mut bia)
            .into_iter()
            .filter(|e| matches!(e, ChatEvent::AdmissionRequested { .. }))
            .count();
        assert_eq!(asked, 1, "um pedido por conexão");
    }

    /// P18 (d, auditoria, baixo 2): a aprovação vale pelo token apresentado. Depois de `approve`, um novo
    /// `Hello` com outro token vivo (de outro convite) não troca o token do peer: o `ready` gastaria o
    /// errado e deixaria o aprovado vivo.
    #[tokio::test]
    async fn an_approved_peer_keeps_the_token_it_was_approved_with() {
        let (mut bia, tokens) = bia_with_links(2);
        let x = StandaloneIdentity::generate();
        let device = DeviceKey::generate();
        let peer = device.peer_id();
        let cert = certificate_of(&x, &device).to_bytes();

        bia.on_hello(peer, Some(tokens[0]), Some(cert.clone()), None)
            .unwrap();
        assert!(bia.approve(peer).unwrap());
        bia.on_hello(peer, Some(tokens[1]), Some(cert), None)
            .unwrap();

        assert_eq!(bia.admitted.get(&peer), Some(&tokens[0]));
    }

    /// P18 (d, auditoria, médio): revogar o convite leva junto os pedidos pendentes dele. Quem recebeu um
    /// link vazado e abriu várias conexões não deixa a fila cheia depois que o dono o revoga.
    #[tokio::test]
    async fn revoking_a_link_drops_its_pending_requests() {
        let (mut bia, tokens) = bia_with_links(2);
        let x = StandaloneIdentity::generate();
        let (leaker, other) = (DeviceKey::generate(), DeviceKey::generate());
        for (device, token) in [(&leaker, tokens[0]), (&other, tokens[1])] {
            let cert = certificate_of(&x, device).to_bytes();
            bia.on_hello(device.peer_id(), Some(token), Some(cert), None)
                .unwrap();
        }
        assert_eq!(bia.pending_approvals().len(), 2);

        let revoked = bia.invites().into_iter().find(|i| {
            let prefix = i.id.clone();
            bia.admitted
                .get(&leaker.peer_id())
                .is_some_and(|t| invite_id(t) == prefix)
        });
        let id = revoked.expect("convite do vazador").id;
        assert!(bia.revoke_invite(&id).unwrap());

        let left: Vec<PeerId> = bia.pending_approvals().iter().map(|(p, _)| *p).collect();
        assert_eq!(left, vec![other.peer_id()]);
        assert!(!bia.approve(leaker.peer_id()).unwrap());
    }

    /// P18 PR 3: com o portão ligado, a identidade autenticada só admite se for a minha ou uma aceita.
    #[tokio::test]
    async fn the_gate_admits_an_authenticated_identity_only_if_it_is_mine_or_trusted() {
        let Household {
            mut bia,
            ana_key,
            bia_key,
            ..
        } = household();
        bia.set_require_invite(true);
        let (ana_cert, own_cert) = (certificate_for(&ana_key), certificate_for(&bia_key));
        let (ana_peer, own_peer) = (ana_cert.network_peer_id(), own_cert.network_peer_id());
        bia.on_hello(ana_peer, None, Some(ana_cert.to_bytes()), None)
            .unwrap();
        bia.on_hello(own_peer, None, Some(own_cert.to_bytes()), None)
            .unwrap();

        assert!(!bia.admits(&ana_peer).unwrap(), "desconhecida sem token");
        assert!(bia.admits(&own_peer).unwrap(), "minha identidade");
        bia.trust(ana_key.id()).unwrap();
        assert!(bia.admits(&ana_peer).unwrap(), "aceita por trust");
        bia.untrust(&ana_key.id()).unwrap();
        assert!(!bia.admits(&ana_peer).unwrap(), "untrust fecha de novo");
    }

    fn device_list_wire(raw: &mut Conversation, invites: Vec<Invite>) -> Vec<u8> {
        raw.encrypt(&Payload::DeviceList(invites).encode()).unwrap()
    }

    /// 3.5 etapa 5: a lista de devices de um contato faz este device discar o que ele ainda não
    /// conhece, exigindo no handshake a identidade do contato.
    #[tokio::test]
    async fn a_contacts_device_list_makes_me_dial_her_unknown_device_expecting_her_identity() {
        let Household {
            mut bia,
            ana,
            ana_key,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let second = invite_for(&ana_key);
        bia.trust(ana.clone()).unwrap();

        let wire = device_list_wire(&mut ana_raw, vec![second.clone()]);
        bia.on_mls(ana_peer, &wire).unwrap();

        assert_eq!(bia.expected.get(&second.peer_id()), Some(&ana));
        assert!(
            bia.household.is_empty(),
            "lista de contato não vira família"
        );
    }

    /// 3.5 etapa 5 (segurança): um item de OUTRA identidade recusa a lista inteira, e nada é discado.
    /// Senão um contato me faria discar endereços de terceiros.
    #[tokio::test]
    async fn a_device_list_with_another_identitys_device_is_refused_whole() {
        let Household {
            mut bia,
            ana_key,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let (genuine, stranger) = (
            invite_for(&ana_key),
            invite_for(&StandaloneIdentity::generate()),
        );
        bia.trust(ana_key.id()).unwrap();

        let wire = device_list_wire(&mut ana_raw, vec![genuine.clone(), stranger.clone()]);
        let err = bia.on_mls(ana_peer, &wire).unwrap_err();

        assert!(matches!(err, Error::InvalidDeviceList(_)), "{err:?}");
        assert!(bia.expected.is_empty(), "nem o item legítimo é discado");
    }

    /// 3.5 etapa 5: a lista de um irmão é guardada (para eu repassá-la aos contatos), e ele também é
    /// discado se for desconhecido; meu próprio device na lista é ignorado.
    #[tokio::test]
    async fn a_siblings_device_list_is_kept_as_my_household_and_skips_myself() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut sibling_raw),
            ..
        } = household();
        let third = invite_for(&bia_key);
        let myself = bia.invite(vec![]);

        let wire = device_list_wire(&mut sibling_raw, vec![third.clone(), myself]);
        bia.on_mls(sibling, &wire).unwrap();

        assert_eq!(bia.household.get(&third.peer_id()), Some(&third));
        assert_eq!(bia.household.len(), 1, "eu não entro na minha família");
        assert!(bia.expected.contains_key(&third.peer_id()));
        assert!(!bia.expected.contains_key(&bia.peer_id()));
    }

    /// 3.5 etapa 5 (auditoria I3): um item da lista não sobrescreve a identidade que EU já exigi para
    /// aquele Peer ID (ex.: por `accept`). Senão uma identidade poderia reivindicar o Peer ID de um
    /// device alheio e travar o handshake legítimo dele.
    #[tokio::test]
    async fn a_device_list_never_overwrites_an_identity_i_already_required() {
        let Household {
            mut bia,
            ana_key,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let claimed = invite_for(&ana_key);
        let cleo = StandaloneIdentity::generate().id();
        bia.trust(ana_key.id()).unwrap();
        bia.expected.insert(claimed.peer_id(), cleo.clone());

        let wire = device_list_wire(&mut ana_raw, vec![claimed.clone()]);
        bia.on_mls(ana_peer, &wire).unwrap();

        assert_eq!(bia.expected.get(&claimed.peer_id()), Some(&cleo));
    }

    /// 3.5 etapa 5 (auditoria I1): de um contato (não irmão) só a primeira lista por conexão dispara
    /// discagens; listas seguintes são ignoradas até a conexão cair. Limita a amplificação.
    #[tokio::test]
    async fn only_the_first_device_list_per_connection_from_a_contact_is_acted_on() {
        let Household {
            mut bia,
            ana_key,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let (first, second, third) = (
            invite_for(&ana_key),
            invite_for(&ana_key),
            invite_for(&ana_key),
        );
        bia.trust(ana_key.id()).unwrap();

        for invite in [&first, &second] {
            let wire = device_list_wire(&mut ana_raw, vec![invite.clone()]);
            bia.on_mls(ana_peer, &wire).unwrap();
        }
        assert!(bia.expected.contains_key(&first.peer_id()));
        assert!(!bia.expected.contains_key(&second.peer_id()), "2ª ignorada");

        bia.handle(NodeEvent::PeerDisconnected(ana_peer));
        let wire = device_list_wire(&mut ana_raw, vec![third.clone()]);
        bia.on_mls(ana_peer, &wire).unwrap();
        assert!(bia.expected.contains_key(&third.peer_id()), "nova conexão");
    }

    /// 3.5 etapa 5 (auditoria I1): irmão não é limitado (é a minha própria identidade).
    #[tokio::test]
    async fn a_sibling_may_send_device_lists_repeatedly() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut sibling_raw),
            ..
        } = household();
        let (first, second) = (invite_for(&bia_key), invite_for(&bia_key));

        for invite in [&first, &second] {
            let wire = device_list_wire(&mut sibling_raw, vec![invite.clone()]);
            bia.on_mls(sibling, &wire).unwrap();
        }

        assert!(bia.household.contains_key(&first.peer_id()));
        assert!(bia.household.contains_key(&second.peer_id()));
    }

    fn feed_list(bia: &mut Chat, peer: PeerId, raw: &mut Conversation, invites: &[Invite]) {
        let wire = device_list_wire(raw, invites.to_vec());
        bia.on_mls(peer, &wire).unwrap();
    }

    /// `n` convites de devices novos de `identity`, em ordem de Peer ID.
    fn invites_by_peer_id(identity: &StandaloneIdentity, n: usize) -> Vec<Invite> {
        let mut invites: Vec<Invite> = (0..n).map(|_| invite_for(identity)).collect();
        invites.sort_by_key(Invite::peer_id);
        invites
    }

    fn peer_ids(map: &HashMap<PeerId, Invite>) -> Vec<PeerId> {
        let mut ids: Vec<PeerId> = map.keys().copied().collect();
        ids.sort();
        ids
    }

    /// P24: `household` guarda no máximo `MAX_KNOWN_DEVICES`; com o mapa cheio, o que fica são os
    /// maiores Peer IDs (nenhum tem conversa), seja qual for a ordem em que chegaram.
    #[tokio::test]
    async fn my_household_stops_growing_at_the_cap_and_keeps_the_largest_peer_ids() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut raw),
            ..
        } = household();
        // Do menor para o maior: sem despejo ficariam os 8 primeiros, não os 8 maiores.
        let invites = invites_by_peer_id(&bia_key, MAX_KNOWN_DEVICES + 1);

        for chunk in invites.chunks(3) {
            feed_list(&mut bia, sibling, &mut raw, chunk);
        }

        let largest: Vec<PeerId> = invites[1..].iter().map(Invite::peer_id).collect();
        assert_eq!(peer_ids(&bia.household), largest);
    }

    /// P24: o mesmo teto e a mesma regra valem para os devices de contatos repassados por um irmão.
    #[tokio::test]
    async fn contact_devices_stop_growing_at_the_cap_and_keep_the_largest_peer_ids() {
        let Household {
            mut bia,
            sibling: (sibling, mut raw),
            ..
        } = household();
        let cleo = StandaloneIdentity::generate();
        let invites = invites_by_peer_id(&cleo, MAX_KNOWN_DEVICES + 1);

        for chunk in invites.chunks(3) {
            feed_list(&mut bia, sibling, &mut raw, chunk);
        }

        let largest: Vec<PeerId> = invites[1..].iter().map(Invite::peer_id).collect();
        assert_eq!(peer_ids(&bia.contact_devices), largest);
    }

    /// P24 (despejo): cheio, um Peer ID maior que o menor sem conversa entra e o menor sai (e deixa de
    /// ser repassado). Se o novo é discado já depende de `expected` ter vaga (outro teto, liberado por
    /// `DialFailed`), e não é fixado aqui.
    #[tokio::test]
    async fn a_full_household_evicts_the_smallest_peer_id_for_a_larger_newcomer() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut raw),
            ..
        } = household();
        let invites = invites_by_peer_id(&bia_key, MAX_KNOWN_DEVICES + 1);
        feed_list(&mut bia, sibling, &mut raw, &invites[..3]);
        feed_list(&mut bia, sibling, &mut raw, &invites[3..6]);
        feed_list(&mut bia, sibling, &mut raw, &invites[6..MAX_KNOWN_DEVICES]);
        assert_eq!(bia.household.len(), MAX_KNOWN_DEVICES);

        feed_list(&mut bia, sibling, &mut raw, &invites[MAX_KNOWN_DEVICES..]);

        let newcomer = invites[MAX_KNOWN_DEVICES].peer_id();
        assert_eq!(bia.household.len(), MAX_KNOWN_DEVICES);
        assert!(bia.household.contains_key(&newcomer), "o maior entrou");
        assert!(
            !bia.household.contains_key(&invites[0].peer_id()),
            "o menor saiu"
        );
        assert!(
            bia.device_list()
                .iter()
                .all(|invite| invite.peer_id() != invites[0].peer_id()),
            "o despejado não é mais repassado"
        );
    }

    /// P24 (despejo): um Peer ID menor que todos os sem conversa é ignorado por inteiro (nem guardado,
    /// nem discado) e o mapa não muda.
    #[tokio::test]
    async fn a_newcomer_smaller_than_every_unprotected_entry_is_ignored() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut raw),
            ..
        } = household();
        let invites = invites_by_peer_id(&bia_key, MAX_KNOWN_DEVICES + 1);
        for chunk in invites[1..].chunks(3) {
            feed_list(&mut bia, sibling, &mut raw, chunk);
        }
        let before = peer_ids(&bia.household);

        feed_list(&mut bia, sibling, &mut raw, &invites[..1]);

        assert_eq!(peer_ids(&bia.household), before);
        assert!(!bia.expected.contains_key(&invites[0].peer_id()));
    }

    /// P24 (despejo): quem tem conversa ou conexão nunca é despejado; o despejado sai só entre os demais.
    #[tokio::test]
    async fn an_entry_with_a_connection_is_never_evicted() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut raw),
            ..
        } = household();
        let invites = invites_by_peer_id(&bia_key, MAX_KNOWN_DEVICES + 1);
        for chunk in invites[..MAX_KNOWN_DEVICES].chunks(3) {
            feed_list(&mut bia, sibling, &mut raw, chunk);
        }
        bia.connected.insert(invites[0].peer_id());
        bia.connected.insert(invites[1].peer_id());

        feed_list(&mut bia, sibling, &mut raw, &invites[MAX_KNOWN_DEVICES..]);

        assert!(bia.household.contains_key(&invites[0].peer_id()));
        assert!(bia.household.contains_key(&invites[1].peer_id()));
        assert!(
            !bia.household.contains_key(&invites[2].peer_id()),
            "o menor sem conexão saiu"
        );
        assert!(
            bia.household
                .contains_key(&invites[MAX_KNOWN_DEVICES].peer_id())
        );
    }

    /// P24 (despejo): um novo com conexão entra mesmo sendo o menor Peer ID: sai o menor dos que não
    /// têm conversa nem conexão.
    #[tokio::test]
    async fn a_connected_newcomer_displaces_an_unreachable_entry_even_if_it_is_the_smallest() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut raw),
            ..
        } = household();
        let invites = invites_by_peer_id(&bia_key, MAX_KNOWN_DEVICES + 1);
        for chunk in invites[1..].chunks(3) {
            feed_list(&mut bia, sibling, &mut raw, chunk);
        }
        bia.connected.insert(invites[0].peer_id());

        feed_list(&mut bia, sibling, &mut raw, &invites[..1]);

        assert_eq!(bia.household.len(), MAX_KNOWN_DEVICES);
        assert!(
            bia.household.contains_key(&invites[0].peer_id()),
            "o conectado entrou"
        );
        assert!(
            !bia.household.contains_key(&invites[1].peer_id()),
            "saiu o menor sem conexão"
        );
    }

    /// P24 (despejo): com todas as entradas protegidas, nada sai e o novo é ignorado.
    #[tokio::test]
    async fn a_household_full_of_connected_devices_ignores_the_newcomer() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut raw),
            ..
        } = household();
        let invites = invites_by_peer_id(&bia_key, MAX_KNOWN_DEVICES + 1);
        for chunk in invites[..MAX_KNOWN_DEVICES].chunks(3) {
            feed_list(&mut bia, sibling, &mut raw, chunk);
        }
        for invite in &invites[..MAX_KNOWN_DEVICES] {
            bia.connected.insert(invite.peer_id());
        }
        let before = peer_ids(&bia.household);

        feed_list(&mut bia, sibling, &mut raw, &invites[MAX_KNOWN_DEVICES..]);

        assert_eq!(peer_ids(&bia.household), before);
        assert!(
            !bia.expected
                .contains_key(&invites[MAX_KNOWN_DEVICES].peer_id())
        );
    }

    /// P24 (despejo): dois devices que recebem as mesmas entradas em ordens diferentes terminam com o
    /// mesmo conjunto: sem isso, irmãos trocando listas despejariam as entradas uns dos outros sem fim.
    #[tokio::test]
    async fn the_kept_set_does_not_depend_on_the_arrival_order() {
        let cleo = StandaloneIdentity::generate();
        let invites = invites_by_peer_id(&cleo, 2 * MAX_KNOWN_DEVICES);
        let mut sets = Vec::new();
        for ascending in [true, false] {
            let Household {
                mut bia,
                sibling: (sibling, mut raw),
                ..
            } = household();
            let mut order = invites.clone();
            if !ascending {
                order.reverse();
            }
            for chunk in order.chunks(3) {
                feed_list(&mut bia, sibling, &mut raw, chunk);
            }
            sets.push(peer_ids(&bia.contact_devices));
        }
        let mut largest: Vec<PeerId> = invites[MAX_KNOWN_DEVICES..]
            .iter()
            .map(Invite::peer_id)
            .collect();
        largest.sort();
        assert_eq!(sets[0], sets[1]);
        assert_eq!(sets[0], largest);
    }

    /// P24: `expected` também tem teto contra listas de contato em conexões sucessivas; mas o `accept`
    /// (decisão explícita do usuário) nunca é cortado por ele.
    #[tokio::test]
    async fn expected_stops_growing_at_the_cap_but_accept_is_never_refused() {
        let Household {
            mut bia,
            ana_key,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        bia.trust(ana_key.id()).unwrap();

        for _ in 0..2 {
            let invites: Vec<Invite> = (0..MAX_KNOWN_DEVICES)
                .map(|_| invite_for(&ana_key))
                .collect();
            feed_list(&mut bia, ana_peer, &mut ana_raw, &invites);
            bia.handle(NodeEvent::PeerDisconnected(ana_peer));
        }
        assert_eq!(bia.expected.len(), MAX_KNOWN_DEVICES);

        let wanted = invite_for(&StandaloneIdentity::generate());
        bia.accept(&wanted).unwrap();
        assert!(bia.expected.contains_key(&wanted.peer_id()));
    }

    /// P24: discagem que falhou libera a vaga em `expected`; falha sem peer não mexe em nada.
    #[tokio::test]
    async fn a_failed_dial_frees_its_slot_in_expected() {
        let Household {
            mut bia,
            ana,
            ana_device: (ana_peer, _),
            ..
        } = household();
        let other = PeerId::random();
        bia.expected.insert(ana_peer, ana.clone());
        bia.expected.insert(other, ana);

        bia.handle(NodeEvent::DialFailed {
            peer: None,
            reason: "sem peer".to_string(),
        });
        assert_eq!(bia.expected.len(), 2, "sem peer, nada a limpar");

        bia.handle(NodeEvent::DialFailed {
            peer: Some(other),
            reason: "fora do ar".to_string(),
        });
        assert!(!bia.expected.contains_key(&other));
        assert!(bia.expected.contains_key(&ana_peer), "só o peer que falhou");
    }

    /// P24 (frescor): discagem que falhou a um device de `household` ou de `contact_devices`, sem
    /// conversa e sem conexão, apaga a entrada; o item some também da lista que repasso.
    #[tokio::test]
    async fn a_failed_dial_forgets_an_unreachable_device_in_both_maps() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut raw),
            ..
        } = household();
        let cleo = StandaloneIdentity::generate();
        let (mine, hers) = (invite_for(&bia_key), invite_for(&cleo));
        feed_list(&mut bia, sibling, &mut raw, &[mine.clone(), hers.clone()]);
        assert!(bia.household.contains_key(&mine.peer_id()));
        assert!(bia.contact_devices.contains_key(&hers.peer_id()));

        for invite in [&mine, &hers] {
            bia.handle(NodeEvent::DialFailed {
                peer: Some(invite.peer_id()),
                reason: "fora do ar".to_string(),
            });
        }

        assert!(bia.household.is_empty());
        assert!(bia.contact_devices.is_empty());
        let forwarded: Vec<PeerId> = bia.device_list().iter().map(Invite::peer_id).collect();
        assert_eq!(forwarded, vec![bia.peer_id()], "só o meu próprio convite");
    }

    /// P24 (frescor): o que a falha NÃO apaga: device com conversa, device conectado, falha sem peer
    /// e peer que não está nos mapas.
    #[tokio::test]
    async fn a_failed_dial_keeps_devices_that_have_a_conversation_or_a_connection() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, _),
            ana_device: (ana_peer, _),
            ..
        } = household();
        let connected = invite_for(&bia_key);
        bia.household.insert(sibling, invite_for(&bia_key));
        bia.contact_devices.insert(ana_peer, invite_for(&bia_key));
        bia.household.insert(connected.peer_id(), connected.clone());
        bia.connected.insert(connected.peer_id());

        for peer in [
            Some(sibling),
            Some(ana_peer),
            Some(connected.peer_id()),
            None,
        ] {
            bia.handle(NodeEvent::DialFailed {
                peer,
                reason: "fora do ar".to_string(),
            });
        }
        bia.handle(NodeEvent::DialFailed {
            peer: Some(PeerId::random()),
            reason: "desconhecido".to_string(),
        });

        assert_eq!(bia.household.len(), 2, "com conversa e conectado ficam");
        assert_eq!(bia.contact_devices.len(), 1, "com conversa fica");
    }

    /// P24 (frescor): com o mapa cheio, a falha libera a vaga e um Peer ID novo passa a caber.
    #[tokio::test]
    async fn a_failed_dial_frees_a_slot_in_a_full_household() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut raw),
            ..
        } = household();
        // O novo é o menor Peer ID: cheio, o despejo o ignora (ele seria o primeiro a sair).
        let invites = invites_by_peer_id(&bia_key, MAX_KNOWN_DEVICES + 1);
        let newcomer = invites[0].peer_id();
        let dead = invites[1].peer_id();
        feed_list(&mut bia, sibling, &mut raw, &invites[1..4]);
        feed_list(&mut bia, sibling, &mut raw, &invites[4..]);
        feed_list(&mut bia, sibling, &mut raw, &invites[..1]);
        assert!(!bia.household.contains_key(&newcomer), "cheio: ignorado");

        bia.handle(NodeEvent::DialFailed {
            peer: Some(dead),
            reason: "fora do ar".to_string(),
        });
        feed_list(&mut bia, sibling, &mut raw, &invites[..1]);

        assert_eq!(bia.household.len(), MAX_KNOWN_DEVICES);
        assert!(bia.household.contains_key(&newcomer));
        assert!(!bia.household.contains_key(&dead));
    }

    /// `household` cheio com o irmão conectado e o intervalo de re-anúncio zerado: qualquer
    /// aprendizado que conte como novo sairia na hora, então `pending` revela se `learned` disparou.
    fn full_household_with_connected_sibling() -> (Chat, StandaloneIdentity, RawDevice, Vec<Invite>)
    {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut raw),
            ..
        } = household();
        let invites = invites_by_peer_id(&bia_key, MAX_KNOWN_DEVICES + 1);
        feed_list(&mut bia, sibling, &mut raw, &invites[1..4]);
        feed_list(&mut bia, sibling, &mut raw, &invites[4..]);
        assert_eq!(bia.household.len(), MAX_KNOWN_DEVICES);
        bia.connected.insert(sibling);
        bia.sibling_announce_gap = Duration::ZERO;
        // Anuncia antes: assim o `on_mls` não anuncia por conta própria e só o re-anúncio é contado.
        bia.announce_devices(sibling);
        (bia, bia_key, (sibling, raw), invites)
    }

    /// P24 (`learned`): um Peer ID novo ignorado por o mapa estar cheio não é aprendizado; não
    /// dispara re-anúncio aos irmãos.
    #[tokio::test]
    async fn an_ignored_newcomer_in_a_full_household_is_not_re_announced() {
        let (mut bia, _key, (sibling, mut raw), invites) = full_household_with_connected_sibling();
        let newcomer = invites[0].peer_id();
        let sent = bia.pending.len();

        feed_list(&mut bia, sibling, &mut raw, &invites[..1]);

        assert!(!bia.household.contains_key(&newcomer), "cheio: ignorado");
        assert_eq!(bia.pending.len(), sent, "ignorado não é aprendizado");
        assert!(bia.sibling_pending.is_empty());
    }

    /// P24 (`learned`): a mesma lista de novo (nada mudou) também não dispara re-anúncio.
    #[tokio::test]
    async fn an_unchanged_list_from_a_sibling_is_not_re_announced() {
        let (mut bia, _key, (sibling, mut raw), invites) = full_household_with_connected_sibling();
        let sent = bia.pending.len();

        feed_list(&mut bia, sibling, &mut raw, &invites[1..4]);

        assert_eq!(bia.pending.len(), sent, "nada mudou, nada a anunciar");
        assert!(bia.sibling_pending.is_empty());
    }

    /// P24 (`learned`), contraprova: um Peer ID novo que cabe (maior que o menor guardado, com o mapa
    /// cheio) é aprendizado e sai na hora; sem isto os dois testes acima passariam por não anunciar nunca.
    #[tokio::test]
    async fn a_newcomer_that_displaces_another_in_a_full_household_is_re_announced() {
        let (mut bia, bia_key, (sibling, mut raw), _invites) =
            full_household_with_connected_sibling();
        let sent = bia.pending.len();
        // Maior que todos os guardados: entra e despeja o menor.
        let bigger = invites_by_peer_id(&bia_key, 64).pop().unwrap();
        let smallest = bia.household.keys().min().copied().unwrap();
        assert!(bigger.peer_id() > smallest);

        feed_list(&mut bia, sibling, &mut raw, std::slice::from_ref(&bigger));

        assert!(bia.household.contains_key(&bigger.peer_id()));
        assert_eq!(
            bia.pending.len(),
            sent + 1,
            "o despejo conta como aprendizado"
        );
    }

    /// P24: re-anúncio ao irmão tem intervalo mínimo; o que chega dentro dele fica pendente e sai no
    /// próximo evento depois do prazo, sem perder a atualização.
    #[tokio::test]
    async fn re_announcing_to_a_sibling_is_rate_limited_and_the_rest_waits() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut raw),
            ..
        } = household();
        bia.connected.insert(sibling);
        bia.sibling_announce_gap = Duration::from_secs(3600);
        let sent = |bia: &Chat| bia.pending.len();
        let listening = || NodeEvent::Listening("/ip4/127.0.0.1/tcp/1".parse().unwrap());

        // Anuncia antes: assim o `on_mls` não anuncia por conta própria e só o re-anúncio é contado.
        bia.announce_devices(sibling);
        let before = sent(&bia);
        feed_list(&mut bia, sibling, &mut raw, &[invite_for(&bia_key)]);
        assert_eq!(sent(&bia), before + 1, "o 1º aprendizado anuncia na hora");
        assert!(bia.sibling_pending.is_empty());

        feed_list(&mut bia, sibling, &mut raw, &[invite_for(&bia_key)]);
        assert_eq!(sent(&bia), before + 1, "dentro do intervalo: nada sai");
        assert!(bia.sibling_pending.contains(&sibling));
        assert_eq!(bia.household.len(), 2, "mas o aprendido não se perde");

        bia.handle(listening());
        assert!(bia.sibling_pending.contains(&sibling), "prazo não passou");
        assert_eq!(sent(&bia), before + 1);

        bia.sibling_announce_gap = Duration::ZERO;
        bia.handle(listening());
        assert!(bia.sibling_pending.is_empty());
        assert_eq!(sent(&bia), before + 2, "o pendente saiu");
    }

    /// P24: envio de controle (handshake/anúncio, sem mensagem na outbox) que falha libera os anúncios
    /// do peer para o próximo gatilho; falha de mensagem de chat não mexe neles.
    #[tokio::test]
    async fn a_failed_control_send_lets_the_announcements_repeat() {
        let Household {
            mut bia,
            ana_device: (ana_peer, _),
            ..
        } = household();
        bia.announced.insert(ana_peer);
        bia.devices_announced.insert(ana_peer);

        bia.transmit(ana_peer, &Envelope::Hello(None, None, None));
        let chat_id = *bia.pending.keys().next().unwrap();
        bia.pending
            .insert(chat_id, Some(MessageId::from_bytes([7; 16])));
        bia.handle(NodeEvent::SendFailed {
            peer: ana_peer,
            id: chat_id,
            reason: "x".to_string(),
        });
        assert!(bia.announced.contains(&ana_peer), "mensagem de chat");
        assert!(bia.devices_announced.contains(&ana_peer));

        bia.transmit(ana_peer, &Envelope::Hello(None, None, None));
        let control_id = *bia.pending.keys().next().unwrap();
        bia.handle(NodeEvent::SendFailed {
            peer: ana_peer,
            id: control_id,
            reason: "x".to_string(),
        });
        assert!(!bia.announced.contains(&ana_peer));
        assert!(!bia.devices_announced.contains(&ana_peer));
    }

    /// O único envio em voo de `bia` (o `Hello` que o `PeerConnected` acabou de mandar).
    fn only_pending(bia: &Chat) -> SendId {
        assert_eq!(bia.pending.len(), 1, "esperava só o Hello em voo");
        *bia.pending.keys().next().unwrap()
    }

    fn fail_send(bia: &mut Chat, peer: PeerId, id: SendId) {
        bia.handle(NodeEvent::SendFailed {
            peer,
            id,
            reason: "Connection was closed before a response was received".to_string(),
        });
    }

    /// P25: o `Hello` que falha com o peer ainda conectado e sem conversa é reenviado (a outra conexão
    /// do dial simultâneo segue de pé; sem isto os dois esperam para sempre).
    #[tokio::test]
    async fn a_failed_hello_is_resent_while_the_peer_is_connected_without_a_conversation() {
        let Household { mut bia, .. } = household();
        let peer = PeerId::random();
        connect_peer(&mut bia, peer);
        let first = only_pending(&bia);

        fail_send(&mut bia, peer, first);

        let second = only_pending(&bia);
        assert_ne!(second, first, "é um envio novo");
    }

    /// P25: sem conexão com o peer, ou com a conversa já feita, não há o que reenviar.
    #[tokio::test]
    async fn a_failed_hello_is_not_resent_without_a_connection_or_with_a_conversation() {
        let Household {
            mut bia,
            ana_device: (ana_peer, _),
            ..
        } = household();
        let gone = PeerId::random();
        connect_peer(&mut bia, gone);
        let id = only_pending(&bia);
        bia.handle(NodeEvent::PeerDisconnected(gone));
        fail_send(&mut bia, gone, id);
        assert!(bia.pending.is_empty(), "peer desconectado: sem reenvio");

        connect_peer(&mut bia, ana_peer);
        let id = only_pending(&bia);
        fail_send(&mut bia, ana_peer, id);
        assert!(bia.pending.is_empty(), "já há conversa: sem reenvio");
    }

    /// P25: no máximo 3 reenvios por conexão; reconectar zera a conta.
    #[tokio::test]
    async fn hello_resends_stop_at_three_per_connection_and_reset_on_reconnect() {
        let Household { mut bia, .. } = household();
        let peer = PeerId::random();
        connect_peer(&mut bia, peer);

        for _ in 0..3 {
            let id = only_pending(&bia);
            fail_send(&mut bia, peer, id);
        }
        let last = only_pending(&bia);
        fail_send(&mut bia, peer, last);
        assert!(
            bia.pending.is_empty(),
            "o 4º envio que falha não é reenviado"
        );

        bia.handle(NodeEvent::PeerDisconnected(peer));
        connect_peer(&mut bia, peer);
        let id = only_pending(&bia);
        fail_send(&mut bia, peer, id);
        only_pending(&bia);
    }

    /// P25: só o handshake (`Hello`, `KeyPackage`, `Welcome`) é reenviado; outro envio de controle
    /// (mensagem MLS de anúncio) que falha não vira reenvio.
    #[tokio::test]
    async fn a_failed_mls_control_send_is_not_resent() {
        let Household { mut bia, .. } = household();
        let peer = PeerId::random();
        bia.connected.insert(peer);
        bia.transmit(peer, &Envelope::Mls(vec![1]));
        let id = only_pending(&bia);

        fail_send(&mut bia, peer, id);

        assert!(bia.pending.is_empty());
    }

    /// P25 (d): o `KeyPackage` que falha com o peer conectado e sem conversa é reenviado (sem ele o
    /// convidador espera para sempre).
    #[tokio::test]
    async fn a_failed_key_package_is_resent_while_connected_without_a_conversation() {
        let Household { mut bia, .. } = household();
        let peer = PeerId::random();
        bia.connected.insert(peer);
        bia.transmit(peer, &Envelope::KeyPackage(vec![1]));
        let first = only_pending(&bia);

        fail_send(&mut bia, peer, first);

        assert_ne!(only_pending(&bia), first, "é um envio novo");
    }

    /// P25 (d): sem conexão, ou com a conversa já feita, o `KeyPackage` não é reenviado.
    #[tokio::test]
    async fn a_failed_key_package_is_not_resent_without_a_connection_or_with_a_conversation() {
        let Household {
            mut bia,
            ana_device: (ana_peer, _),
            ..
        } = household();
        let gone = PeerId::random();
        bia.transmit(gone, &Envelope::KeyPackage(vec![1]));
        let id = only_pending(&bia);
        fail_send(&mut bia, gone, id);
        assert!(bia.pending.is_empty(), "peer desconectado: sem reenvio");

        bia.connected.insert(ana_peer);
        bia.transmit(ana_peer, &Envelope::KeyPackage(vec![1]));
        let id = only_pending(&bia);
        fail_send(&mut bia, ana_peer, id);
        assert!(bia.pending.is_empty(), "já há conversa: sem reenvio");
    }

    /// P25 (d): o `Welcome` que falha com o peer conectado é reenviado. Quem o manda já tem a
    /// conversa (`ready`), então a conversa existente NÃO impede o reenvio.
    #[tokio::test]
    async fn a_failed_welcome_is_resent_while_connected_even_with_a_conversation() {
        let Household {
            mut bia,
            ana_device: (ana_peer, _),
            ..
        } = household();
        bia.connected.insert(ana_peer);
        bia.transmit(ana_peer, &Envelope::Welcome(vec![1]));
        let first = only_pending(&bia);

        fail_send(&mut bia, ana_peer, first);

        assert_ne!(only_pending(&bia), first, "é um envio novo");
    }

    /// P25 (d): sem conexão, o `Welcome` não é reenviado.
    #[tokio::test]
    async fn a_failed_welcome_is_not_resent_without_a_connection() {
        let Household { mut bia, .. } = household();
        let gone = PeerId::random();
        bia.transmit(gone, &Envelope::Welcome(vec![1]));
        let id = only_pending(&bia);

        fail_send(&mut bia, gone, id);

        assert!(bia.pending.is_empty());
    }

    /// P25 (d): `KeyPackage` e `Welcome` também páram em 3 reenvios por conexão, e reconectar zera.
    #[tokio::test]
    async fn key_package_and_welcome_resends_stop_at_three_per_connection() {
        for envelope in [Envelope::KeyPackage(vec![1]), Envelope::Welcome(vec![1])] {
            let Household {
                mut bia,
                ana_device: (with_conversation, _),
                ..
            } = household();
            // O `Welcome` só é reenviado com conversa (quem o manda já a tem); o `KeyPackage`, sem.
            let peer = if matches!(envelope, Envelope::Welcome(_)) {
                with_conversation
            } else {
                PeerId::random()
            };
            bia.connected.insert(peer);
            bia.transmit(peer, &envelope);

            for _ in 0..3 {
                let id = only_pending(&bia);
                fail_send(&mut bia, peer, id);
            }
            let last = only_pending(&bia);
            fail_send(&mut bia, peer, last);
            assert!(
                bia.pending.is_empty(),
                "{envelope:?}: o 4º que falha não é reenviado"
            );

            bia.handle(NodeEvent::PeerDisconnected(peer));
            bia.connected.insert(peer);
            bia.transmit(peer, &envelope);
            let id = only_pending(&bia);
            fail_send(&mut bia, peer, id);
            only_pending(&bia);
        }
    }

    /// P25 (d): o teto é por tipo. Gastar as 3 tentativas do `Hello` não tira as do `KeyPackage`
    /// nem as do `Welcome`.
    #[tokio::test]
    async fn the_resend_cap_is_per_kind_of_handshake_message() {
        let Household {
            mut bia,
            ana_device: (with_conversation, _),
            ..
        } = household();
        let peer = PeerId::random();
        connect_peer(&mut bia, peer);
        bia.connected.insert(with_conversation);
        for _ in 0..4 {
            let id = only_pending(&bia);
            fail_send(&mut bia, peer, id);
        }
        assert!(bia.pending.is_empty(), "o Hello esgotou as 3 tentativas");

        for envelope in [Envelope::KeyPackage(vec![1]), Envelope::Welcome(vec![1])] {
            // O `Welcome` só é reenviado com conversa: vai para o peer que a tem.
            let peer = if matches!(envelope, Envelope::Welcome(_)) {
                with_conversation
            } else {
                peer
            };
            bia.transmit(peer, &envelope);
            let id = only_pending(&bia);
            fail_send(&mut bia, peer, id);
            assert_eq!(bia.pending.len(), 1, "{envelope:?} ainda tem as suas");
            let again = only_pending(&bia);
            bia.pending.remove(&again);
        }
    }

    /// P25 (d): o reenvio leva os MESMOS bytes (um `Welcome` só abre para o `KeyPackage` que o gerou).
    #[tokio::test]
    async fn a_resent_handshake_message_carries_the_same_bytes() {
        let Household {
            mut bia,
            ana_device: (with_conversation, _),
            ..
        } = household();
        bia.connected.insert(with_conversation);
        for envelope in [Envelope::KeyPackage(vec![7, 8]), Envelope::Welcome(vec![9])] {
            // O `Welcome` só é reenviado com conversa; o `KeyPackage`, sem.
            let peer = if matches!(envelope, Envelope::Welcome(_)) {
                with_conversation
            } else {
                PeerId::random()
            };
            bia.connected.insert(peer);
            bia.transmit(peer, &envelope);
            let first = only_pending(&bia);
            fail_send(&mut bia, peer, first);
            let second = only_pending(&bia);
            assert_eq!(bia.handshake.get(&second), Some(&envelope));
            bia.pending.remove(&second);
        }
    }

    /// P25 (d): o ack do `Welcome` encerra o envio; uma falha tardia do mesmo id não reenvia.
    #[tokio::test]
    async fn a_delivered_welcome_is_not_resent_by_a_late_failure() {
        let Household { mut bia, .. } = household();
        let peer = PeerId::random();
        bia.connected.insert(peer);
        bia.transmit(peer, &Envelope::Welcome(vec![1]));
        let id = only_pending(&bia);

        bia.handle(NodeEvent::MessageDelivered { peer, id });
        fail_send(&mut bia, peer, id);

        assert!(bia.pending.is_empty());
    }

    /// 3.5 etapa 5 (auditoria I1): de um contato aceitam-se poucos endereços por device; de irmão, todos.
    #[test]
    fn a_contacts_device_gets_few_dial_addresses_and_a_siblings_gets_all() {
        let addrs: Vec<Multiaddr> = (1..=10)
            .map(|port| format!("/ip4/203.0.113.1/tcp/{port}").parse().unwrap())
            .collect();

        assert_eq!(dial_addrs(&addrs, false).len(), CONTACT_DIAL_ADDRS);
        assert_eq!(dial_addrs(&addrs, false), addrs[..CONTACT_DIAL_ADDRS]);
        assert_eq!(dial_addrs(&addrs, true), addrs);
    }

    /// 3.5 etapa 5: o que eu anuncio é o meu convite (primeiro) mais os irmãos que conheço.
    #[tokio::test]
    async fn my_device_list_is_my_invite_first_then_my_siblings() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling, mut sibling_raw),
            ..
        } = household();
        let third = invite_for(&bia_key);
        let wire = device_list_wire(&mut sibling_raw, vec![third.clone()]);
        bia.on_mls(sibling, &wire).unwrap();

        let list = bia.device_list();

        assert_eq!(list.len(), 2);
        assert_eq!(list[0].peer_id(), bia.peer_id());
        assert_eq!(list[1], third);
    }

    /// 3.5 etapa 5: os endereços anunciados podem ser fixados à mão (IP fixo, port-forward, teste).
    #[tokio::test]
    async fn advertised_addresses_override_the_nodes_own() {
        let mut bia = chat(&StandaloneIdentity::generate(), &DeviceKey::generate());
        assert!(bia.own_invite().addrs().is_empty());
        let addr: Multiaddr = "/ip4/203.0.113.9/tcp/4001".parse().unwrap();

        bia.set_advertised_addrs(vec![addr.clone()]);

        assert_eq!(bia.own_invite().addrs(), [addr]);
    }

    /// 3.5 etapa 5 (consentimento): a lista de um peer que eu NÃO aceitei é ignorada em silêncio: nada
    /// é discado nem guardado. Quem apenas conectou não me faz discar endereços dele.
    #[tokio::test]
    async fn a_device_list_from_a_peer_i_did_not_accept_is_ignored() {
        let Household {
            mut bia,
            ana_key,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let second = invite_for(&ana_key);

        let wire = device_list_wire(&mut ana_raw, vec![second]);
        bia.on_mls(ana_peer, &wire).unwrap();

        assert!(bia.expected.is_empty());
        assert!(bia.contact_devices.is_empty());
    }

    /// 3.5 etapa 5 (consentimento): aceitar um convite aceita a identidade dele.
    #[tokio::test]
    async fn accepting_an_invite_trusts_its_identity() {
        let mut bia = chat(&StandaloneIdentity::generate(), &DeviceKey::generate());
        let ana = StandaloneIdentity::generate();
        let invite = invite_for(&ana);

        bia.accept(&invite).unwrap();

        assert!(bia.is_trusted(&ana.id()));
        assert!(!bia.is_trusted(&StandaloneIdentity::generate().id()));
    }

    /// P24: a confiança sobrevive a reabrir o `Chat` no mesmo banco.
    #[tokio::test]
    async fn trust_survives_reopening_the_chat() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("bia.db");
        let (bia_key, bia_device) = (StandaloneIdentity::generate(), DeviceKey::generate());
        let (ana, cleo) = (
            StandaloneIdentity::generate().id(),
            StandaloneIdentity::generate().id(),
        );
        let config = || NodeConfig {
            mdns: false,
            ..Default::default()
        };
        {
            let mut bia = Chat::open(&bia_key, &bia_device, config(), &db).unwrap();
            bia.trust(ana.clone()).unwrap();
        }

        let bia = Chat::open(&bia_key, &bia_device, config(), &db).unwrap();

        assert!(bia.is_trusted(&ana));
        assert!(!bia.is_trusted(&cleo));
    }

    /// P24 (`untrust`): deixar de confiar tira a confiança e grava a lista negra.
    #[tokio::test]
    async fn untrust_withdraws_the_trust_and_remembers_the_distrust() {
        let mut bia = chat(&StandaloneIdentity::generate(), &DeviceKey::generate());
        let ana = StandaloneIdentity::generate().id();
        bia.trust(ana.clone()).unwrap();

        bia.untrust(&ana).unwrap();

        assert!(!bia.is_trusted(&ana));
        assert!(bia.is_distrusted(&ana));
        assert!(bia.outbox.trusted_identities().unwrap().is_empty());
        assert_eq!(bia.outbox.distrusted_identities().unwrap(), vec![ana]);
    }

    /// P24 (`untrust`): a desconfiança sobrevive a reabrir o `Chat`; confiar de propósito depois a
    /// apaga, e isso também sobrevive.
    #[tokio::test]
    async fn distrust_survives_reopening_and_an_explicit_trust_clears_it_for_good() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("bia.db");
        let (bia_key, bia_device) = (StandaloneIdentity::generate(), DeviceKey::generate());
        let (ana, cleo) = (
            StandaloneIdentity::generate().id(),
            StandaloneIdentity::generate().id(),
        );
        let config = || NodeConfig {
            mdns: false,
            ..Default::default()
        };
        {
            let mut bia = Chat::open(&bia_key, &bia_device, config(), &db).unwrap();
            bia.trust(ana.clone()).unwrap();
            bia.untrust(&ana).unwrap();
        }
        {
            let mut bia = Chat::open(&bia_key, &bia_device, config(), &db).unwrap();
            assert!(!bia.is_trusted(&ana));
            assert!(bia.is_distrusted(&ana));
            assert!(!bia.is_distrusted(&cleo));
            bia.trust(ana.clone()).unwrap();
        }

        let bia = Chat::open(&bia_key, &bia_device, config(), &db).unwrap();

        assert!(bia.is_trusted(&ana));
        assert!(!bia.is_distrusted(&ana));
    }

    /// P24 (`untrust`): se um `untrust` caiu no meio (a lista negra gravada, a confiança ainda no banco), a
    /// desconfiança vence ao reabrir.
    #[tokio::test]
    async fn a_half_done_untrust_reopens_as_distrusted() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("bia.db");
        let (bia_key, bia_device) = (StandaloneIdentity::generate(), DeviceKey::generate());
        let ana = StandaloneIdentity::generate().id();
        let config = || NodeConfig {
            mdns: false,
            ..Default::default()
        };
        {
            let mut bia = Chat::open(&bia_key, &bia_device, config(), &db).unwrap();
            bia.trust(ana.clone()).unwrap();
            bia.outbox.add_distrusted(&ana).unwrap();
        }

        let bia = Chat::open(&bia_key, &bia_device, config(), &db).unwrap();

        assert!(bia.is_distrusted(&ana));
        assert!(!bia.is_trusted(&ana));
    }

    /// P24 (`untrust`): aceitar o convite da identidade (ato explícito) volta a confiar nela.
    #[tokio::test]
    async fn accepting_an_invite_after_untrust_trusts_again() {
        let mut bia = chat(&StandaloneIdentity::generate(), &DeviceKey::generate());
        let ana = StandaloneIdentity::generate();
        bia.trust(ana.id()).unwrap();
        bia.untrust(&ana.id()).unwrap();

        bia.accept(&invite_for(&ana)).unwrap();

        assert!(bia.is_trusted(&ana.id()));
        assert!(!bia.is_distrusted(&ana.id()));
    }

    /// P24 (`untrust`): os meus devices são família, não contatos: recusa e nada muda.
    #[tokio::test]
    async fn untrust_refuses_my_own_identity() {
        let key = StandaloneIdentity::generate();
        let mut bia = chat(&key, &DeviceKey::generate());

        let result = bia.untrust(&key.id());

        assert!(matches!(result, Err(Error::OwnIdentity)));
        assert!(bia.is_trusted(&key.id()));
        assert!(!bia.is_distrusted(&key.id()));
    }

    /// P24 (`untrust`): um irmão que repassa um device da identidade desconfiada não a faz ser confiada
    /// de novo, nem guardada, nem discada.
    #[tokio::test]
    async fn a_sibling_forwarding_a_distrusted_identity_does_not_bring_the_trust_back() {
        let Household {
            mut bia,
            sibling: (sibling, mut raw),
            ..
        } = household();
        let cleo = StandaloneIdentity::generate();
        bia.trust(cleo.id()).unwrap();
        bia.untrust(&cleo.id()).unwrap();
        let forwarded = invite_for(&cleo);

        feed_list(
            &mut bia,
            sibling,
            &mut raw,
            std::slice::from_ref(&forwarded),
        );

        assert!(!bia.is_trusted(&cleo.id()));
        assert!(bia.contact_devices.is_empty(), "não guardou");
        assert!(
            !bia.expected.contains_key(&forwarded.peer_id()),
            "nem discou"
        );
    }

    /// P24 (`untrust`): a lista negra é por identidade: outra identidade repassada pelo irmão segue
    /// sendo confiada, guardada e discada.
    #[tokio::test]
    async fn distrusting_one_identity_does_not_affect_another_forwarded_by_a_sibling() {
        let Household {
            mut bia,
            sibling: (sibling, mut raw),
            ..
        } = household();
        let (cleo, dora) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        bia.untrust(&cleo.id()).unwrap();
        let forwarded = invite_for(&dora);

        feed_list(
            &mut bia,
            sibling,
            &mut raw,
            std::slice::from_ref(&forwarded),
        );

        assert!(bia.is_trusted(&dora.id()));
        assert!(bia.contact_devices.contains_key(&forwarded.peer_id()));
        assert!(bia.expected.contains_key(&forwarded.peer_id()));
    }

    /// P24 (`untrust`): esquece os devices da identidade em `contact_devices` e `expected`, sem mexer nos
    /// de outras identidades nem na conversa já existente com ela.
    #[tokio::test]
    async fn untrust_forgets_that_identitys_devices_but_keeps_its_conversation() {
        let Household {
            mut bia,
            ana_key,
            ana_device: (ana_peer, _),
            sibling: (sibling, mut raw),
            ..
        } = household();
        let dora = StandaloneIdentity::generate();
        let (ana_other, dora_device) = (invite_for(&ana_key), invite_for(&dora));
        bia.trust(ana_key.id()).unwrap();
        feed_list(
            &mut bia,
            sibling,
            &mut raw,
            &[ana_other.clone(), dora_device.clone()],
        );
        assert!(bia.contact_devices.contains_key(&ana_other.peer_id()));
        assert!(bia.expected.contains_key(&ana_other.peer_id()));

        bia.untrust(&ana_key.id()).unwrap();

        assert!(!bia.contact_devices.contains_key(&ana_other.peer_id()));
        assert!(!bia.expected.contains_key(&ana_other.peer_id()));
        assert!(bia.contact_devices.contains_key(&dora_device.peer_id()));
        assert!(bia.expected.contains_key(&dora_device.peer_id()));
        assert!(
            bia.conversations.contains_key(&ana_peer),
            "a conversa segue"
        );
    }

    /// P24: confiar DEPOIS de a conversa ficar pronta faz o anúncio que antes foi calado (a lista só
    /// saía quando a conversa ficava pronta); confiar em outra identidade não anuncia a este peer.
    #[tokio::test]
    async fn trusting_after_the_conversation_is_ready_announces_my_device() {
        let Household {
            mut bia,
            ana,
            ana_device: (ana_peer, _),
            ..
        } = household();
        bia.connected.insert(ana_peer);
        bia.announce_devices(ana_peer);
        assert!(
            !bia.devices_announced.contains(&ana_peer),
            "não aceita: nada"
        );

        bia.trust(StandaloneIdentity::generate().id()).unwrap();
        assert!(
            !bia.devices_announced.contains(&ana_peer),
            "outra identidade"
        );

        bia.trust(ana).unwrap();
        assert!(bia.devices_announced.contains(&ana_peer));
    }

    /// 3.5 etapa 5 (privacidade, I4): irmão recebe a lista completa; contato aceito, só o meu
    /// convite; quem não aceitei, nada.
    #[tokio::test]
    async fn what_i_announce_depends_on_who_is_asking() {
        let Household {
            mut bia,
            ana,
            bia_key,
            sibling: (sibling, mut sibling_raw),
            ana_device: (ana_peer, _),
            ..
        } = household();
        let third = invite_for(&bia_key);
        let wire = device_list_wire(&mut sibling_raw, vec![third.clone()]);
        bia.on_mls(sibling, &wire).unwrap();

        assert_eq!(bia.device_list_for(&ana_peer), None, "não aceita: nada");

        bia.trust(ana).unwrap();
        let to_contact = bia.device_list_for(&ana_peer).unwrap();
        assert_eq!(to_contact.len(), 1, "contato aceito: só o meu convite");
        assert_eq!(to_contact[0].peer_id(), bia.peer_id());

        let to_sibling = bia.device_list_for(&sibling).unwrap();
        assert!(to_sibling.len() >= 2, "irmão: eu e os irmãos");
        assert!(to_sibling.contains(&third));
    }

    /// 3.5 etapa 5: o convite que um contato aceito anuncia de si é guardado e repassado aos meus
    /// irmãos (que assim o acham sem o contato listar nada a mais).
    #[tokio::test]
    async fn a_trusted_contacts_own_invite_is_kept_and_passed_on_to_my_siblings() {
        let bia_key = StandaloneIdentity::generate();
        let ana_key = StandaloneIdentity::generate();
        let mut bia = chat(&bia_key, &DeviceKey::generate());
        let (sibling, _) = add_device_of(&mut bia, &bia_key);
        let (ana_peer, mut ana_raw, ana_own) = add_device_with_invite(&mut bia, &ana_key);
        bia.trust(ana_key.id()).unwrap();

        let wire = device_list_wire(&mut ana_raw, vec![ana_own.clone()]);
        bia.on_mls(ana_peer, &wire).unwrap();

        assert_eq!(bia.contact_devices.get(&ana_peer), Some(&ana_own));
        let to_sibling = bia.device_list_for(&sibling).unwrap();
        assert!(
            to_sibling.contains(&ana_own),
            "irmão recebe o convite da Ana"
        );
        assert!(
            !bia.expected.contains_key(&ana_peer),
            "já tem conversa: não disca"
        );
    }

    /// 3.5 etapa 5: um irmão que repassa o convite de um device de contato faz este device discar o
    /// contato, exigindo a identidade DO ITEM, e passar a confiar nela (a família compartilha a confiança).
    #[tokio::test]
    async fn a_contact_invite_forwarded_by_a_sibling_is_dialed_and_trusted() {
        let Household {
            mut bia,
            sibling: (sibling, mut sibling_raw),
            ..
        } = household();
        let cleo = StandaloneIdentity::generate();
        let forwarded = invite_for(&cleo);

        let wire = device_list_wire(&mut sibling_raw, vec![forwarded.clone()]);
        bia.on_mls(sibling, &wire).unwrap();

        assert_eq!(
            bia.expected.get(&forwarded.peer_id()),
            Some(&cleo.id()),
            "exige a identidade do item, não a do irmão"
        );
        assert!(bia.is_trusted(&cleo.id()));
        assert_eq!(
            bia.contact_devices.get(&forwarded.peer_id()),
            Some(&forwarded)
        );
        assert!(bia.household.is_empty(), "contato não vira família");
    }

    /// 3.5 etapa 5 (I2): de um contato só se discam endereços globais por tcp/quic ou circuito por
    /// relay global; de irmão, tudo menos /dnsaddr.
    #[test]
    fn contact_addresses_must_be_global_ip_over_tcp_quic_or_a_circuit() {
        let ok = |s: &str| s.parse::<Multiaddr>().unwrap();
        for good in [
            "/ip4/203.0.113.7/tcp/4001",
            "/ip4/8.8.8.8/udp/4001/quic-v1",
            "/ip6/2001:db8::1/tcp/4001",
            "/ip4/203.0.113.7/tcp/4001/p2p-circuit",
        ] {
            assert!(dialable(&ok(good), false), "{good}");
        }
        for bad in [
            "/ip4/127.0.0.1/tcp/22",
            "/ip4/10.0.0.5/tcp/4001",
            "/ip4/192.168.1.2/tcp/4001",
            "/ip4/169.254.169.254/tcp/80",
            "/ip4/100.64.0.1/tcp/4001",
            "/ip6/::1/tcp/4001",
            "/ip6/fe80::1/tcp/4001",
            "/dns4/atacante.example/tcp/443",
            "/dnsaddr/atacante.example",
            "/ip4/0.0.0.0/tcp/1",
        ] {
            assert!(!dialable(&ok(bad), false), "{bad}");
        }
        assert!(dialable(&ok("/ip4/127.0.0.1/tcp/1"), true));
        assert!(dialable(&ok("/dns4/relay.example/tcp/443"), true));
        assert!(!dialable(&ok("/dnsaddr/atacante.example"), true));
    }

    /// 3.5 etapa 5 (I2): um device de contato que só tem endereço recusado pelo filtro não é discado.
    #[tokio::test]
    async fn a_contacts_device_with_only_private_addresses_is_not_dialed() {
        let Household {
            mut bia,
            ana,
            ana_key,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        bia.trust(ana).unwrap();
        let device = DeviceKey::generate();
        let certificate = ana_key
            .authorize_device(
                &DeviceKey::generate().public_key(),
                &device.public_key(),
                &device.nostr_public_key(),
                1,
            )
            .unwrap();
        let private = Invite::new(certificate, vec!["/ip4/10.0.0.5/tcp/4001".parse().unwrap()]);

        let wire = device_list_wire(&mut ana_raw, vec![private.clone()]);
        bia.on_mls(ana_peer, &wire).unwrap();

        assert!(!bia.expected.contains_key(&private.peer_id()));
    }

    fn payload_of(raw: &mut Conversation, blob: &[u8]) -> Payload {
        match raw.decrypt(&mls_of(blob)).unwrap() {
            Decrypted::Message { plaintext, .. } => Payload::decode(&plaintext).unwrap(),
            other => panic!("esperava mensagem, veio {other:?}"),
        }
    }

    /// 3.5 etapa 4: ao enviar a um contato, cada device irmão recebe um espelho `Sent` com a MESMA
    /// mensagem, e o device do contato recebe a mensagem comum.
    #[tokio::test]
    async fn send_to_mirrors_the_message_to_my_other_devices_and_sends_it_plain_to_the_contact() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut sibling_raw),
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let relay = MemoryRelay::default();
        bia.set_mailbox(MemoryMailbox::new(&relay, bia.peer_id()));

        let id = bia.send_to(&ana, "oi, ana", None).unwrap();
        for _ in 0..2 {
            until(&mut bia, |e| {
                matches!(e, ChatEvent::Deposited { .. }).then_some(())
            })
            .await
            .expect("depósito confirmado");
        }

        let deposits = relay.deposits();
        assert_eq!(deposits.len(), 2);
        for deposit in &deposits {
            if deposit.to == sibling {
                let Payload::Sent { to, message } = payload_of(&mut sibling_raw, &deposit.blob)
                else {
                    panic!("o irmão deveria receber um espelho");
                };
                assert_eq!(
                    (to, message.id, message.text),
                    (ana.clone(), id, "oi, ana".into())
                );
            } else {
                assert_eq!(deposit.to, ana_peer);
                let Payload::Message(message) = payload_of(&mut ana_raw, &deposit.blob) else {
                    panic!("a Ana deveria receber a mensagem comum");
                };
                assert_eq!(message.id, id);
            }
        }
    }

    /// 3.5 etapa 4: o espelho espera na outbox do irmão, marcado com a identidade de destino.
    #[tokio::test]
    async fn the_mirror_waits_in_the_siblings_outbox_marked_with_the_destination() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, _),
            ana_device: (ana_peer, _),
            ..
        } = household();

        let id = bia.send_to(&ana, "oi", None).unwrap();

        assert_eq!((bia.queued(&sibling), bia.queued(&ana_peer)), (1, 1));
        assert_eq!(
            bia.outbox.entry_kind(&sibling, &id).unwrap(),
            EntryKind::Mirror(ana)
        );
        assert_eq!(
            bia.outbox.entry_kind(&ana_peer, &id).unwrap(),
            EntryKind::Plain
        );
    }

    /// P24: o ack de um device IRMÃO (espelho) só libera a fila: quem diz "entregue" é o device do
    /// destinatário. O ack do device do contato continua emitindo `Delivered`.
    #[tokio::test]
    async fn a_siblings_ack_frees_the_queue_without_reporting_delivery() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, _),
            ana_device: (ana_peer, _),
            ..
        } = household();
        connect_peer(&mut bia, sibling);
        let id = bia.send_to(&ana, "oi", None).unwrap();
        events(&mut bia);

        bia.handle(NodeEvent::MessageDelivered {
            peer: sibling,
            id: first_in_flight(&bia),
        });

        assert_eq!(bia.queued(&sibling), 0, "saiu da fila");
        let after_sibling = events(&mut bia);
        assert!(
            !after_sibling
                .iter()
                .any(|e| matches!(e, ChatEvent::Delivered { .. })),
            "irmão não é entrega: {after_sibling:?}"
        );

        connect_peer(&mut bia, ana_peer);
        bia.handle(NodeEvent::MessageDelivered {
            peer: ana_peer,
            id: first_in_flight(&bia),
        });
        assert!(
            events(&mut bia).contains(&ChatEvent::Delivered {
                peer: ana_peer,
                message: id
            }),
            "o contato entrega"
        );
    }

    /// P24: uma entrada de espelho com destino corrompido não trava a fila do peer: é apagada, avisada
    /// com `Dropped`, e a próxima mensagem sai.
    #[tokio::test]
    async fn a_corrupt_mirror_entry_is_dropped_and_does_not_block_the_queue() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, _),
            ..
        } = household();
        let broken = bia.send_to(&ana, "quebrada", None).unwrap();
        let healthy = bia.send_to(&ana, "boa", None).unwrap();
        bia.outbox.corrupt_destination(&sibling, &broken);
        events(&mut bia);

        connect_peer(&mut bia, sibling);

        let all = events(&mut bia);
        assert!(
            all.iter()
                .any(|e| matches!(e, ChatEvent::Dropped { peer, .. } if *peer == sibling)),
            "avisa: {all:?}"
        );
        assert_eq!(
            bia.outbox.entry_kind(&sibling, &broken).unwrap(),
            EntryKind::Missing,
            "a entrada ruim foi apagada"
        );
        assert_eq!(bia.queued(&sibling), 1, "só a boa resta");
        assert_eq!(in_flight(&bia), 1, "e ela já saiu");
        assert_eq!(bia.in_flight[&sibling].iter().next(), Some(&healthy));
    }

    /// P24: um espelho "para a minha própria identidade" nunca sai de um irmão honesto (`send_to` o
    /// pula); se chegar, é defeito ou ataque e não vira `SentElsewhere`.
    #[tokio::test]
    async fn a_mirror_addressed_to_my_own_identity_is_refused() {
        let Household {
            mut bia,
            bia_identity,
            sibling: (sibling, mut sibling_raw),
            ..
        } = household();
        let payload = Payload::Sent {
            to: bia_identity,
            message: Message::new("para mim".into(), None),
        };
        let wire = sibling_raw.encrypt(&payload.encode()).unwrap();

        let err = bia.on_mls(sibling, &wire).unwrap_err();

        assert!(matches!(err, Error::Malformed), "{err:?}");
        assert!(
            !events(&mut bia)
                .iter()
                .any(|e| matches!(e, ChatEvent::SentElsewhere { .. }))
        );
    }

    /// 3.5 etapa 4: mensagem para a própria identidade ("notas para mim") não é espelhada: o irmão
    /// já a recebe como mensagem comum, e um espelho a duplicaria.
    #[tokio::test]
    async fn sending_to_my_own_identity_is_not_mirrored() {
        let Household {
            mut bia,
            bia_identity,
            sibling: (sibling, _),
            ..
        } = household();

        let id = bia.send_to(&bia_identity, "nota", None).unwrap();

        assert_eq!(bia.queued(&sibling), 1, "uma cópia só");
        assert_eq!(
            bia.outbox.entry_kind(&sibling, &id).unwrap(),
            EntryKind::Plain
        );
        let queued = events(&mut bia)
            .into_iter()
            .filter(|e| matches!(e, ChatEvent::Queued { peer, .. } if *peer == sibling))
            .count();
        assert_eq!(queued, 1, "um aviso só, não um por alvo repetido");
    }

    /// 3.5 etapa 4: `send` a um device específico continua sendo só aquilo, sem espelho.
    #[tokio::test]
    async fn sending_to_a_single_device_does_not_mirror() {
        let Household {
            mut bia,
            sibling: (sibling, _),
            ana_device: (ana_peer, _),
            ..
        } = household();

        bia.send(ana_peer, "oi", None).unwrap();

        assert_eq!(bia.queued(&sibling), 0);
    }

    /// 3.5 etapa 4: um espelho vindo de um device da MINHA identidade vira `SentElsewhere`, não
    /// `Received`, e reentrega não duplica o evento.
    #[tokio::test]
    async fn a_mirror_from_my_own_device_becomes_sent_elsewhere_once() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut sibling_raw),
            ..
        } = household();
        let message = Message::new("oi, ana".into(), None);
        let payload = Payload::Sent {
            to: ana.clone(),
            message: message.clone(),
        };

        for _ in 0..2 {
            let wire = sibling_raw.encrypt(&payload.encode()).unwrap();
            bia.on_mls(sibling, &wire).unwrap();
        }

        assert_eq!(
            events(&mut bia),
            [ChatEvent::SentElsewhere {
                peer: sibling,
                to: ana,
                message
            }]
        );
    }

    fn history_texts(chat: &Chat, contact: &IdentityId) -> Vec<(bool, String)> {
        chat.history(contact, None, 100)
            .unwrap()
            .into_iter()
            .map(|entry| (entry.outgoing, entry.message.text))
            .collect()
    }

    /// P10a: a mensagem recebida de Ana entra no histórico de Ana, uma vez, mesmo que o transporte a
    /// reentregue (outros bytes cifrados, mesmo `MessageId`).
    #[tokio::test]
    async fn a_received_message_is_recorded_under_the_sender_identity_once() {
        let Household {
            mut bia,
            ana,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let message = Message::new("oi".into(), None);

        for _ in 0..2 {
            let wire = ana_raw.encrypt(&message.encode()).unwrap();
            bia.on_mls(ana_peer, &wire).unwrap();
        }

        let history = bia.history(&ana, None, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert!(!history[0].outgoing);
        assert_eq!(history[0].message, message);
    }

    /// P10a: "notas para mim" vindas de um irmão ficam sob a minha própria identidade.
    #[tokio::test]
    async fn a_message_from_a_sibling_is_recorded_under_my_own_identity() {
        let Household {
            mut bia,
            bia_identity,
            sibling: (sibling, mut sibling_raw),
            ..
        } = household();

        let wire = sibling_raw
            .encrypt(&Message::new("nota".into(), None).encode())
            .unwrap();
        bia.on_mls(sibling, &wire).unwrap();

        assert_eq!(
            history_texts(&bia, &bia_identity),
            [(false, "nota".to_string())]
        );
    }

    /// P10a: `send_to` põe uma cópia na fila de cada device de Ana, mas o histórico guarda a mensagem
    /// lógica uma vez, e só na conversa de Ana.
    #[tokio::test]
    async fn sending_to_an_identity_records_one_line_not_one_per_device() {
        let (mut bia, ana, [_, _], (_, _)) = bia_with_two_ana_devices_and_one_cleo();
        let stranger = StandaloneIdentity::generate().id();

        bia.send_to(&ana, "tudo bem?", None).unwrap();

        assert_eq!(history_texts(&bia, &ana), [(true, "tudo bem?".to_string())]);
        assert!(history_texts(&bia, &stranger).is_empty());
    }

    /// P26a: o teto vale pela API do `Chat`: depois de 5 envios com teto 3, o histórico tem os 3 últimos.
    #[tokio::test]
    async fn chat_history_respects_the_configured_limit() {
        let Household { mut bia, ana, .. } = household();
        bia.set_history_limit(std::num::NonZeroUsize::new(3));

        for n in 1..=5 {
            bia.send_to(&ana, format!("msg {n}"), None).unwrap();
        }

        assert_eq!(
            history_texts(&bia, &ana),
            [
                (true, "msg 3".to_string()),
                (true, "msg 4".to_string()),
                (true, "msg 5".to_string())
            ]
        );
    }

    /// P26a: um irmão que importa mais linhas que o teto fica com as últimas, e o cursor salvo é o
    /// `next` da página (podar não faz puxar de novo).
    #[tokio::test]
    async fn importing_from_a_sibling_keeps_the_newest_within_the_limit_and_the_cursor() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut raw),
            ..
        } = household();
        bia.set_history_limit(std::num::NonZeroUsize::new(3));
        asking(&mut bia, sibling);
        let items = (1..=5)
            .map(|n| history_item(&ana, true, &format!("h{n}")))
            .collect();

        let wire = chunk_wire(&mut raw, items, 5, true);
        bia.on_mls(sibling, &wire).unwrap();

        assert_eq!(
            history_texts(&bia, &ana),
            [
                (true, "h3".to_string()),
                (true, "h4".to_string()),
                (true, "h5".to_string())
            ]
        );
        assert_eq!(bia.outbox.history_cursor(&sibling).unwrap(), 5);
    }

    /// P10a: `send` a um device específico grava sob a identidade dele.
    #[tokio::test]
    async fn sending_to_a_single_device_records_under_its_identity() {
        let Household {
            mut bia,
            ana,
            ana_device: (ana_peer, _),
            ..
        } = household();

        bia.send(ana_peer, "oi", None).unwrap();

        assert_eq!(history_texts(&bia, &ana), [(true, "oi".to_string())]);
    }

    /// P10a: o espelho de um irmão ("enviei isto a Ana") entra como enviada, uma vez, mesmo reentregue.
    #[tokio::test]
    async fn a_mirror_from_a_sibling_is_recorded_as_sent_once() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut sibling_raw),
            ..
        } = household();
        let payload = Payload::Sent {
            to: ana.clone(),
            message: Message::new("e aí".into(), None),
        };

        for _ in 0..2 {
            let wire = sibling_raw.encrypt(&payload.encode()).unwrap();
            bia.on_mls(sibling, &wire).unwrap();
        }

        assert_eq!(history_texts(&bia, &ana), [(true, "e aí".to_string())]);
    }

    /// P10a: a mesma mensagem lógica por dois caminhos (eu a enviei e um irmão a espelha com o mesmo
    /// id) vale uma linha.
    #[tokio::test]
    async fn the_same_message_by_direct_send_and_by_mirror_is_one_line() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut sibling_raw),
            ..
        } = household();
        bia.send_to(&ana, "uma só", None).unwrap();
        let message = bia.history(&ana, None, 10).unwrap()[0].message.clone();

        let wire = sibling_raw
            .encrypt(
                &Payload::Sent {
                    to: ana.clone(),
                    message,
                }
                .encode(),
            )
            .unwrap();
        bia.on_mls(sibling, &wire).unwrap();

        assert_eq!(history_texts(&bia, &ana), [(true, "uma só".to_string())]);
    }

    /// P10a (segurança): um espelho forjado por um contato é recusado e NÃO entra no histórico
    /// ("você enviou isto" falso).
    #[tokio::test]
    async fn a_forged_mirror_does_not_enter_the_history() {
        let Household {
            mut bia,
            ana,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let forged = Payload::Sent {
            to: ana.clone(),
            message: Message::new("forjada".into(), None),
        };
        let wire = ana_raw.encrypt(&forged.encode()).unwrap();

        assert!(bia.on_mls(ana_peer, &wire).is_err());

        assert!(history_texts(&bia, &ana).is_empty());
    }

    // Sincronização do histórico entre irmãos (P10a, fatia 2)

    fn chunk_wire(
        raw: &mut Conversation,
        items: Vec<HistoryItem>,
        next: u64,
        done: bool,
    ) -> Vec<u8> {
        raw.encrypt(&Payload::HistoryChunk { items, next, done }.encode())
            .unwrap()
    }

    fn history_item(contact: &IdentityId, outgoing: bool, text: &str) -> HistoryItem {
        HistoryItem {
            contact: contact.clone(),
            outgoing,
            message: Message::new(text.into(), None),
        }
    }

    fn synced(events: &[ChatEvent]) -> Vec<(usize, bool)> {
        events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::HistorySynced { imported, done, .. } => Some((*imported, *done)),
                _ => None,
            })
            .collect()
    }

    /// Bia pede o histórico ao irmão (conectado) e fica à espera da página.
    fn asking(bia: &mut Chat, sibling: PeerId) {
        bia.connected.insert(sibling);
        bia.request_history(sibling);
    }

    /// P10a: `announce_devices` a um irmão também pede o histórico (o mesmo ponto: o outro lado já
    /// tem a conversa); a um contato, não.
    #[tokio::test]
    async fn announcing_devices_to_a_sibling_also_asks_for_its_history() {
        let Household {
            mut bia,
            sibling: (sibling, _),
            ..
        } = household();
        bia.connected.insert(sibling);
        let before = bia.pending.len();

        bia.announce_devices(sibling);

        assert_eq!(bia.pending.len(), before + 2, "lista de devices + pedido");
    }

    /// P10a: só se pede histórico a irmão, e uma vez por conexão.
    #[tokio::test]
    async fn history_is_asked_of_siblings_only_and_once_per_connection() {
        let Household {
            mut bia,
            sibling: (sibling, _),
            ana_device: (ana_peer, _),
            ..
        } = household();
        bia.connected.insert(ana_peer);
        let before = bia.pending.len();
        bia.request_history(ana_peer);
        assert_eq!(bia.pending.len(), before, "contato: nada");

        asking(&mut bia, sibling);
        assert_eq!(bia.pending.len(), before + 1);
        bia.request_history(sibling);
        assert_eq!(bia.pending.len(), before + 1, "a mesma conexão: uma vez");

        bia.handle(NodeEvent::PeerDisconnected(sibling));
        bia.connected.insert(sibling);
        bia.request_history(sibling);
        assert_eq!(bia.pending.len(), before + 2, "nova conexão: pede de novo");
    }

    /// P10a: a página de um irmão que eu pedi é importada, cada linha sob o contato e a direção certos,
    /// o cursor é gravado e o evento chega.
    #[tokio::test]
    async fn a_chunk_from_a_sibling_i_asked_is_imported_and_the_cursor_saved() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut raw),
            ..
        } = household();
        let cleo = StandaloneIdentity::generate().id();
        asking(&mut bia, sibling);
        events(&mut bia);

        let wire = chunk_wire(
            &mut raw,
            vec![
                history_item(&ana, true, "enviei a ana"),
                history_item(&cleo, false, "recebi de cleo"),
            ],
            5,
            true,
        );
        bia.on_mls(sibling, &wire).unwrap();

        assert_eq!(
            history_texts(&bia, &ana),
            [(true, "enviei a ana".to_string())]
        );
        assert_eq!(
            history_texts(&bia, &cleo),
            [(false, "recebi de cleo".to_string())]
        );
        assert_eq!(synced(&events(&mut bia)), [(2, true)]);
        assert_eq!(bia.outbox.history_cursor(&sibling).unwrap(), 5);
    }

    /// P10a: o que eu já tinha não é importado de novo nem conta no evento.
    #[tokio::test]
    async fn lines_i_already_have_are_not_imported_again() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut raw),
            ..
        } = household();
        let known = history_item(&ana, false, "já tinha");
        bia.outbox
            .record_history(&known.contact, known.outgoing, &known.message)
            .unwrap();
        asking(&mut bia, sibling);
        events(&mut bia);

        let wire = chunk_wire(
            &mut raw,
            vec![known, history_item(&ana, true, "nova")],
            2,
            true,
        );
        bia.on_mls(sibling, &wire).unwrap();

        assert_eq!(bia.history(&ana, None, 10).unwrap().len(), 2);
        assert_eq!(synced(&events(&mut bia)), [(1, true)]);
    }

    /// P10a: página que não é final faz pedir a seguinte; a final encerra a sessão.
    #[tokio::test]
    async fn a_non_final_chunk_asks_for_the_next_page_and_the_final_one_ends_the_session() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut raw),
            ..
        } = household();
        asking(&mut bia, sibling);
        // Anuncia antes: assim o `on_mls` não anuncia por conta própria e só o pedido seguinte é contado.
        bia.announce_devices(sibling);
        let after_request = bia.pending.len();

        let first = chunk_wire(&mut raw, vec![history_item(&ana, true, "p1")], 3, false);
        bia.on_mls(sibling, &first).unwrap();
        assert_eq!(bia.pending.len(), after_request + 1, "pediu a seguinte");

        let last = chunk_wire(&mut raw, vec![history_item(&ana, true, "p2")], 6, true);
        bia.on_mls(sibling, &last).unwrap();
        assert_eq!(
            bia.pending.len(),
            after_request + 1,
            "a final não pede mais"
        );

        let late = chunk_wire(&mut raw, vec![history_item(&ana, true, "p3")], 9, true);
        assert!(matches!(
            bia.on_mls(sibling, &late),
            Err(Error::UnauthorizedHistory)
        ));
        assert_eq!(
            history_texts(&bia, &ana),
            [(true, "p1".to_string()), (true, "p2".to_string())]
        );
        assert_eq!(bia.outbox.history_cursor(&sibling).unwrap(), 6);
    }

    fn all_sends(bia: &Chat) -> std::collections::HashSet<SendId> {
        bia.pending.keys().copied().collect()
    }

    /// Os envios que saíram depois de `before`.
    fn sent_since(bia: &Chat, before: &std::collections::HashSet<SendId>) -> Vec<SendId> {
        bia.pending
            .keys()
            .copied()
            .filter(|id| !before.contains(id))
            .collect()
    }

    /// P27a: o envio do `HistoryRequest` falha com o irmão conectado: a sessão fecha e o estado do
    /// pedido é liberado; o próximo `announce_devices` pede de novo.
    #[tokio::test]
    async fn a_failed_history_request_is_asked_again_on_the_next_announce() {
        let Household {
            mut bia,
            sibling: (sibling, _),
            ..
        } = household();
        let before = all_sends(&bia);
        asking(&mut bia, sibling);
        let request = sent_since(&bia, &before);
        assert_eq!(request.len(), 1, "só o pedido saiu");

        fail_send(&mut bia, sibling, request[0]);

        assert!(!bia.history_requested.contains(&sibling));
        assert!(!bia.history_sessions.contains_key(&sibling));
        bia.announce_devices(sibling);
        assert!(bia.history_requested.contains(&sibling), "pediu de novo");
        assert!(bia.history_sessions.contains_key(&sibling));
    }

    /// P27a: a falha de outro envio de controle (o anúncio de devices) não fecha a sessão de histórico.
    #[tokio::test]
    async fn a_failed_device_list_does_not_close_the_history_session() {
        let Household {
            mut bia,
            sibling: (sibling, _),
            ..
        } = household();
        asking(&mut bia, sibling);
        let before = all_sends(&bia);
        bia.announce_devices(sibling);
        let list = sent_since(&bia, &before);
        assert_eq!(list.len(), 1, "só o anúncio saiu (o pedido já existe)");

        fail_send(&mut bia, sibling, list[0]);

        assert!(bia.history_sessions.contains_key(&sibling));
        assert!(bia.history_requested.contains(&sibling));
    }

    /// P27a: o pedido da PRÓXIMA página também pode se perder; a sessão fecha, o cursor salvo fica e o
    /// próximo gatilho pede de novo a partir dele.
    #[tokio::test]
    async fn a_failed_next_page_request_closes_the_session_and_resumes_from_the_saved_cursor() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut raw),
            ..
        } = household();
        asking(&mut bia, sibling);
        // Anuncia antes: assim o `on_mls` não anuncia por conta própria e só o pedido seguinte sai.
        bia.announce_devices(sibling);
        let before = all_sends(&bia);
        let first = chunk_wire(&mut raw, vec![history_item(&ana, true, "p1")], 3, false);
        bia.on_mls(sibling, &first).unwrap();
        let next_request = sent_since(&bia, &before);
        assert_eq!(next_request.len(), 1, "pediu a seguinte");

        fail_send(&mut bia, sibling, next_request[0]);

        assert!(!bia.history_sessions.contains_key(&sibling));
        assert_eq!(bia.outbox.history_cursor(&sibling).unwrap(), 3);
        bia.announce_devices(sibling);
        assert!(bia.history_sessions.contains_key(&sibling), "retomou");
        assert_eq!(bia.outbox.history_cursor(&sibling).unwrap(), 3);
    }

    /// P27a: o id do pedido sai da lista quando ele é entregue (não vaza).
    #[tokio::test]
    async fn a_delivered_history_request_leaves_the_in_flight_list() {
        let Household {
            mut bia,
            sibling: (sibling, _),
            ..
        } = household();
        let before = all_sends(&bia);
        asking(&mut bia, sibling);
        let request = sent_since(&bia, &before)[0];
        assert!(bia.history_requests.contains_key(&request));

        bia.handle(NodeEvent::MessageDelivered {
            peer: sibling,
            id: request,
        });

        assert!(bia.history_requests.is_empty());
    }

    /// P27a: ao desconectar, os ids de pedido em voo daquele peer saem da lista.
    #[tokio::test]
    async fn disconnecting_forgets_the_in_flight_history_requests() {
        let Household {
            mut bia,
            sibling: (sibling, _),
            ..
        } = household();
        asking(&mut bia, sibling);
        assert!(!bia.history_requests.is_empty());

        bia.handle(NodeEvent::PeerDisconnected(sibling));

        assert!(bia.history_requests.is_empty());
    }

    /// P27d: a checagem de `sender` do `HistoryChunk` vale por si, não só pela de sessão: com a sessão
    /// aberta, uma página assinada por OUTRA identidade é recusada e nada é gravado.
    #[tokio::test]
    async fn a_chunk_from_another_identity_is_refused_even_with_the_session_open() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, _),
            ..
        } = household();
        asking(&mut bia, sibling);
        let stranger = StandaloneIdentity::generate().id();

        let result = bia.on_history_chunk(
            sibling,
            &stranger,
            vec![history_item(&ana, true, "intrusa")],
            5,
            true,
        );

        assert!(matches!(result, Err(Error::UnauthorizedHistory)));
        assert!(history_texts(&bia, &ana).is_empty());
        assert_eq!(bia.outbox.history_cursor(&sibling).unwrap(), 0);
        assert!(bia.history_sessions.contains_key(&sibling));
    }

    /// P10a (segurança): uma página que eu não pedi é recusada e nada é gravado.
    #[tokio::test]
    async fn a_chunk_i_did_not_ask_for_is_refused() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut raw),
            ..
        } = household();

        let wire = chunk_wire(&mut raw, vec![history_item(&ana, true, "intrusa")], 1, true);

        assert!(matches!(
            bia.on_mls(sibling, &wire),
            Err(Error::UnauthorizedHistory)
        ));
        assert!(history_texts(&bia, &ana).is_empty());
    }

    /// P10a (segurança): uma página de um CONTATO é recusada mesmo com um pedido em voo para o irmão.
    #[tokio::test]
    async fn a_chunk_from_a_contact_is_refused() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, _),
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        asking(&mut bia, sibling);

        let wire = chunk_wire(
            &mut ana_raw,
            vec![history_item(&ana, true, "forjada")],
            1,
            true,
        );

        assert!(matches!(
            bia.on_mls(ana_peer, &wire),
            Err(Error::UnauthorizedHistory)
        ));
        assert!(history_texts(&bia, &ana).is_empty());
    }

    /// P10a (segurança): um CONTATO pedindo histórico é recusado e nada é enviado.
    #[tokio::test]
    async fn a_history_request_from_a_contact_is_refused_and_nothing_is_sent() {
        let Household {
            mut bia,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        bia.outbox
            .record_history(
                &StandaloneIdentity::generate().id(),
                true,
                &Message::new("segredo".into(), None),
            )
            .unwrap();
        let before = bia.pending.len();

        let wire = ana_raw
            .encrypt(&Payload::HistoryRequest { cursor: 0 }.encode())
            .unwrap();

        assert!(matches!(
            bia.on_mls(ana_peer, &wire),
            Err(Error::UnauthorizedHistory)
        ));
        assert_eq!(bia.pending.len(), before, "nada enviado");
    }

    /// P10a: o pedido de um irmão é respondido (uma página enviada).
    #[tokio::test]
    async fn a_history_request_from_a_sibling_is_answered() {
        let Household {
            mut bia,
            sibling: (sibling, mut raw),
            ..
        } = household();
        bia.outbox
            .record_history(
                &StandaloneIdentity::generate().id(),
                true,
                &Message::new("a enviar".into(), None),
            )
            .unwrap();
        let before = bia.pending.len();

        let wire = raw
            .encrypt(&Payload::HistoryRequest { cursor: 0 }.encode())
            .unwrap();
        bia.on_mls(sibling, &wire).unwrap();

        assert_eq!(bia.pending.len(), before + 1);
    }

    /// P10a: o que o respondente monta: todas as conversas, na ordem, e o cursor devolvido avança.
    #[tokio::test]
    async fn the_page_i_serve_has_every_conversation_in_order_and_resumes_after_next() {
        let Household { bia, .. } = household();
        let (ana, cleo) = (
            StandaloneIdentity::generate().id(),
            StandaloneIdentity::generate().id(),
        );
        for (contact, outgoing, text) in [
            (&ana, true, "um"),
            (&cleo, false, "dois"),
            (&ana, true, "tres"),
        ] {
            bia.outbox
                .record_history(contact, outgoing, &Message::new(text.into(), None))
                .unwrap();
        }

        let Payload::HistoryChunk { items, next, done } = bia.serve_history(0).unwrap() else {
            panic!("esperava uma página");
        };

        let texts: Vec<&str> = items.iter().map(|i| i.message.text.as_str()).collect();
        assert_eq!(texts, ["um", "dois", "tres"]);
        assert!(done);
        assert_eq!(next, bia.outbox.max_history_seq().unwrap());
        assert_eq!(items[1].contact, cleo);
        assert!(!items[1].outgoing);
    }

    /// P10a (anti-laço): uma página que não é a final e não traz nenhum item encerra a sessão (senão um
    /// irmão comprometido giraria o laço de pedidos sem fim, sem gastar o teto de linhas).
    #[tokio::test]
    async fn a_non_final_chunk_without_items_stops_the_session() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut raw),
            ..
        } = household();
        asking(&mut bia, sibling);

        let empty = chunk_wire(&mut raw, vec![], 5, false);

        assert!(matches!(
            bia.on_mls(sibling, &empty),
            Err(Error::HistoryLimit(_))
        ));
        let later = chunk_wire(&mut raw, vec![history_item(&ana, true, "depois")], 9, true);
        assert!(
            matches!(bia.on_mls(sibling, &later), Err(Error::UnauthorizedHistory)),
            "a sessão acabou"
        );
        assert!(history_texts(&bia, &ana).is_empty());
    }

    /// P10a: se o irmão recriou o banco, ele serve do começo e o `next` volta a ser menor que o cursor
    /// que eu tinha salvo; isso é aceito e o cursor passa a ser o novo.
    #[tokio::test]
    async fn a_chunk_below_the_saved_cursor_is_accepted_when_the_sibling_restarted() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut raw),
            ..
        } = household();
        bia.outbox.set_history_cursor(&sibling, 42).unwrap();
        asking(&mut bia, sibling);

        let wire = chunk_wire(&mut raw, vec![history_item(&ana, true, "do zero")], 3, true);
        bia.on_mls(sibling, &wire).unwrap();

        assert_eq!(history_texts(&bia, &ana), [(true, "do zero".to_string())]);
        assert_eq!(bia.outbox.history_cursor(&sibling).unwrap(), 3);
    }

    /// P10a: com o cursor salvo em 42, uma página que avança (43) é aceita.
    #[tokio::test]
    async fn a_chunk_that_advances_from_the_saved_cursor_is_accepted() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut raw),
            ..
        } = household();
        bia.outbox.set_history_cursor(&sibling, 42).unwrap();
        asking(&mut bia, sibling);

        let wire = chunk_wire(&mut raw, vec![history_item(&ana, true, "novo")], 43, true);
        bia.on_mls(sibling, &wire).unwrap();

        assert_eq!(history_texts(&bia, &ana), [(true, "novo".to_string())]);
        assert_eq!(bia.outbox.history_cursor(&sibling).unwrap(), 43);
    }

    /// P10a (teto): passar do teto de linhas por sessão para a sincronização.
    #[tokio::test]
    async fn the_session_stops_when_it_passes_the_per_session_cap() {
        let Household {
            mut bia,
            ana,
            sibling: (sibling, mut raw),
            ..
        } = household();
        bia.history_cap = 3;
        asking(&mut bia, sibling);

        let two = chunk_wire(
            &mut raw,
            vec![history_item(&ana, true, "a"), history_item(&ana, true, "b")],
            2,
            false,
        );
        bia.on_mls(sibling, &two).unwrap();
        let over = chunk_wire(
            &mut raw,
            vec![history_item(&ana, true, "c"), history_item(&ana, true, "d")],
            4,
            false,
        );

        assert!(matches!(
            bia.on_mls(sibling, &over),
            Err(Error::HistoryLimit(_))
        ));
        assert_eq!(history_texts(&bia, &ana).len(), 2, "o excedente não entra");
    }

    // ---- P30 3c-i: tombstone de device recebido pelo canal MLS ----

    /// Um tombstone (da mestra, device v3) de um device novo de `owner`, e o certificado revogado.
    fn revocation_by(owner: &StandaloneIdentity) -> (DeviceRevocation, DeviceCertificate) {
        let device = DeviceKey::generate();
        let certificate = owner
            .authorize_device(
                &DeviceKey::generate().public_key(),
                &device.public_key(),
                &device.nostr_public_key(),
                1,
            )
            .unwrap();
        (owner.revoke_device(&certificate, 2).unwrap(), certificate)
    }

    fn revocation_wire(raw: &mut Conversation, revocation: &DeviceRevocation) -> Vec<u8> {
        raw.encrypt(&Payload::Revocation(Box::new(revocation.clone())).encode())
            .unwrap()
    }

    /// A identidade da própria Bia: o tombstone de um device dela (vindo de um irmão) é gravado.
    #[tokio::test]
    async fn a_revocation_of_my_own_identity_is_stored() {
        let Household {
            mut bia,
            bia_key,
            sibling: (sibling_peer, mut sibling_raw),
            ..
        } = household();
        let (revocation, certificate) = revocation_by(&bia_key);

        bia.on_mls(
            sibling_peer,
            &revocation_wire(&mut sibling_raw, &revocation),
        )
        .unwrap();

        assert!(bia.device.is_revoked(&certificate).unwrap());
        assert!(events(&mut bia).is_empty());
    }

    /// Auditoria 3c-i, C1: ter conversa não basta. Qualquer estranho que conecte ganha uma conversa MLS (o
    /// handshake aceita qualquer identidade com certificado válido), então "conversa" não filtra ninguém e
    /// deixaria gravar tombstones sem limite no disco. A Ana tem conversa, mas a Bia não a aceitou.
    #[tokio::test]
    async fn a_revocation_of_an_identity_i_only_have_a_conversation_with_is_refused() {
        let Household {
            mut bia,
            ana_key,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let (revocation, certificate) = revocation_by(&ana_key);

        let err = bia
            .on_mls(ana_peer, &revocation_wire(&mut ana_raw, &revocation))
            .unwrap_err();

        assert!(matches!(err, Error::UnauthorizedRevocation), "{err:?}");
        assert!(!bia.device.is_revoked(&certificate).unwrap());
    }

    /// Auditoria 3c-i, I3: `untrust` corta a admissão (a conversa continua, mas o tombstone não entra).
    #[tokio::test]
    async fn after_untrust_a_revocation_of_that_identity_is_refused() {
        let Household {
            mut bia,
            ana_key,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        bia.trust(ana_key.id()).unwrap();
        bia.untrust(&ana_key.id()).unwrap();
        let (revocation, certificate) = revocation_by(&ana_key);

        let err = bia
            .on_mls(ana_peer, &revocation_wire(&mut ana_raw, &revocation))
            .unwrap_err();

        assert!(matches!(err, Error::UnauthorizedRevocation), "{err:?}");
        assert!(!bia.device.is_revoked(&certificate).unwrap());
    }

    /// O tombstone assinado por uma autoridade (v4) também é gravado pelo caminho do `chat`.
    #[tokio::test]
    async fn a_revocation_signed_by_an_authority_of_a_trusted_identity_is_stored() {
        let Household {
            mut bia,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let carol = StandaloneIdentity::generate();
        bia.trust(carol.id()).unwrap();
        let key = AuthorityKey::generate();
        let authority_cert = carol.certify_authority(&key.public_key(), 1).unwrap();
        let authority = AuthorityIdentity::new(key, authority_cert).unwrap();
        let device = DeviceKey::generate();
        let certificate = authority
            .authorize_device(
                &DeviceKey::generate().public_key(),
                &device.public_key(),
                &device.nostr_public_key(),
                1,
            )
            .unwrap();
        let revocation = authority.revoke_device(&certificate, 2).unwrap();

        bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, &revocation))
            .unwrap();

        assert!(bia.device.is_revoked(&certificate).unwrap());
    }

    /// Repasse: a Ana entrega o tombstone de uma terceira identidade que a Bia aceitou (`trust`). O
    /// remetente não é o dono da identidade, e mesmo assim vale.
    #[tokio::test]
    async fn a_revocation_of_a_trusted_identity_relayed_by_someone_else_is_stored() {
        let Household {
            mut bia,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let carol = StandaloneIdentity::generate();
        bia.trust(carol.id()).unwrap();
        let (revocation, certificate) = revocation_by(&carol);

        bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, &revocation))
            .unwrap();

        assert!(bia.device.is_revoked(&certificate).unwrap());
    }

    /// Admissão pela identidade do tombstone, não pelo remetente: a Ana é conhecida, mas o tombstone é de
    /// uma identidade que a Bia não aceitou e com quem não conversa.
    #[tokio::test]
    async fn a_revocation_of_an_unknown_identity_is_refused_and_not_stored() {
        let Household {
            mut bia,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let stranger = StandaloneIdentity::generate();
        let (revocation, certificate) = revocation_by(&stranger);

        let err = bia
            .on_mls(ana_peer, &revocation_wire(&mut ana_raw, &revocation))
            .unwrap_err();

        assert!(matches!(err, Error::UnauthorizedRevocation), "{err:?}");
        assert!(!bia.device.is_revoked(&certificate).unwrap());
    }

    /// Identidade conhecida, mas com a assinatura adulterada: não grava.
    #[tokio::test]
    async fn a_revocation_with_a_bad_signature_is_not_stored() {
        let Household {
            mut bia,
            ana_key,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        bia.trust(ana_key.id()).unwrap();
        let (mut revocation, certificate) = revocation_by(&ana_key);
        revocation.signature[0] ^= 1;

        let result = bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, &revocation));

        // Passa na admissão e no limite e cai na conferência da assinatura (erro do `crypto`).
        assert!(matches!(result, Err(Error::Crypto(_))), "{result:?}");
        assert!(!bia.device.is_revoked(&certificate).unwrap());
    }

    /// O mesmo tombstone duas vezes não dá erro e continua revogado.
    #[tokio::test]
    async fn receiving_the_same_revocation_twice_is_harmless() {
        let Household {
            mut bia,
            ana_key,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        bia.trust(ana_key.id()).unwrap();
        let (revocation, certificate) = revocation_by(&ana_key);

        for _ in 0..2 {
            bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, &revocation))
                .unwrap();
        }

        assert!(bia.device.is_revoked(&certificate).unwrap());
    }

    /// Limite por peer: os 16 primeiros passam; o 17º (mesmo sendo repetição de um já gravado) é descartado
    /// e não grava; reconectar NÃO devolve a cota (auditoria 3c-i, I2); outro peer não é afetado; e com a
    /// janela vencida o mesmo peer volta a passar.
    #[tokio::test]
    async fn a_peer_over_the_revocation_limit_is_dropped_and_others_are_not() {
        let Household {
            mut bia,
            ana_key,
            sibling: (sibling_peer, mut sibling_raw),
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        bia.trust(ana_key.id()).unwrap();
        let all: Vec<_> = (0..REVOCATIONS_PER_WINDOW)
            .map(|_| revocation_by(&ana_key))
            .collect();
        for (revocation, _) in &all {
            bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, revocation))
                .unwrap();
        }
        let (extra, extra_certificate) = revocation_by(&ana_key);

        let err = bia
            .on_mls(ana_peer, &revocation_wire(&mut ana_raw, &extra))
            .unwrap_err();
        assert!(matches!(err, Error::RevocationRateLimited), "{err:?}");
        assert!(!bia.device.is_revoked(&extra_certificate).unwrap());
        // Repetir um já gravado também conta (a assinatura custa CPU).
        let again = bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, &all[0].0));
        assert!(
            matches!(again, Err(Error::RevocationRateLimited)),
            "{again:?}"
        );

        // Reconectar não zera a cota (senão 16 por conexão = inundação por reconexão).
        bia.handle(NodeEvent::PeerDisconnected(ana_peer));
        let after = bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, &all[0].0));
        assert!(
            matches!(after, Err(Error::RevocationRateLimited)),
            "{after:?}"
        );

        // Outro peer tem a sua própria cota.
        bia.on_mls(sibling_peer, &revocation_wire(&mut sibling_raw, &extra))
            .unwrap();
        assert!(bia.device.is_revoked(&extra_certificate).unwrap());

        // Janela vencida: o mesmo peer volta a passar.
        bia.revocation_window = Duration::ZERO;
        bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, &all[0].0))
            .unwrap();
    }

    /// Teto global (auditoria 3c-i, C1/I2): 4 peers com 16 cada esgotam os 64 da janela; o primeiro
    /// tombstone de um 5º peer, que sozinho estaria dentro da cota dele, é descartado. Com a janela
    /// vencida volta a passar.
    #[tokio::test]
    async fn the_revocation_limit_is_also_global_across_peers() {
        let Household { mut bia, .. } = household();
        let carol = StandaloneIdentity::generate();
        bia.trust(carol.id()).unwrap();
        let mut senders: Vec<_> = (0..5)
            .map(|_| add_device_of(&mut bia, &StandaloneIdentity::generate()))
            .collect();
        for (peer, raw) in &mut senders[..4] {
            for _ in 0..REVOCATIONS_PER_WINDOW {
                let (revocation, _) = revocation_by(&carol);
                bia.on_mls(*peer, &revocation_wire(raw, &revocation))
                    .unwrap();
            }
        }
        let (fifth, fifth_raw) = &mut senders[4];
        let (revocation, certificate) = revocation_by(&carol);

        let err = bia
            .on_mls(*fifth, &revocation_wire(fifth_raw, &revocation))
            .unwrap_err();

        assert!(matches!(err, Error::RevocationRateLimited), "{err:?}");
        assert!(!bia.device.is_revoked(&certificate).unwrap());
        bia.revocation_window = Duration::ZERO;
        bia.on_mls(*fifth, &revocation_wire(fifth_raw, &revocation))
            .unwrap();
        assert!(bia.device.is_revoked(&certificate).unwrap());
    }

    /// Auditoria 3c-i, gap 4: o tombstone recusado pela admissão também gasta a cota (a conta vem antes da
    /// admissão, para o estranho não inundar de graça), e o excedente dele é `RateLimited`, não `Unauthorized`.
    #[tokio::test]
    async fn refused_revocations_also_spend_the_quota() {
        let Household {
            mut bia,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let stranger = StandaloneIdentity::generate();
        for _ in 0..REVOCATIONS_PER_WINDOW {
            let (revocation, _) = revocation_by(&stranger);
            let err = bia
                .on_mls(ana_peer, &revocation_wire(&mut ana_raw, &revocation))
                .unwrap_err();
            assert!(matches!(err, Error::UnauthorizedRevocation), "{err:?}");
        }
        let (revocation, _) = revocation_by(&stranger);

        let err = bia
            .on_mls(ana_peer, &revocation_wire(&mut ana_raw, &revocation))
            .unwrap_err();

        assert!(matches!(err, Error::RevocationRateLimited), "{err:?}");
    }

    // ---- P30 3c-ii: emitir e repassar (ADR 0025) ----

    /// A Bia com cinco devices alheios: o irmão, a Ana e o Dan (aceitos), conectados; um estranho (sem
    /// `trust`), conectado; e uma Carol aceita que **não** está conectada.
    struct Fanout {
        bia: Chat,
        bia_key: StandaloneIdentity,
        ana_key: StandaloneIdentity,
        carol_key: StandaloneIdentity,
        sibling: RawDevice,
        ana: RawDevice,
        dan: RawDevice,
        stranger: RawDevice,
        carol: RawDevice,
    }

    fn fanout() -> Fanout {
        let Household {
            mut bia,
            bia_key,
            ana_key,
            sibling,
            ana_device: ana,
            ..
        } = household();
        bia.trust(ana_key.id()).unwrap();
        let dan_key = StandaloneIdentity::generate();
        bia.trust(dan_key.id()).unwrap();
        let dan = add_device_of(&mut bia, &dan_key);
        let stranger = add_device_of(&mut bia, &StandaloneIdentity::generate());
        let carol_key = StandaloneIdentity::generate();
        bia.trust(carol_key.id()).unwrap();
        let carol = add_device_of(&mut bia, &carol_key);
        for peer in [sibling.0, ana.0, dan.0, stranger.0] {
            bia.connected.insert(peer);
        }
        Fanout {
            bia,
            bia_key,
            ana_key,
            carol_key,
            sibling,
            ana,
            dan,
            stranger,
            carol,
        }
    }

    fn peer_set(peers: Vec<PeerId>) -> std::collections::HashSet<PeerId> {
        peers.into_iter().collect()
    }

    /// Destinos de um tombstone da identidade `I` (auditoria 3c-ii, I1): só quem **aceita I**, porque o
    /// receptor admite por `is_trusted(I)`. Irmão sempre; device da própria `I` (aceita a si mesma); e,
    /// se `I` é a minha, os contatos que aceitei. Tombstone de TERCEIRA identidade não vai a outros
    /// contatos (revelaria em quem eu confio e gastaria a cota de quem a recusa). Sem o remetente.
    #[tokio::test]
    async fn revocation_targets_are_only_those_who_accept_the_identity_except_the_sender() {
        let f = fanout();
        let (bia, ana, carol) = (f.bia_key.id(), f.ana_key.id(), f.carol_key.id());
        let (sibling, ana_peer, dan) = (f.sibling.0, f.ana.0, f.dan.0);

        // Minha identidade: irmão e os contatos aceitos conectados (Ana e Dan); sem o estranho nem a Carol
        // (desconectada).
        assert_eq!(
            peer_set(f.bia.revocation_targets(&bia, None)),
            peer_set(vec![sibling, ana_peer, dan])
        );
        assert_eq!(
            peer_set(f.bia.revocation_targets(&bia, Some(sibling))),
            peer_set(vec![ana_peer, dan])
        );
        // Identidade da Ana (terceira para os outros contatos): o irmão e os devices da própria Ana; o Dan não.
        assert_eq!(
            peer_set(f.bia.revocation_targets(&ana, None)),
            peer_set(vec![sibling, ana_peer])
        );
        // A Carol é aceita mas está desconectada: só o irmão.
        assert_eq!(
            peer_set(f.bia.revocation_targets(&carol, None)),
            peer_set(vec![sibling])
        );
        assert!(!f.bia.revocation_targets(&bia, None).contains(&f.stranger.0));
        assert!(!f.bia.revocation_targets(&bia, None).contains(&f.carol.0));
    }

    /// Repasse: um tombstone NOVO da identidade da Bia vindo da Ana sai para o irmão e o Dan (não volta
    /// para a Ana, nem vai ao estranho ou à Carol); o MESMO tombstone de novo não repassa nada.
    #[tokio::test]
    async fn a_new_revocation_is_relayed_to_the_other_targets_and_a_repeated_one_is_not() {
        let Fanout {
            mut bia,
            bia_key,
            ana: (ana_peer, mut ana_raw),
            ..
        } = fanout();
        // O primeiro tombstone faz a Bia anunciar relays e devices à Ana (uma vez por conexão): fora da conta.
        let (first, _) = revocation_by(&bia_key);
        bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, &first))
            .unwrap();
        let (second, _) = revocation_by(&bia_key);
        // O reenvio ao conectar (3c-ii-b) já foi avaliado (vazio) no primeiro `on_mls`, uma vez por conexão:
        // o segundo não reenvia o `first` à Ana, então a contagem abaixo mede só o repasse.
        let before = bia.pending.len();

        bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, &second))
            .unwrap();
        assert_eq!(bia.pending.len(), before + 2, "o irmão e o Dan");

        bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, &second))
            .unwrap();
        assert_eq!(bia.pending.len(), before + 2, "repetido não repassa");
    }

    /// Auditoria 3c-ii, I1: um tombstone da CAROL (aceita pela Bia) recebido da Ana NÃO vai ao Dan nem a
    /// outros contatos, só ao irmão e aos devices da própria Carol.
    #[tokio::test]
    async fn a_revocation_of_a_third_identity_goes_only_to_siblings_and_its_own_devices() {
        let Fanout {
            mut bia,
            carol_key,
            ana: (ana_peer, mut ana_raw),
            carol: (carol_peer, _),
            ..
        } = fanout();
        bia.connected.insert(carol_peer);
        let (first, _) = revocation_by(&carol_key);
        bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, &first))
            .unwrap();
        let (second, _) = revocation_by(&carol_key);
        let before = bia.pending.len();

        bia.on_mls(ana_peer, &revocation_wire(&mut ana_raw, &second))
            .unwrap();

        // O irmão e o device da Carol: 2. Não o Dan, não o estranho, e não a Ana (remetente).
        assert_eq!(bia.pending.len(), before + 2);
    }

    /// `broadcast_revocation`: grava e envia a todos os destinos (irmão e contatos aceitos), e de novo a
    /// cada chamada (reenvio explícito).
    #[tokio::test]
    async fn broadcasting_stores_the_revocation_and_sends_it_to_every_target_each_time() {
        let Fanout {
            mut bia, bia_key, ..
        } = fanout();
        let (revocation, certificate) = revocation_by(&bia_key);
        let before = bia.pending.len();

        bia.broadcast_revocation(&revocation).unwrap();
        assert!(bia.is_revoked(&certificate).unwrap());
        assert_eq!(bia.pending.len(), before + 3, "irmão, Ana e Dan");

        bia.broadcast_revocation(&revocation).unwrap();
        assert_eq!(bia.pending.len(), before + 6, "reenvio explícito");
    }

    // ---- P30 3c-ii-b: reenviar ao conectar (ADR 0026) ----

    /// Grava `count` tombstones novos de `owner` direto no store da Bia (sem passar pela admissão).
    fn store_revocations_of(bia: &Chat, owner: &StandaloneIdentity, count: usize) {
        for _ in 0..count {
            let (revocation, _) = revocation_by(owner);
            bia.device.store_revocation(&revocation).unwrap();
        }
    }

    /// Quantos envios cifrados a Bia tem em andamento (cada `transmit` entra em `pending`).
    fn sent(bia: &Chat) -> usize {
        bia.pending.len()
    }

    /// O irmão recebe o reenvio de tombstones de QUALQUER identidade (da Bia, da Ana e da Carol).
    #[tokio::test]
    async fn a_sibling_gets_the_stored_revocations_of_every_identity() {
        let Fanout {
            mut bia,
            bia_key,
            ana_key,
            carol_key,
            sibling: (sibling, _),
            ..
        } = fanout();
        for owner in [&bia_key, &ana_key, &carol_key] {
            store_revocations_of(&bia, owner, 1);
        }
        let before = sent(&bia);

        bia.announce_revocations(sibling);

        assert_eq!(sent(&bia), before + 3);
    }

    /// Um contato aceito (a Ana) recebe só os tombstones da identidade DELA e da minha; nada de terceira
    /// identidade (da Carol ou do Dan), que revelaria em quem eu confio (ADR 0025).
    #[tokio::test]
    async fn a_contact_gets_only_its_own_identity_and_mine() {
        let Fanout {
            mut bia,
            bia_key,
            ana_key,
            carol_key,
            ana: (ana_peer, _),
            ..
        } = fanout();
        store_revocations_of(&bia, &bia_key, 2);
        store_revocations_of(&bia, &ana_key, 1);
        store_revocations_of(&bia, &carol_key, 4);
        let before = sent(&bia);

        bia.announce_revocations(ana_peer);

        assert_eq!(
            sent(&bia),
            before + 3,
            "2 da Bia e 1 da Ana, nenhum da Carol"
        );
    }

    /// Um peer que eu não aceitei (o estranho) não recebe nada dos tombstones da Bia ou de contatos.
    #[tokio::test]
    async fn a_stranger_gets_nothing() {
        let Fanout {
            mut bia,
            bia_key,
            ana_key,
            stranger: (stranger, _),
            ..
        } = fanout();
        store_revocations_of(&bia, &bia_key, 2);
        store_revocations_of(&bia, &ana_key, 2);
        let before = sent(&bia);

        bia.announce_revocations(stranger);

        assert_eq!(sent(&bia), before);
    }

    /// No máximo 4 por reenvio (auditoria 3c-ii-b, I2: o lote deixa 12 dos 16 da cota do receptor livres).
    #[tokio::test]
    async fn at_most_four_revocations_are_resent() {
        let Fanout {
            mut bia,
            bia_key,
            sibling: (sibling, _),
            ..
        } = fanout();
        store_revocations_of(&bia, &bia_key, 10);
        let before = sent(&bia);

        bia.announce_revocations(sibling);

        assert_eq!(sent(&bia), before + 4);
    }

    /// No máximo um reenvio por intervalo por peer (maior que a janela de cota do receptor); com o
    /// intervalo vencido, reenvia; peers diferentes têm intervalos separados.
    #[tokio::test]
    async fn resending_to_the_same_peer_waits_for_the_window() {
        let Fanout {
            mut bia,
            bia_key,
            sibling: (sibling, _),
            ana: (ana_peer, _),
            ..
        } = fanout();
        store_revocations_of(&bia, &bia_key, 1);
        let before = sent(&bia);

        bia.announce_revocations(sibling);
        assert_eq!(sent(&bia), before + 1);
        // Nova conexão (a flag por conexão zera) dentro do intervalo: não reenvia.
        bia.revocations_checked.clear();
        bia.announce_revocations(sibling);
        assert_eq!(sent(&bia), before + 1, "dentro do intervalo: nada");

        bia.announce_revocations(ana_peer);
        assert_eq!(sent(&bia), before + 2, "outro peer tem o próprio intervalo");

        bia.revocations_checked.clear();
        bia.revocation_resend_gap = Duration::ZERO;
        bia.announce_revocations(sibling);
        assert_eq!(sent(&bia), before + 3, "intervalo vencido: reenvia");
    }

    /// Auditoria 3c-ii-b, I3: a consulta ao store roda UMA vez por conexão, mesmo vazia. O que entra depois
    /// não sai por aqui (sai pelo repasse); uma nova conexão avalia de novo.
    #[tokio::test]
    async fn the_resend_is_evaluated_once_per_connection_even_when_empty() {
        let Fanout {
            mut bia,
            bia_key,
            sibling: (sibling, _),
            ..
        } = fanout();
        let before = sent(&bia);
        bia.announce_revocations(sibling);
        assert_eq!(sent(&bia), before);
        assert!(bia.revocations_checked.contains(&sibling));

        store_revocations_of(&bia, &bia_key, 1);
        bia.announce_revocations(sibling);
        assert_eq!(sent(&bia), before, "mesma conexão: não consulta de novo");

        bia.handle(NodeEvent::PeerDisconnected(sibling));
        assert!(!bia.revocations_checked.contains(&sibling));
        bia.connected.insert(sibling);
        bia.announce_revocations(sibling);
        assert_eq!(sent(&bia), before + 1, "conexão nova: avalia e envia");
    }

    /// Antes de a conversa estar pronta não há avaliação nem marca (a primeira oportunidade vem depois).
    #[tokio::test]
    async fn a_connected_peer_without_a_conversation_is_not_marked() {
        let Fanout { mut bia, .. } = fanout();
        let early = PeerId::random();
        bia.connected.insert(early);

        bia.announce_revocations(early);

        assert!(!bia.revocations_checked.contains(&early));
    }

    /// Auditoria 3c-ii-b, I1: os tombstones da MINHA identidade vêm primeiro, mesmo com muitos mais novos
    /// de um contato; e no máximo 4 no total, cada grupo do mais recente ao mais antigo.
    #[tokio::test]
    async fn my_identitys_revocations_come_first_in_the_resend() {
        let Fanout {
            bia,
            bia_key,
            ana_key,
            ..
        } = fanout();
        let own_old = revocation_by(&bia_key).0;
        let own_new = revocation_by(&bia_key).0;
        bia.device.store_revocation(&own_old).unwrap();
        bia.device.store_revocation(&own_new).unwrap();
        let ana_revocations: Vec<_> = (0..6).map(|_| revocation_by(&ana_key).0).collect();
        for revocation in &ana_revocations {
            bia.device.store_revocation(revocation).unwrap();
        }
        let sibling = bia.device.identity().clone();

        // Irmão (sem filtro): as 2 minhas primeiro (a mais nova antes), depois as 2 mais novas da Ana.
        let got = bia.revocations_to_resend(&sibling).unwrap();

        let expected = vec![
            own_new.clone(),
            own_old.clone(),
            ana_revocations[5].clone(),
            ana_revocations[4].clone(),
        ];
        assert_eq!(got, expected);
        // Contato aceito (a Ana): as minhas primeiro e depois as DELA; nenhuma de terceira.
        let for_ana = bia.revocations_to_resend(&ana_key.id()).unwrap();
        assert_eq!(for_ana[..2], [own_new, own_old]);
        assert!(for_ana[2..].iter().all(|r| r.identity() == &ana_key.id()));
        assert_eq!(for_ana.len(), 4);
    }

    /// Um peer que não aceito não recebe os da minha identidade: só os dele (que, sem `trust`, não existem).
    #[tokio::test]
    async fn an_unaccepted_peer_gets_none_of_my_identitys_revocations() {
        let Fanout { bia, bia_key, .. } = fanout();
        bia.device
            .store_revocation(&revocation_by(&bia_key).0)
            .unwrap();
        let stranger = StandaloneIdentity::generate().id();

        assert!(bia.revocations_to_resend(&stranger).unwrap().is_empty());
    }

    /// Os gatilhos: uma mensagem recebida dispara a avaliação do reenvio (o convidador só tem a primeira
    /// oportunidade depois de receber algo), e o `PeerConnected` de conversa recarregada também.
    #[tokio::test]
    async fn receiving_a_message_and_connecting_both_trigger_the_resend_evaluation() {
        let Fanout {
            mut bia,
            sibling: (sibling, mut sibling_raw),
            ana: (ana_peer, _),
            ..
        } = fanout();
        assert!(!bia.revocations_checked.contains(&sibling));
        let wire = sibling_raw
            .encrypt(&Message::new("oi".into(), None).encode())
            .unwrap();

        bia.on_mls(sibling, &wire).unwrap();
        assert!(bia.revocations_checked.contains(&sibling), "on_mls");

        assert!(!bia.revocations_checked.contains(&ana_peer));
        connect_peer(&mut bia, ana_peer);
        assert!(bia.revocations_checked.contains(&ana_peer), "PeerConnected");
    }

    /// Peer desconectado (a Carol) ou sem conversa: nada, e não gasta o intervalo.
    #[tokio::test]
    async fn nothing_is_resent_to_a_disconnected_peer() {
        let Fanout {
            mut bia,
            bia_key,
            carol: (carol, _),
            ..
        } = fanout();
        store_revocations_of(&bia, &bia_key, 1);
        let before = sent(&bia);

        bia.announce_revocations(carol);
        bia.announce_revocations(PeerId::random());

        assert_eq!(sent(&bia), before);
    }

    /// As duas formas da regra de destino (`accepts_identity` e `revocation_identities_for`) concordam,
    /// para toda combinação de peer e identidade do fixture.
    #[tokio::test]
    async fn the_two_forms_of_the_destination_rule_agree() {
        let f = fanout();
        let whos = [&f.bia_key, &f.ana_key, &f.carol_key];
        let stranger = StandaloneIdentity::generate().id();
        let mut all: Vec<IdentityId> = whos.iter().map(|k| k.id()).collect();
        all.push(stranger.clone());

        for who in &all {
            let listed = f.bia.revocation_identities_for(who);
            for identity in &all {
                let expected = f.bia.accepts_identity(who, identity);
                let got = listed
                    .as_ref()
                    .is_none_or(|identities| identities.contains(identity));
                assert_eq!(got, expected, "who={who:?} identity={identity:?}");
            }
        }
    }

    /// Auditoria 3c-ii, I2: a devolução de cota em erro de armazenamento tem teto por janela (senão, com o
    /// disco falhando para sempre, o limite deixaria de existir). Devolve a do peer e a global; passado o
    /// teto não devolve mais; com a janela vencida volta a devolver.
    #[tokio::test]
    async fn quota_refunds_are_capped_per_window() {
        let Fanout { mut bia, ana, .. } = fanout();
        let peer = ana.0;
        let now = Instant::now();
        bia.revocations_seen.insert(peer, (now, 16));
        bia.revocations_global = (now, 32);

        for _ in 0..16 {
            bia.refund_revocation_quota(peer);
        }
        assert_eq!(bia.revocations_seen[&peer].1, 0);
        assert_eq!(bia.revocations_global.1, 16);

        bia.revocations_seen.insert(peer, (now, 10));
        bia.refund_revocation_quota(peer);
        assert_eq!(bia.revocations_seen[&peer].1, 10, "17ª devolução: no teto");
        assert_eq!(bia.revocations_global.1, 16);

        bia.revocation_window = Duration::ZERO;
        bia.refund_revocation_quota(peer);
        assert_eq!(bia.revocations_seen[&peer].1, 9, "janela vencida");
        assert_eq!(bia.revocations_global.1, 15);
    }

    /// `broadcast_revocation` de identidade que não aceito: recusa, não grava e não envia nada.
    #[tokio::test]
    async fn broadcasting_a_revocation_of_an_unknown_identity_is_refused_and_sends_nothing() {
        let Fanout { mut bia, .. } = fanout();
        let (revocation, certificate) = revocation_by(&StandaloneIdentity::generate());
        let before = bia.pending.len();

        let err = bia.broadcast_revocation(&revocation).unwrap_err();

        assert!(matches!(err, Error::UnauthorizedRevocation), "{err:?}");
        assert!(!bia.is_revoked(&certificate).unwrap());
        assert_eq!(bia.pending.len(), before);
    }

    /// `broadcast_revocation` com assinatura adulterada (mesmo de identidade aceita): erro do `crypto`,
    /// não grava e não envia.
    #[tokio::test]
    async fn broadcasting_a_revocation_with_a_bad_signature_sends_nothing() {
        let Fanout {
            mut bia, bia_key, ..
        } = fanout();
        let (mut revocation, certificate) = revocation_by(&bia_key);
        revocation.signature[0] ^= 1;
        let before = bia.pending.len();

        let result = bia.broadcast_revocation(&revocation);

        assert!(matches!(result, Err(Error::Crypto(_))), "{result:?}");
        assert!(!bia.is_revoked(&certificate).unwrap());
        assert_eq!(bia.pending.len(), before);
    }

    /// Só falha de armazenamento devolve a cota; assinatura inválida e recusa de admissão gastam.
    #[test]
    fn only_a_storage_error_refunds_the_revocation_quota() {
        assert!(Chat::refunds_quota(&Error::Crypto(
            kin_crypto::Error::Storage("disco cheio".into())
        )));
        assert!(!Chat::refunds_quota(&Error::Crypto(
            kin_crypto::Error::Identity(kin_identity::Error::InvalidRevocation)
        )));
        assert!(!Chat::refunds_quota(&Error::UnauthorizedRevocation));
        assert!(!Chat::refunds_quota(&Error::RevocationRateLimited));
    }

    /// 3.5 etapa 4 (segurança): um espelho vindo de um device de OUTRA identidade é recusado. Senão
    /// um contato poderia forjar "você enviou isto" no meu histórico.
    #[tokio::test]
    async fn a_mirror_from_another_identity_is_refused() {
        let Household {
            mut bia,
            ana,
            ana_device: (ana_peer, mut ana_raw),
            ..
        } = household();
        let forged = Payload::Sent {
            to: ana,
            message: Message::new("eu disse isso".into(), None),
        };
        let wire = ana_raw.encrypt(&forged.encode()).unwrap();

        let err = bia.on_mls(ana_peer, &wire).unwrap_err();

        assert!(matches!(err, Error::UnauthorizedMirror), "{err:?}");
        assert!(events(&mut bia).is_empty());
    }

    /// Device cru de `identity` que ainda não está em nenhuma conversa, para ser adicionado por commit.
    fn outsider(identity: &StandaloneIdentity) -> CryptoDevice {
        let key = DeviceKey::generate();
        CryptoDevice::new(identity, &key.public_key(), &key.nostr_public_key(), 1).unwrap()
    }

    /// 3.5 (segurança, achado da auditoria): um commit que leva um intruso ao grupo é recusado pelo
    /// `crypto`, então o espelho que o intruso fabrique (numa época que a Bia nunca alcança) não vira
    /// `SentElsewhere`. Antes, o grupo crescia e `peers().first()` virava meu irmão.
    #[tokio::test]
    async fn an_intruders_mirror_never_shows_because_the_group_cannot_grow() {
        let (bia_key, evil_key) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        let mut bia = chat(&bia_key, &DeviceKey::generate());
        let (sibling, mut sibling_raw) = add_device_of(&mut bia, &bia_key);
        let evil = outsider(&evil_key);
        let added = sibling_raw
            .invite(&evil.key_package().unwrap(), &Expected::default())
            .unwrap();
        let refused = bia.on_mls(sibling, &added.commit).unwrap_err();
        assert!(
            matches!(refused, Error::Crypto(CryptoError::TooManyMembers)),
            "{refused:?}"
        );
        // O intruso também não consegue entrar pelo Welcome do grupo de 3: sem ele não há quem
        // assine um espelho forjado nesta conversa.
        assert!(matches!(
            Conversation::join(&evil, &added.welcome, &Expected::default()),
            Err(CryptoError::TooManyMembers)
        ));
        assert_eq!(bia.conversations[&sibling].peers().unwrap().len(), 1);
        assert!(
            !events(&mut bia)
                .iter()
                .any(|e| matches!(e, ChatEvent::SentElsewhere { .. }))
        );
    }

    /// 3.5 (segurança): como o grupo não cresce, o fan-out continua indo só ao device certo, e quem
    /// tentou entrar por commit não recebe texto em claro.
    #[tokio::test]
    async fn the_fan_out_keeps_going_only_to_the_real_device_after_a_refused_growth() {
        let (ana_key, evil_key) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        let mut bia = chat(&StandaloneIdentity::generate(), &DeviceKey::generate());
        let (ana_peer, mut ana_raw) = add_device_of(&mut bia, &ana_key);
        let evil = outsider(&evil_key);
        let added = ana_raw
            .invite(&evil.key_package().unwrap(), &Expected::default())
            .unwrap();
        assert!(bia.on_mls(ana_peer, &added.commit).is_err());

        assert_eq!(bia.devices_of(&ana_key.id()), [ana_peer]);
        bia.send_to(&ana_key.id(), "segredo", None).unwrap();
        assert_eq!(bia.queued(&ana_peer), 1);
        assert_eq!(bia.conversations[&ana_peer].peers().unwrap().len(), 1);
    }

    /// 3.5: sem caixa postal e offline, cada device vira um `Queued` (a aplicação vê os dois).
    #[tokio::test]
    async fn send_to_reports_queued_once_per_offline_device() {
        let (mut bia, ana, [(a1, _), (a2, _)], _) = bia_with_two_ana_devices_and_one_cleo();

        let id = bia.send_to(&ana, "oi", None).unwrap();

        let mut queued: Vec<_> = events(&mut bia)
            .into_iter()
            .filter_map(|e| match e {
                ChatEvent::Queued { peer, message } if message == id => Some(peer),
                _ => None,
            })
            .collect();
        queued.sort();
        let mut expected = vec![a1, a2];
        expected.sort();
        assert_eq!(queued, expected);
    }

    // ---- P30 3d-ii-b2-ii: o `Chat` derruba o par revogado ----

    /// Caixa postal que anota o que o `Chat` lhe ensina e o que manda esquecer.
    #[derive(Default)]
    struct MailboxLog {
        learned_peers: Vec<PeerId>,
        learned_relays: Vec<PeerId>,
        forgotten: Vec<PeerId>,
    }

    type SharedLog = std::sync::Arc<std::sync::Mutex<MailboxLog>>;

    struct Forgetting(SharedLog);

    impl Mailbox for Forgetting {
        fn attach(&mut self, _events: futures::channel::mpsc::UnboundedSender<MailboxEvent>) {}

        fn deposit(&mut self, _to: PeerId, _blob: Vec<u8>) -> DepositId {
            DepositId(0)
        }

        fn learn_peer(&mut self, peer: PeerId, _nostr: kin_identity::NostrPublicKey) {
            self.0.lock().unwrap().learned_peers.push(peer);
        }

        fn learn_relays(&mut self, peer: PeerId, _relays: Vec<String>) {
            self.0.lock().unwrap().learned_relays.push(peer);
        }

        fn forget_peer(&mut self, peer: PeerId) {
            self.0.lock().unwrap().forgotten.push(peer);
        }
    }

    /// A Bia aceita a Ana e tem conversa com: um device irmão, o device `ana_peer` da Ana (o que vai ser
    /// revogado) e um segundo device da Ana. `wire` é o tombstone do `ana_peer`, assinado pela Ana e
    /// entregue pelo irmão (a identidade é aceita, então vale).
    struct Revoked {
        bia: Chat,
        bia_key: StandaloneIdentity,
        ana: IdentityId,
        ana_key: StandaloneIdentity,
        ana_peer: PeerId,
        ana_raw: Conversation,
        other_ana: PeerId,
        other_raw: Conversation,
        sibling: PeerId,
        revocation: DeviceRevocation,
        wire: Vec<u8>,
    }

    fn revoked_ana() -> Revoked {
        let Household {
            mut bia,
            ana,
            ana_key,
            bia_key,
            sibling: (sibling, mut sibling_raw),
            ana_device: (ana_peer, ana_raw),
            ..
        } = household();
        let (other_ana, other_raw) = add_device_of(&mut bia, &ana_key);
        bia.trust(ana.clone()).unwrap();
        let certificate = bia.conversations[&ana_peer]
            .peer_certificates()
            .unwrap()
            .remove(0);
        let revocation = ana_key.revoke_device(&certificate, 2).unwrap();
        let wire = revocation_wire(&mut sibling_raw, &revocation);
        events(&mut bia);
        Revoked {
            bia,
            bia_key,
            ana,
            ana_key,
            ana_peer,
            ana_raw,
            other_ana,
            other_raw,
            sibling,
            revocation,
            wire,
        }
    }

    fn send_failed_ids(events: &[ChatEvent], peer: PeerId) -> Vec<MessageId> {
        events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::SendFailed {
                    peer: p,
                    message: Some(id),
                    ..
                } if *p == peer => Some(*id),
                _ => None,
            })
            .collect()
    }

    /// Chegou o tombstone de um device da Ana com quem a Bia conversa: a conversa some, o evento avisa uma
    /// vez, e nem o irmão nem o outro device da Ana são tocados.
    #[tokio::test]
    async fn a_tombstone_for_a_contact_device_drops_only_that_conversation() {
        let mut r = revoked_ana();

        r.bia.on_mls(r.sibling, &r.wire).unwrap();

        assert!(!r.bia.is_ready(&r.ana_peer));
        assert!(r.bia.is_ready(&r.other_ana) && r.bia.is_ready(&r.sibling));
        let revoked: Vec<_> = events(&mut r.bia)
            .into_iter()
            .filter(|e| matches!(e, ChatEvent::PeerRevoked { .. }))
            .collect();
        assert_eq!(
            revoked,
            [ChatEvent::PeerRevoked {
                peer: r.ana_peer,
                identity: r.ana.clone()
            }]
        );
        // `send_to` a quem a Bia ainda alcança segue valendo; o device derrubado já não conta.
        let to_ana = r.bia.send_to(&r.ana, "oi", None).unwrap();
        assert_eq!(r.bia.queued(&r.other_ana), 1);
        assert_eq!(r.bia.queued(&r.ana_peer), 0, "{to_ana:?}");
    }

    /// O mesmo tombstone repetido (já no store): a conversa que ainda existe também cai. É o que repara uma
    /// derrubada que falhou no meio (disco) na primeira vez.
    #[tokio::test]
    async fn a_tombstone_already_in_the_store_still_drops_a_conversation_that_survived() {
        let mut r = revoked_ana();
        assert!(r.bia.device.store_revocation(&r.revocation).unwrap());

        r.bia.on_mls(r.sibling, &r.wire).unwrap();

        assert!(!r.bia.is_ready(&r.ana_peer));
    }

    /// Cada mensagem que esperava na fila do device derrubado vira um `SendFailed` com o id dela, a fila
    /// esvazia, e a do outro device da Ana fica.
    #[tokio::test]
    async fn queued_messages_for_the_dropped_device_fail_one_by_one_and_the_queue_empties() {
        let mut r = revoked_ana();
        let first = r.bia.send(r.ana_peer, "um", None).unwrap();
        let second = r.bia.send(r.ana_peer, "dois", None).unwrap();
        let kept = r.bia.send(r.other_ana, "fica", None).unwrap();
        events(&mut r.bia);

        r.bia.on_mls(r.sibling, &r.wire).unwrap();

        let after = events(&mut r.bia);
        assert_eq!(send_failed_ids(&after, r.ana_peer), [first, second]);
        assert!(send_failed_ids(&after, r.other_ana).is_empty());
        assert_eq!(r.bia.queued(&r.ana_peer), 0);
        assert_eq!(r.bia.queued(&r.other_ana), 1, "{kept:?}");
    }

    /// Um device irmão revogado: a conversa cai e a identidade do evento é a da própria Bia. A entrada de
    /// espelho ("enviei isto à Ana") que esperava por ele some sem `SendFailed`: não é mensagem do usuário.
    #[tokio::test]
    async fn a_revoked_sibling_is_dropped_and_its_mirror_entries_fail_silently() {
        let mut r = revoked_ana();
        let certificate = r.bia.conversations[&r.sibling]
            .peer_certificates()
            .unwrap()
            .remove(0);
        let own = r.bia_key.revoke_device(&certificate, 2).unwrap();
        let mirror = Message::new("eco".into(), None);
        r.bia
            .outbox
            .enqueue_mirror(&r.sibling, &mirror, &r.ana)
            .unwrap();
        events(&mut r.bia);

        // A própria identidade é aceita sempre; quem entrega é o outro device da Ana.
        let wire = revocation_wire(&mut r.other_raw, &own);
        r.bia.on_mls(r.other_ana, &wire).unwrap();

        assert!(!r.bia.is_ready(&r.sibling));
        let after = events(&mut r.bia);
        assert!(after.contains(&ChatEvent::PeerRevoked {
            peer: r.sibling,
            identity: r.bia_key.id()
        }));
        assert!(send_failed_ids(&after, r.sibling).is_empty());
        assert_eq!(r.bia.queued(&r.sibling), 0);
    }

    /// A caixa postal é mandada esquecer o device derrubado (e só ele), e o que a Bia tinha guardado dos
    /// relays dele sai do banco. Ligar uma caixa postal nova depois não o ensina de novo.
    #[tokio::test]
    async fn the_mailbox_forgets_the_dropped_device_and_a_new_one_is_not_taught_it_again() {
        let mut r = revoked_ana();
        let log = SharedLog::default();
        r.bia.set_mailbox(Forgetting(log.clone()));
        r.bia
            .outbox
            .set_peer_relays(&r.ana_peer, &["wss://a.example".to_string()])
            .unwrap();

        r.bia.on_mls(r.sibling, &r.wire).unwrap();

        assert_eq!(log.lock().unwrap().forgotten, [r.ana_peer]);
        assert_eq!(r.bia.outbox.peer_relays(&r.ana_peer).unwrap(), None);
        let fresh = SharedLog::default();
        r.bia.set_mailbox(Forgetting(fresh.clone()));
        let fresh = fresh.lock().unwrap();
        assert!(!fresh.learned_peers.contains(&r.ana_peer));
        assert!(!fresh.learned_relays.contains(&r.ana_peer));
        assert!(fresh.learned_peers.contains(&r.other_ana), "o outro fica");
    }

    /// Um depósito da caixa postal que já estava a caminho quando o device caiu chega depois: sem conversa,
    /// vira `Dropped` e nunca uma mensagem.
    #[tokio::test]
    async fn a_late_delivery_from_the_dropped_device_is_dropped_not_shown() {
        let mut r = revoked_ana();
        r.bia.on_mls(r.sibling, &r.wire).unwrap();
        events(&mut r.bia);
        let late = Envelope::Mls(wire(&mut r.ana_raw, "tarde")).encode();

        r.bia.handle_mailbox(MailboxEvent::Retrieved {
            from: r.ana_peer,
            blob: late,
        });

        let after = events(&mut r.bia);
        assert!(received_texts(&after).is_empty());
        assert!(
            after
                .iter()
                .any(|e| matches!(e, ChatEvent::Dropped { peer, .. } if *peer == r.ana_peer))
        );
    }

    /// A memória por Peer ID do device derrubado some (inclusive o que esperava o handshake), a do outro
    /// device fica, e a conexão de rede continua marcada: o `Node` não desconecta.
    #[tokio::test]
    async fn the_per_peer_memory_of_the_dropped_device_is_cleared_and_the_other_kept() {
        let mut r = revoked_ana();
        let (gone, kept) = (r.ana_peer, r.other_ana);
        for (n, peer) in [gone, kept].into_iter().enumerate() {
            r.bia.connected.insert(peer);
            r.bia.expected.insert(peer, r.ana.clone());
            r.bia.future.insert(peer, vec![vec![1]]);
            r.bia.announced.insert(peer);
            r.bia.devices_announced.insert(peer);
            r.bia.lists_acted.insert(peer);
            r.bia
                .in_flight
                .entry(peer)
                .or_default()
                .insert(MessageId::from_bytes([9; 16]));
            r.bia.contact_devices.insert(peer, invite_for(&r.ana_key));
            r.bia.household.insert(peer, invite_for(&r.bia_key));
            r.bia.history_requested.insert(peer);
            r.bia.history_sessions.insert(peer, 1);
            r.bia
                .depositing
                .insert(DepositId(n as u64), (peer, MessageId::from_bytes([9; 16])));
        }

        r.bia.on_mls(r.sibling, &r.wire).unwrap();

        let bia = &r.bia;
        assert!(!bia.expected.contains_key(&gone) && bia.expected.contains_key(&kept));
        assert!(!bia.future.contains_key(&gone) && bia.future.contains_key(&kept));
        assert!(!bia.announced.contains(&gone) && bia.announced.contains(&kept));
        assert!(!bia.devices_announced.contains(&gone) && bia.devices_announced.contains(&kept));
        assert!(!bia.lists_acted.contains(&gone) && bia.lists_acted.contains(&kept));
        assert!(!bia.in_flight.contains_key(&gone) && bia.in_flight.contains_key(&kept));
        assert!(
            !bia.contact_devices.contains_key(&gone) && bia.contact_devices.contains_key(&kept)
        );
        assert!(!bia.household.contains_key(&gone) && bia.household.contains_key(&kept));
        assert!(!bia.history_requested.contains(&gone) && bia.history_requested.contains(&kept));
        assert!(
            !bia.history_sessions.contains_key(&gone) && bia.history_sessions.contains_key(&kept)
        );
        assert!(bia.depositing.values().all(|(peer, _)| *peer != gone));
        assert!(bia.depositing.values().any(|(peer, _)| *peer == kept));
        assert!(bia.connected.contains(&gone), "a rede não é fechada");
    }

    /// Um tombstone de um device que a Bia não tem em nenhuma conversa não derruba ninguém e não avisa.
    #[tokio::test]
    async fn a_tombstone_for_a_device_in_no_conversation_drops_nobody() {
        let mut r = revoked_ana();
        let (revocation, _) = revocation_by(&r.ana_key);

        r.bia
            .on_mls(r.other_ana, &revocation_wire(&mut r.other_raw, &revocation))
            .unwrap();

        assert!(r.bia.is_ready(&r.ana_peer) && r.bia.is_ready(&r.other_ana));
        assert!(
            !events(&mut r.bia)
                .iter()
                .any(|e| matches!(e, ChatEvent::PeerRevoked { .. }))
        );
    }

    /// Auditoria b2-ii, I1: revogar pelo meu próprio `broadcast_revocation` (o caminho de "meu device foi
    /// roubado") também derruba a conversa com o device revogado, e só com ele.
    #[tokio::test]
    async fn broadcasting_a_tombstone_also_drops_the_conversation_with_the_revoked_device() {
        let mut r = revoked_ana();

        r.bia.broadcast_revocation(&r.revocation).unwrap();

        assert!(!r.bia.is_ready(&r.ana_peer));
        assert!(r.bia.is_ready(&r.other_ana) && r.bia.is_ready(&r.sibling));
        assert!(events(&mut r.bia).contains(&ChatEvent::PeerRevoked {
            peer: r.ana_peer,
            identity: r.ana.clone()
        }));
    }

    /// Auditoria b2-ii, I2: se a fila não pôde ser apagada, a conversa **fica** (é o que a varredura
    /// reencontra), nada é esquecido na caixa postal, a fila continua inteira e o aviso é `Dropped`, não
    /// `PeerRevoked` (que diria "apagado" sem ser verdade).
    #[tokio::test]
    async fn when_the_queue_cannot_be_cleared_the_conversation_stays_and_nothing_is_announced() {
        let mut r = revoked_ana();
        let log = SharedLog::default();
        r.bia.set_mailbox(Forgetting(log.clone()));
        r.bia.send(r.ana_peer, "fica", None).unwrap();
        events(&mut r.bia);
        r.bia.outbox.run_sql("DROP TABLE kin_history_cursor;");

        r.bia.on_mls(r.sibling, &r.wire).unwrap();

        assert!(
            r.bia.is_ready(&r.ana_peer),
            "a conversa é o ponto de reparo"
        );
        assert_eq!(r.bia.queued(&r.ana_peer), 1, "a transação desfez tudo");
        assert!(log.lock().unwrap().forgotten.is_empty());
        let after = events(&mut r.bia);
        assert!(
            !after
                .iter()
                .any(|e| matches!(e, ChatEvent::PeerRevoked { .. }))
        );
        assert!(
            after
                .iter()
                .any(|e| matches!(e, ChatEvent::Dropped { peer, .. } if *peer == r.ana_peer))
        );
    }

    /// Auditoria b2-ii, I2: consertado o banco, a varredura (a que a reabertura da 3d-ii-c também usa)
    /// termina o que falhou: a conversa cai, a fila falha por mensagem e `PeerRevoked` chega uma vez.
    #[tokio::test]
    async fn a_later_sweep_finishes_a_drop_that_failed() {
        let mut r = revoked_ana();
        let id = r.bia.send(r.ana_peer, "fica", None).unwrap();
        r.bia.outbox.run_sql("DROP TABLE kin_history_cursor;");
        r.bia.on_mls(r.sibling, &r.wire).unwrap();
        events(&mut r.bia);
        r.bia.outbox.run_sql(
            "CREATE TABLE kin_history_cursor (peer TEXT PRIMARY KEY, cursor INTEGER NOT NULL);",
        );

        r.bia.drop_revoked_peers();

        assert!(!r.bia.is_ready(&r.ana_peer));
        let after = events(&mut r.bia);
        assert_eq!(send_failed_ids(&after, r.ana_peer), [id]);
        assert_eq!(
            after
                .iter()
                .filter(|e| matches!(e, ChatEvent::PeerRevoked { .. }))
                .count(),
            1
        );
    }

    /// Auditoria b2-ii, I3: um `Welcome` em voo que falha depois do drop não é reenviado ao revogado
    /// (o grupo já foi apagado). Sem conversa, só o `Hello` e o `KeyPackage` voltam a sair.
    #[tokio::test]
    async fn a_welcome_is_not_resent_when_there_is_no_conversation() {
        let mut r = revoked_ana();
        r.bia.on_mls(r.sibling, &r.wire).unwrap();
        r.bia.connected.insert(r.ana_peer);
        let before = r.bia.pending.len();

        r.bia
            .resend_handshake(r.ana_peer, Envelope::Welcome(vec![1, 2, 3]));

        assert_eq!(r.bia.pending.len(), before, "nada foi transmitido");
        assert!(r.bia.handshake_retries.is_empty());
        r.bia
            .resend_handshake(r.ana_peer, Envelope::Hello(None, None, None));
        assert!(
            !r.bia.handshake_retries.is_empty(),
            "o Hello continua saindo"
        );
    }

    /// O device revogado não volta pelo handshake: o `KeyPackage` dele é recusado e nenhuma conversa nasce.
    #[tokio::test]
    async fn a_revoked_device_cannot_restart_a_conversation_with_a_key_package() {
        let mut r = revoked_ana();
        let key = loop {
            let key = DeviceKey::generate();
            if key.peer_id() >= r.bia.peer_id() {
                break key;
            }
        };
        let device =
            CryptoDevice::new(&r.ana_key, &key.public_key(), &key.nostr_public_key(), 1).unwrap();
        let revocation = r.ana_key.revoke_device(device.certificate(), 2).unwrap();
        r.bia.device.store_revocation(&revocation).unwrap();

        let result = r
            .bia
            .on_key_package(key.peer_id(), &device.key_package().unwrap());

        assert!(result.is_err());
        assert!(!r.bia.is_ready(&key.peer_id()));
    }
}
