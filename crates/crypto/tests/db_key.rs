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

/// Nomes e tamanhos dos arquivos do diretório, em ordem.
fn listing(dir: &Path) -> Vec<(String, u64)> {
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                e.metadata().unwrap().len(),
            )
        })
        .collect();
    files.sort();
    files
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

        let files_before = listing(env.db.parent().unwrap());

        if encrypted {
            assert!(matches!(
                env.open(Some(&other_key())),
                Err(Error::DbWrongKey)
            ));
        } else {
            assert!(matches!(env.open(Some(&key())), Err(Error::DbNotEncrypted)));
        }

        assert_eq!(fs::read(&env.db).unwrap(), before, "encrypted={encrypted}");
        // Nem um `-wal` ou `-shm` novo: a recusa não deixa rastro no diretório.
        if !encrypted {
            assert_eq!(listing(env.db.parent().unwrap()), files_before);
        }
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

// --- auditoria da fatia 2 -------------------------------------------------------------------------------

/// A chave entra como **chave bruta** (`x'…'` de 64 dígitos), com zero à esquerda: um byte < 0x10 escrito com
/// um dígito só faria o SQLCipher cair em passphrase (PBKDF2) sem erro, e os outros testes (que abrem e
/// reabrem pelo mesmo caminho) não notariam. Aqui o banco é reaberto **sem** o `open_db`, com o hex escrito à
/// mão a partir dos bytes literais.
#[test]
fn the_key_is_applied_as_a_raw_64_digit_key_with_leading_zeros() {
    let mut bytes = [0xA5u8; 32];
    bytes[0] = 0x00;
    bytes[1] = 0x0F;
    bytes[2] = 0x01;
    bytes[31] = 0xFF;
    let literal = format!("000f01{}ff", "a5".repeat(28));
    assert_eq!(literal.len(), 64);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw.sqlite");
    {
        let conn = kin_crypto::open_db(&path, Some(&DbKey::from_bytes(bytes))).unwrap();
        conn.execute_batch("CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (42);")
            .unwrap();
    }

    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!("PRAGMA key = \"x'{literal}'\";"))
        .unwrap();
    let v: i64 = conn.query_row("SELECT v FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(v, 42);
}

/// O 0600 agora é do `open_db` (os três bancos passam por ele), e vale na criação e num arquivo antigo.
#[test]
fn the_database_is_created_0600_and_an_old_one_is_tightened() {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let dir = tempfile::tempdir().unwrap();

    for (name, key) in [("com-chave", Some(key())), ("sem-chave", None)] {
        let path = dir.path().join(name);
        let conn = kin_crypto::open_db(&path, key.as_ref()).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t (v INTEGER);")
            .unwrap();
        assert_eq!(mode(&path), 0o600, "{name}");
        assert_eq!(mode(&wal_of(&path)), 0o600, "{name} -wal");
    }

    let old = dir.path().join("antigo");
    drop(kin_crypto::open_db(&old, None).unwrap());
    fs::set_permissions(&old, fs::Permissions::from_mode(0o644)).unwrap();
    drop(kin_crypto::open_db(&old, None).unwrap());
    assert_eq!(mode(&old), 0o600);
}

#[test]
fn check_db_applies_the_policy_without_creating_anything() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nao-existe");
    assert!(kin_crypto::check_db(&missing, Some(&key())).is_ok());
    assert!(!missing.exists());

    let plain = dir.path().join("plain");
    drop(kin_crypto::open_db(&plain, None).unwrap());
    assert!(matches!(
        kin_crypto::check_db(&plain, Some(&key())),
        Err(Error::DbNotEncrypted)
    ));
    assert!(kin_crypto::check_db(&plain, None).is_ok());

    let encrypted = dir.path().join("encrypted");
    drop(kin_crypto::open_db(&encrypted, Some(&key())).unwrap());
    assert!(matches!(
        kin_crypto::check_db(&encrypted, None),
        Err(Error::DbKeyRequired)
    ));
}

/// Arquivos que não são um banco de verdade: nunca abrem "limpos".
#[test]
fn odd_files_never_open_as_a_clean_plain_database() {
    let dir = tempfile::tempdir().unwrap();

    // Um diretório no lugar do banco.
    let folder = dir.path().join("pasta");
    fs::create_dir(&folder).unwrap();
    assert!(matches!(
        kin_crypto::open_db(&folder, None),
        Err(Error::Storage(_))
    ));

    // 15 bytes (um a menos que o cabeçalho): não é "em claro", logo exige chave.
    let short = dir.path().join("curto");
    fs::write(&short, &PLAIN_HEADER[..15]).unwrap();
    assert!(matches!(
        kin_crypto::open_db(&short, None),
        Err(Error::DbKeyRequired)
    ));

    // Só o cabeçalho (16 bytes): é lido como em claro, então recusa a chave.
    let header_only = dir.path().join("so-cabecalho");
    fs::write(&header_only, PLAIN_HEADER).unwrap();
    assert!(matches!(
        kin_crypto::open_db(&header_only, Some(&key())),
        Err(Error::DbNotEncrypted)
    ));
}

/// O device pareado (`open_paired`) também leva a chave: sem este teste, trocar `key` por `None` ali passaria.
#[test]
fn a_paired_device_database_is_encrypted_end_to_end() {
    use kin_identity::{AuthorityIdentity, AuthorityKey, IdentityProvider as _};

    let master = StandaloneIdentity::generate();
    let authority = AuthorityKey::generate();
    let cert = master
        .certify_authority(&authority.public_key(), 1_000)
        .unwrap();
    let primary = AuthorityIdentity::new(authority, cert).unwrap();
    let device = DeviceKey::generate();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("paired.sqlite");

    let pending = PendingDevice::open(&db, Some(&key())).unwrap();
    let request = pending.request(&device).unwrap();
    let sas = request.sas_code(&primary.id());
    let certificate = primary.answer_pair_request(&request, &sas, 2_000).unwrap();
    pending
        .install_certificate(&device, &certificate, &primary.id())
        .unwrap();

    let open = |k: Option<&DbKey>| {
        CryptoDevice::open_paired(
            &primary.id(),
            &device.public_key(),
            &device.nostr_public_key(),
            &db,
            k,
        )
    };
    assert!(open(Some(&key())).is_ok());
    assert!(matches!(open(None), Err(Error::DbKeyRequired)));
    assert!(matches!(open(Some(&other_key())), Err(Error::DbWrongKey)));
    assert!(!is_plain(&db));
}
