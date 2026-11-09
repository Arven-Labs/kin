use std::time::Duration;

use kin_identity::DeviceKey;
use kin_transport::{Node, NodeConfig, NodeEvent, PeerId};
use tokio::time::timeout;

/// Dirige o nó até ele conectar com `target`.
async fn until_connected(node: &mut Node, target: PeerId) {
    loop {
        if node.next_event().await == NodeEvent::PeerConnected(target) {
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_discover_each_other_on_lan_and_connect() {
    let key_a = DeviceKey::generate();
    let key_b = DeviceKey::generate();
    let (id_a, id_b) = (key_a.peer_id(), key_b.peer_id());

    let mut a = Node::new(key_a.keypair().clone(), NodeConfig::default()).unwrap();
    let mut b = Node::new(key_b.keypair().clone(), NodeConfig::default()).unwrap();
    assert_eq!(a.peer_id(), id_a);
    a.listen().unwrap();
    b.listen().unwrap();

    let both = async { tokio::join!(until_connected(&mut a, id_b), until_connected(&mut b, id_a)) };
    timeout(Duration::from_secs(30), both)
        .await
        .expect("os nós não se acharam via mDNS em 30s");
}

/// Quantas conexões `node` estabeleceu com `peer` durante `window` (um `PeerRoute` por conexão).
async fn count_routes(node: &mut Node, peer: PeerId, window: Duration) -> usize {
    let mut routes = 0;
    let _ = timeout(window, async {
        loop {
            if let NodeEvent::PeerRoute { peer: from, .. } = node.next_event().await
                && from == peer
            {
                routes += 1;
            }
        }
    })
    .await;
    routes
}

/// P25: o mDNS devolve um endereço por interface e por transporte (tcp e quic). Discar um por um abre
/// várias conexões ao mesmo peer e estoura o teto por peer (`max_established_per_peer`), o que nega
/// conexões e perde requisições em voo. Um dial por peer deixa no máximo uma conexão por sentido.
#[tokio::test(flavor = "multi_thread")]
async fn mdns_discovery_opens_at_most_one_connection_per_direction() {
    let key_a = DeviceKey::generate();
    let key_b = DeviceKey::generate();
    let (id_a, id_b) = (key_a.peer_id(), key_b.peer_id());

    let mut a = Node::new(key_a.keypair().clone(), NodeConfig::default()).unwrap();
    let mut b = Node::new(key_b.keypair().clone(), NodeConfig::default()).unwrap();
    a.listen().unwrap();
    b.listen().unwrap();

    let both = async { tokio::join!(until_connected(&mut a, id_b), until_connected(&mut b, id_a)) };
    timeout(Duration::from_secs(30), both)
        .await
        .expect("os nós não se acharam via mDNS em 30s");

    // O `PeerRoute` da primeira conexão vem logo depois do `PeerConnected`; as demais, conforme abrem.
    let window = Duration::from_secs(3);
    let (routes_a, routes_b) = tokio::join!(
        count_routes(&mut a, id_b, window),
        count_routes(&mut b, id_a, window)
    );

    assert!(routes_a <= 2, "a abriu/recebeu {routes_a} conexões com b");
    assert!(routes_b <= 2, "b abriu/recebeu {routes_b} conexões com a");
}
