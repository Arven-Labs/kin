//! P9 (c, ADR 0041): `Node::disconnect` fecha as conexões com um peer e os dois lados veem a queda.

use std::time::Duration;

use kin_identity::DeviceKey;
use kin_transport::{Node, NodeConfig, NodeEvent, PeerId};
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
    let (key_a, key_b, mut a, mut b) = connected_pair().await;
    let stranger = DeviceKey::generate().peer_id();
    assert!(!a.disconnect(stranger));

    // pedir para fechar um desconhecido não derruba a conexão que existe: o `b` ainda recebe do `a`
    a.send(key_b.peer_id(), b"ainda de pe".to_vec());
    let received = async {
        loop {
            if let NodeEvent::MessageReceived { peer, data } = b.next_event().await {
                return (peer, data);
            }
        }
    };
    let pump_a = async {
        loop {
            a.next_event().await;
        }
    };
    let (from, data) = timeout(WAIT, async {
        tokio::select! {
            got = received => got,
            () = pump_a => unreachable!(),
        }
    })
    .await
    .expect("a conexão caiu depois do disconnect de um desconhecido");
    assert_eq!(from, key_a.peer_id());
    assert_eq!(data, b"ainda de pe");
}

/// Com **duas** conexões abertas com o mesmo peer (o que o P36 chama de conexão antiga meio-aberta), um
/// `disconnect` fecha as duas: cada lado vê **um só** `PeerDisconnected` (o transporte só o emite quando a
/// última cai) e um 2º `disconnect` não encontra mais nada.
#[tokio::test(flavor = "multi_thread")]
async fn disconnect_closes_every_connection_with_the_peer() {
    let (key_a, key_b) = (DeviceKey::generate(), DeviceKey::generate());
    let (mut a, mut b) = (node(&key_a), node(&key_b));
    a.listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap())
        .unwrap();
    let addr = loop {
        if let NodeEvent::Listening(addr) = a.next_event().await {
            break addr;
        }
    };
    let (id_a, id_b) = (key_a.peer_id(), key_b.peer_id());

    // Duas discagens ao mesmo endereço: o `Node::dial` usa porta de origem nova, então são 2 conexões.
    b.dial(addr.clone()).unwrap();
    b.dial(addr).unwrap();
    // Cada conexão estabelecida vira um `PeerRoute` (a 1ª, junto do `PeerConnected`): espera 2 em cada lado.
    let routes = |node: &mut Node, other: PeerId| {
        let mut seen = 0;
        async move {
            while seen < 2 {
                if matches!(node.next_event().await, NodeEvent::PeerRoute { peer, .. } if peer == other)
                {
                    seen += 1;
                }
            }
        }
    };
    timeout(WAIT, async {
        tokio::join!(routes(&mut a, id_b), routes(&mut b, id_a))
    })
    .await
    .expect("as duas conexões não se estabeleceram nos dois lados");

    assert!(b.disconnect(id_a), "havia conexões para fechar");

    // Um `PeerDisconnected` por lado; depois dele, mais nenhum evento de queda no resto da janela.
    let mut drops = (0, 0);
    let _ = timeout(Duration::from_secs(3), async {
        loop {
            tokio::select! {
                e = a.next_event() => if e == NodeEvent::PeerDisconnected(id_b) { drops.0 += 1 },
                e = b.next_event() => if e == NodeEvent::PeerDisconnected(id_a) { drops.1 += 1 },
            }
        }
    })
    .await;
    assert_eq!(
        drops,
        (1, 1),
        "uma queda por lado, só quando a última conexão fechou"
    );
    assert!(!b.disconnect(id_a), "não sobrou conexão em b");
    assert!(!a.disconnect(id_b), "não sobrou conexão em a");
}
