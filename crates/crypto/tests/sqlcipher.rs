//! P13 fatia 1 (ADR 0045): o build usa SQLCipher, e um banco cifrado não deixa texto conhecido no arquivo.
//!
//! Só `rusqlite`, sem tocar o `Provider`: a fatia 1 prova a **biblioteca**; ligá-la aos bancos do kin é a fatia 2.
//! Sem a feature `bundled-sqlcipher` o SQLite comum **ignora** `PRAGMA key` e grava tudo em claro; por isso os
//! testes olham os **bytes** do arquivo (com um controle em claro) e `PRAGMA cipher_version`, não só "abriu".

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

const SECRET: &str = "segredo-conhecido-do-teste-p13";
/// Chave bruta de 32 bytes (64 hex), como o ADR 0045 manda entregar ao SQLCipher (`x'…'`, sem PBKDF2 extra).
const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const OTHER_KEY: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";

/// Abre `path`; com `key`, aplica `PRAGMA key` como chave bruta.
fn open(path: &Path, key: Option<&str>) -> Connection {
    let conn = Connection::open(path).unwrap();
    if let Some(key) = key {
        conn.execute_batch(&format!("PRAGMA key = \"x'{key}'\";"))
            .unwrap();
    }
    conn
}

fn write_secret(conn: &Connection) {
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE t (v TEXT NOT NULL);
         INSERT INTO t (v) VALUES ('{SECRET}');"
    ))
    .unwrap();
}

fn read_secret(conn: &Connection) -> rusqlite::Result<String> {
    conn.query_row("SELECT v FROM t", [], |row| row.get(0))
}

fn contains(path: &Path, needle: &str) -> bool {
    let bytes = fs::read(path).unwrap();
    bytes
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

fn wal_of(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", path.display()))
}

/// O teste que prova que a feature entrou: só o SQLCipher responde a `PRAGMA cipher_version` (no SQLite comum
/// o pragma não existe e a consulta não devolve linha).
#[test]
fn the_build_is_sqlcipher_not_plain_sqlite() {
    let conn = Connection::open_in_memory().unwrap();
    let version: Option<String> = conn
        .query_row("PRAGMA cipher_version", [], |row| row.get(0))
        .expect("sem `cipher_version`: o build é SQLite comum, sem SQLCipher");
    // "4.6.1 community" no SQLCipher embutido; a série 4 é a que o ADR 0045 assume (formato de arquivo v4).
    assert!(version.is_some_and(|v| v.starts_with("4.")));
}

/// `NOTADB`: o erro que o SQLCipher dá quando a chave não decifra a página 1 (ou quando o arquivo é em claro).
/// Casar o **código** evita que "no such table" ou "database is locked" passem por falha de chave.
fn is_not_a_database(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::NotADatabase
    )
}

/// Cabeçalho de um SQLite em claro. Um arquivo cifrado não começa assim (os 16 primeiros bytes são o sal).
const PLAIN_HEADER: &[u8; 16] = b"SQLite format 3\0";

#[test]
fn a_keyed_database_does_not_start_with_the_plain_sqlite_header() {
    let dir = tempfile::tempdir().unwrap();
    let keyed = dir.path().join("h-keyed.db");
    let plain = dir.path().join("h-plain.db");
    {
        write_secret(&open(&keyed, Some(KEY)));
        write_secret(&open(&plain, None));
    }
    assert_ne!(&fs::read(&keyed).unwrap()[..16], PLAIN_HEADER);
    // Controle: o banco sem chave começa com o cabeçalho conhecido.
    assert_eq!(&fs::read(&plain).unwrap()[..16], PLAIN_HEADER);
}

#[test]
fn a_keyed_database_reopens_with_the_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.db");
    {
        let conn = open(&path, Some(KEY));
        write_secret(&conn);
    }
    let conn = open(&path, Some(KEY));
    assert_eq!(read_secret(&conn).unwrap(), SECRET);
}

