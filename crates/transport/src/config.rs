use std::time::Duration;

use libp2p::{Multiaddr, relay};

/// Opções do nó.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Descobrir e conectar a peers da LAN via mDNS.
    pub mdns: bool,
    /// Relays candidatos para quando este nó está atrás de NAT. O nó mede cada um (sucesso e
    /// latência), pede reserva nos melhores (`relay_count`) e troca por outro quando um falha. Com
    /// reserva, ganha um endereço de circuito (`<relay>/p2p-circuit/p2p/<este peer>`) que outros
    /// podem discar. Cada endereço precisa terminar em `/p2p/<peer id do relay>`.
    pub relays: Vec<Multiaddr>,
    /// Em quantos relays (dos melhores, ver [`NodeConfig::relays`]) manter reserva ao mesmo tempo.
    /// Se um falha, o próximo candidato assume.
    pub relay_count: usize,
    /// Se `Some`, este nó também **serve** de relay para os outros, com estes limites. O relay
    /// anuncia seus endereços de escuta como externos (o cliente recusa uma reserva sem endereços);
    /// atrás de port-forward, informe o endereço público em [`NodeConfig::external_addrs`].
    pub relay_server: Option<RelayLimits>,
    /// Endereços pelos quais este nó sabe que é alcançável de fora (ex.: IP público + porta
    /// encaminhada). Anunciados aos peers e usados pelo hole punching.
    pub external_addrs: Vec<Multiaddr>,
    /// O AutoNAT só considera peers/endereços de IP global (padrão). Desligar em testes e
    /// laboratórios, onde tudo é loopback ou rede privada.
    pub autonat_global_only: bool,
    /// Tetos de conexão do nó. `None` = o padrão do papel: [`ConnectionLimits::CLIENT`], ou
    /// [`ConnectionLimits::RELAY`] se este nó serve de relay ([`NodeConfig::relay_server`]).
    pub connection_limits: Option<ConnectionLimits>,
}

impl NodeConfig {
    /// Os tetos que valem de fato (o configurado, ou o padrão do papel).
    pub(crate) fn effective_connection_limits(&self) -> ConnectionLimits {
        self.connection_limits
            .unwrap_or(if self.relay_server.is_some() {
                ConnectionLimits::RELAY
            } else {
                ConnectionLimits::CLIENT
            })
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            mdns: true,
            relays: Vec::new(),
            relay_count: 2,
            relay_server: None,
            external_addrs: Vec::new(),
            autonat_global_only: true,
            connection_limits: None,
        }
    }
}

/// Tetos de conexão (contra esgotar descritores de arquivo e memória). Cada conexão aberta, mesmo
/// ociosa, consome um descritor; sem teto, qualquer peer que alcance o nó pode derrubá-lo abrindo
/// conexões. "Pendente" é a conexão ainda em handshake (antes de a segurança fechar).
///
/// Os relays configurados em [`NodeConfig::relays`] ficam fora destes tetos: um nó cheio de
/// estranhos continua falando com o próprio relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimits {
    /// Conexões estabelecidas no total (entrada e saída).
    pub max_established: u32,
    /// Conexões estabelecidas iniciadas por outros.
    pub max_established_incoming: u32,
    /// Conexões de entrada ainda em handshake.
    pub max_pending_incoming: u32,
    /// Discagens nossas ainda em handshake.
    pub max_pending_outgoing: u32,
    /// Conexões estabelecidas com um mesmo peer.
    pub max_established_per_peer: u32,
}

impl ConnectionLimits {
    /// Padrão de um nó comum: folgado para uso pessoal (2 a 6 devices e seus contatos) e longe do
    /// limite de ~1024 descritores do Linux.
    pub const CLIENT: Self = Self {
        max_established: 256,
        max_established_incoming: 128,
        max_pending_incoming: 32,
        max_pending_outgoing: 32,
        max_established_per_peer: 4,
    };

    /// Padrão de um nó que serve de relay: uma conexão por cliente reservado
    /// ([`RelayLimits::max_reservations`] é 1024 por padrão). Em produção o limite de descritores do
    /// processo (`ulimit -n`) precisa ser maior que `max_established`.
    pub const RELAY: Self = Self {
        max_established: 4096,
        max_established_incoming: 4096,
        max_pending_incoming: 256,
        max_pending_outgoing: 32,
        max_established_per_peer: 8,
    };
}

impl From<ConnectionLimits> for libp2p::connection_limits::ConnectionLimits {
    fn from(limits: ConnectionLimits) -> Self {
        Self::default()
            .with_max_established(Some(limits.max_established))
            .with_max_established_incoming(Some(limits.max_established_incoming))
            .with_max_pending_incoming(Some(limits.max_pending_incoming))
            .with_max_pending_outgoing(Some(limits.max_pending_outgoing))
            .with_max_established_per_peer(Some(limits.max_established_per_peer))
    }
}

/// Limites de um nó que serve de relay (circuit relay v2).
///
/// O padrão do libp2p (128 KiB e 2 min por circuito) só serve para coordenar hole punching. Um relay
/// que **carrega conversas** (D3) precisa de limites bem maiores, por isso o padrão aqui é generoso e
/// configurável.
#[derive(Debug, Clone)]
pub struct RelayLimits {
    pub max_reservations: usize,
    pub max_circuits: usize,
    /// Quanto tempo um circuito pode durar (sempre é aplicado).
    pub max_circuit_duration: Duration,
    /// Bytes por circuito; `0` = sem limite.
    pub max_circuit_bytes: u64,
    pub reservation_duration: Duration,
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self {
            max_reservations: 1024,
            max_circuits: 256,
            max_circuit_duration: Duration::from_secs(60 * 60),
            max_circuit_bytes: 0,
            reservation_duration: Duration::from_secs(60 * 60),
        }
    }
}

impl From<&RelayLimits> for relay::Config {
    fn from(limits: &RelayLimits) -> Self {
        relay::Config {
            max_reservations: limits.max_reservations,
            max_circuits: limits.max_circuits,
            max_circuit_duration: limits.max_circuit_duration,
            max_circuit_bytes: limits.max_circuit_bytes,
            reservation_duration: limits.reservation_duration,
            ..relay::Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_node_gets_the_client_caps_and_a_relay_gets_the_relay_caps() {
        let plain = NodeConfig::default();
        let relay = NodeConfig {
            relay_server: Some(RelayLimits::default()),
            ..Default::default()
        };

        assert_eq!(
            plain.effective_connection_limits(),
            ConnectionLimits::CLIENT
        );
        assert_eq!(relay.effective_connection_limits(), ConnectionLimits::RELAY);
        assert_eq!(
            (
                ConnectionLimits::CLIENT.max_established,
                ConnectionLimits::CLIENT.max_established_incoming,
                ConnectionLimits::CLIENT.max_pending_incoming,
                ConnectionLimits::CLIENT.max_pending_outgoing,
                ConnectionLimits::CLIENT.max_established_per_peer,
            ),
            (256, 128, 32, 32, 4)
        );
        assert_eq!(ConnectionLimits::RELAY.max_established, 4096);
    }

    #[test]
    fn an_explicit_cap_wins_over_the_role_default() {
        let custom = ConnectionLimits {
            max_established: 7,
            ..ConnectionLimits::CLIENT
        };
        let config = NodeConfig {
            relay_server: Some(RelayLimits::default()),
            connection_limits: Some(custom),
            ..Default::default()
        };

        assert_eq!(config.effective_connection_limits(), custom);
    }
}
