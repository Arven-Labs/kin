//! Fase 3.1: a caixa postal entre dois nós reais (o destinatário volta sem o remetente online).

mod common;

use std::time::Duration;

use common::{Person, WAIT, connect, settle, until_delivered, until_disconnected};
use kin_chat::{Chat, ChatEvent, DepositId, Mailbox, MailboxEvent, MemoryMailbox, MemoryRelay};
use kin_transport::PeerId;
use tokio::time::timeout;

/// Dirige `chat` até ele anunciar `count` depósitos aceitos.
async fn until_deposited(chat: &mut Chat, count: usize) {
    timeout(WAIT, async {
        let mut seen = 0;
        while seen < count {
            if matches!(chat.next_event().await, ChatEvent::Deposited { .. }) {
                seen += 1;
            }
        }
    })
    .await
    .expect("os depósitos não foram confirmados");
}

/// Dirige `chat` sozinho (sem ninguém discando) até receber `count` mensagens.
async fn until_received(chat: &mut Chat, count: usize) -> Vec<String> {
    timeout(WAIT, async {
        let mut texts = Vec::new();
        while texts.len() < count {
            if let ChatEvent::Received { message, .. } = chat.next_event().await {
                texts.push(message.text);
            }
        }
        texts
    })
    .await
    .expect("as mensagens não chegaram pela caixa postal")
}

/// `a` deposita com `b` fora; `b` volta só com a caixa postal e recebe; quando os dois se conectam,
/// `a` confirma a entrega direta e `b` não vê nada repetido.
#[tokio::test(flavor = "multi_thread")]
async fn deposited_messages_arrive_through_the_mailbox_and_are_not_repeated_on_connect() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    let addr = connect(&mut a, &mut b).await;
    let relay = MemoryRelay::default();
    a.set_mailbox(MemoryMailbox::new(&relay, pa.peer_id()));

    drop(b);
    until_disconnected(&mut a).await;
    a.send(pb.peer_id(), "um", None).unwrap();
    a.send(pb.peer_id(), "dois", None).unwrap();
    until_deposited(&mut a, 2).await;
    assert_eq!(relay.deposits().len(), 2);

    // `b` volta só com a caixa postal: ninguém disca ninguém.
    let mut b = pb.open();
    b.set_mailbox(MemoryMailbox::new(&relay, pb.peer_id()));
    assert_eq!(until_received(&mut b, 2).await, ["um", "dois"]);
    assert_eq!(a.queued(&pb.peer_id()), 2, "depositar não tira da fila");

    // Agora se conectam: `a` reenvia direto e só o ack zera a fila.
    b.dial(addr).unwrap();
    let repeated = until_delivered(&mut a, &mut b, 2, 0).await;
    assert!(
        repeated.is_empty(),
        "reentrega apareceu de novo: {repeated:?}"
    );
    assert_eq!(a.queued(&pb.peer_id()), 0);

    let (_, seen_b) = settle(&mut a, &mut b, Duration::from_secs(2)).await;
    assert!(
        !seen_b
            .iter()
            .any(|e| matches!(e, ChatEvent::Received { .. }))
    );
}

/// Caixa postal em memória que declara guardar por `keep`, como o backend Nostr com TTL.
struct Retaining(MemoryMailbox, Duration);

impl Mailbox for Retaining {
    fn attach(&mut self, events: futures::channel::mpsc::UnboundedSender<MailboxEvent>) {
        self.0.attach(events);
    }

    fn deposit(&mut self, to: PeerId, blob: Vec<u8>) -> DepositId {
        self.0.deposit(to, blob)
    }

    fn retention(&self) -> Option<Duration> {
        Some(self.1)
    }
}

/// 3.4 B: a hora do depósito sobrevive ao reinício. Com retenção de 4 s, depósitos de até ~2 s não se
/// repetem; passados 3,5 s, um reinício seguido de um envio os deposita de novo.
#[tokio::test(flavor = "multi_thread")]
async fn a_deposit_older_than_half_the_retention_is_redone_after_a_restart() {
    let keep = Duration::from_secs(4);
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    connect(&mut a, &mut b).await;
    let relay = MemoryRelay::default();
    a.set_mailbox(Retaining(MemoryMailbox::new(&relay, pa.peer_id()), keep));
    drop(b);
    until_disconnected(&mut a).await;
    a.send(pb.peer_id(), "um", None).unwrap();
    a.send(pb.peer_id(), "dois", None).unwrap();
    until_deposited(&mut a, 2).await;
    drop(a);

    // Reinício logo em seguida: nada é velho, só a nova é depositada.
    let mut a = pa.open();
    a.set_mailbox(Retaining(MemoryMailbox::new(&relay, pa.peer_id()), keep));
    a.send(pb.peer_id(), "tres", None).unwrap();
    until_deposited(&mut a, 1).await;
    assert_eq!(relay.deposits().len(), 3);
    drop(a);

    // Passada mais da metade da retenção: as três voltam, na ordem, mais a nova.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let mut a = pa.open();
    a.set_mailbox(Retaining(MemoryMailbox::new(&relay, pa.peer_id()), keep));
    a.send(pb.peer_id(), "quatro", None).unwrap();
    until_deposited(&mut a, 4).await;

    assert_eq!(relay.deposits().len(), 7, "3 repetidas + a nova");
}

/// Reiniciar o remetente não redeposita o que já foi depositado.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_does_not_deposit_the_same_messages_again() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    connect(&mut a, &mut b).await;
    let relay = MemoryRelay::default();
    a.set_mailbox(MemoryMailbox::new(&relay, pa.peer_id()));

    drop(b);
    until_disconnected(&mut a).await;
    a.send(pb.peer_id(), "um", None).unwrap();
    a.send(pb.peer_id(), "dois", None).unwrap();
    until_deposited(&mut a, 2).await;
    drop(a);

    let mut a = pa.open();
    a.set_mailbox(MemoryMailbox::new(&relay, pa.peer_id()));
    assert_eq!(a.queued(&pb.peer_id()), 2);
    a.send(pb.peer_id(), "tres", None).unwrap();
    until_deposited(&mut a, 1).await;

    assert_eq!(relay.deposits().len(), 3, "só a terceira foi depositada");
}
