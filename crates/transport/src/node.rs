use std::collections::VecDeque;
use std::net::IpAddr;
use std::time::Duration;

use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::request_response::{self, OutboundRequestId, ProtocolSupport};
use libp2p::swarm::DialError;
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::dial_opts::DialOpts;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{
    Multiaddr, PeerId, Swarm, SwarmBuilder, autonat, connection_limits, dcutr, identify,
    identity::Keypair, mdns, noise, relay, tcp, yamux,
};

use crate::codec::{MessageCodec, PROTOCOL};
use crate::relays::{RelayBook, RelayStat};
use crate::{Error, NodeConfig};

/// Quanto esperar a reserva no relay antes de discar um circuito mesmo assim.
const RELAY_WAIT: Duration = Duration::from_secs(10);

/// Quantos dials de circuito podem esperar a reserva no relay ao mesmo tempo. Passou disso, o dial
/// falha na hora em vez de a fila crescer sem limite.
const MAX_DEFERRED: usize = 64;

const PROTOCOL_VERSION: &str = concat!("/kin/", env!("CARGO_PKG_VERSION"));

#[derive(NetworkBehaviour)]
struct Behaviour {
    /// Tetos de conexão (ver [`crate::ConnectionLimits`]); primeiro, para negar antes dos demais.
    limits: connection_limits::Behaviour,
    mdns: Toggle<mdns::tokio::Behaviour>,
    identify: identify::Behaviour,
    messages: request_response::Behaviour<MessageCodec>,
    relay_client: relay::client::Behaviour,
    relay_server: Toggle<relay::Behaviour>,
    dcutr: dcutr::Behaviour,
    autonat: autonat::Behaviour,
}

/// Identifica um envio, para casar com [`NodeEvent::MessageDelivered`] / [`NodeEvent::SendFailed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SendId(OutboundRequestId);

/// O que o AutoNAT concluiu sobre a alcançabilidade deste nó pela internet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatStatus {
    /// Peers de fora conseguem discar este nó direto.
    Public,
    /// Atrás de NAT/firewall: só alcançável por relay ou hole punching.
    Private,
    Unknown,
}

/// Eventos de rede expostos ao resto do Kin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeEvent {
    /// O nó passou a escutar neste endereço.
    Listening(Multiaddr),
    /// Peer encontrado na LAN via mDNS.
    PeerDiscovered(PeerId, Multiaddr),
    /// Primeira conexão com o peer estabelecida (por qual caminho: ver [`NodeEvent::PeerRoute`]).
    PeerConnected(PeerId),
    /// Uma conexão com o peer foi estabelecida; `relayed` diz se passa por um relay. Chega junto de
    /// `PeerConnected` e de novo quando o hole punching troca o relay por uma conexão direta.
    PeerRoute { peer: PeerId, relayed: bool },
    /// O AutoNAT mudou a avaliação de alcançabilidade deste nó.
    NatStatus(NatStatus),
    /// O relay aceitou a reserva: este nó agora é alcançável pelo endereço de circuito.
    RelayReserved { relay: PeerId },
    /// A reserva num relay acabou (a conexão caiu, ele recusou ou demorou demais); o nó já tenta o
    /// próximo candidato. Se ainda havia outro relay reservado, os endereços de circuito dele valem.
    RelayLost { relay: PeerId },
    /// Resultado de uma tentativa de hole punching com o peer (DCUtR).
    HolePunch {
        peer: PeerId,
        result: Result<(), String>,
    },
    /// O peer informou seus endereços de escuta e o endereço pelo qual nos viu.
    PeerIdentified {
        peer: PeerId,
        listen_addrs: Vec<Multiaddr>,
        observed_addr: Multiaddr,
    },
    /// Última conexão com o peer encerrada.
    PeerDisconnected(PeerId),
    /// Tentativa de conexão falhou (inclui Peer ID diferente do esperado).
    DialFailed {
        peer: Option<PeerId>,
        reason: String,
    },
    /// Chegou uma mensagem (bytes opacos) do peer; o ack já foi devolvido.
    MessageReceived { peer: PeerId, data: Vec<u8> },
    /// O peer confirmou o recebimento da mensagem enviada com [`Node::send`].
    MessageDelivered { peer: PeerId, id: SendId },
    /// O envio falhou (sem conexão, timeout, conexão encerrada ou peer sem o protocolo).
    SendFailed {
        peer: PeerId,
        id: SendId,
        reason: String,
    },
}

