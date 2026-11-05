//! Fase 3.2: a outbox entre dois nós reais (peer volta, processo reinicia).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{WAIT, connect_until_ready, listen, settle, wait_for};
use kin_chat::{Chat, ChatEvent};
use kin_identity::{DeviceKey, StandaloneIdentity};
use kin_transport::{Multiaddr, NodeConfig, PeerId};
use tokio::time::timeout;

/// Uma pessoa com identidade, device e banco próprios; pode "reiniciar" reabrindo o mesmo banco.
struct Person {
    identity: StandaloneIdentity,
    device: DeviceKey,
    db: PathBuf,
}

impl Person {
    fn new(dir: &Path, name: &str) -> Self {
        Self {
            identity: StandaloneIdentity::generate(),
            device: DeviceKey::generate(),
            db: dir.join(format!("{name}.db")),
        }
    }

    fn peer_id(&self) -> PeerId {
        self.device.peer_id()
    }

    fn open(&self) -> Chat {
        Chat::open(
            &self.identity,
            &self.device,
            NodeConfig {
                mdns: false,
                ..Default::default()
            },
            &self.db,
        )
        .unwrap()
    }
}

/// `a` escuta, `b` disca, e a conversa fica pronta nos dois lados.
async fn connect(a: &mut Chat, b: &mut Chat) -> Multiaddr {
    let addr = listen(a).await;
    b.dial(addr.clone()).unwrap();
    connect_until_ready(a, b).await;
    addr
}

/// Dirige `a` até ele notar que `peer` caiu.
async fn until_disconnected(a: &mut Chat) {
    timeout(
        WAIT,
        wait_for(a, |e| {
            matches!(e, ChatEvent::PeerDisconnected(_)).then_some(())
        }),
    )
    .await
    .expect("a queda do peer não foi notada");
}

/// Dirige os dois até `a` confirmar `delivered` entregas e `b` receber `received` mensagens;
/// devolve os textos recebidos por `b`, na ordem.
async fn until_delivered(
    a: &mut Chat,
    b: &mut Chat,
    delivered: usize,
    received: usize,
) -> Vec<String> {
    let (mut acks, mut texts) = (0, Vec::new());
    timeout(WAIT, async {
        while acks < delivered || texts.len() < received {
            tokio::select! {
                e = a.next_event() => if matches!(e, ChatEvent::Delivered { .. }) { acks += 1 },
                e = b.next_event() => if let ChatEvent::Received { message, .. } = e { texts.push(message.text) },
            }
        }
    })
    .await
    .expect("as mensagens da fila não chegaram");
    texts
}

/// O peer cai, `a` escreve três mensagens, o peer volta: chegam as três, na ordem, uma vez cada.
#[tokio::test(flavor = "multi_thread")]
async fn messages_sent_while_the_peer_is_away_arrive_once_and_in_order_when_it_returns() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    let addr = connect(&mut a, &mut b).await;

    drop(b);
    until_disconnected(&mut a).await;
    for text in ["um", "dois", "tres"] {
        a.send(pb.peer_id(), text, None).unwrap();
    }
    assert_eq!(a.queued(&pb.peer_id()), 3);

    let mut b = pb.open();
    b.dial(addr).unwrap();
    let texts = until_delivered(&mut a, &mut b, 3, 3).await;
    assert_eq!(texts, ["um", "dois", "tres"]);
    assert_eq!(a.queued(&pb.peer_id()), 0);

    // Nada de reentrega nem duplicata depois.
    let (_, seen_b) = settle(&mut a, &mut b, Duration::from_secs(2)).await;
    assert!(
        !seen_b
            .iter()
            .any(|e| matches!(e, ChatEvent::Received { .. }))
    );
}

/// A fila sobrevive a reiniciar o remetente: nada se perde se `a` cai com mensagens esperando.
#[tokio::test(flavor = "multi_thread")]
async fn the_queue_survives_a_restart_of_the_sender() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    connect(&mut a, &mut b).await;

    drop(b);
    until_disconnected(&mut a).await;
    a.send(pb.peer_id(), "antes do reinicio", None).unwrap();
    a.send(pb.peer_id(), "tambem", None).unwrap();
    drop(a);

    let mut a = pa.open();
    assert_eq!(a.queued(&pb.peer_id()), 2, "a fila voltou do disco");
    let addr = listen(&mut a).await;
    let mut b = pb.open();
    b.dial(addr).unwrap();
    let texts = until_delivered(&mut a, &mut b, 2, 2).await;
    assert_eq!(texts, ["antes do reinicio", "tambem"]);
    assert_eq!(a.queued(&pb.peer_id()), 0);
}
