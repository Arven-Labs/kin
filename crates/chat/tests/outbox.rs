//! Fase 3.2: a outbox entre dois nós reais (peer volta, processo reinicia).

mod common;

use std::time::Duration;

use common::{Person, connect, listen, settle, until_delivered, until_disconnected};
use kin_chat::ChatEvent;

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
