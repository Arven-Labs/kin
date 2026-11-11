use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kin_crypto::{Conversation, CryptoDevice, Decrypted, Error as CryptoError, Expected};
use kin_identity::{DeviceKey, IdentityId, IdentityProvider};
use kin_transport::{Multiaddr, NatStatus, Node, NodeConfig, NodeEvent, PeerId, SendId};

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use futures::future::{self, Either};
use futures::pin_mut;

use crate::nostr::validate_peer_relays;
use crate::outbox::{EntryKind, Outbox, SeenKind};
use crate::payload::{HISTORY_CHUNK_BUDGET, HistoryItem, MAX_DECODED_HISTORY, Payload};
use crate::wire::Envelope;
use crate::{DepositId, Mailbox, MailboxEvent};
use crate::{Error, HistoryEntry, Invite, Message, MessageId};

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
    /// provavelmente não virá e a conversa divergiu (recuperar é P16). O buffer foi descartado.
    Stale {
        peer: PeerId,
    },
    /// O peer anunciou onde recebe (lista já validada); os depósitos para ele passam a ir para lá.
    PeerRelays {
        peer: PeerId,
        relays: Vec<String>,
    },
    /// Algo do peer foi descartado (malformado, de outra conversa, etc.).
    Dropped {
        peer: PeerId,
        reason: String,
    },
}

/// Quantas mensagens de época futura se guardam por peer à espera do commit (P9).
const FUTURE_LIMIT: usize = 32;

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

/// Teto de linhas de histórico importadas de um irmão por sessão de sincronização (P10a). Um irmão
/// comprometido (chave mestra vazada) não pode encher o disco sem limite.
const MAX_SYNC_ENTRIES: usize = 100_000;

