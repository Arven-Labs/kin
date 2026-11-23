use std::fs::{File, OpenOptions};
use std::io::Read as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

use kin_identity::DbKey;
use rusqlite::{Connection, ErrorCode};
use zeroize::Zeroizing;

use crate::Error;

/// Os 16 primeiros bytes de um banco SQLite **em claro**. Um banco SQLCipher não os tem (a página 1 inteira é
/// cifrada, inclusive o cabeçalho).
const PLAIN_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// Dígitos hexadecimais, para montar o `PRAGMA key` sem `format!` por byte (cada um deixaria uma `String`
/// temporária com pedaço da chave na heap).
const HEX: &[u8; 16] = b"0123456789abcdef";

/// O que o arquivo no disco é, antes de abri-lo.
enum OnDisk {
    /// Não existe ou tem 0 bytes: o banco será criado.
    New,
    /// Cabeçalho de SQLite comum.
    Plain,
    /// Tem conteúdo e não é SQLite em claro: cifrado (ou lixo, que não se distingue sem a chave).
    Other,
}

fn inspect(path: &Path) -> Result<OnDisk, Error> {
    // Só arquivo comum: um FIFO bloquearia o `open` para sempre, e um diretório só falharia mais tarde.
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_file() => {
            return Err(Error::Storage("o caminho do banco não é um arquivo".into()));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OnDisk::New),
        Err(e) => return Err(Error::storage(e)),
    }
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OnDisk::New),
        Err(e) => return Err(Error::storage(e)),
    };
    let mut header = [0u8; 16];
    let mut read = 0;
    while read < header.len() {
        match file.read(&mut header[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::storage(e)),
        }
    }
    Ok(match read {
        0 => OnDisk::New,
        16 if &header == PLAIN_HEADER => OnDisk::Plain,
        _ => OnDisk::Other,
    })
}

/// A política contra downgrade do ADR 0045 (P13), **sem abrir nem criar nada**:
///
/// | arquivo | chave | resultado |
/// |---|---|---|
/// | novo ou vazio | `Some` / `None` | ok (cria cifrado / em claro) |
/// | cifrado | certa ou errada | ok aqui (a chave errada só aparece ao abrir: [`Error::DbWrongKey`]) |
/// | cifrado | `None` | [`Error::DbKeyRequired`] |
/// | em claro | `Some` | [`Error::DbNotEncrypted`] |
/// | em claro | `None` | ok |
///
/// Quem abre **dois** bancos (o `Chat`) chama isto nos dois antes de abrir qualquer um, para uma recusa do
/// segundo não deixar o primeiro já criado.
pub fn check_db(path: &Path, key: Option<&DbKey>) -> Result<(), Error> {
    match (inspect(path)?, key) {
        (OnDisk::Plain, Some(_)) => Err(Error::DbNotEncrypted),
        (OnDisk::Other, None) => Err(Error::DbKeyRequired),
        _ => Ok(()),
    }
}

/// Abre o banco SQLite em `path` com a política de [`check_db`] mais a chave errada ([`Error::DbWrongKey`]).
///
/// O SQLCipher dá o mesmo erro para "chave errada" e "arquivo em claro"; por isso o cabeçalho é lido **antes**
/// de abrir, e uma recusa de política nunca toca o arquivo. O `PRAGMA key` é o primeiro comando da conexão
/// (qualquer outro antes dele lê ou grava a página 1 sem cifra). O arquivo é criado já 0600 (sem janela em que
/// outro usuário o abra); `-wal` e `-shm` herdam o modo.
pub fn open_db(path: &Path, key: Option<&DbKey>) -> Result<Connection, Error> {
    check_db(path, key)?;
    // Cria antes, com o modo certo; num arquivo que já existe isto não muda nada.
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(Error::storage)?;
    // Arquivo antigo, criado com outro modo.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(Error::storage)?;
    let conn = Connection::open(path).map_err(Error::storage)?;
    if let Some(key) = key {
        apply_key(&conn, key)?;
    }
    Ok(conn)
}

