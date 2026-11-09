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
