//! Fase 3.3 (ADR 0004): o `Chat` ensina à caixa postal a chave Nostr de cada par, vinda do certificado.

mod common;

use std::sync::{Arc, Mutex};

use common::{Person, connect};
use kin_chat::{DepositId, Mailbox, MailboxEvent};
use kin_identity::NostrPublicKey;
use kin_transport::PeerId;

type Learned = Arc<Mutex<Vec<(PeerId, NostrPublicKey)>>>;

/// Backend que só anota o que aprendeu.
struct Recording(Learned);

impl Mailbox for Recording {
    fn attach(&mut self, _events: futures::channel::mpsc::UnboundedSender<MailboxEvent>) {}

    fn deposit(&mut self, _to: PeerId, _blob: Vec<u8>) -> DepositId {
        DepositId(0)
    }

    fn learn_peer(&mut self, peer: PeerId, nostr: NostrPublicKey) {
        self.0.lock().unwrap().push((peer, nostr));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn both_mailboxes_learn_the_other_nostr_key_after_the_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    let (learned_a, learned_b) = (Learned::default(), Learned::default());
    a.set_mailbox(Recording(learned_a.clone()));
    b.set_mailbox(Recording(learned_b.clone()));

    connect(&mut a, &mut b).await;

    assert_eq!(
        *learned_a.lock().unwrap(),
        vec![(pb.peer_id(), pb.device.nostr_public_key())]
    );
    assert_eq!(
        *learned_b.lock().unwrap(),
        vec![(pa.peer_id(), pa.device.nostr_public_key())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reopened_chat_teaches_the_mailbox_without_a_new_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    {
        let (mut a, mut b) = (pa.open(), pb.open());
        connect(&mut a, &mut b).await;
    }

    // Reinício: a conversa volta do banco, sem ninguém discar ninguém.
    let mut b = pb.open();
    let learned = Learned::default();
    b.set_mailbox(Recording(learned.clone()));

    assert_eq!(
        *learned.lock().unwrap(),
        vec![(pa.peer_id(), pa.device.nostr_public_key())]
    );
}