fn apply_key(conn: &Connection, key: &DbKey) -> Result<(), Error> {
    // Um buffer só, com a capacidade exata (17 + 64 + 3): sem realloc, e é ele que é zerado ao sair.
    let mut pragma = Zeroizing::new(String::with_capacity(84));
    pragma.push_str("PRAGMA key = \"x'");
    for byte in key.expose_secret() {
        pragma.push(HEX[usize::from(byte >> 4)] as char);
        pragma.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    pragma.push_str("'\";");
    // Mensagem fixa: o erro do SQLite não é repassado, para a chave nunca ir parar numa mensagem.
    conn.execute_batch(&pragma)
        .map_err(|_| Error::Storage("PRAGMA key recusado".into()))?;
    // A chave só é testada na primeira leitura de uma página.
    match conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    }) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::NotADatabase => {
            Err(Error::DbWrongKey)
        }
        Err(e) => Err(Error::storage(e)),
    }
}

/// O que o arquivo de banco é no disco, para a CLI decidir o que fazer (P13 fatia 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    /// Não existe ou tem 0 bytes.
    Absent,
    Plain,
    /// Tem conteúdo e não é SQLite em claro (cifrado, ou lixo que não se distingue sem a chave).
    Encrypted,
}

/// Lê só o cabeçalho; não abre nem cria nada. Só arquivo comum (ver [`check_db`]).
pub fn db_kind(path: &Path) -> Result<DbKind, Error> {
    Ok(match inspect(path)? {
        OnDisk::New => DbKind::Absent,
        OnDisk::Plain => DbKind::Plain,
        OnDisk::Other => DbKind::Encrypted,
    })
}

/// Cifra no lugar um banco **em claro** (ADR 0045, item 5). `Ok(true)` se migrou; `Ok(false)` se não havia o
/// que migrar (ausente, vazio ou já cifrado: nunca toca o arquivo nesses casos, nem com chave errada).
///
/// Consolida o `-wal`, exporta para um temporário cifrado no mesmo diretório, **confere** o temporário
/// (`user_version` e linhas por tabela) e só então troca por `rename`. Qualquer falha antes do `rename` deixa o
/// original intacto e apaga o temporário.
pub fn migrate_to_encrypted(path: &Path, key: &DbKey) -> Result<bool, Error> {
    migrate_inner(path, key, |_| Ok(()))
}

/// `after_export` roda com o caminho do temporário já exportado e antes da conferência: é o ponto onde os
/// testes injetam falha ou corrompem o temporário.
fn migrate_inner(
    _path: &Path,
    _key: &DbKey,
    _after_export: impl FnOnce(&Path) -> Result<(), Error>,
) -> Result<bool, Error> {
    unimplemented!("P13 fatia 3")
}

#[cfg(test)]
mod migrate_tests {
    use super::*;

    const SECRET: &str = "segredo-da-migracao-p13";

    fn key() -> DbKey {
        DbKey::from_bytes([0x42; 32])
    }

    fn plain_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE t (v TEXT NOT NULL);
             INSERT INTO t (v) VALUES ('{SECRET}');
             PRAGMA user_version = 7;"
        ))
        .unwrap();
    }

    fn only_the_db_in(dir: &Path) -> bool {
        std::fs::read_dir(dir).unwrap().count() == 1
    }

    #[test]
    fn a_failure_after_the_export_keeps_the_original_and_removes_the_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        plain_db(&path);
        let before = std::fs::read(&path).unwrap();

        let err = migrate_inner(&path, &key(), |tmp| {
            assert!(tmp.exists(), "o temporário já deveria existir");
            Err(Error::Storage("falha injetada".into()))
        })
        .unwrap_err();

        assert!(matches!(err, Error::Storage(_)), "{err:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(db_kind(&path).unwrap(), DbKind::Plain);
        assert!(only_the_db_in(dir.path()), "sobrou temporário");
    }

    #[test]
    fn a_corrupted_temporary_is_caught_by_the_check_and_the_original_survives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        plain_db(&path);
        let before = std::fs::read(&path).unwrap();

        let result = migrate_inner(&path, &key(), |tmp| {
            // Troca o temporário por outro banco cifrado com a MESMA chave, mas vazio: abre bem, só a
            // contagem de linhas e o `user_version` o denunciam.
            std::fs::remove_file(tmp).unwrap();
            let conn = open_db(tmp, Some(&key())).unwrap();
            conn.execute_batch("CREATE TABLE t (v TEXT NOT NULL);")
                .unwrap();
            Ok(())
        });

        assert!(result.is_err(), "a conferência deixou passar: {result:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(only_the_db_in(dir.path()), "sobrou temporário");
    }
}