/// Nó de rede: escuta em TCP, conecta por endereço ou via mDNS e troca identificação com os peers.
pub struct Node {
    swarm: Swarm<Behaviour>,
    /// Eventos já produzidos mas ainda não entregues, em ordem.
    queue: VecDeque<NodeEvent>,
    /// Há relays configurados e nenhuma reserva aceita ainda: discar um circuito agora faria o
    /// libp2p cancelar o dial (concorre com a conexão que abre a reserva), então ele espera.
    awaiting_relay: bool,
    relays: RelayBook,
    deferred: Vec<DialOpts>,
    deferred_deadline: Option<tokio::time::Instant>,
}

impl Node {
    /// Cria o nó usando a chave do device (o Peer ID sai dela).
    pub fn new(keypair: Keypair, config: NodeConfig) -> Result<Self, Error> {
        let relays = RelayBook::new(&config.relays, config.relay_count)?;
        let swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )
            .map_err(|e| Error::Build(e.to_string()))?
            .with_quic()
            .with_dns()
            .map_err(|e| Error::Build(e.to_string()))?
            .with_relay_client(noise::Config::new, yamux::Config::default)
            .map_err(|e| Error::Build(e.to_string()))?
            .with_behaviour(|key, relay_client| {
                let mdns = if config.mdns {
                    Some(mdns::tokio::Behaviour::new(
                        mdns::Config::default(),
                        key.public().to_peer_id(),
                    )?)
                } else {
                    None
                };
                let identify = identify::Behaviour::new(identify::Config::new(
                    PROTOCOL_VERSION.into(),
                    key.public(),
                ));
                let messages = request_response::Behaviour::with_codec(
                    MessageCodec,
                    [(PROTOCOL, ProtocolSupport::Full)],
                    request_response::Config::default(),
                );
                let local = key.public().to_peer_id();
                let relay_server = config
                    .relay_server
                    .as_ref()
                    .map(|limits| relay::Behaviour::new(local, limits.into()));
                let autonat = autonat::Behaviour::new(
                    local,
                    autonat::Config {
                        only_global_ips: config.autonat_global_only,
                        ..Default::default()
                    },
                );
                Ok(Behaviour {
                    limits: connection_limits::Behaviour::new(
                        config.effective_connection_limits().into(),
                    ),
                    mdns: mdns.into(),
                    identify,
                    messages,
                    relay_client,
                    relay_server: relay_server.into(),
                    dcutr: dcutr::Behaviour::new(local),
                    autonat,
                })
            })
            .map_err(|e| Error::Build(e.to_string()))?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();
        let mut node = Self {
            swarm,
            queue: VecDeque::new(),
            awaiting_relay: !relays.is_empty(),
            relays,
            deferred: Vec::new(),
            deferred_deadline: None,
        };
        for addr in &config.external_addrs {
            node.swarm.add_external_address(addr.clone());
        }
        node.bypass_relays();
        node.fill_relays();
        Ok(node)
    }

    /// Os relays configurados ficam fora dos tetos de conexão: um nó cheio de estranhos não pode ficar
    /// sem falar com o próprio relay (perderia a alcançabilidade em silêncio).
    fn bypass_relays(&mut self) {
        for stat in self.relays.stats() {
            self.swarm.behaviour_mut().limits.bypass_peer_id(&stat.peer);
        }
    }

    pub fn peer_id(&self) -> PeerId {
        *self.swarm.local_peer_id()
    }

    /// Escuta em todas as interfaces (TCP e QUIC), portas escolhidas pelo SO. Escutar também em
    /// UDP/QUIC é o que dá a melhor chance ao hole punching.
    pub fn listen(&mut self) -> Result<(), Error> {
        self.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;
        self.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)
    }

    /// Acrescenta `addr` (que deve terminar em `/p2p/<peer id do relay>`) aos relays candidatos. Se
    /// ainda faltam reservas (`relay_count`), já tenta. Quando aceita, chega
    /// [`NodeEvent::RelayReserved`] e o endereço de circuito aparece em [`NodeEvent::Listening`];
    /// outros peers discam `Node::circuit_address(addr)`.
    pub fn add_relay(&mut self, addr: Multiaddr) -> Result<(), Error> {
        self.relays.add(addr)?;
        self.bypass_relays();
        self.awaiting_relay |= self.relays.active() == 0;
        self.fill_relays();
        Ok(())
    }

    /// Relays candidatos, do melhor para o pior, com o que o nó mediu de cada um.
    pub fn relay_stats(&self) -> Vec<RelayStat> {
        self.relays.stats()
    }

    /// Pede reserva nos melhores candidatos até completar `relay_count`.
    fn fill_relays(&mut self) {
        let now = tokio::time::Instant::now();
        while self.relays.wants_more() {
            let Some(i) = self.relays.next_eligible() else {
                break;
            };
            let circuit = self.relays.addr(i).clone().with(Protocol::P2pCircuit);
            match self.swarm.listen_on(circuit) {
                Ok(listener) => self.relays.mark_pending(i, listener, now),
                Err(_) => {
                    let relay = self.relays.mark_failed(i, now);
                    self.queue.push_back(NodeEvent::RelayLost { relay });
                }
            }
        }
    }

    /// Algo dos relays ou dos dials adiados venceu o prazo.
    fn on_timer(&mut self) {
        let now = tokio::time::Instant::now();
        if self.deferred_deadline.is_some_and(|d| now >= d) {
            self.flush_deferred();
        }
        for (i, listener) in self.relays.expired(now) {
            self.swarm.remove_listener(listener);
            let relay = self.relays.mark_failed(i, now);
            self.queue.push_back(NodeEvent::RelayLost { relay });
        }
        self.relays.tick(now);
        self.fill_relays();
    }

    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        [self.deferred_deadline, self.relays.next_deadline()]
            .into_iter()
            .flatten()
            .min()
    }

    /// Endereço pelo qual outros alcançam este nó através do relay `relay_addr`.
    pub fn circuit_address(&self, relay_addr: &Multiaddr) -> Multiaddr {
        relay_addr
            .clone()
            .with(Protocol::P2pCircuit)
            .with(Protocol::P2p(self.peer_id()))
    }

    /// Escuta em um endereço específico.
    pub fn listen_on(&mut self, addr: Multiaddr) -> Result<(), Error> {
        self.swarm.listen_on(addr)?;
        Ok(())
    }

    /// Conecta direto a um endereço. Se o endereço terminar em `/p2p/<peer-id>`, a conexão só vale
    /// se o peer do outro lado provar ter esse Peer ID.
    ///
    /// Um endereço de circuito (`.../p2p-circuit/p2p/<peer>`) discado antes de a reserva no relay
    /// sair espera por ela (até 10 s) em vez de falhar.
    pub fn dial(&mut self, addr: Multiaddr) -> Result<(), Error> {
        let circuit = is_circuit(&addr);
        self.start_dial(DialOpts::from(addr), circuit)
    }

    /// Conecta ao peer `peer` tentando vários endereços de uma vez (ex.: os de um convite). A conexão
    /// só vale se o outro lado provar ter esse Peer ID. Se já houver conexão, não faz nada.
    pub fn dial_peer(&mut self, peer: PeerId, addrs: Vec<Multiaddr>) -> Result<(), Error> {
        let circuit = addrs.iter().any(is_circuit);
        let opts = DialOpts::peer_id(peer).addresses(addrs).build();
        self.start_dial(opts, circuit)
    }

    fn start_dial(&mut self, opts: DialOpts, circuit: bool) -> Result<(), Error> {
        if circuit && self.awaiting_relay {
            if self.deferred.len() >= MAX_DEFERRED {
                return Err(Error::Dial("fila de discagens adiadas cheia".into()));
            }
            self.deferred.push(opts);
            self.deferred_deadline
                .get_or_insert_with(|| tokio::time::Instant::now() + RELAY_WAIT);
            return Ok(());
        }
        match self.swarm.dial(opts) {
            Ok(()) | Err(DialError::DialPeerConditionFalse(_)) => Ok(()),
            Err(e) => Err(Error::Dial(e.to_string())),
        }
    }

    /// Dispara os dials que esperavam a reserva (ela saiu, ou o prazo acabou).
    fn flush_deferred(&mut self) {
        self.awaiting_relay = false;
        self.deferred_deadline = None;
        for opts in std::mem::take(&mut self.deferred) {
            if let Err(e) = self.start_dial(opts, true) {
                self.queue.push_back(NodeEvent::DialFailed {
                    peer: None,
                    reason: e.to_string(),
                });
            }
        }
    }

    /// Endereços que vale pôr num convite: os de circuito ativos (sem loopback), os externos
    /// informados e os de escuta de IP global, todos terminando em `/p2p/<este peer>`.
    pub fn shareable_addresses(&self) -> Vec<Multiaddr> {
        let me = self.peer_id();
        let mut out: Vec<Multiaddr> = Vec::new();
        let candidates = self
            .swarm
            .listeners()
            .filter(|a| is_circuit(a) && !is_loopback(a) || is_global(a))
            // O cliente de relay também registra o circuito como endereço externo.
            .chain(self.swarm.external_addresses().filter(|a| !is_loopback(a)));
        for addr in candidates {
            let addr = with_peer(addr.clone(), me);
            if !out.contains(&addr) {
                out.push(addr);
            }
        }
        out
    }

    /// Envia bytes opacos a um peer já conectado. O resultado chega como `MessageDelivered` ou
    /// `SendFailed` com o mesmo [`SendId`].
    pub fn send(&mut self, peer: PeerId, data: Vec<u8>) -> SendId {
        SendId(
            self.swarm
                .behaviour_mut()
                .messages
                .send_request(&peer, data),
        )
    }

    /// Avança o nó até o próximo evento relevante.
    pub async fn next_event(&mut self) -> NodeEvent {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return event;
            }
            let event = match self.next_deadline() {
                Some(deadline) => tokio::select! {
                    event = self.swarm.select_next_some() => event,
                    () = tokio::time::sleep_until(deadline) => {
                        self.on_timer();
                        continue;
                    }
                },
                None => self.swarm.select_next_some().await,
            };
            match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    if self.swarm.behaviour().relay_server.is_enabled() {
                        self.swarm.add_external_address(address.clone());
                    }
                    return NodeEvent::Listening(address);
                }
                SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                    let mut first = None;
                    for (peer, addr) in list {
                        if self.swarm.is_connected(&peer) {
                            continue;
                        }
                        // Falha ao discar aqui não é fatal: o mDNS reanuncia depois.
                        let _ = self.swarm.dial(addr.clone());
                        first.get_or_insert((peer, addr));
                    }
                    if let Some((peer, addr)) = first {
                        return NodeEvent::PeerDiscovered(peer, addr);
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received {
                    peer_id,
                    info,
                    ..
                })) => {
                    return NodeEvent::PeerIdentified {
                        peer: peer_id,
                        listen_addrs: info.listen_addrs,
                        observed_addr: info.observed_addr,
                    };
                }
                SwarmEvent::Behaviour(BehaviourEvent::Messages(event)) => {
                    if let Some(event) = self.on_message_event(event) {
                        return event;
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::RelayClient(
                    relay::client::Event::ReservationReqAccepted { relay_peer_id, .. },
                )) => {
                    self.relays
                        .mark_reserved(relay_peer_id, tokio::time::Instant::now());
                    self.flush_deferred();
                    return NodeEvent::RelayReserved {
                        relay: relay_peer_id,
                    };
                }
                SwarmEvent::Behaviour(BehaviourEvent::Dcutr(event)) => {
                    return NodeEvent::HolePunch {
                        peer: event.remote_peer_id,
                        result: event.result.map(|_| ()).map_err(|e| e.to_string()),
                    };
                }
                SwarmEvent::Behaviour(BehaviourEvent::Autonat(autonat::Event::StatusChanged {
                    new,
                    ..
                })) => {
                    return NodeEvent::NatStatus(match new {
                        autonat::NatStatus::Public(_) => NatStatus::Public,
                        autonat::NatStatus::Private => NatStatus::Private,
                        autonat::NatStatus::Unknown => NatStatus::Unknown,
                    });
                }
                SwarmEvent::ConnectionEstablished {
                    peer_id,
                    endpoint,
                    num_established,
                    ..
                } => {
                    let route = NodeEvent::PeerRoute {
                        peer: peer_id,
                        relayed: endpoint.is_relayed(),
                    };
                    // Só a primeira conexão é "conectou"; as seguintes (ex.: o hole punching
                    // trocando o relay por uma direta) são só mudança de rota.
                    if num_established.get() == 1 {
                        self.queue.push_back(route);
                        return NodeEvent::PeerConnected(peer_id);
                    }
                    return route;
                }
                SwarmEvent::ListenerClosed { listener_id, .. } => {
                    // O ouvinte de circuito de um relay fechou: a conexão caiu ou ele recusou.
                    if let Some(i) = self.relays.find_listener(listener_id) {
                        let relay = self.relays.mark_failed(i, tokio::time::Instant::now());
                        self.fill_relays();
                        return NodeEvent::RelayLost { relay };
                    }
                }
                SwarmEvent::ConnectionClosed {
                    peer_id,
                    num_established: 0,
                    ..
                } => return NodeEvent::PeerDisconnected(peer_id),
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                    return NodeEvent::DialFailed {
                        peer: peer_id,
                        reason: error.to_string(),
                    };
                }
                _ => {}
            }
        }
    }

    fn on_message_event(
        &mut self,
        event: request_response::Event<Vec<u8>, ()>,
    ) -> Option<NodeEvent> {
        use request_response::{Event, Message};
        match event {
            Event::Message {
                peer,
                message:
                    Message::Request {
                        request, channel, ..
                    },
                ..
            } => {
                // Se o ack não puder ser enviado, o canal já fechou; o remetente reenvia.
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .messages
                    .send_response(channel, ());
                Some(NodeEvent::MessageReceived {
                    peer,
                    data: request,
                })
            }
            Event::Message {
                peer,
                message: Message::Response { request_id, .. },
                ..
            } => Some(NodeEvent::MessageDelivered {
                peer,
                id: SendId(request_id),
            }),
            Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => Some(NodeEvent::SendFailed {
                peer,
                id: SendId(request_id),
                reason: error.to_string(),
            }),
            Event::InboundFailure { .. } | Event::ResponseSent { .. } => None,
        }
    }
}

