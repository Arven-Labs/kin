//! Conexões entre peers e abstração de mailbox (D1, D3).
//!
//! Fases 1.3 e 1.4: um [`Node`] que escuta em TCP (Noise + Yamux), descobre peers na LAN via mDNS
//! (opcional), conecta por endereço quando o IP é alcançável e troca `identify` com os peers.
//!
//! Fase 1.6: [`Node::send`] entrega bytes opacos (já cifrados pelo `crypto`) a um peer conectado,
//! com ack de recebimento.
//!
//! Fase 2: a escada de fallback. O nó escuta também em QUIC; atrás de NAT usa um relay
//! ([`NodeConfig::relays`], circuit relay v2) para ser alcançável e o DCUtR tenta trocar o relay por
//! uma conexão direta (hole punching); o AutoNAT diz se este nó é público. Qualquer nó pode servir de
//! relay ([`NodeConfig::relay_server`]).

mod codec;
mod config;
mod error;
mod node;
mod relays;

pub use codec::MAX_MESSAGE_SIZE;
pub use config::{ConnectionLimits, NodeConfig, RelayLimits};
pub use error::Error;
pub use libp2p::multiaddr::Protocol;
pub use libp2p::{Multiaddr, PeerId};
pub use node::{NatStatus, Node, NodeEvent, SendId};
pub use relays::{RelayStat, RelayState};
