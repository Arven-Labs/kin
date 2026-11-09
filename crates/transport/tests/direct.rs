use std::time::Duration;

use kin_identity::DeviceKey;
use kin_transport::{Multiaddr, Node, NodeConfig, NodeEvent, PeerId};
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

/// Sobe o nó em loopback e devolve o endereço em que ele escuta.
async fn listen_loopback(node: &mut Node) -> Multiaddr {
    node.listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap())
        .unwrap();
    loop {
        if let NodeEvent::Listening(addr) = node.next_event().await {
            return addr;
        }
    }
}

async fn until_connected(node: &mut Node, target: PeerId) {
    loop {
        if node.next_event().await == NodeEvent::PeerConnected(target) {
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dial_by_address_connects_and_identify_is_exchanged() {
    let (key_a, key_b) = (DeviceKey::generate(), DeviceKey::generate());
    let (mut a, mut b) = (node(&key_a), node(&key_b));
    let addr_a = listen_loopback(&mut a).await;

    b.dial(addr_a.clone().with_p2p(key_a.peer_id()).unwrap())
        .unwrap();

    let a_id = key_a.peer_id();
    let identified = async {
        // `a` precisa ser dirigido junto, senão o handshake não avança.
        let drive_a = async {
            loop {
                a.next_event().await;
            }
        };
        let wait_b = async {
            loop {
                if let NodeEvent::PeerIdentified {
                    peer, listen_addrs, ..
                } = b.next_event().await
                {
                    return (peer, listen_addrs);
                }
            }
        };
        tokio::select! { r = wait_b => r, _ = drive_a => unreachable!() }
    };
    let (peer, listen_addrs) = timeout(WAIT, identified).await.expect("sem identify");
    assert_eq!(peer, a_id);
    assert!(listen_addrs.contains(&addr_a), "a deve anunciar {addr_a}");
}

#[tokio::test(flavor = "multi_thread")]
async fn both_sides_see_the_connection() {
    let (key_a, key_b) = (DeviceKey::generate(), DeviceKey::generate());
    let (id_a, id_b) = (key_a.peer_id(), key_b.peer_id());
    let (mut a, mut b) = (node(&key_a), node(&key_b));
    let addr_a = listen_loopback(&mut a).await;
    b.dial(addr_a).unwrap();

    let both = async { tokio::join!(until_connected(&mut a, id_b), until_connected(&mut b, id_a)) };
    timeout(WAIT, both).await.expect("sem conexão");
}

#[tokio::test(flavor = "multi_thread")]
async fn dial_with_wrong_peer_id_is_rejected() {
    let (key_a, key_b) = (DeviceKey::generate(), DeviceKey::generate());
    let impostor_id = DeviceKey::generate().peer_id();
    let (mut a, mut b) = (node(&key_a), node(&key_b));
    let addr_a = listen_loopback(&mut a).await;

    b.dial(addr_a.with_p2p(impostor_id).unwrap()).unwrap();

    let failed = async {
        let drive_a = async {
            loop {
                a.next_event().await;
            }
        };
        let wait_b = async {
            loop {
                match b.next_event().await {
                    NodeEvent::DialFailed { .. } => return true,
                    NodeEvent::PeerConnected(_) => return false,
                    _ => {}
                }
            }
        };
        tokio::select! { r = wait_b => r, _ = drive_a => unreachable!() }
    };
    assert!(
        timeout(WAIT, failed).await.expect("sem resultado"),
        "conexão com Peer ID errado não pode ser aceita"
    );
}

/// P25 (c): dois nós que discam um ao outro ao mesmo tempo. Com o dial reusando a porta de escuta, os dois
/// sockets ficam com o mesmo par de portas (TCP simultaneous open) e o Noise quebra
/// (`Handshake failed: input error`); medido em 40 a 79% das rodadas em loopback. Repete várias rodadas
/// porque a falha é estocástica.
#[tokio::test(flavor = "multi_thread")]
async fn simultaneous_tcp_dial_connects_both_ways() {
    const ROUNDS: usize = 50;
    for round in 0..ROUNDS {
        let (key_a, key_b) = (DeviceKey::generate(), DeviceKey::generate());
        let (mut a, mut b) = (node(&key_a), node(&key_b));
        let addr_a = listen_loopback(&mut a).await;
        let addr_b = listen_loopback(&mut b).await;

        a.dial_peer(key_b.peer_id(), vec![addr_b]).unwrap();
        b.dial_peer(key_a.peer_id(), vec![addr_a]).unwrap();

        timeout(Duration::from_secs(10), async {
            tokio::join!(
                until_connected(&mut a, key_b.peer_id()),
                until_connected(&mut b, key_a.peer_id())
            )
        })
        .await
        .unwrap_or_else(|_| panic!("rodada {round}: os dois nós não se conectaram"));
    }
}

/// Um convite traz vários endereços; basta um funcionar, e o Peer ID é conferido.
#[tokio::test(flavor = "multi_thread")]
async fn dial_peer_tries_all_addresses_and_connects_through_the_good_one() {
    let (key_a, key_b) = (DeviceKey::generate(), DeviceKey::generate());
    let (mut a, mut b) = (node(&key_a), node(&key_b));
    let good = listen_loopback(&mut a).await;
    // Uma porta onde ninguém escuta e um endereço de documentação (não roteável).
    let bad: Multiaddr = "/ip4/127.0.0.1/tcp/1".parse().unwrap();
    let nowhere: Multiaddr = "/ip4/192.0.2.1/tcp/4001".parse().unwrap();

    b.dial_peer(key_a.peer_id(), vec![bad.clone(), nowhere, good])
        .unwrap();
    timeout(Duration::from_secs(20), async {
        tokio::join!(
            until_connected(&mut a, key_b.peer_id()),
            until_connected(&mut b, key_a.peer_id())
        )
    })
    .await
    .expect("não conectou por nenhum dos endereços");

    // Com outro Peer ID esperado, o mesmo endereço bom é recusado.
    let (key_c, mut c) = {
        let key = DeviceKey::generate();
        let n = node(&key);
        (key, n)
    };
    let a_addr = listen_loopback(&mut a).await;
    c.dial_peer(PeerId::random(), vec![a_addr]).unwrap();
    let failed = timeout(Duration::from_secs(20), async {
        loop {
            tokio::select! {
                e = a.next_event() => { let _ = e; }
                e = c.next_event() => if let NodeEvent::DialFailed { .. } = e { return true },
            }
        }
    })
    .await
    .expect("o Peer ID errado deveria falhar");
    assert!(failed);
    let _ = key_c;
}
