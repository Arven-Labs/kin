//! Device pendente de pareamento (P30 fatia 2c-ii, ADR 0016): o device novo guarda a chave MLS e o
//! pedido, recebe o certificado v4 do primário e depois reabre sem `IdentityProvider`.

use std::path::Path;

use kin_crypto::{Conversation, CryptoDevice, Decrypted, Error, Expected, PendingDevice};
use kin_identity::{
    AuthorityIdentity, AuthorityKey, DeviceKey, IdentityProvider, PairRequest, StandaloneIdentity,
};

fn text(d: Decrypted) -> Vec<u8> {
    match d {
        Decrypted::Message { plaintext, .. } => plaintext,
        other => panic!("esperava mensagem, veio {other:?}"),
    }
}

fn primary_of(master: &StandaloneIdentity) -> AuthorityIdentity {
    let key = AuthorityKey::generate();
    let cert = master.certify_authority(&key.public_key(), 1_000).unwrap();
    AuthorityIdentity::new(key, cert).unwrap()
}

/// O primário responde ao pedido com o código confirmado, como a pessoa faria.
fn answer(primary: &AuthorityIdentity, request: &PairRequest) -> kin_identity::DeviceCertificate {
    let sas = request.sas_code(&primary.id());
    primary.answer_pair_request(request, &sas, 2_000).unwrap()
}

/// O caminho feliz inteiro: pede, o primário responde, instala.
fn pair(db: &Path, key: &DeviceKey, primary: &AuthorityIdentity) -> CryptoDevice {
    let pending = PendingDevice::open(db).unwrap();
    let request = pending.request(key).unwrap();
    let cert = answer(primary, &request);
    pending
        .install_certificate(key, &cert, &primary.id())
        .unwrap()
}

/// Um device normal (com a mestra), para conversar com o pareado.
fn normal_device(master: &StandaloneIdentity) -> CryptoDevice {
    let key = DeviceKey::generate();
    CryptoDevice::new(master, &key.public_key(), &key.nostr_public_key(), 1).unwrap()
}

fn db_in(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    dir.path().join(name)
}

// ---- o pedido ----

#[test]
fn a_pending_device_asks_with_its_own_keys() {
    let dir = tempfile::tempdir().unwrap();
    let key = DeviceKey::generate();
    let pending = PendingDevice::open(&db_in(&dir, "new.db")).unwrap();

    let request = pending.request(&key).unwrap();

    request.verify().unwrap();
    assert_eq!(request.network_key, key.public_key());
    assert_eq!(request.nostr_key, key.nostr_public_key());
}

/// O código que a pessoa comparou é calculado sobre este pedido: reabrir não pode trocá-lo.
#[test]
fn reopening_a_pending_device_gives_the_same_request() {
    let dir = tempfile::tempdir().unwrap();
    let db = db_in(&dir, "new.db");
    let key = DeviceKey::generate();

    let first = PendingDevice::open(&db).unwrap().request(&key).unwrap();
    let again_same_session = PendingDevice::open(&db).unwrap().request(&key).unwrap();

    assert_eq!(first, again_same_session);
}

// ---- o fluxo completo ----

#[test]
fn a_paired_device_belongs_to_the_master_and_talks_with_a_normal_device() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let dir = tempfile::tempdir().unwrap();
    let key = DeviceKey::generate();

    let paired = pair(&db_in(&dir, "new.db"), &key, &primary);

    paired.certificate().verify().unwrap();
    assert_eq!(paired.identity(), &master.id());
    assert!(
        paired.certificate().to_bytes().starts_with(b"KDC\x04"),
        "esperava v4"
    );
    assert_eq!(paired.certificate().network_key, key.public_key());

    // A credencial v4 funciona no MLS: um device normal da mesma identidade convida e conversa.
    let normal = normal_device(&master);
    let mut from_normal = Conversation::create(&normal).unwrap();
    let invite = from_normal
        .invite(&paired.key_package().unwrap(), &Expected::default())
        .unwrap();
    let mut on_paired = Conversation::join(&paired, &invite.welcome, &Expected::default()).unwrap();
    let hello = from_normal.encrypt(b"oi").unwrap();
    assert_eq!(text(on_paired.decrypt(&hello).unwrap()), b"oi");
    let back = on_paired.encrypt(b"oi de volta").unwrap();
    assert_eq!(text(from_normal.decrypt(&back).unwrap()), b"oi de volta");
}