/// O que importa: o texto conhecido não está no arquivo **nem no `-wal`** (que existe enquanto a conexão está
/// aberta, e é onde a escrita recente fica antes do checkpoint).
#[test]
fn a_keyed_database_leaves_no_known_text_in_the_file_or_in_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.db");
    let wal = wal_of(&path);
    {
        let conn = open(&path, Some(KEY));
        write_secret(&conn);
        // Conexão aberta: a escrita recente está no `-wal`. Sem um `-wal` não vazio a checagem abaixo não
        // provaria nada sobre ele.
        assert!(
            fs::metadata(&wal).is_ok_and(|m| m.len() > 0),
            "o -wal não existe ou está vazio: o teste não olhou o WAL"
        );
        assert!(!contains(&wal, SECRET), "texto em claro no -wal");
        assert!(!contains(&path, SECRET), "texto em claro no arquivo");
    }
    // Fechada a conexão o WAL é incorporado ao arquivo: ainda sem o texto.
    assert!(
        !contains(&path, SECRET),
        "texto em claro depois do checkpoint"
    );
}

/// Controle: o mesmo texto num banco **sem** chave aparece nos bytes. Sem isto, "não aparece" seria verdade
/// também se a busca lesse o arquivo errado.
#[test]
fn control_a_plain_database_shows_the_text_in_its_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.db");
    {
        let conn = open(&path, None);
        write_secret(&conn);
        // Conexão aberta: a escrita recente está no `-wal`, e a busca por bytes também o enxerga em claro.
        assert!(
            contains(&wal_of(&path), SECRET),
            "o controle não achou o texto em claro no -wal"
        );
    }
    assert!(
        contains(&path, SECRET),
        "o controle não achou o texto em claro"
    );
}

#[test]
fn the_wrong_key_and_no_key_cannot_read_and_leave_the_file_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.db");
    {
        let conn = open(&path, Some(KEY));
        write_secret(&conn);
    }
    let before = fs::read(&path).unwrap();
    // Sem imprimir a chave na mensagem: o padrão será copiado para as fatias 2 e 3, com chave de verdade.
    for (i, key) in [Some(OTHER_KEY), None].into_iter().enumerate() {
        let conn = open(&path, key);
        let error = read_secret(&conn).expect_err(&format!("leu com a chave de índice {i}"));
        assert!(
            is_not_a_database(&error),
            "erro inesperado no índice {i}: {error}"
        );
    }
    assert!(fs::read(&path).unwrap() == before, "o arquivo mudou");
}

/// Política do ADR 0045 (item 3): banco em claro **só abre sem chave**; com chave é erro.
#[test]
fn a_plain_database_refuses_a_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.db");
    {
        let conn = open(&path, None);
        write_secret(&conn);
    }
    let conn = open(&path, Some(KEY));
    let error = read_secret(&conn).expect_err("um banco em claro abriu com chave");
    assert!(is_not_a_database(&error), "erro inesperado: {error}");
}

/// De-risca a migração da fatia 3 (`--protect`): exportar um banco em claro para uma cópia cifrada.
#[test]
fn export_copies_a_plain_database_into_an_encrypted_one() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.db");
    let encrypted = dir.path().join("enc.db");
    {
        let conn = open(&plain, None);
        write_secret(&conn);
        conn.execute_batch("INSERT INTO t (v) VALUES ('dois'), ('tres'); PRAGMA user_version = 7;")
            .unwrap();
        // `sqlcipher_export` copia tabelas, índices, views e triggers, mas **não** os pragmas: o `user_version`
        // decide as migrações do `Outbox`, então a cópia o recebe à mão (a fatia 3 precisa fazer o mesmo).
        conn.execute_batch(&format!(
            "ATTACH DATABASE '{}' AS enc KEY \"x'{KEY}'\";
             SELECT sqlcipher_export('enc');
             PRAGMA enc.user_version = 7;
             DETACH DATABASE enc;",
            encrypted.display()
        ))
        .unwrap();
    }
    assert!(!contains(&encrypted, SECRET), "a cópia saiu em claro");
    let conn = open(&encrypted, Some(KEY));
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 3);
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7, "o user_version não chegou à cópia cifrada");
    assert_eq!(
        conn.query_row("SELECT v FROM t ORDER BY rowid LIMIT 1", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
        SECRET
    );
}
