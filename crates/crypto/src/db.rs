use std::fs::File;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use kin_identity::DbKey;
use rusqlite::{Connection, ErrorCode};
use zeroize::Zeroizing;

use crate::Error;

/// Os 16 primeiros bytes de um banco SQLite **em claro**. Um banco SQLCipher não os tem (a página 1 inteira é
/// cifrada, inclusive o cabeçalho).
const PLAIN_HEADER: &[u8; 16] = b"SQLite format 3\0";

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
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OnDisk::New),
        Err(e) => return Err(Error::storage(e)),
    };
    let mut header = [0u8; 16];
    let mut read = 0;
    while read < header.len() {
        match file.read(&mut header[read..]).map_err(Error::storage)? {
            0 => break,
            n => read += n,
        }
    }
    Ok(match read {
        0 => OnDisk::New,
        16 if &header == PLAIN_HEADER => OnDisk::Plain,
        _ => OnDisk::Other,
    })
}

/// Abre o banco SQLite em `path`, com a política contra downgrade do ADR 0045 (P13):
///
/// | arquivo | chave | resultado |
/// |---|---|---|
/// | novo ou vazio | `Some` / `None` | cria cifrado / cria em claro |
/// | cifrado | certa | abre |
/// | cifrado | errada | [`Error::DbWrongKey`] |
/// | cifrado | `None` | [`Error::DbKeyRequired`] |
/// | em claro | `Some` | [`Error::DbNotEncrypted`] |
/// | em claro | `None` | abre |
///
/// O SQLCipher dá o mesmo erro para "chave errada" e "arquivo em claro"; por isso o cabeçalho é lido **antes**
/// de abrir, e uma recusa nunca toca o arquivo. O `PRAGMA key` é o primeiro comando da conexão (qualquer outro
/// antes dele lê ou grava a página 1 sem cifra). O arquivo fica 0600; `-wal` e `-shm` herdam o modo.
pub fn open_db(path: &Path, key: Option<&DbKey>) -> Result<Connection, Error> {
    match (inspect(path)?, key) {
        (OnDisk::Plain, Some(_)) => return Err(Error::DbNotEncrypted),
        (OnDisk::Other, None) => {}
        _ => {}
    }
    let conn = Connection::open(path).map_err(Error::storage)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(Error::storage)?;
    if let Some(key) = key {
        apply_key(&conn, key)?;
    }
    Ok(conn)
}

fn apply_key(conn: &Connection, key: &DbKey) -> Result<(), Error> {
    let mut hex = Zeroizing::new(String::with_capacity(64));
    for byte in key.expose_secret() {
        hex.push_str(&format!("{byte:02x}"));
    }
    let pragma = Zeroizing::new(format!("PRAGMA key = \"x'{}'\";", hex.as_str()));
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
