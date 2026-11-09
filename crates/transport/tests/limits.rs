//! Tetos de abuso (P24): conexões acima do teto são recusadas, os relays configurados ficam fora do
//! teto e os dials adiados à espera de relay têm limite.

use std::time::Duration;

use kin_identity::DeviceKey;
use kin_transport::{
    ConnectionLimits, Multiaddr, Node, NodeConfig, NodeEvent, PeerId, Protocol, RelayLimits,
};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(30);

fn plain() -> NodeConfig {
    NodeConfig {
        mdns: false,
        ..Default::default()
    }
}

fn node(key: &DeviceKey, config: NodeConfig) -> Node {
    Node::new(key.keypair().clone(), config).unwrap()
}

async fn listen_loopback(node: &mut Node) -> Multiaddr {
    node.listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap())
        .unwrap();
    loop {
        if let NodeEvent::Listening(addr) = node.next_event().await {
            return addr;
        }
    }
}

/// Sobe um servidor com `limits`, conecta o cliente 1 e depois discando o cliente 2: diz se o
/// servidor chegou a ver o cliente 2 conectado (espera até 4 s, o que sobra em loopback).
async fn second_client_reaches_the_server(limits: Option<ConnectionLimits>) -> bool {
    let key_server = DeviceKey::generate();
    let mut server = node(
        &key_server,
        NodeConfig {
            connection_limits: limits,
            ..plain()
        },
    );
    let addr = listen_loopback(&mut server)
        .await
        .with(Protocol::P2p(key_server.peer_id()));
    let (key_1, key_2) = (DeviceKey::generate(), DeviceKey::generate());
    let (mut client_1, mut client_2) = (node(&key_1, plain()), node(&key_2, plain()));
    client_1.dial(addr.clone()).unwrap();

    let first_up = async {
        loop {
            tokio::select! {
                e = server.next_event() => {
                    if e == NodeEvent::PeerConnected(key_1.peer_id()) {
                        return;
                    }
                }
                _ = client_1.next_event() => {}
            }
        }
    };
    timeout(WAIT, first_up)
        .await
        .expect("o cliente 1 devia conectar");

    client_2.dial(addr).unwrap();
    let second_peer = key_2.peer_id();
    let seen = async {
        loop {
            tokio::select! {
                e = server.next_event() => {
                    if e == NodeEvent::PeerConnected(second_peer) {
                        return;
                    }
                }
                _ = client_1.next_event() => {}
                _ = client_2.next_event() => {}
            }
        }
    };
    timeout(Duration::from_secs(4), seen).await.is_ok()
}

/// Controle: sem teto de entrada, o 2º cliente conecta (prova que o teste enxerga a conexão).
#[tokio::test(flavor = "multi_thread")]
async fn without_a_cap_the_second_client_connects() {
    assert!(second_client_reaches_the_server(None).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_incoming_connection_over_the_cap_is_refused() {
    let one_incoming = ConnectionLimits {
        max_established_incoming: 1,
        ..ConnectionLimits::CLIENT
    };

    assert!(!second_client_reaches_the_server(Some(one_incoming)).await);
}

fn peer_of(addr: &Multiaddr) -> PeerId {
    addr.iter()
        .filter_map(|p| match p {
            Protocol::P2p(id) => Some(id),
            _ => None,
        })
        .last()
        .unwrap()
}

/// O relay configurado fica fora do teto: com `max_established = 0` NENHUMA conexão comum seria
/// aceita, e mesmo assim a reserva no relay sai.
#[tokio::test(flavor = "multi_thread")]
async fn the_configured_relay_bypasses_the_connection_cap() {
    let relay_key = DeviceKey::generate();
    let mut relay = node(
        &relay_key,
        NodeConfig {
            relay_server: Some(RelayLimits::default()),
            ..plain()
        },
    );
    let relay_addr = listen_loopback(&mut relay)
        .await
        .with(Protocol::P2p(relay_key.peer_id()));
    tokio::spawn(async move {
        loop {
            relay.next_event().await;
        }
    });
    let mut client = node(
        &DeviceKey::generate(),
        NodeConfig {
            relays: vec![relay_addr.clone()],
            connection_limits: Some(ConnectionLimits {
                max_established: 0,
                ..ConnectionLimits::CLIENT
            }),
            ..plain()
        },
    );

    let reserved = timeout(WAIT, async {
        loop {
            if let NodeEvent::RelayReserved { relay } = client.next_event().await {
                return relay;
            }
        }
    })
    .await
    .expect("a reserva no relay devia sair apesar do teto");

    assert_eq!(reserved, peer_of(&relay_addr));
}

/// Dials de circuito adiados (à espera da reserva no relay) têm teto de 64: o 65º falha em vez de
/// crescer sem limite.
#[tokio::test]
async fn deferred_circuit_dials_are_capped() {
    let relay = DeviceKey::generate().peer_id();
    let relay_addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/1/p2p/{relay}").parse().unwrap();
    let mut client = node(
        &DeviceKey::generate(),
        NodeConfig {
            relays: vec![relay_addr.clone()],
            ..plain()
        },
    );
    let circuit = relay_addr
        .with(Protocol::P2pCircuit)
        .with(Protocol::P2p(DeviceKey::generate().peer_id()));

    for i in 0..64 {
        client
            .dial(circuit.clone())
            .unwrap_or_else(|e| panic!("o dial adiado {i} devia caber: {e}"));
    }

    assert!(client.dial(circuit).is_err(), "o 65º passou do teto");
}