#[test]
fn installing_works_after_reopening_the_pending_device() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let dir = tempfile::tempdir().unwrap();
    let db = db_in(&dir, "new.db");
    let key = DeviceKey::generate();

    let request = PendingDevice::open(&db).unwrap().request(&key).unwrap();
    // o device reiniciou; o primário respondeu ao pedido que a pessoa comparou
    let cert = answer(&primary, &request);
    let pending = PendingDevice::open(&db).unwrap();
    assert_eq!(pending.request(&key).unwrap(), request);

    let device = pending
        .install_certificate(&key, &cert, &primary.id())
        .unwrap();
    assert_eq!(device.identity(), &master.id());
}

// ---- certificado errado: recusa e o banco segue pendente ----

/// Os três tipos de certificado que não podem ser instalados, cada um com o que o atacante faria.
fn wrong_certificates(
    request: &PairRequest,
    primary: &AuthorityIdentity,
) -> Vec<(&'static str, kin_identity::DeviceCertificate)> {
    let intruder = StandaloneIdentity::generate();
    let other_keys = DeviceKey::generate();
    let other_request = {
        let dir = tempfile::tempdir().unwrap();
        PendingDevice::open(&db_in(&dir, "other.db"))
            .unwrap()
            .request(&other_keys)
            .unwrap()
    };
    let mut broken = answer(primary, request);
    broken.signature[0] ^= 1;
    vec![
        (
            "identidade do intruso, com as chaves certas",
            intruder
                .authorize_device(
                    &request.signing_key,
                    &request.network_key,
                    &request.nostr_key,
                    2_000,
                )
                .unwrap(),
        ),
        (
            "certificado de outro pedido",
            answer(primary, &other_request),
        ),
        ("assinatura quebrada", broken),
    ]
}

#[test]
fn a_wrong_certificate_is_refused_and_the_database_stays_pending() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let dir = tempfile::tempdir().unwrap();
    let db = db_in(&dir, "new.db");
    let key = DeviceKey::generate();
    let request = PendingDevice::open(&db).unwrap().request(&key).unwrap();

    for (what, cert) in wrong_certificates(&request, &primary) {
        let pending = PendingDevice::open(&db).unwrap();
        let result = pending.install_certificate(&key, &cert, &primary.id());
        assert!(result.is_err(), "instalou: {what}");
    }

    // continua pendente, com o mesmo pedido, e o certificado certo ainda vale
    let pending = PendingDevice::open(&db).unwrap();
    assert_eq!(pending.request(&key).unwrap(), request);
    let device = pending
        .install_certificate(&key, &answer(&primary, &request), &primary.id())
        .unwrap();
    assert_eq!(device.identity(), &master.id());
}

#[test]
fn a_certificate_of_another_expected_identity_is_refused() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let dir = tempfile::tempdir().unwrap();
    let key = DeviceKey::generate();
    let pending = PendingDevice::open(&db_in(&dir, "new.db")).unwrap();
    let request = pending.request(&key).unwrap();
    let cert = answer(&primary, &request);

    // o certificado é bom, mas a pessoa esperava outra identidade
    let expected = StandaloneIdentity::generate().id();
    assert!(pending.install_certificate(&key, &cert, &expected).is_err());
}

/// Auditoria 2c-ii, I2: o pedido guardado é de uma `device.key`; se a que está em uso agora é outra
/// (regenerada ou restaurada), instalar deixaria o banco pareado com chaves que o device não tem.
#[test]
fn installing_with_another_device_key_is_refused_and_the_database_stays_pending() {
    let primary = primary_of(&StandaloneIdentity::generate());
    let dir = tempfile::tempdir().unwrap();
    let db = db_in(&dir, "new.db");
    let key = DeviceKey::generate();
    let pending = PendingDevice::open(&db).unwrap();
    let request = pending.request(&key).unwrap();
    let cert = answer(&primary, &request);

    let other = DeviceKey::generate();
    assert!(matches!(
        pending.install_certificate(&other, &cert, &primary.id()),
        Err(Error::DeviceMismatch)
    ));

    // segue pendente, e com a chave certa ainda instala
    let pending = PendingDevice::open(&db).unwrap();
    assert_eq!(pending.request(&key).unwrap(), request);
    pending
        .install_certificate(&key, &cert, &primary.id())
        .unwrap();
}

// ---- descartar o pedido (auditoria 2c-ii, S4) ----

/// A pessoa viu um código diferente no primário e desistiu: o pedido some, o próximo é outro, e o
/// certificado do pedido antigo não serve mais.
#[test]
fn a_discarded_request_is_replaced_and_its_certificate_stops_working() {
    let primary = primary_of(&StandaloneIdentity::generate());
    let dir = tempfile::tempdir().unwrap();
    let db = db_in(&dir, "new.db");
    let key = DeviceKey::generate();
    let pending = PendingDevice::open(&db).unwrap();
    let old = pending.request(&key).unwrap();
    let old_cert = answer(&primary, &old);

    pending.discard_request().unwrap();
    let new = pending.request(&key).unwrap();

    new.verify().unwrap();
    assert_ne!(new.nonce, old.nonce);
    assert_ne!(new.sas_code(&primary.id()), old.sas_code(&primary.id()));
    assert_eq!(new.signing_key, old.signing_key, "a chave MLS é a mesma");
    assert!(
        pending
            .install_certificate(&key, &old_cert, &primary.id())
            .is_err()
    );
}

