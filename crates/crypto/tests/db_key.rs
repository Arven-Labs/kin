//! P13 fatia 2 (ADR 0045): `CryptoDevice::open` e `PendingDevice::open` aceitam `Option<&DbKey>` e aplicam a
//! política contra downgrade. A tabela de casos é a do plano (arquivo novo, cifrado, em claro; chave certa,
//! errada, ausente). Códigos e cabeçalho vêm de `tests/sqlcipher.rs` (fatia 1): `SQLite format 3\0` é o
//! cabeçalho de um banco em claro; um cifrado não o tem.

use std::fs;
use std::path::{Path, PathBuf};

use kin_crypto::{CryptoDevice, Error, PendingDevice};
use kin_identity::{DbKey, DeviceKey, StandaloneIdentity};

const LABEL: &str = "rotulo-conhecido-do-teste-p13";
const PLAIN_HEADER: &[u8; 16] = b"SQLite format 3\0";

fn key() -> DbKey {
    DbKey::from_bytes([0x11; 32])
}

fn other_key() -> DbKey {
    DbKey::from_bytes([0x22; 32])
}

struct Env {
    _dir: tempfile::TempDir,
    db: PathBuf,
    identity: StandaloneIdentity,
    device: DeviceKey,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            db: dir.path().join("mls.sqlite"),
            _dir: dir,
            identity: StandaloneIdentity::generate(),
            device: DeviceKey::generate(),
        }
    }

    fn open(&self, key: Option<&DbKey>) -> Result<CryptoDevice, Error> {
        CryptoDevice::open(
            &self.identity,
            &self.device.public_key(),
            &self.device.nostr_public_key(),
            &self.db,
            1,
            key,
        )
    }

    /// Abre com `key`, grava o rótulo conhecido e fecha (o `-wal` fica ou é incorporado ao fechar).
    fn create_with(&self, key: Option<&DbKey>) {
        let device = self.open(key).unwrap();
        device.remember(LABEL, &[7u8; 16]).unwrap();
    }
}

fn bytes_contain(path: &Path, needle: &[u8]) -> bool {
    fs::read(path)
        .map(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
        .unwrap_or(false)
}

fn wal_of(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", path.display()))
}

fn is_plain(path: &Path) -> bool {
    fs::read(path).unwrap().starts_with(PLAIN_HEADER)
}

// --- arquivo novo ---------------------------------------------------------------------------------------

#[test]
fn a_new_database_with_a_key_is_created_encrypted() {
    let env = Env::new();
    env.create_with(Some(&key()));
    assert!(!is_plain(&env.db), "cabeçalho de banco em claro");
    assert!(!bytes_contain(&env.db, LABEL.as_bytes()));
    assert!(!bytes_contain(&wal_of(&env.db), LABEL.as_bytes()));
}

#[test]
fn a_new_database_without_a_key_is_plain_as_before() {
    // Controle: sem ele, "não aparece" acima poderia ser só um rótulo que nunca chega ao disco.
    let env = Env::new();
    env.create_with(None);
    assert!(is_plain(&env.db));
    assert!(
        bytes_contain(&env.db, LABEL.as_bytes())
            || bytes_contain(&wal_of(&env.db), LABEL.as_bytes())
    );
}

#[test]
fn an_existing_empty_file_counts_as_new() {
    // O 0600 é aplicado logo depois do open, então um arquivo de 0 bytes pode existir.
    let env = Env::new();
    fs::write(&env.db, b"").unwrap();
    env.create_with(Some(&key()));
    assert!(!is_plain(&env.db));
}

// --- arquivo cifrado ------------------------------------------------------------------------------------

#[test]
fn an_encrypted_database_reopens_with_the_same_key() {
    let env = Env::new();
    env.create_with(Some(&key()));
    let device = env.open(Some(&key())).unwrap();
    assert_eq!(
        device.remembered().unwrap(),
        vec![(LABEL.to_string(), vec![7u8; 16])]
    );
}

#[test]
fn an_encrypted_database_refuses_the_wrong_key() {
    let env = Env::new();
    env.create_with(Some(&key()));
    assert!(matches!(
        env.open(Some(&other_key())),
        Err(Error::DbWrongKey)
    ));
}

#[test]
fn an_encrypted_database_refuses_to_open_without_a_key() {
    let env = Env::new();
    env.create_with(Some(&key()));
    assert!(matches!(env.open(None), Err(Error::DbKeyRequired)));
}

// --- arquivo em claro (o downgrade) ---------------------------------------------------------------------

#[test]
fn a_plain_database_refuses_a_key() {
    // Quem troca o banco cifrado por um em claro não pode fazer o app seguir como se nada fosse.
    let env = Env::new();
    env.create_with(None);
    assert!(matches!(env.open(Some(&key())), Err(Error::DbNotEncrypted)));
}

#[test]
fn a_plain_database_still_opens_without_a_key() {
    let env = Env::new();
    env.create_with(None);
    assert!(env.open(None).is_ok());
}

// --- uma recusa não mexe no arquivo ---------------------------------------------------------------------

#[test]
fn a_refused_open_leaves_the_file_untouched() {
    for encrypted in [false, true] {
        let env = Env::new();
        env.create_with(encrypted.then(key).as_ref());
        // Fecha tudo, incorpora o -wal.
        drop(env.open(encrypted.then(key).as_ref()).unwrap());
        let before = fs::read(&env.db).unwrap();

        let refused = if encrypted {
            env.open(Some(&other_key()))
        } else {
            env.open(Some(&key()))
        };
        assert!(refused.is_err());

        assert_eq!(fs::read(&env.db).unwrap(), before, "encrypted={encrypted}");
    }
}

// --- a chave não vaza -----------------------------------------------------------------------------------

#[test]
fn error_messages_do_not_contain_the_key() {
    let env = Env::new();
    env.create_with(Some(&key()));
    let hex = "11".repeat(32);
    for error in [
        env.open(Some(&other_key())).err().unwrap(),
        env.open(None).err().unwrap(),
    ] {
        for shown in [error.to_string(), format!("{error:?}")] {
            assert!(!shown.contains(&hex), "{shown}");
            assert!(!shown.contains(&"22".repeat(32)), "{shown}");
        }
    }
}

// --- PendingDevice (mesmo helper, outro ponto de entrada) -----------------------------------------------

#[test]
fn a_pending_device_database_follows_the_same_policy() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pending.sqlite");

    drop(PendingDevice::open(&db, Some(&key())).unwrap());
    assert!(!is_plain(&db));
    assert!(PendingDevice::open(&db, Some(&key())).is_ok());
    assert!(matches!(
        PendingDevice::open(&db, Some(&other_key())),
        Err(Error::DbWrongKey)
    ));
    assert!(matches!(
        PendingDevice::open(&db, None),
        Err(Error::DbKeyRequired)
    ));

    let plain = dir.path().join("plain.sqlite");
    drop(PendingDevice::open(&plain, None).unwrap());
    assert!(matches!(
        PendingDevice::open(&plain, Some(&key())),
        Err(Error::DbNotEncrypted)
    ));
}
