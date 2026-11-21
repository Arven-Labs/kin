//! P9 (c, ADR 0041): `Node::disconnect` fecha as conexões com um peer e os dois lados veem a queda.

use std::time::Duration;

use kin_identity::DeviceKey;
use kin_transport::{Node, NodeConfig, NodeEvent};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(15);

fn node(key: &DeviceKey) -> Node {
    Node::new(
        key.keypair().clone(),
        NodeConfig {
            mdns: false,
            ..Default::default()
        },
    )
    .unwrap()
}

/// Conecta `b` em `a` (loopback) e avança os dois até ambos verem a conexão.
async fn connected_pair() -> (DeviceKey, DeviceKey, Node, Node) {
    let (key_a, key_b) = (DeviceKey::generate(), DeviceKey::generate());
    let (mut a, mut b) = (node(&key_a), node(&key_b));
    a.listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap())
        .unwrap();
    let addr = loop {
        if let NodeEvent::Listening(addr) = a.next_event().await {
            break addr;
        }
    };
    b.dial(addr).unwrap();
    let (id_a, id_b) = (key_a.peer_id(), key_b.peer_id());
    let seen_a = async { while a.next_event().await != NodeEvent::PeerConnected(id_b) {} };
    let seen_b = async { while b.next_event().await != NodeEvent::PeerConnected(id_a) {} };
    timeout(WAIT, async { tokio::join!(seen_a, seen_b) })
        .await
        .expect("sem conexão");
    (key_a, key_b, a, b)
}

/// Quem chama `disconnect` devolve `true`, e os dois lados recebem `PeerDisconnected`.
#[tokio::test(flavor = "multi_thread")]
async fn disconnect_closes_the_connection_and_both_sides_see_it() {
    let (key_a, key_b, mut a, mut b) = connected_pair().await;
    let (id_a, id_b) = (key_a.peer_id(), key_b.peer_id());

    assert!(b.disconnect(id_a), "havia conexão para fechar");

    let seen_a = async { while a.next_event().await != NodeEvent::PeerDisconnected(id_b) {} };
    let seen_b = async { while b.next_event().await != NodeEvent::PeerDisconnected(id_a) {} };
    timeout(WAIT, async { tokio::join!(seen_a, seen_b) })
        .await
        .expect("a queda não chegou aos dois lados");
}

/// Sem conexão com o peer, `disconnect` devolve `false` e não faz nada.
#[tokio::test(flavor = "multi_thread")]
async fn disconnect_without_a_connection_returns_false() {
    let (_key_a, key_b, mut a, _b) = connected_pair().await;
    let stranger = DeviceKey::generate().peer_id();
    assert!(!a.disconnect(stranger));
    // pedir para fechar um desconhecido não derruba a conexão que existe
    assert!(
        a.disconnect(key_b.peer_id()),
        "a conexão com o b seguia de pé"
    );
}