#[test]
fn discarding_without_a_request_is_harmless() {
    let dir = tempfile::tempdir().unwrap();
    let pending = PendingDevice::open(&db_in(&dir, "new.db")).unwrap();
    pending.discard_request().unwrap();
    pending.request(&DeviceKey::generate()).unwrap();
}

#[test]
fn installing_without_a_request_is_an_error() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let dir = tempfile::tempdir().unwrap();
    // um certificado qualquer, de outro pedido
    let someone = PendingDevice::open(&db_in(&dir, "someone.db")).unwrap();
    let cert = answer(&primary, &someone.request(&DeviceKey::generate()).unwrap());

    let empty = PendingDevice::open(&db_in(&dir, "empty.db")).unwrap();
    assert!(matches!(
        empty.install_certificate(&DeviceKey::generate(), &cert, &primary.id()),
        Err(Error::NotPending)
    ));
}

// ---- depois de instalar ----

#[test]
fn a_paired_device_reopens_without_an_identity_provider_and_keeps_its_conversations() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let dir = tempfile::tempdir().unwrap();
    let db = db_in(&dir, "new.db");
    let key = DeviceKey::generate();
    let normal = normal_device(&master);

    let (cert, id) = {
        let paired = pair(&db, &key, &primary);
        let mut from_normal = Conversation::create(&normal).unwrap();
        let invite = from_normal
            .invite(&paired.key_package().unwrap(), &Expected::default())
            .unwrap();
        let on_paired = Conversation::join(&paired, &invite.welcome, &Expected::default()).unwrap();
        paired.remember("normal", &on_paired.id()).unwrap();
        (paired.certificate().clone(), on_paired.id())
    };

    let reopened = CryptoDevice::open_paired(
        &master.id(),
        &key.public_key(),
        &key.nostr_public_key(),
        &db,
    )
    .unwrap();
    assert_eq!(reopened.certificate(), &cert);
    assert_eq!(reopened.identity(), &master.id());
    assert!(Conversation::load(&reopened, &id).unwrap().is_some());
}

#[test]
fn a_paired_database_is_no_longer_pending() {
    let primary = primary_of(&StandaloneIdentity::generate());
    let dir = tempfile::tempdir().unwrap();
    let db = db_in(&dir, "new.db");
    let _ = pair(&db, &DeviceKey::generate(), &primary);

    assert!(matches!(
        PendingDevice::open(&db),
        Err(Error::AlreadyPaired)
    ));
}

// ---- open_paired recusa o que não é um device pareado ----

#[test]
fn open_paired_refuses_a_pending_or_empty_database() {
    let dir = tempfile::tempdir().unwrap();
    let key = DeviceKey::generate();
    let (net, nostr) = (key.public_key(), key.nostr_public_key());
    let who = StandaloneIdentity::generate().id();

    let pending_db = db_in(&dir, "pending.db");
    PendingDevice::open(&pending_db)
        .unwrap()
        .request(&key)
        .unwrap();
    assert!(matches!(
        CryptoDevice::open_paired(&who, &net, &nostr, &pending_db),
        Err(Error::NotPaired)
    ));

    assert!(matches!(
        CryptoDevice::open_paired(&who, &net, &nostr, &db_in(&dir, "empty.db")),
        Err(Error::NotPaired)
    ));
}

#[test]
fn open_paired_refuses_other_network_or_nostr_keys() {
    let primary = primary_of(&StandaloneIdentity::generate());
    let dir = tempfile::tempdir().unwrap();
    let db = db_in(&dir, "new.db");
    let key = DeviceKey::generate();
    let _ = pair(&db, &key, &primary);
    let other = DeviceKey::generate();
    let me = primary.id();

    assert!(matches!(
        CryptoDevice::open_paired(&me, &other.public_key(), &key.nostr_public_key(), &db),
        Err(Error::DeviceMismatch)
    ));
    assert!(matches!(
        CryptoDevice::open_paired(&me, &key.public_key(), &other.nostr_public_key(), &db),
        Err(Error::DeviceMismatch)
    ));
}

