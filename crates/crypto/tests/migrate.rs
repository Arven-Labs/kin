//! P13 fatia 3 (ADR 0045): `migrate_to_encrypted` cifra no lugar um banco em claro sem perder dado, sem deixar
//! texto no arquivo nem no `-wal`, e nunca toca um banco que não está em claro.

use std::fs;
use std::path::{Path, PathBuf};

use kin_crypto::{DbKind, db_kind, migrate_to_encrypted, open_db};
use kin_identity::DbKey;
use rusqlite::Connection;

const SECRET: &str = "segredo-da-migracao-p13";
const IN_WAL: &str = "so-no-wal-p13";

fn key() -> DbKey {
    DbKey::from_bytes([0x42; 32])
}

fn other_key() -> DbKey {
    DbKey::from_bytes([0x24; 32])
}

fn contains(path: &Path, needle: &str) -> bool {
    fs::read(path)
        .map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
        .unwrap_or(false)
}

fn wal_of(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", path.display()))
}

/// Banco em claro com duas tabelas, um índice, `user_version = 7` e uma linha (`IN_WAL`) que **só está no
/// `-wal`** (cópia tirada com a conexão ainda aberta, antes do checkpoint do fechamento).
fn plain_db_with_wal(path: &Path) {
    let live = path.with_extension("live");
    let conn = Connection::open(&live).unwrap();
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = WAL;
         PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE t (v TEXT NOT NULL);
         CREATE INDEX t_v ON t (v);
         CREATE TABLE u (n INTEGER NOT NULL);
         INSERT INTO t (v) VALUES ('{SECRET}');
         INSERT INTO t (v) VALUES ('{IN_WAL}');
         INSERT INTO u (n) VALUES (1), (2), (3);
         PRAGMA user_version = 7;"
    ))
    .unwrap();
    fs::copy(&live, path).unwrap();
    fs::copy(wal_of(&live), wal_of(path)).unwrap();
    drop(conn);
    for f in [
        live.clone(),
        wal_of(&live),
        PathBuf::from(format!("{}-shm", live.display())),
    ] {
        let _ = fs::remove_file(f);
    }
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_plain_database_becomes_encrypted_and_keeps_everything_including_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mls.sqlite");
    plain_db_with_wal(&path);
    assert_eq!(db_kind(&path).unwrap(), DbKind::Plain);
    assert!(
        contains(&wal_of(&path), IN_WAL),
        "a montagem do teste falhou"
    );

    assert!(migrate_to_encrypted(&path, &key()).unwrap());

    assert_eq!(db_kind(&path).unwrap(), DbKind::Encrypted);
    let conn = open_db(&path, Some(&key())).unwrap();
    assert_eq!(count(&conn, "t"), 2, "linha do -wal perdida");
    assert_eq!(count(&conn, "u"), 3);
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 7, "o sqlcipher_export não copia o user_version");
    let indexes: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 't_v'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexes, 1);
}

#[test]
fn no_known_text_is_left_in_the_file_or_in_a_leftover_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mls.sqlite");
    plain_db_with_wal(&path);

    migrate_to_encrypted(&path, &key()).unwrap();

    for needle in [SECRET, IN_WAL] {
        assert!(!contains(&path, needle), "{needle} no arquivo");
        assert!(!contains(&wal_of(&path), needle), "{needle} no -wal");
    }
    assert!(
        !wal_of(&path).exists(),
        "um -wal em claro ao lado do arquivo cifrado"
    );
}

#[test]
fn the_migrated_file_is_0600_and_nothing_else_is_left_in_the_directory() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mls.sqlite");
    plain_db_with_wal(&path);

    migrate_to_encrypted(&path, &key()).unwrap();

    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    // O `-shm` é recriado pela própria verificação; nada de `.tmp`, `-wal` ou cópias.
    assert!(
        names
            .iter()
            .all(|n| n == "mls.sqlite" || n == "mls.sqlite-shm"),
        "{names:?}"
    );
}

#[test]
fn an_already_encrypted_database_is_left_untouched_even_with_another_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mls.sqlite");
    plain_db_with_wal(&path);
    assert!(migrate_to_encrypted(&path, &key()).unwrap());
    let before = fs::read(&path).unwrap();

    assert!(!migrate_to_encrypted(&path, &key()).unwrap());
    assert!(!migrate_to_encrypted(&path, &other_key()).unwrap());

    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(count(&open_db(&path, Some(&key())).unwrap(), "t"), 2);
}

#[test]
fn a_missing_or_empty_file_is_not_migrated_and_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nao-existe.db");
    assert!(!migrate_to_encrypted(&missing, &key()).unwrap());
    assert!(!missing.exists());

    let empty = dir.path().join("vazio.db");
    fs::write(&empty, b"").unwrap();
    assert!(!migrate_to_encrypted(&empty, &key()).unwrap());
    assert_eq!(fs::metadata(&empty).unwrap().len(), 0);
}

#[test]
fn a_directory_in_place_of_the_database_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(migrate_to_encrypted(dir.path(), &key()).is_err());
}
