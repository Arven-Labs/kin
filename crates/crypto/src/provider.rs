use std::path::Path;
use std::rc::Rc;

use openmls_rust_crypto::RustCrypto;
use openmls_sqlite_storage::{Codec, Connection, SqliteStorageProvider};
use openmls_traits::OpenMlsProvider;
use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;

use crate::Error;

/// Codec do storage MLS: JSON. Simples e estável; o custo de tamanho não importa neste volume.
#[derive(Default)]
pub(crate) struct JsonCodec;

impl Codec for JsonCodec {
    type Error = serde_json::Error;

    fn to_vec<T: Serialize>(value: &T) -> Result<Vec<u8>, Self::Error> {
        serde_json::to_vec(value)
    }

    fn from_slice<T: serde::de::DeserializeOwned>(slice: &[u8]) -> Result<T, Self::Error> {
        serde_json::from_slice(slice)
    }
}

/// Provider do `openmls`: criptografia em Rust + estado MLS no SQLite. Os metadados do Kin
/// (chave do device, conversas conhecidas) ficam no mesmo banco, em tabelas próprias.
pub(crate) struct Provider {
    crypto: RustCrypto,
    storage: SqliteStorageProvider<JsonCodec, Rc<Connection>>,
    conn: Rc<Connection>,
}

impl Provider {
    /// Abre (ou cria) o banco em `path`; sem caminho, usa memória (nada sobrevive ao processo).
    pub(crate) fn open(path: Option<&Path>) -> Result<Self, Error> {
        let mut conn = match path {
            Some(path) => Connection::open(path),
            None => Connection::open_in_memory(),
        }
        .map_err(Error::storage)?;
        if let Some(path) = path {
            // Só o dono lê o estado MLS (sem cifra em repouso, P13); -wal e -shm herdam o modo.
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(Error::storage)?;
        }
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS kin_meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS kin_conversations (
                 label TEXT PRIMARY KEY, group_id BLOB NOT NULL);",
        )
        .map_err(Error::storage)?;
        SqliteStorageProvider::<JsonCodec, &mut Connection>::new(&mut conn)
            .run_migrations()
            .map_err(Error::storage)?;
        let conn = Rc::new(conn);
        Ok(Self {
            crypto: RustCrypto::default(),
            storage: SqliteStorageProvider::new(conn.clone()),
            conn,
        })
    }

    pub(crate) fn meta(&self, key: &str) -> Result<Option<Vec<u8>>, Error> {
        self.conn
            .query_row("SELECT value FROM kin_meta WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(Error::storage)
    }

    pub(crate) fn set_meta(&self, key: &str, value: &[u8]) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO kin_meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    /// Conclui um pareamento: grava o certificado e apaga o pedido pendente **na mesma transação**, para
    /// um corte no meio não deixar o banco com os dois.
    pub(crate) fn finish_pairing(
        &self,
        certificate: &[u8],
        cert_key: &str,
        request_key: &str,
    ) -> Result<(), Error> {
        let tx = self.conn.unchecked_transaction().map_err(Error::storage)?;
        tx.execute(
            "INSERT OR REPLACE INTO kin_meta (key, value) VALUES (?1, ?2)",
            params![cert_key, certificate],
        )
        .map_err(Error::storage)?;
        tx.execute("DELETE FROM kin_meta WHERE key = ?1", [format!("{request_key}_x")])
            .map_err(Error::storage)?;
        tx.commit().map_err(Error::storage)
    }

    pub(crate) fn remember(&self, label: &str, group_id: &[u8]) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO kin_conversations (label, group_id) VALUES (?1, ?2)",
                params![label, group_id],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    pub(crate) fn remembered(&self) -> Result<Vec<(String, Vec<u8>)>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT label, group_id FROM kin_conversations ORDER BY label")
            .map_err(Error::storage)?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(Error::storage)?
            .collect::<Result<_, _>>()
            .map_err(Error::storage)
    }
}

impl OpenMlsProvider for Provider {
    type CryptoProvider = RustCrypto;
    type RandProvider = RustCrypto;
    type StorageProvider = SqliteStorageProvider<JsonCodec, Rc<Connection>>;

    fn storage(&self) -> &Self::StorageProvider {
        &self.storage
    }

    fn crypto(&self) -> &Self::CryptoProvider {
        &self.crypto
    }

    fn rand(&self) -> &Self::RandProvider {
        &self.crypto
    }
}