/// Cabe `key` em `map` sem passar de [`MAX_KNOWN_DEVICES`]? Quem já está sempre cabe (atualiza).
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
        addrs.len()
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
        return true;
    }
    let mut parts = addr.iter();
    let global = match parts.next() {
        Some(Ip4(ip)) => {
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_broadcast()
                || (ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 64))
        }
        Some(Ip6(ip)) => {
            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || first & 0xfe00 == 0xfc00
                || first & 0xffc0 == 0xfe80)
        }
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
    conversations: HashMap<PeerId, Conversation>,
    pending: HashMap<SendId, Option<MessageId>>,
    /// Envios em voo que são handshake (`Hello`, `KeyPackage`, `Welcome`), com o envelope: só estes são
    /// reenviados quando falham, com os mesmos bytes (P25). Em memória: reiniciar refaz pelo `Hello`.
    handshake: HashMap<SendId, Envelope>,
    /// Quantas vezes reenviei cada tipo de handshake a cada peer nesta conexão (zera ao desconectar).
    handshake_retries: HashMap<(PeerId, std::mem::Discriminant<Envelope>), u8>,
    /// Identidade exigida de cada peer discado por convite (`accept`), até a conversa ficar pronta.
    expected: HashMap<PeerId, IdentityId>,
    /// Mensagens MLS de época futura por peer, à espera do commit que falta (P9). Só em memória.
    future: HashMap<PeerId, Vec<Vec<u8>>>,
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
    /// Irmãos cujo re-anúncio caiu dentro do intervalo mínimo; saem no próximo evento depois do prazo.
    sibling_pending: std::collections::HashSet<PeerId>,
    /// Intervalo mínimo entre re-anúncios ao mesmo irmão (campo, para o teste não esperar 5 s).
    sibling_announce_gap: Duration,
    /// Teto de linhas importadas por sessão de sincronização de histórico (campo, para o teste não
    /// alimentar 100 000 linhas).
    history_cap: usize,
    /// Irmãos a quem já pedi o histórico nesta conexão (zera ao desconectar).
    history_requested: std::collections::HashSet<PeerId>,
    /// Sessões de sincronização em curso (um pedido em voo por irmão): quantas linhas já recebi nela.
    history_sessions: HashMap<PeerId, usize>,
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
            conversations: HashMap::new(),
            pending: HashMap::new(),
            handshake: HashMap::new(),
            handshake_retries: HashMap::new(),
            expected: HashMap::new(),
            future: HashMap::new(),
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
            sibling_pending: Default::default(),
            sibling_announce_gap: SIBLING_ANNOUNCE_MIN,
            history_cap: MAX_SYNC_ENTRIES,
            history_requested: Default::default(),
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

    pub fn listen(&mut self) -> Result<(), Error> {
        Ok(self.node.listen()?)
    }

    pub fn listen_on(&mut self, addr: Multiaddr) -> Result<(), Error> {
        Ok(self.node.listen_on(addr)?)
    }

    /// Conecta direto ou, se `addr` for um endereço de circuito (`.../p2p-circuit/p2p/<peer>`), por
    /// um relay.
    pub fn dial(&mut self, addr: Multiaddr) -> Result<(), Error> {
        Ok(self.node.dial(addr)?)
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

    /// Convite com os endereços que o nó considera compartilháveis (circuito de relay, externos e
    /// escuta de IP global; ver `Node::shareable_addresses`). Vazio se ainda não há nenhum.
    pub fn shareable_invite(&self) -> Invite {
        self.invite(self.node.shareable_addresses())
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
    pub fn accept(&mut self, invite: &Invite) -> Result<(), Error> {
        let peer = invite.peer_id();
        if peer == self.peer_id() {
            return Err(Error::InvalidInvite);
        }
        self.trust(invite.identity().clone())?;
        self.expected.insert(peer, invite.identity().clone());
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
            // Com re-anúncio pendente (P24), acorda no prazo mesmo sem tráfego: sem isto a lista ficaria
            // parada numa rede quieta e os irmãos nunca convergiriam.
            let wake = self.sibling_wake_in();
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
                Next::Tick => self.flush_sibling_pending(),
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
                    if let Err(e) = self.outbox.mark_deposited(&peer, &message, now_secs()) {
                        self.drop_message(peer, e);
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
                Ok(Envelope::Mls(wire)) => self.on_envelope(from, Envelope::Mls(wire)),
                Ok(_) => self.drop_message(from, Error::Malformed),
                Err(e) => self.drop_message(from, e),
            },
        }
    }

    fn handle(&mut self, event: NodeEvent) {
        // Sem timer: o re-anúncio que caiu no intervalo mínimo sai no primeiro evento depois do prazo.
        self.flush_sibling_pending();
        match event {
            NodeEvent::Listening(addr) => self.queue.push_back(ChatEvent::Listening(addr)),
            NodeEvent::PeerConnected(peer) => {
                self.connected.insert(peer);
                self.queue.push_back(ChatEvent::PeerConnected(peer));
                self.announce_if_ready(peer);
                // Conversa recarregada do banco: os dois lados já a têm, então dá para anunciar já.
                self.announce_relays(peer);
                self.announce_devices(peer);
                self.send_hello(peer);
                self.flush(peer);
            }
            NodeEvent::PeerDisconnected(peer) => {
                self.connected.remove(&peer);
                self.handshake_retries.retain(|(p, _), _| *p != peer);
                self.announced.remove(&peer);
                self.devices_announced.remove(&peer);
                self.lists_acted.remove(&peer);
                self.last_sibling_announce.remove(&peer);
                self.sibling_pending.remove(&peer);
                self.history_requested.remove(&peer);
                self.history_sessions.remove(&peer);
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
                if let Some(Some(message)) = self.pending.remove(&id) {
                    // O ack fecha o ciclo: sai da fila e libera uma vaga na janela. Só o ack de um
                    // device do destinatário é "entregue": o de um irmão (espelho) só libera a fila.
                    // O tipo tem de ser lido ANTES do `remove`, que apaga a linha.
                    let mirror = matches!(
                        self.outbox.entry_kind(&peer, &message),
                        Ok(EntryKind::Mirror(_))
                    );
                    self.leave_flight(peer, &message);
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
                match self.pending.remove(&id).flatten() {
                    // Mensagem de chat: continua na outbox e volta a esperar a próxima conexão.
                    Some(message) => {
                        self.leave_flight(peer, &message);
                        self.queue.push_back(ChatEvent::Queued { peer, message });
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
                    self.expected.remove(&peer);
                    self.forget_unreachable(&peer);
                }
                self.queue.push_back(ChatEvent::DialFailed { peer, reason });
            }
            NodeEvent::PeerDiscovered(..) | NodeEvent::PeerIdentified { .. } => {}
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
            Envelope::Hello => self.on_hello(peer),
            Envelope::KeyPackage(kp) => self.on_key_package(peer, &kp),
            Envelope::Welcome(welcome) => self.on_welcome(peer, &welcome),
            Envelope::Mls(wire) => self.on_mls(peer, &wire),
        };
        if let Err(e) = result {
            self.drop_message(peer, e);
        }
    }

    /// Só o de maior Peer ID responde com KeyPackage; o de menor convida (evita convite duplo).
    fn on_hello(&mut self, peer: PeerId) -> Result<(), Error> {
        if !self.conversations.contains_key(&peer) && self.peer_id() > peer {
            let kp = self.device.key_package()?;
            self.transmit(peer, &Envelope::KeyPackage(kp));
        }
        Ok(())
    }

    fn on_key_package(&mut self, peer: PeerId, key_package: &[u8]) -> Result<(), Error> {
        if self.conversations.contains_key(&peer) || self.peer_id() > peer {
            return Ok(());
        }
        let mut conversation = Conversation::create(&self.device)?;
        let invite = conversation.invite(
            key_package,
            &Expected {
                identity: self.expected.get(&peer),
                peer: Some(&peer),
            },
        )?;
        self.transmit(peer, &Envelope::Welcome(invite.welcome));
        self.ready(peer, conversation)
    }

    fn on_welcome(&mut self, peer: PeerId, welcome: &[u8]) -> Result<(), Error> {
        if self.conversations.contains_key(&peer) || self.peer_id() < peer {
            return Ok(());
        }
        let conversation = Conversation::join(
            &self.device,
            welcome,
            &Expected {
                identity: self.expected.get(&peer),
                peer: Some(&peer),
            },
        )?;
        self.ready(peer, conversation)?;
        // Quem entra por Welcome anuncia primeiro: o convidador já tem a conversa. O convidador só
        // anuncia ao receber algo do outro (ver `on_mls`), porque o Welcome e esta mensagem seguem
        // por substreams diferentes e a ordem de chegada não é garantida.
        self.announce_relays(peer);
        self.announce_devices(peer);
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
                self.expected.remove(&device);
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
        self.device
            .remember(&peer.to_string(), &conversation.id())?;
        self.conversations.insert(peer, conversation);
        self.teach_mailbox(peer);
        self.expected.remove(&peer);
        self.announce_if_ready(peer);
        // Conversa recém-pronta com o peer já conectado: o que estava na fila sai agora.
        self.flush(peer);
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
            Ok(wire) => self.transmit(peer, &Envelope::Mls(wire)),
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
        let received = self.history_sessions.get(&peer).copied().unwrap_or(0);
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
    /// descartado e a aplicação é avisada com [`ChatEvent::Stale`] (recuperar é P16).
    fn hold_future(&mut self, peer: PeerId, wire: &[u8]) {
        let held = self.future.entry(peer).or_default();
        if held.iter().any(|w| w == wire) {
            return;
        }
        if held.len() >= FUTURE_LIMIT {
            self.future.remove(&peer);
            self.queue.push_back(ChatEvent::Stale { peer });
            return;
        }
        held.push(wire.to_vec());
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

    fn send_hello(&mut self, peer: PeerId) {
        self.transmit(peer, &Envelope::Hello);
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
        let needs_no_conversation = !matches!(envelope, Envelope::Welcome(_));
        if !self.connected.contains(&peer)
            || (needs_no_conversation && self.conversations.contains_key(&peer))
        {
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

    fn transmit(&mut self, peer: PeerId, envelope: &Envelope) {
        let id = self.node.send(peer, envelope.encode());
        self.pending.insert(id, None);
        if matches!(
            envelope,
            Envelope::Hello | Envelope::KeyPackage(_) | Envelope::Welcome(_)
        ) {
            self.handshake.insert(id, envelope.clone());
        }
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

#[cfg(test)]
mod tests {
    use kin_identity::StandaloneIdentity;

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
        assert_eq!(events(&mut bia), [ChatEvent::Stale { peer: ana_peer }]);

        bia.on_mls(ana_peer, &commit).unwrap();
        assert!(received_texts(&events(&mut bia)).is_empty());
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

        bia.handle(NodeEvent::PeerConnected(ana_peer));
        assert_eq!(in_flight(&bia), SEND_WINDOW);
        assert_eq!(bia.queued(&ana_peer), 3, "só sai da fila com o ack");
    }

    /// 3.2: no máximo `SEND_WINDOW` em voo; o ack de uma libera a próxima.
    #[tokio::test]
    async fn the_send_window_limits_messages_in_flight() {
        let (mut bia, _ana, ana_peer) = bia_and_raw_ana();
        bia.handle(NodeEvent::PeerConnected(ana_peer));
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
        bia.handle(NodeEvent::PeerConnected(ana_peer));
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
        bia.handle(NodeEvent::PeerConnected(ana_peer));
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
        bia.handle(NodeEvent::PeerConnected(ana_peer));

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
        MemoryMailbox::new(&relay, stranger).deposit(bia.peer_id(), Envelope::Hello.encode());
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

        bia.transmit(ana_peer, &Envelope::Hello);
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

        bia.transmit(ana_peer, &Envelope::Hello);
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
        bia.handle(NodeEvent::PeerConnected(peer));
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
        bia.handle(NodeEvent::PeerConnected(gone));
        let id = only_pending(&bia);
        bia.handle(NodeEvent::PeerDisconnected(gone));
        fail_send(&mut bia, gone, id);
        assert!(bia.pending.is_empty(), "peer desconectado: sem reenvio");

        bia.handle(NodeEvent::PeerConnected(ana_peer));
        let id = only_pending(&bia);
        fail_send(&mut bia, ana_peer, id);
        assert!(bia.pending.is_empty(), "já há conversa: sem reenvio");
    }

    /// P25: no máximo 3 reenvios por conexão; reconectar zera a conta.
    #[tokio::test]
    async fn hello_resends_stop_at_three_per_connection_and_reset_on_reconnect() {
        let Household { mut bia, .. } = household();
        let peer = PeerId::random();
        bia.handle(NodeEvent::PeerConnected(peer));

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
        bia.handle(NodeEvent::PeerConnected(peer));
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
            let Household { mut bia, .. } = household();
            let peer = PeerId::random();
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
        let Household { mut bia, .. } = household();
        let peer = PeerId::random();
        bia.handle(NodeEvent::PeerConnected(peer));
        for _ in 0..4 {
            let id = only_pending(&bia);
            fail_send(&mut bia, peer, id);
        }
        assert!(bia.pending.is_empty(), "o Hello esgotou as 3 tentativas");

        for envelope in [Envelope::KeyPackage(vec![1]), Envelope::Welcome(vec![1])] {
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
        let Household { mut bia, .. } = household();
        let peer = PeerId::random();
        bia.connected.insert(peer);
        for envelope in [Envelope::KeyPackage(vec![7, 8]), Envelope::Welcome(vec![9])] {
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
        bia.handle(NodeEvent::PeerConnected(sibling));
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

        bia.handle(NodeEvent::PeerConnected(ana_peer));
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

        bia.handle(NodeEvent::PeerConnected(sibling));

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
}
