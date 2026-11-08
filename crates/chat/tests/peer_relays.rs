//! P23 PR 2: cada dispositivo anuncia seus relays de entrada ao contato por uma mensagem de controle
//! dentro do MLS, e o `Chat` ensina à caixa postal os relays do contato. Dois `Chat` reais, com uma
//! caixa postal que só anota o que lhe ensinam.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Person, connect, settle};
use kin_chat::{ChatEvent, DepositId, Mailbox, MailboxEvent};
use kin_transport::PeerId;

#[derive(Default)]
struct Log {
    /// "attach" e "learn_relays", na ordem em que o `Chat` chamou.
    calls: Vec<&'static str>,
    learned: Vec<(PeerId, Vec<String>)>,
}

struct Recording {
    log: Arc<Mutex<Log>>,
    own: Vec<String>,
}

impl Mailbox for Recording {
    fn attach(&mut self, _events: futures::channel::mpsc::UnboundedSender<MailboxEvent>) {
        self.log.lock().unwrap().calls.push("attach");
    }

    fn deposit(&mut self, _to: PeerId, _blob: Vec<u8>) -> DepositId {
        DepositId(0)
    }

    fn relays(&self) -> Vec<String> {
        self.own.clone()
    }

    fn learn_relays(&mut self, peer: PeerId, relays: Vec<String>) {
        let mut log = self.log.lock().unwrap();
        log.calls.push("learn_relays");
        log.learned.push((peer, relays));
    }
}

fn recording(own: &[&str]) -> (Recording, Arc<Mutex<Log>>) {
    let log = Arc::new(Mutex::new(Log::default()));
    let mailbox = Recording {
        log: log.clone(),
        own: own.iter().map(|s| (*s).to_string()).collect(),
    };
    (mailbox, log)
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn each_side_learns_the_relays_the_other_announced() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    let (mail_a, log_a) = recording(&["wss://a1.example", "wss://a2.example"]);
    let (mail_b, log_b) = recording(&["wss://b.example"]);
    a.set_mailbox(mail_a);
    b.set_mailbox(mail_b);

    connect(&mut a, &mut b).await;
    settle(&mut a, &mut b, Duration::from_millis(500)).await;

    assert_eq!(
        log_a.lock().unwrap().learned,
        [(pb.peer_id(), strings(&["wss://b.example"]))]
    );
    assert_eq!(
        log_b.lock().unwrap().learned,
        [(
            pa.peer_id(),
            strings(&["wss://a1.example", "wss://a2.example"])
        )]
    );
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn the_control_message_is_not_shown_as_a_chat_message_nor_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    a.set_mailbox(recording(&["wss://a.example"]).0);
    b.set_mailbox(recording(&["wss://b.example"]).0);

    connect(&mut a, &mut b).await;
    let (seen_a, seen_b) = settle(&mut a, &mut b, Duration::from_millis(500)).await;

    for event in seen_a.iter().chain(&seen_b) {
        assert!(
            !matches!(
                event,
                ChatEvent::Received { .. } | ChatEvent::Dropped { .. }
            ),
            "evento inesperado: {event:?}"
        );
    }
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn a_mailbox_with_no_relays_announces_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    let (mail_b, log_b) = recording(&["wss://b.example"]);
    a.set_mailbox(recording(&[]).0);
    b.set_mailbox(mail_b);

    connect(&mut a, &mut b).await;
    settle(&mut a, &mut b, Duration::from_millis(500)).await;

    assert!(log_b.lock().unwrap().learned.is_empty());
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn the_contact_relays_survive_a_restart_and_are_taught_before_attach() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    {
        let (mut a, mut b) = (pa.open(), pb.open());
        a.set_mailbox(recording(&["wss://a.example"]).0);
        b.set_mailbox(recording(&["wss://b.example"]).0);
        connect(&mut a, &mut b).await;
        settle(&mut a, &mut b, Duration::from_millis(500)).await;
    }

    // B reabre sozinho, sem o A conectado: o que ele sabia do A tem de estar no banco.
    let mut b = pb.open();
    let (mail_b, log_b) = recording(&["wss://b.example"]);
    b.set_mailbox(mail_b);

    let log = log_b.lock().unwrap();
    assert_eq!(log.calls, ["learn_relays", "attach"]);
    assert_eq!(log.learned, [(pa.peer_id(), strings(&["wss://a.example"]))]);
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn a_new_connection_resends_the_list_and_the_latest_one_wins() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    let (mail_a, log_a) = recording(&["wss://a.example"]);
    a.set_mailbox(mail_a);
    b.set_mailbox(recording(&["wss://antigo.example"]).0);
    let addr = common::listen(&mut a).await;
    b.dial(addr.clone()).unwrap();
    common::connect_until_ready(&mut a, &mut b).await;
    settle(&mut a, &mut b, Duration::from_millis(500)).await;
    drop(b);
    common::until_disconnected(&mut a).await;

    // B reinicia com outra lista de relays e conecta de novo.
    let mut b = pb.open();
    b.set_mailbox(recording(&["wss://novo.example"]).0);
    b.dial(addr).unwrap();
    settle(&mut a, &mut b, Duration::from_secs(2)).await;

    let learned = log_a.lock().unwrap().learned.clone();
    assert_eq!(
        learned.last().unwrap(),
        &(pb.peer_id(), strings(&["wss://novo.example"]))
    );
}