fn is_circuit(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| matches!(p, Protocol::P2pCircuit))
}

fn ip_of(addr: &Multiaddr) -> Option<IpAddr> {
    addr.iter().find_map(|p| match p {
        Protocol::Ip4(ip) => Some(IpAddr::V4(ip)),
        Protocol::Ip6(ip) => Some(IpAddr::V6(ip)),
        _ => None,
    })
}

fn is_loopback(addr: &Multiaddr) -> bool {
    ip_of(addr).is_some_and(|ip| ip.is_loopback())
}

/// Endereço de escuta alcançável de fora: IP público (nome DNS também conta). Fora: loopback,
/// não especificado, rede privada/local e CGNAT (100.64.0.0/10).
fn is_global(addr: &Multiaddr) -> bool {
    if is_circuit(addr) {
        return false;
    }
    match ip_of(addr) {
        None => addr
            .iter()
            .any(|p| matches!(p, Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_))),
        Some(IpAddr::V4(ip)) => {
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_broadcast()
                || (ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 64))
        }
        Some(IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || first & 0xfe00 == 0xfc00 // fc00::/7, local única
                || first & 0xffc0 == 0xfe80) // fe80::/10, link-local
        }
    }
}

/// Garante que o endereço termina em `/p2p/<peer>` (endereços de circuito já vêm assim).
fn with_peer(addr: Multiaddr, peer: PeerId) -> Multiaddr {
    match addr.iter().last() {
        Some(Protocol::P2p(_)) => addr,
        _ => addr.with(Protocol::P2p(peer)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> Multiaddr {
        s.parse().unwrap()
    }

    #[test]
    fn only_publicly_reachable_listen_addresses_are_global() {
        for public in [
            "/ip4/203.0.113.7/tcp/4001",
            "/ip4/8.8.8.8/udp/4001/quic-v1",
            "/ip6/2001:db8::1/tcp/4001",
            "/dns4/relay.example.org/tcp/4001",
        ] {
            assert!(is_global(&addr(public)), "{public} deveria ser global");
        }
        for private in [
            "/ip4/127.0.0.1/tcp/4001",
            "/ip4/0.0.0.0/tcp/4001",
            "/ip4/10.1.2.3/tcp/4001",
            "/ip4/192.168.1.82/tcp/4001",
            "/ip4/172.17.0.1/tcp/4001",
            "/ip4/169.254.1.1/tcp/4001",
            "/ip4/100.64.0.1/tcp/4001", // CGNAT
            "/ip6/::1/tcp/4001",
            "/ip6/fe80::1/tcp/4001",
            "/ip6/fd00::1/tcp/4001",
        ] {
            assert!(
                !is_global(&addr(private)),
                "{private} não deveria ser global"
            );
        }
        // Endereço de circuito nunca é "global": entra no convite por outro caminho.
        assert!(!is_global(&addr("/ip4/203.0.113.7/tcp/4001/p2p-circuit")));
    }

    #[test]
    fn with_peer_appends_the_peer_id_only_once() {
        let me = PeerId::random();
        let plain = with_peer(addr("/ip4/203.0.113.7/tcp/4001"), me);
        assert_eq!(plain.iter().last(), Some(Protocol::P2p(me)));
        assert_eq!(with_peer(plain.clone(), me), plain);
    }

    #[test]
    fn loopback_and_circuit_detection() {
        assert!(is_loopback(&addr("/ip4/127.0.0.1/tcp/1")));
        assert!(!is_loopback(&addr("/ip4/8.8.8.8/tcp/1")));
        assert!(is_circuit(&addr("/ip4/8.8.8.8/tcp/1/p2p-circuit")));
        assert!(!is_circuit(&addr("/ip4/8.8.8.8/tcp/1")));
    }
}
