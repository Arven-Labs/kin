use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use kin_crypto::{Conversation, CryptoDevice, Decrypted, Error as CryptoError, Expected};
use kin_identity::{DeviceKey, IdentityId, IdentityProvider};
use kin_transport::{Multiaddr, NatStatus, Node, NodeConfig, NodeEvent, PeerId, SendId};

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use futures::future::{self, Either};
use futures::pin_mut;

use crate::nostr::validate_peer_relays;
use crate::outbox::Outbox;
use crate::payload::Payload;
use crate::wire::Envelope;
use crate::{DepositId, Mailbox, MailboxEvent};
use crate::{Error, Invite, Message, MessageId};

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

/// De onde veio o próximo evento do laço de [`Chat::next_event`].
enum Next {
    Node(NodeEvent),
    Mailbox(Option<MailboxEvent>),
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
            expected: HashMap::new(),
            future: HashMap::new(),
            outbox,
            connected: Default::default(),
            announced: Default::default(),
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

    /// Aceita um convite: disca o dono dele e, no handshake, **exige** a identidade do convite.
    /// Identidade errada vira `ChatEvent::Dropped` e nunca uma conversa.
    pub fn accept(&mut self, invite: &Invite) -> Result<(), Error> {
        let peer = invite.peer_id();
        if peer == self.peer_id() {
            return Err(Error::InvalidInvite);
        }
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
        self.queue_and_flush(&[(peer, None)], &message)?;
        Ok(message.id)
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
                Ok(plaintext) => plaintext,
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
    /// um espelho (3.5), o `Payload::Sent` com a identidade de destino.
    fn outgoing_plaintext(&self, peer: &PeerId, message: &Message) -> Result<Vec<u8>, Error> {
        Ok(match self.outbox.mirror_target(peer, &message.id)? {
            Some(to) => Payload::Sent {
                to,
                message: message.clone(),
            }
            .encode(),
            None => message.encode(),
        })
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
                Ok(plaintext) => plaintext,
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
            let next = {
                let node = self.node.next_event();
                match self.mailbox_events.as_mut() {
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
            match next {
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
        match event {
            NodeEvent::Listening(addr) => self.queue.push_back(ChatEvent::Listening(addr)),
            NodeEvent::PeerConnected(peer) => {
                self.connected.insert(peer);
                self.queue.push_back(ChatEvent::PeerConnected(peer));
                self.announce_if_ready(peer);
                // Conversa recarregada do banco: os dois lados já a têm, então dá para anunciar já.
                self.announce_relays(peer);
                self.transmit(peer, &Envelope::Hello);
                self.flush(peer);
            }
            NodeEvent::PeerDisconnected(peer) => {
                self.connected.remove(&peer);
                self.announced.remove(&peer);
                self.queue.push_back(ChatEvent::PeerDisconnected(peer));
                // O que ficou na fila sem poder sair direto vai para a caixa postal.
                self.flush(peer);
            }
            NodeEvent::MessageReceived { peer, data } => match Envelope::decode(&data) {
                Ok(envelope) => self.on_envelope(peer, envelope),
                Err(e) => self.drop_message(peer, e),
            },
            NodeEvent::MessageDelivered { peer, id } => {
                if let Some(Some(message)) = self.pending.remove(&id) {
                    // O ack fecha o ciclo: sai da fila e libera uma vaga na janela.
                    self.leave_flight(peer, &message);
                    if let Err(e) = self.outbox.remove(&peer, &message) {
                        self.drop_message(peer, e);
                    }
                    self.queue.push_back(ChatEvent::Delivered { peer, message });
                    self.flush(peer);
                }
            }
            NodeEvent::SendFailed { peer, id, reason } => {
                match self.pending.remove(&id).flatten() {
                    // Mensagem de chat: continua na outbox e volta a esperar a próxima conexão.
                    Some(message) => {
                        self.leave_flight(peer, &message);
                        self.queue.push_back(ChatEvent::Queued { peer, message });
                    }
                    // Handshake: sem outbox, quem decide é a aplicação.
                    None => self.queue.push_back(ChatEvent::SendFailed {
                        peer,
                        message: None,
                        reason,
                    }),
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
                self.queue.push_back(ChatEvent::DialFailed { peer, reason });
            }
            NodeEvent::PeerDiscovered(..) | NodeEvent::PeerIdentified { .. } => {}
        }
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
                // O outro lado já tem a conversa: agora é seguro anunciar os nossos relays.
                self.announce_relays(peer);
                match payload {
                    Payload::Message(message) => {
                        // Nova tentativa do remetente = outros bytes cifrados, o MLS não vê
                        // repetição; quem identifica a mesma mensagem é o `MessageId`.
                        if !self.outbox.mark_seen(&peer, &message.id)? {
                            return Ok(());
                        }
                        self.queue.push_back(ChatEvent::Received {
                            peer,
                            sender,
                            message,
                        });
                        Ok(())
                    }
                    Payload::RelayList(relays) => self.on_relay_list(peer, &relays),
                    Payload::Sent { to, message } => self.on_sent(peer, &sender, to, message),
                    // A descoberta de devices vem na etapa 5; até lá, ignorar é seguro.
                    Payload::DeviceList(_) => Ok(()),
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
        if !self.outbox.mark_seen(&peer, &message.id)? {
            return Ok(());
        }
        self.queue
            .push_back(ChatEvent::SentElsewhere { peer, to, message });
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

    fn transmit(&mut self, peer: PeerId, envelope: &Envelope) {
        let id = self.node.send(peer, envelope.encode());
        self.pending.insert(id, None);
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
        assert_eq!(bia.outbox.seen_count(&ana_peer).unwrap(), SEEN_LIMIT);
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
        (device_key.peer_id(), raw)
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
        }
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
        assert_eq!(bia.outbox.mirror_target(&sibling, &id).unwrap(), Some(ana));
        assert_eq!(bia.outbox.mirror_target(&ana_peer, &id).unwrap(), None);
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
        assert_eq!(bia.outbox.mirror_target(&sibling, &id).unwrap(), None);
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
