//! Prazo do pedido de pareamento (P30 fatia 2d-ii-a1): o pedido vale 900 s (15 min) a partir de quando
//! foi criado; passado o prazo, o certificado não é instalado e o próximo pedido é outro.

use std::path::Path;

use kin_crypto::{Error, PendingDevice};
use kin_identity::{
    AuthorityIdentity, AuthorityKey, DeviceCertificate, DeviceKey, IdentityProvider, PairRequest,
    StandaloneIdentity,
};

const T0: u64 = 1_000_000;

fn primary() -> AuthorityIdentity {
    let master = StandaloneIdentity::generate();
    let key = AuthorityKey::generate();
    let cert = master.certify_authority(&key.public_key(), 1_000).unwrap();
    AuthorityIdentity::new(key, cert).unwrap()
}

fn answer(primary: &AuthorityIdentity, request: &PairRequest) -> DeviceCertificate {
    let sas = request.sas_code(&primary.id());
    primary.answer_pair_request(request, &sas, 2_000).unwrap()
}

/// Escreve direto no banco, como quem edita o arquivo: o `crypto` não expõe a hora do pedido.
fn set_meta(db: &Path, key: &str, value: Option<&[u8]>) {
    let conn = rusqlite::Connection::open(db).unwrap();
    match value {
        Some(v) => conn
            .execute(
                "INSERT OR REPLACE INTO kin_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, v],
            )
            .unwrap(),
        None => conn
            .execute("DELETE FROM kin_meta WHERE key = ?1", [key])
            .unwrap(),
    };
}

fn setup() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    DeviceKey,
    AuthorityIdentity,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("new.db");
    (dir, db, DeviceKey::generate(), primary())
}

#[test]
fn a_certificate_is_installed_up_to_900_seconds_after_the_request() {
    let (_dir, db, key, primary) = setup();
    let pending = PendingDevice::open(&db).unwrap();
    let request = pending.request_at(&key, T0).unwrap();

    let device = pending
        .install_certificate_at(&key, &answer(&primary, &request), &primary.id(), T0 + 900)
        .unwrap();

    assert_eq!(device.identity(), &primary.id());
}

#[test]
fn a_certificate_one_second_late_is_refused_and_the_device_stays_pending() {
    let (_dir, db, key, primary) = setup();
    let pending = PendingDevice::open(&db).unwrap();
    let request = pending.request_at(&key, T0).unwrap();
    let cert = answer(&primary, &request);

    let refused = pending.install_certificate_at(&key, &cert, &primary.id(), T0 + 901);
    assert!(matches!(refused, Err(Error::PairRequestExpired)));

    // Continua pendente (não virou pareado): o mesmo certificado, mas dentro do prazo, ainda instala.
    let again = PendingDevice::open(&db).unwrap();
    again
        .install_certificate_at(&key, &cert, &primary.id(), T0 + 500)
        .unwrap();
}

#[test]
fn asking_again_before_the_deadline_gives_the_same_request() {
    let (_dir, db, key, _primary) = setup();
    let pending = PendingDevice::open(&db).unwrap();

    let first = pending.request_at(&key, T0).unwrap();
    let again = pending.request_at(&key, T0 + 900).unwrap();

    assert_eq!(first, again);
}

#[test]
fn asking_after_the_deadline_gives_a_new_request() {
    let (_dir, db, key, _primary) = setup();
    let pending = PendingDevice::open(&db).unwrap();

    let first = pending.request_at(&key, T0).unwrap();
    let renewed = pending.request_at(&key, T0 + 901).unwrap();

    assert_ne!(first.nonce, renewed.nonce);
    renewed.verify().unwrap();
    // O prazo do pedido novo conta a partir de agora, não do primeiro.
    assert_eq!(pending.request_at(&key, T0 + 901 + 900).unwrap(), renewed);
}

#[test]
fn a_request_without_a_stored_time_counts_as_expired() {
    let (_dir, db, key, primary) = setup();
    let pending = PendingDevice::open(&db).unwrap();
    let request = pending.request_at(&key, T0).unwrap();
    drop(pending);
    set_meta(&db, "pair_requested_at", None);
    let cert = answer(&primary, &request);

    let result =
        PendingDevice::open(&db)
            .unwrap()
            .install_certificate_at(&key, &cert, &primary.id(), T0);

    assert!(matches!(result, Err(Error::PairRequestExpired)));
}

#[test]
fn a_garbled_stored_time_counts_as_expired() {
    let (_dir, db, key, primary) = setup();
    let pending = PendingDevice::open(&db).unwrap();
    let request = pending.request_at(&key, T0).unwrap();
    drop(pending);
    set_meta(&db, "pair_requested_at", Some(b"lixo"));
    let cert = answer(&primary, &request);

    let result =
        PendingDevice::open(&db)
            .unwrap()
            .install_certificate_at(&key, &cert, &primary.id(), T0);

    assert!(matches!(result, Err(Error::PairRequestExpired)));
}

/// O relógio do sistema é o das variantes sem `_at`: um pedido recém-criado instala na hora.
#[test]
fn the_system_clock_variants_accept_a_fresh_request() {
    let (_dir, db, key, primary) = setup();
    let pending = PendingDevice::open(&db).unwrap();
    let request = pending.request(&key).unwrap();

    pending
        .install_certificate(&key, &answer(&primary, &request), &primary.id())
        .unwrap();
}
