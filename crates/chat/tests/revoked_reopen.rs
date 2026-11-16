//! P30 3d-ii-c (ADR 0029): ao reabrir o `Chat`, os tombstones gravados são reconsultados. Um device revogado
//! enquanto o processo estava fechado, ou cuja derrubada parou no meio (o tombstone já no store, a conversa
//! ainda no banco), não volta vivo. O estado é montado gravando o tombstone direto no banco do B com o
//! `CryptoDevice`, com o `Chat` fechado: é o que uma derrubada interrompida deixa em disco.

mod common;

use common::{Person, WAIT, connect, wait_for};
use kin_chat::{Chat, ChatEvent};
use kin_crypto::CryptoDevice;
use kin_identity::{DeviceCertificate, IdentityProvider};
use tokio::time::timeout;

/// Sessão 1: A e B fazem o handshake e fecham. Devolve o certificado do device de A.
async fn talked_once(a: &Person, b: &Person) -> DeviceCertificate {
    let (mut chat_a, mut chat_b) = (a.open(), b.open());
    connect(&mut chat_a, &mut chat_b).await;
    chat_a.certificate().clone()
}

/// Com o `Chat` de B fechado, grava no banco dele o tombstone do device de A (assinado por A).
fn store_tombstone_while_closed(a: &Person, b: &Person, certificate: &DeviceCertificate) {
    let device = CryptoDevice::open(
        &b.identity,
        &b.device.public_key(),
        &b.device.nostr_public_key(),
        &b.db,
        1,
    )
    .unwrap();
    let revocation = a.identity.revoke_device(certificate, 2).unwrap();
    assert!(device.store_revocation(&revocation).unwrap());
}

/// A conversa com um device revogado enquanto estava fechado não volta ao reabrir, e a aplicação é avisada.
#[tokio::test(flavor = "multi_thread")]
async fn reopening_drops_the_conversation_with_a_device_revoked_while_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let certificate = talked_once(&a, &b).await;
    store_tombstone_while_closed(&a, &b, &certificate);

    let mut reopened = b.open();

    assert!(!reopened.is_ready(&a.peer_id()));
    assert!(reopened.ready_peers().is_empty());
    let event = timeout(
        WAIT,
        wait_for(&mut reopened, |e| {
            matches!(e, ChatEvent::PeerRevoked { .. }).then_some(e)
        }),
    )
    .await
    .expect("sem PeerRevoked na reabertura");
    assert_eq!(
        event,
        ChatEvent::PeerRevoked {
            peer: a.peer_id(),
            identity: a.identity.id()
        }
    );
}

/// Sem tombstone, a reabertura continua devolvendo a conversa (controle: nada é derrubado à toa).
#[tokio::test(flavor = "multi_thread")]
async fn reopening_without_a_tombstone_keeps_the_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    talked_once(&a, &b).await;

    let reopened = b.open();

    assert!(reopened.is_ready(&a.peer_id()));
}

/// Um tombstone de **outro** device da mesma identidade não derruba o par vivo (controle de seletividade).
#[tokio::test(flavor = "multi_thread")]
async fn reopening_keeps_a_device_whose_sibling_was_revoked() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    talked_once(&a, &b).await;
    let other = kin_identity::DeviceKey::generate();
    let other_certificate = a
        .identity
        .authorize_device(
            &kin_identity::DeviceKey::generate().public_key(),
            &other.public_key(),
            &other.nostr_public_key(),
            1,
        )
        .unwrap();
    store_tombstone_while_closed(&a, &b, &other_certificate);

    let reopened = b.open();

    assert!(reopened.is_ready(&a.peer_id()));
}

/// Se o store de tombstones não pode ser lido, não dá para saber quem está revogado: a abertura falha
/// (falha fechada), em vez de abrir com um par possivelmente revogado.
#[tokio::test(flavor = "multi_thread")]
async fn reopening_fails_when_the_revocation_store_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    talked_once(&a, &b).await;
    // A tabela existe (o `open` do `crypto` usa `IF NOT EXISTS`) mas sem as colunas: a leitura falha.
    let conn = rusqlite::Connection::open(&b.db).unwrap();
    conn.execute_batch(
        "DROP TABLE kin_revocations; CREATE TABLE kin_revocations (broken INTEGER);",
    )
    .unwrap();
    drop(conn);

    let result = Chat::open(
        &b.identity,
        &b.device,
        kin_transport::NodeConfig {
            mdns: false,
            ..Default::default()
        },
        &b.db,
    );

    assert!(result.is_err(), "abriu com o store ilegível");
}
