//! P30 fatia 2d-i: um device pareado (sem mestra nem autoridade, só com o certificado v4 do primário)
//! abre o `Chat` com `Chat::open_paired` e conversa como a identidade do primário.

mod common;

use std::path::Path;

use common::{Person, connect, receive_while_driving};
use kin_chat::{Chat, Error};
use kin_crypto::{Error as CryptoError, PendingDevice};
use kin_identity::{
    AuthorityIdentity, AuthorityKey, DeviceKey, IdentityId, IdentityProvider, StandaloneIdentity,
};
use kin_transport::NodeConfig;

fn config() -> NodeConfig {
    NodeConfig {
        mdns: false,
        ..Default::default()
    }
}

fn primary_of(master: &StandaloneIdentity) -> AuthorityIdentity {
    let key = AuthorityKey::generate();
    let cert = master.certify_authority(&key.public_key(), 1_000).unwrap();
    AuthorityIdentity::new(key, cert).unwrap()
}

/// O pareamento inteiro, como a pessoa faria: pede, o primário responde com o código confirmado, instala.
fn pair(db: &Path, key: &DeviceKey, primary: &AuthorityIdentity) {
    let pending = PendingDevice::open(db).unwrap();
    let request = pending.request(key).unwrap();
    let sas = request.sas_code(&primary.id());
    let cert = primary.answer_pair_request(&request, &sas, 2_000).unwrap();
    pending
        .install_certificate(key, &cert, &primary.id())
        .unwrap();
}

/// Uma identidade com o primário e um device já pareado, no banco `name` de `dir`.
struct Paired {
    primary: AuthorityIdentity,
    key: DeviceKey,
    db: std::path::PathBuf,
}

impl Paired {
    fn new(dir: &Path, name: &str) -> Self {
        let primary = primary_of(&StandaloneIdentity::generate());
        let key = DeviceKey::generate();
        let db = dir.join(format!("{name}.db"));
        pair(&db, &key, &primary);
        Self { primary, key, db }
    }

    fn open(&self) -> Chat {
        Chat::open_paired(&self.primary.id(), &self.key, config(), &self.db).unwrap()
    }
}

/// `Chat` não é `Debug`: pega o erro sem `unwrap_err`.
fn open_error(result: Result<Chat, Error>) -> Error {
    match result {
        Ok(_) => panic!("o chat abriu e devia ter recusado"),
        Err(e) => e,
    }
}

#[test]
fn a_paired_device_opens_as_the_primarys_identity() {
    let dir = tempfile::tempdir().unwrap();
    let paired = Paired::new(dir.path(), "s");

    let chat = paired.open();

    assert_eq!(chat.certificate().identity(), &paired.primary.id());
    assert_eq!(chat.peer_id(), paired.key.peer_id());
}

#[test]
fn another_expected_identity_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let paired = Paired::new(dir.path(), "s");
    let other: IdentityId = StandaloneIdentity::generate().id();

    let error = open_error(Chat::open_paired(&other, &paired.key, config(), &paired.db));

    assert!(matches!(
        error,
        Error::Crypto(CryptoError::IdentityMismatch)
    ));
}

#[test]
fn a_device_still_waiting_for_its_certificate_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("s.db");
    let key = DeviceKey::generate();
    PendingDevice::open(&db).unwrap().request(&key).unwrap();
    let expected = StandaloneIdentity::generate().id();

    let error = open_error(Chat::open_paired(&expected, &key, config(), &db));

    assert!(matches!(error, Error::Crypto(CryptoError::NotPaired)));
}

#[test]
fn another_device_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let paired = Paired::new(dir.path(), "s");

    let error = open_error(Chat::open_paired(
        &paired.primary.id(),
        &DeviceKey::generate(),
        config(),
        &paired.db,
    ));

    assert!(matches!(error, Error::Crypto(CryptoError::DeviceMismatch)));
}

/// O contato conversa com o device pareado e vê a identidade do **primário**, nos dois sentidos.
#[tokio::test(flavor = "multi_thread")]
async fn a_contact_talks_to_a_paired_device_as_the_primarys_identity() {
    let dir = tempfile::tempdir().unwrap();
    let paired = Paired::new(dir.path(), "s");
    let contact = Person::new(dir.path(), "c");
    let (mut s, mut c) = (paired.open(), contact.open());
    connect(&mut s, &mut c).await;

    c.send(paired.key.peer_id(), "oi, secundário", None)
        .unwrap();
    let (sender, message) = receive_while_driving(&mut s, &mut c).await;
    assert_eq!(sender, contact.identity.id());
    assert_eq!(message.text, "oi, secundário");

    s.send(contact.peer_id(), "oi, contato", None).unwrap();
    let (sender, message) = receive_while_driving(&mut c, &mut s).await;
    assert_eq!(sender, paired.primary.id());
    assert_eq!(message.text, "oi, contato");
}

/// Reabrir com `open_paired` recarrega a conversa, sem novo handshake: o envio é aceito de imediato.
#[tokio::test(flavor = "multi_thread")]
async fn reopening_a_paired_device_keeps_its_conversations() {
    let dir = tempfile::tempdir().unwrap();
    let paired = Paired::new(dir.path(), "s");
    let contact = Person::new(dir.path(), "c");
    let (mut s, mut c) = (paired.open(), contact.open());
    connect(&mut s, &mut c).await;
    drop(s);

    let mut reopened = paired.open();

    reopened
        .send(contact.peer_id(), "depois de reiniciar", None)
        .expect("a conversa devia voltar do banco");
}
