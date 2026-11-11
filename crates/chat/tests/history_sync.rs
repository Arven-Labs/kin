//! P10a (fatia 2): histórico entre irmãos de ponta a ponta, com `Chat` reais. A1 conversa com Bia; depois
//! A2 (a mesma identidade, device e banco novos) entra e tem de receber o histórico de A1, sem ninguém
//! pedir por fora: o pedido sai quando a conversa entre os irmãos fica pronta.

mod common;

use common::{Person, WAIT, connect, until_delivered};
use kin_chat::{Chat, ChatEvent};
use kin_identity::{DeviceKey, IdentityId, IdentityProvider, StandaloneIdentity};
use tokio::time::timeout;

/// O mesmo `StandaloneIdentity` em outro device (o pareamento copia a chave mestra).
fn copy_of(identity: &StandaloneIdentity) -> StandaloneIdentity {
    StandaloneIdentity::from_bytes(&identity.to_bytes().unwrap()).unwrap()
}

fn texts(chat: &Chat, contact: &IdentityId) -> Vec<(bool, String)> {
    chat.history(contact, None, 100)
        .unwrap()
        .into_iter()
        .map(|entry| (entry.outgoing, entry.message.text))
        .collect()
}

fn expected() -> Vec<(bool, String)> {
    vec![
        (true, "oi, bia".to_string()),
        (false, "oi, ana".to_string()),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_joins_later_receives_the_history_of_its_sibling() {
    let dir = tempfile::tempdir().unwrap();
    let a1 = Person::new(dir.path(), "a1");
    let bia = Person::new(dir.path(), "bia");
    let a2 = Person {
        identity: copy_of(&a1.identity),
        device: DeviceKey::generate(),
        db: dir.path().join("a2.db"),
    };
    let (mut a1_chat, mut bia_chat, mut a2_chat) = (a1.open(), bia.open(), a2.open());
    let (ana_id, bia_id) = (a1.identity.id(), bia.identity.id());

    // A1 e Bia conversam, uma mensagem em cada sentido: isso vira o histórico de A1.
    let a1_addr = connect(&mut a1_chat, &mut bia_chat).await;
    a1_chat.send_to(&bia_id, "oi, bia", None).unwrap();
    until_delivered(&mut a1_chat, &mut bia_chat, 1, 1).await;
    bia_chat.send_to(&ana_id, "oi, ana", None).unwrap();
    until_delivered(&mut bia_chat, &mut a1_chat, 1, 1).await;
    assert_eq!(texts(&a1_chat, &bia_id), expected());
    assert!(
        texts(&a2_chat, &bia_id).is_empty(),
        "A2 ainda não sabe de nada"
    );

    // A2 entra. Os três são dirigidos juntos até A2 terminar de sincronizar com A1.
    a2_chat.dial(a1_addr).unwrap();
    let mut dropped = Vec::new();
    let mut a2_imported = None;
    timeout(WAIT, async {
        while a2_imported.is_none() {
            tokio::select! {
                e = a1_chat.next_event() => if let ChatEvent::Dropped { reason, .. } = e { dropped.push(reason) },
                e = bia_chat.next_event() => if let ChatEvent::Dropped { reason, .. } = e { dropped.push(reason) },
                e = a2_chat.next_event() => match e {
                    ChatEvent::HistorySynced { imported, done: true, .. } => a2_imported = Some(imported),
                    ChatEvent::Dropped { reason, .. } => dropped.push(reason),
                    _ => {}
                },
            }
        }
    })
    .await
    .expect("A2 não terminou de sincronizar o histórico com A1");

    assert!(dropped.is_empty(), "nada pode ser descartado: {dropped:?}");
    assert_eq!(a2_imported, Some(2));
    assert_eq!(
        texts(&a2_chat, &bia_id),
        expected(),
        "mesma ordem e a mesma direção do histórico de A1"
    );
    assert_eq!(
        texts(&a1_chat, &bia_id).len(),
        2,
        "A1 não ganha nada (A2 não tinha histórico)"
    );
}