/// Auditoria 2c-ii, I1: um certificado **válido** de outra identidade, para as mesmas três chaves,
/// colocado no banco. `authorize_device` não exige prova de posse, então qualquer um o emite. Sem fixar a
/// identidade esperada (guardada fora do banco), a reabertura o aceitaria.
#[test]
fn open_paired_refuses_a_certificate_of_another_identity_for_the_same_keys() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let dir = tempfile::tempdir().unwrap();
    let db = db_in(&dir, "new.db");
    let key = DeviceKey::generate();
    let paired = pair(&db, &key, &primary);
    let mls = paired.certificate().signing_key.clone();
    drop(paired);

    let intruder = StandaloneIdentity::generate();
    let forged = intruder
        .authorize_device(&mls, &key.public_key(), &key.nostr_public_key(), 2_000)
        .unwrap();
    forged.verify().unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE kin_meta SET value = ?1 WHERE key = 'device_certificate'",
        [forged.to_bytes()],
    )
    .unwrap();
    drop(conn);

    let result = CryptoDevice::open_paired(
        &master.id(),
        &key.public_key(),
        &key.nostr_public_key(),
        &db,
    );
    assert!(matches!(result, Err(Error::IdentityMismatch)));
}

/// P31(b): o certificado do banco é verificado a cada abertura, e a chave MLS dele é a do signer.
#[test]
fn open_paired_refuses_a_tampered_or_swapped_certificate_in_the_database() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let dir = tempfile::tempdir().unwrap();
    let key = DeviceKey::generate();
    let (net, nostr) = (key.public_key(), key.nostr_public_key());

    let tamper = |name: &str, new_cert: &dyn Fn(&kin_identity::DeviceCertificate) -> Vec<u8>| {
        let db = db_in(&dir, name);
        let paired = pair(&db, &key, &primary);
        let replacement = new_cert(paired.certificate());
        drop(paired);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE kin_meta SET value = ?1 WHERE key = 'device_certificate'",
            [replacement],
        )
        .unwrap();
        drop(conn);
        db
    };

    // assinatura quebrada
    let broken = tamper("broken.db", &|cert| {
        let mut cert = cert.clone();
        cert.signature[0] ^= 1;
        cert.to_bytes()
    });
    assert!(CryptoDevice::open_paired(&master.id(), &net, &nostr, &broken).is_err());

    // certificado válido, das mesmas chaves de rede e Nostr, mas de outra chave MLS
    let other_mls = DeviceKey::generate();
    let swapped = tamper("swapped.db", &|_| {
        primary
            .authorize_device(&other_mls.public_key(), &net, &nostr, 2_000)
            .unwrap()
            .to_bytes()
    });
    assert!(CryptoDevice::open_paired(&master.id(), &net, &nostr, &swapped).is_err());
}

/// O pedido só existe enquanto o certificado não chegou: pareado, o banco não o guarda mais.
#[test]
fn pairing_removes_the_pending_request_from_the_database() {
    let primary = primary_of(&StandaloneIdentity::generate());
    let dir = tempfile::tempdir().unwrap();
    let db = db_in(&dir, "new.db");
    let key = DeviceKey::generate();

    let before = {
        let _ = PendingDevice::open(&db).unwrap().request(&key).unwrap();
        stored_keys(&db)
    };
    let _ = pair(&db, &key, &primary);
    let after = stored_keys(&db);

    assert!(before.contains(&"pair_request".to_string()));
    assert!(!after.contains(&"pair_request".to_string()), "{after:?}");
    assert!(after.contains(&"device_certificate".to_string()));
}

fn stored_keys(db: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn.prepare("SELECT key FROM kin_meta").unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// O banco é só um arquivo: um pedido guardado de outra `device.key` não pode ser devolvido como se
/// fosse deste device.
#[test]
fn a_stored_request_of_another_device_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = db_in(&dir, "new.db");
    let key = DeviceKey::generate();
    PendingDevice::open(&db).unwrap().request(&key).unwrap();

    let other = DeviceKey::generate();
    let pending = PendingDevice::open(&db).unwrap();
    assert!(matches!(
        pending.request(&other),
        Err(Error::DeviceMismatch)
    ));
}

// ---- um banco pendente não vira mestra ou primário por engano ----

#[test]
fn a_pending_database_cannot_be_opened_as_a_master_device() {
    let master = StandaloneIdentity::generate();
    let dir = tempfile::tempdir().unwrap();
    let db = db_in(&dir, "new.db");
    let key = DeviceKey::generate();
    PendingDevice::open(&db).unwrap().request(&key).unwrap();

    let result = CryptoDevice::open(&master, &key.public_key(), &key.nostr_public_key(), &db, 1);

    assert!(matches!(result, Err(Error::PairingInProgress)));
}
