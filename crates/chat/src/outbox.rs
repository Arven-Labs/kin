//! Estado local do chat que precisa sobreviver a reinícios (Fase 3.2): a fila de saída e os ids das
//! mensagens já vistas. Fica em SQLite próprio, separado do banco do `crypto`.
//!
//! A fila guarda o texto em claro até a mensagem ser confirmada (só é cifrada na hora de enviar, para
//! o ciphertext não envelhecer; ver ADR 0002). Por isso o arquivo é 0600, como o do `crypto` (P13).

use std::path::Path;

use kin_identity::{IdentityId, PublicKey};
use kin_transport::PeerId;
use rusqlite::{Connection, params};

use crate::{Error, Message, MessageId};

/// Quantos ids de mensagem já vistas se lembra por peer; os mais antigos são esquecidos.
pub(crate) const SEEN_LIMIT: usize = 1024;

/// Versão do esquema (`PRAGMA user_version`). 0 = até a 3.4; 1 = 3.5 etapa 1 (mensagem por
/// `(peer, id)`); 2 = 3.5 etapa 4 (coluna `to_identity` do espelho); 3 = P24 (tabela `kin_trusted`,
/// aditiva: só `CREATE TABLE IF NOT EXISTS`); 4 = P24 (tabela `kin_seen_mirror`, também aditiva);
/// 5 = P24 (tabela `kin_distrusted`, a lista negra do `untrust`, também aditiva). Um build antigo
/// recusa o banco v5 (ver `migrate`): não há downgrade.
const SCHEMA_VERSION: i64 = 5;

/// De qual memória de ids vistos se fala. Cada tipo de payload tem a sua, com teto próprio por peer:
/// um irmão que espelha muito não pode expulsar os ids de mensagens comuns, nem um espelho com o
/// mesmo id de uma mensagem ser descartado como repetição.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeenKind {
    /// `Payload::Message`.
    Message,
    /// `Payload::Sent` (o espelho 3.5).
    Mirror,
}

impl SeenKind {
    /// Nome fixo da tabela: nunca vem de fora, então entra no SQL sem risco de injeção.
    fn table(self) -> &'static str {
        match self {
            Self::Message => "kin_seen",
            Self::Mirror => "kin_seen_mirror",
        }
    }
}

/// O que uma entrada da outbox é (ver [`Outbox::entry_kind`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
    /// Mensagem comum para o device do peer.
    Plain,
    /// Espelho 3.5: "enviei isto a esta identidade", para um device irmão.
    Mirror(IdentityId),
    /// Não há entrada com esse peer e id (já saiu da fila).
    Missing,
}

pub(crate) struct Outbox {
    conn: Connection,
}

impl Outbox {
    /// Só em memória: nada sobrevive ao processo.
    pub(crate) fn memory() -> Result<Self, Error> {
        Self::init(Connection::open_in_memory().map_err(Error::storage)?)
    }

    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let conn = Connection::open(path).map_err(Error::storage)?;
        // Tem texto em claro: só o dono lê. O -wal e o -shm herdam o modo.
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(Error::storage)?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, Error> {
        conn.execute_batch("PRAGMA journal_mode = WAL;")
            .map_err(Error::storage)?;
        Self::migrate(&conn)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kin_outbox (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 peer TEXT NOT NULL,
                 id BLOB NOT NULL,
                 parent BLOB,
                 sent_at_ms INTEGER NOT NULL,
                 text TEXT NOT NULL,
                 to_identity BLOB,
                 UNIQUE (peer, id));
             CREATE INDEX IF NOT EXISTS kin_outbox_peer ON kin_outbox (peer, seq);
             CREATE TABLE IF NOT EXISTS kin_deposited (
                 peer TEXT NOT NULL,
                 id BLOB NOT NULL,
                 PRIMARY KEY (peer, id));
             CREATE TABLE IF NOT EXISTS kin_deposit_times (
                 peer TEXT NOT NULL,
                 id BLOB NOT NULL,
                 at_secs INTEGER NOT NULL,
                 PRIMARY KEY (peer, id));
             CREATE TABLE IF NOT EXISTS kin_peer_relays (
                 peer TEXT PRIMARY KEY,
                 relays TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS kin_trusted (
                 identity BLOB PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS kin_distrusted (
                 identity BLOB PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS kin_seen (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 peer TEXT NOT NULL,
                 id BLOB NOT NULL,
                 UNIQUE (peer, id));
             CREATE TABLE IF NOT EXISTS kin_seen_mirror (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 peer TEXT NOT NULL,
                 id BLOB NOT NULL,
                 UNIQUE (peer, id));",
        )
        .map_err(Error::storage)?;
        Ok(Self { conn })
    }

    /// Leva um banco da 3.4 (versão 0) ao esquema da 3.5: a mesma mensagem lógica pode esperar por
    /// vários devices, então `id` deixa de ser único sozinho e passa a ser por `(peer, id)`. SQLite não
    /// troca constraint no lugar: renomeia, recria, copia e apaga, tudo numa transação (se falhar, volta
    /// ao estado anterior). Banco novo, sem tabelas, só recebe a versão.
    fn migrate(conn: &Connection) -> Result<(), Error> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Error::storage)?;
        // Versão futura (o app foi rebaixado): este código não conhece o esquema, então recusa em vez de
        // ler ou gravar errado. Não toca no banco.
        if version > SCHEMA_VERSION {
            return Err(Error::storage(format!(
                "outbox schema v{version} is newer than this build supports (v{SCHEMA_VERSION})"
            )));
        }
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        let has_old: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'kin_outbox'",
                [],
                |row| row.get(0),
            )
            .map_err(Error::storage)?;
        let tx = conn.unchecked_transaction().map_err(Error::storage)?;
        if has_old && version < 1 {
            tx.execute_batch(
                "-- Bancos anteriores à 3.4 podem não ter estas duas; o formato antigo basta para copiar.
                 CREATE TABLE IF NOT EXISTS kin_deposited (id BLOB PRIMARY KEY, peer TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS kin_deposit_times (id BLOB PRIMARY KEY, at_secs INTEGER NOT NULL);
                 DROP INDEX IF EXISTS kin_outbox_peer;
                 ALTER TABLE kin_outbox RENAME TO kin_outbox_v0;
                 ALTER TABLE kin_deposited RENAME TO kin_deposited_v0;
                 ALTER TABLE kin_deposit_times RENAME TO kin_deposit_times_v0;
                 CREATE TABLE kin_outbox (
                     seq INTEGER PRIMARY KEY AUTOINCREMENT,
                     peer TEXT NOT NULL,
                     id BLOB NOT NULL,
                     parent BLOB,
                     sent_at_ms INTEGER NOT NULL,
                     text TEXT NOT NULL,
                     UNIQUE (peer, id));
                 CREATE TABLE kin_deposited (
                     peer TEXT NOT NULL,
                     id BLOB NOT NULL,
                     PRIMARY KEY (peer, id));
                 CREATE TABLE kin_deposit_times (
                     peer TEXT NOT NULL,
                     id BLOB NOT NULL,
                     at_secs INTEGER NOT NULL,
                     PRIMARY KEY (peer, id));
                 INSERT INTO kin_outbox (seq, peer, id, parent, sent_at_ms, text)
                     SELECT seq, peer, id, parent, sent_at_ms, text FROM kin_outbox_v0;
                 INSERT INTO kin_deposited (peer, id) SELECT peer, id FROM kin_deposited_v0;
                 -- A hora não tinha peer: vem do depósito correspondente. Hora sem depósito já era órfã.
                 INSERT INTO kin_deposit_times (peer, id, at_secs)
                     SELECT d.peer, t.id, t.at_secs FROM kin_deposit_times_v0 t
                     JOIN kin_deposited_v0 d ON d.id = t.id;
                 DROP TABLE kin_outbox_v0;
                 DROP TABLE kin_deposited_v0;
                 DROP TABLE kin_deposit_times_v0;",
            )
            .map_err(Error::storage)?;
        }
        // Versão 2: coluna nula `to_identity`, a marca do espelho multi-device. Só acrescenta; as linhas
        // antigas ficam nulas (envio comum). Banco sem tabelas já nasce com ela (ver `init`).
        if has_old && version < 2 {
            tx.execute_batch("ALTER TABLE kin_outbox ADD COLUMN to_identity BLOB")
                .map_err(Error::storage)?;
        }
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .map_err(Error::storage)?;
        tx.commit().map_err(Error::storage)
    }

    pub(crate) fn enqueue(&self, peer: &PeerId, message: &Message) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO kin_outbox (peer, id, parent, sent_at_ms, text)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    peer.to_string(),
                    &message.id.as_bytes()[..],
                    message.parent.as_ref().map(|p| p.as_bytes().to_vec()),
                    message.sent_at_ms as i64,
                    message.text,
                ],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    /// Como [`Self::enqueue`], mas a entrada é um espelho (3.5): `peer` é um device meu e a mensagem
    /// foi enviada a `to`. Na hora de sair, vai como `Payload::Sent` em vez de mensagem comum.
    pub(crate) fn enqueue_mirror(
        &self,
        peer: &PeerId,
        message: &Message,
        to: &IdentityId,
    ) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO kin_outbox (peer, id, parent, sent_at_ms, text, to_identity)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    peer.to_string(),
                    &message.id.as_bytes()[..],
                    message.parent.as_ref().map(|p| p.as_bytes().to_vec()),
                    message.sent_at_ms as i64,
                    message.text,
                    to.public_key().encode_protobuf(),
                ],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    /// O que é a entrada `id` na fila de `peer`: envio comum, espelho (com a identidade de destino) ou
    /// inexistente. Uma identidade gravada que não decodifica é erro (`Malformed`), nunca `Plain`.
    pub(crate) fn entry_kind(&self, peer: &PeerId, id: &MessageId) -> Result<EntryKind, Error> {
        let row: Option<Option<Vec<u8>>> = self
            .conn
            .query_row(
                "SELECT to_identity FROM kin_outbox WHERE peer = ?1 AND id = ?2",
                params![peer.to_string(), &id.as_bytes()[..]],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
            .map_err(Error::storage)?;
        Ok(match row {
            None => EntryKind::Missing,
            Some(None) => EntryKind::Plain,
            Some(Some(bytes)) => EntryKind::Mirror(
                PublicKey::try_decode_protobuf(&bytes)
                    .map(IdentityId::new)
                    .map_err(|_| Error::Malformed)?,
            ),
        })
    }

    /// Só para teste: grava um destino que não decodifica na entrada (banco corrompido).
    #[cfg(test)]
    pub(crate) fn corrupt_destination(&self, peer: &PeerId, id: &MessageId) {
        self.conn
            .execute(
                "UPDATE kin_outbox SET to_identity = X'00FF' WHERE peer = ?1 AND id = ?2",
                params![peer.to_string(), &id.as_bytes()[..]],
            )
            .unwrap();
    }

    /// Mensagens esperando por `peer`, da mais antiga para a mais nova.
    pub(crate) fn entries(&self, peer: &PeerId) -> Result<Vec<Message>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, parent, sent_at_ms, text FROM kin_outbox
                 WHERE peer = ?1 ORDER BY seq",
            )
            .map_err(Error::storage)?;
        let rows = stmt
            .query_map([peer.to_string()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(Error::storage)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, parent, sent_at_ms, text) = row.map_err(Error::storage)?;
            out.push(Message {
                id: message_id(&id)?,
                parent: parent.as_deref().map(message_id).transpose()?,
                sent_at_ms: sent_at_ms as u64,
                text,
            });
        }
        Ok(out)
    }

    /// Grava que aceitei `identity` (protobuf da chave pública); repetir é inofensivo.
    pub(crate) fn add_trusted(&self, identity: &IdentityId) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO kin_trusted (identity) VALUES (?1)",
                params![identity.public_key().encode_protobuf()],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    /// Deixa de confiar em `identity` (apaga de `kin_trusted`); repetir é inofensivo.
    pub(crate) fn remove_trusted(&self, identity: &IdentityId) -> Result<(), Error> {
        self.conn
            .execute(
                "DELETE FROM kin_trusted WHERE identity = ?1",
                params![identity.public_key().encode_protobuf()],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    /// Grava que desconfio de `identity` (lista negra que vence a confiança herdada de um irmão);
    /// repetir é inofensivo.
    pub(crate) fn add_distrusted(&self, identity: &IdentityId) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO kin_distrusted (identity) VALUES (?1)",
                params![identity.public_key().encode_protobuf()],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    /// Tira `identity` da lista negra (o usuário voltou a confiar de propósito); repetir é inofensivo.
    pub(crate) fn remove_distrusted(&self, identity: &IdentityId) -> Result<(), Error> {
        self.conn
            .execute(
                "DELETE FROM kin_distrusted WHERE identity = ?1",
                params![identity.public_key().encode_protobuf()],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    /// As identidades desconfiadas. Linha que não decodifica é ignorada (como em `trusted_identities`).
    pub(crate) fn distrusted_identities(&self) -> Result<Vec<IdentityId>, Error> {
        let mut statement = self
            .conn
            .prepare("SELECT identity FROM kin_distrusted")
            .map_err(Error::storage)?;
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(Error::storage)?;
        let mut out = Vec::new();
        for row in rows {
            let bytes = row.map_err(Error::storage)?;
            if let Ok(key) = PublicKey::try_decode_protobuf(&bytes) {
                out.push(IdentityId::new(key));
            }
        }
        Ok(out)
    }

    /// As identidades aceitas. Linha que não decodifica é ignorada: vale menos confiar a menos.
    pub(crate) fn trusted_identities(&self) -> Result<Vec<IdentityId>, Error> {
        let mut statement = self
            .conn
            .prepare("SELECT identity FROM kin_trusted")
            .map_err(Error::storage)?;
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(Error::storage)?;
        let mut out = Vec::new();
        for row in rows {
            let bytes = row.map_err(Error::storage)?;
            if let Ok(key) = PublicKey::try_decode_protobuf(&bytes) {
                out.push(IdentityId::new(key));
            }
        }
        Ok(out)
    }

    /// Grava os relays de entrada anunciados por `peer` (já validados); a última lista substitui a
    /// anterior. Um relay por linha: URL validada não tem quebra de linha.
    pub(crate) fn set_peer_relays(&self, peer: &PeerId, relays: &[String]) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO kin_peer_relays (peer, relays) VALUES (?1, ?2)",
                params![peer.to_string(), relays.join("\n")],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    pub(crate) fn peer_relays(&self, peer: &PeerId) -> Result<Option<Vec<String>>, Error> {
        let text: Option<String> = self
            .conn
            .query_row(
                "SELECT relays FROM kin_peer_relays WHERE peer = ?1",
                [peer.to_string()],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
            .map_err(Error::storage)?;
        Ok(text.map(|t| t.lines().map(str::to_string).collect()))
    }

    /// Tira a mensagem da fila de `peer` (e a marca de depositada, se houver). A cópia da mesma
    /// mensagem que espera por outro device fica.
    pub(crate) fn remove(&self, peer: &PeerId, id: &MessageId) -> Result<(), Error> {
        for table in ["kin_outbox", "kin_deposited", "kin_deposit_times"] {
            self.conn
                .execute(
                    &format!("DELETE FROM {table} WHERE peer = ?1 AND id = ?2"),
                    params![peer.to_string(), &id.as_bytes()[..]],
                )
                .map_err(Error::storage)?;
        }
        Ok(())
    }

    /// Registra que a caixa postal aceitou a mensagem `id` para `peer`, às `now` (segundos Unix). Marcar
    /// de novo (re-depósito) só atualiza a hora.
    pub(crate) fn mark_deposited(
        &self,
        peer: &PeerId,
        id: &MessageId,
        now: u64,
    ) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO kin_deposited (peer, id) VALUES (?1, ?2)",
                params![peer.to_string(), &id.as_bytes()[..]],
            )
            .map_err(Error::storage)?;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO kin_deposit_times (peer, id, at_secs) VALUES (?1, ?2, ?3)",
                params![peer.to_string(), &id.as_bytes()[..], now as i64],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    /// Devolve à fila de depósito as mensagens de `peer` depositadas às `cutoff` ou antes: o relay pode
    /// já tê-las expirado. Depósitos sem hora registrada (de antes da 3.4, sem expiração no relay)
    /// nunca expiram. Devolve quantas voltaram.
    pub(crate) fn expire_deposits(&self, peer: &PeerId, cutoff: u64) -> Result<usize, Error> {
        let expired = self
            .conn
            .execute(
                "DELETE FROM kin_deposited WHERE peer = ?1
                 AND id IN (SELECT id FROM kin_deposit_times WHERE peer = ?1 AND at_secs <= ?2)",
                params![peer.to_string(), cutoff as i64],
            )
            .map_err(Error::storage)?;
        // Horas sem depósito correspondente (acabaram de expirar) não servem mais.
        self.conn
            .execute(
                "DELETE FROM kin_deposit_times WHERE peer = ?1 AND at_secs <= ?2
                 AND id NOT IN (SELECT id FROM kin_deposited WHERE peer = ?1)",
                params![peer.to_string(), cutoff as i64],
            )
            .map_err(Error::storage)?;
        Ok(expired)
    }

    /// Quantas mensagens de `peer` estão depositadas e ainda na fila (sem ack direto).
    pub(crate) fn deposited_count(&self, peer: &PeerId) -> Result<usize, Error> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM kin_deposited WHERE peer = ?1",
                [peer.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .map_err(Error::storage)
    }

    /// Mensagens de `peer` na fila que ainda não foram depositadas, da mais antiga para a mais nova.
    pub(crate) fn undeposited(&self, peer: &PeerId) -> Result<Vec<Message>, Error> {
        let deposited: std::collections::HashSet<Vec<u8>> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM kin_deposited WHERE peer = ?1")
                .map_err(Error::storage)?;
            stmt.query_map([peer.to_string()], |row| row.get::<_, Vec<u8>>(0))
                .map_err(Error::storage)?
                .collect::<Result<_, _>>()
                .map_err(Error::storage)?
        };
        Ok(self
            .entries(peer)?
            .into_iter()
            .filter(|m| !deposited.contains(&m.id.as_bytes()[..]))
            .collect())
    }

    /// Registra que `peer` mandou `id` (do tipo `kind`); `false` se já tinha sido visto. Mantém só os
    /// [`SEEN_LIMIT`] ids mais recentes de cada peer, em cada tipo.
    pub(crate) fn mark_seen(
        &self,
        peer: &PeerId,
        id: &MessageId,
        kind: SeenKind,
    ) -> Result<bool, Error> {
        let (peer, table) = (peer.to_string(), kind.table());
        let inserted = self
            .conn
            .execute(
                &format!("INSERT OR IGNORE INTO {table} (peer, id) VALUES (?1, ?2)"),
                params![peer, &id.as_bytes()[..]],
            )
            .map_err(Error::storage)?;
        if inserted == 0 {
            return Ok(false);
        }
        // Apaga do (SEEN_LIMIT+1)º mais novo para trás; se não houver tantos, a subconsulta é NULL.
        self.conn
            .execute(
                &format!(
                    "DELETE FROM {table} WHERE peer = ?1 AND seq <= (
                         SELECT seq FROM {table} WHERE peer = ?1
                         ORDER BY seq DESC LIMIT 1 OFFSET ?2)"
                ),
                params![peer, SEEN_LIMIT as i64],
            )
            .map_err(Error::storage)?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn seen_count(&self, peer: &PeerId, kind: SeenKind) -> Result<usize, Error> {
        self.conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {} WHERE peer = ?1", kind.table()),
                [peer.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .map_err(Error::storage)
    }
}

fn message_id(bytes: &[u8]) -> Result<MessageId, Error> {
    Ok(MessageId::from_bytes(
        bytes.try_into().map_err(|_| Error::Malformed)?,
    ))
}

#[cfg(test)]
mod tests {
    use kin_identity::{DeviceKey, IdentityId, IdentityProvider, StandaloneIdentity};

    use super::*;

    fn peer() -> PeerId {
        DeviceKey::generate().peer_id()
    }

    /// Enfileira `text` para `peer` e o marca como depositado em `at`.
    fn deposited(outbox: &Outbox, peer: &PeerId, text: &str, at: u64) -> MessageId {
        let message = Message::new(text.into(), None);
        outbox.enqueue(peer, &message).unwrap();
        outbox.mark_deposited(peer, &message.id, at).unwrap();
        message.id
    }

    fn waiting(outbox: &Outbox, peer: &PeerId) -> Vec<String> {
        outbox
            .undeposited(peer)
            .unwrap()
            .into_iter()
            .map(|m| m.text)
            .collect()
    }

    fn time_rows(outbox: &Outbox) -> i64 {
        outbox
            .conn
            .query_row("SELECT COUNT(*) FROM kin_deposit_times", [], |r| r.get(0))
            .unwrap()
    }

    // 1
    #[test]
    fn expiring_returns_only_the_deposits_at_or_before_the_cutoff_to_the_queue() {
        let (outbox, peer) = (Outbox::memory().unwrap(), peer());
        deposited(&outbox, &peer, "velha", 100);
        deposited(&outbox, &peer, "no limite", 150);
        deposited(&outbox, &peer, "nova", 200);

        assert_eq!(outbox.expire_deposits(&peer, 150).unwrap(), 2);

        assert_eq!(waiting(&outbox, &peer), ["velha", "no limite"]);
        assert_eq!(outbox.deposited_count(&peer).unwrap(), 1);
    }

    // 2
    #[test]
    fn a_deposit_without_a_time_row_never_expires() {
        let (outbox, peer) = (Outbox::memory().unwrap(), peer());
        let message = Message::new("de antes da 3.4".into(), None);
        outbox.enqueue(&peer, &message).unwrap();
        outbox
            .conn
            .execute(
                "INSERT INTO kin_deposited (id, peer) VALUES (?1, ?2)",
                params![&message.id.as_bytes()[..], peer.to_string()],
            )
            .unwrap();

        assert_eq!(outbox.expire_deposits(&peer, u64::MAX / 2).unwrap(), 0);

        assert!(waiting(&outbox, &peer).is_empty());
    }

    // 3
    #[test]
    fn marking_again_moves_the_time_forward() {
        let (outbox, peer) = (Outbox::memory().unwrap(), peer());
        let id = deposited(&outbox, &peer, "x", 100);

        outbox.mark_deposited(&peer, &id, 300).unwrap();

        assert_eq!(outbox.expire_deposits(&peer, 200).unwrap(), 0);
        assert_eq!(outbox.expire_deposits(&peer, 300).unwrap(), 1);
    }

    // 4
    #[test]
    fn removing_a_message_clears_its_deposit_time() {
        let (outbox, peer) = (Outbox::memory().unwrap(), peer());
        let id = deposited(&outbox, &peer, "x", 100);
        assert_eq!(time_rows(&outbox), 1);

        outbox.remove(&peer, &id).unwrap();

        assert_eq!(time_rows(&outbox), 0);
        assert_eq!(outbox.expire_deposits(&peer, 1000).unwrap(), 0);
    }

    // 5
    #[test]
    fn expiring_one_peer_does_not_touch_another() {
        let (outbox, ana, bia) = (Outbox::memory().unwrap(), peer(), peer());
        deposited(&outbox, &ana, "da ana", 100);
        deposited(&outbox, &bia, "da bia", 100);

        assert_eq!(outbox.expire_deposits(&ana, 500).unwrap(), 1);

        assert_eq!(waiting(&outbox, &ana), ["da ana"]);
        assert!(waiting(&outbox, &bia).is_empty());
    }

    /// Mesma mensagem lógica (mesmo `MessageId`) enfileirada para dois devices.
    fn same_message_for(outbox: &Outbox, peers: [&PeerId; 2]) -> Message {
        let message = Message::new("para todos".into(), None);
        for peer in peers {
            outbox.enqueue(peer, &message).unwrap();
        }
        message
    }

    // 7
    #[test]
    fn the_same_message_id_can_wait_for_two_devices() {
        let (outbox, b1, b2) = (Outbox::memory().unwrap(), peer(), peer());

        let message = same_message_for(&outbox, [&b1, &b2]);

        assert_eq!(outbox.entries(&b1).unwrap(), std::slice::from_ref(&message));
        assert_eq!(outbox.entries(&b2).unwrap(), [message]);
    }

    // 8
    #[test]
    fn an_ack_from_one_device_leaves_the_copy_for_the_other() {
        let (outbox, b1, b2) = (Outbox::memory().unwrap(), peer(), peer());
        let message = same_message_for(&outbox, [&b1, &b2]);
        outbox.mark_deposited(&b1, &message.id, 100).unwrap();
        outbox.mark_deposited(&b2, &message.id, 100).unwrap();

        outbox.remove(&b1, &message.id).unwrap();

        assert!(outbox.entries(&b1).unwrap().is_empty());
        assert_eq!(outbox.deposited_count(&b1).unwrap(), 0);
        assert_eq!(outbox.entries(&b2).unwrap(), [message]);
        assert_eq!(outbox.deposited_count(&b2).unwrap(), 1);
        assert_eq!(time_rows(&outbox), 1, "a hora do b2 continua");
    }

    // 9
    #[test]
    fn a_deposit_for_one_device_does_not_count_for_the_other() {
        let (outbox, b1, b2) = (Outbox::memory().unwrap(), peer(), peer());
        let message = same_message_for(&outbox, [&b1, &b2]);

        outbox.mark_deposited(&b1, &message.id, 100).unwrap();

        assert!(waiting(&outbox, &b1).is_empty());
        assert_eq!(waiting(&outbox, &b2), ["para todos"]);
        assert_eq!(outbox.expire_deposits(&b2, 1000).unwrap(), 0);
        assert_eq!(outbox.expire_deposits(&b1, 1000).unwrap(), 1);
    }

    // 10
    #[test]
    fn a_database_from_before_3_5_keeps_its_data_and_accepts_the_same_id_for_another_peer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        let (old, other) = (peer(), peer());
        let message = Message::new("antiga".into(), None);
        {
            // Esquema da 3.4: `id` único na tabela inteira e PK só por `id`.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE kin_outbox (
                     seq INTEGER PRIMARY KEY AUTOINCREMENT,
                     peer TEXT NOT NULL,
                     id BLOB NOT NULL UNIQUE,
                     parent BLOB,
                     sent_at_ms INTEGER NOT NULL,
                     text TEXT NOT NULL);
                 CREATE TABLE kin_deposited (id BLOB PRIMARY KEY, peer TEXT NOT NULL);
                 CREATE TABLE kin_deposit_times (id BLOB PRIMARY KEY, at_secs INTEGER NOT NULL);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kin_outbox (peer, id, parent, sent_at_ms, text) VALUES (?1, ?2, NULL, ?3, ?4)",
                params![
                    old.to_string(),
                    &message.id.as_bytes()[..],
                    message.sent_at_ms as i64,
                    message.text
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kin_deposited (id, peer) VALUES (?1, ?2)",
                params![&message.id.as_bytes()[..], old.to_string()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kin_deposit_times (id, at_secs) VALUES (?1, 100)",
                params![&message.id.as_bytes()[..]],
            )
            .unwrap();
        }

        let outbox = Outbox::open(&path).unwrap();

        assert_eq!(
            outbox.entries(&old).unwrap(),
            std::slice::from_ref(&message)
        );
        assert_eq!(outbox.deposited_count(&old).unwrap(), 1);
        assert_eq!(
            outbox.expire_deposits(&old, 100).unwrap(),
            1,
            "a hora migrou junto"
        );
        outbox.enqueue(&other, &message).unwrap();
        assert_eq!(outbox.entries(&other).unwrap(), [message]);
    }

    // 11
    #[test]
    fn opening_twice_does_not_migrate_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        let peer = peer();
        {
            let outbox = Outbox::open(&path).unwrap();
            deposited(&outbox, &peer, "x", 100);
        }

        let outbox = Outbox::open(&path).unwrap();

        assert_eq!(outbox.entries(&peer).unwrap().len(), 1);
        assert_eq!(outbox.deposited_count(&peer).unwrap(), 1);
    }

    /// P24: banco de uma versão futura (downgrade do app) não abre em silêncio: este código não conhece
    /// o esquema e poderia ler ou gravar errado. O banco fica intocado.
    #[test]
    fn a_database_from_a_newer_schema_is_refused_and_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        drop(Outbox::open(&path).unwrap());
        let future = SCHEMA_VERSION + 1;
        Connection::open(&path)
            .unwrap()
            .execute_batch(&format!("PRAGMA user_version = {future};"))
            .unwrap();

        let result = Outbox::open(&path);

        assert!(matches!(result, Err(Error::Storage(_))), "recusa");
        let version: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, future, "a versão não foi rebaixada");
    }

    fn identity() -> IdentityId {
        StandaloneIdentity::generate().id()
    }

    // 12
    #[test]
    fn a_mirror_entry_remembers_who_the_message_was_sent_to_and_a_plain_one_does_not() {
        let (outbox, sibling, other) = (Outbox::memory().unwrap(), peer(), peer());
        let (message, bia) = (Message::new("oi".into(), None), identity());

        outbox.enqueue_mirror(&sibling, &message, &bia).unwrap();
        outbox.enqueue(&other, &message).unwrap();

        assert_eq!(
            outbox.entries(&sibling).unwrap(),
            std::slice::from_ref(&message)
        );
        assert_eq!(
            outbox.entry_kind(&sibling, &message.id).unwrap(),
            EntryKind::Mirror(bia)
        );
        assert_eq!(
            outbox.entry_kind(&other, &message.id).unwrap(),
            EntryKind::Plain
        );
    }

    // 16
    #[test]
    fn an_entry_that_does_not_exist_is_missing_not_plain() {
        let (outbox, peer) = (Outbox::memory().unwrap(), peer());
        let message = Message::new("oi".into(), None);

        assert_eq!(
            outbox.entry_kind(&peer, &message.id).unwrap(),
            EntryKind::Missing
        );
        outbox.enqueue(&peer, &message).unwrap();
        assert_eq!(
            outbox.entry_kind(&peer, &message.id).unwrap(),
            EntryKind::Plain
        );
        outbox.remove(&peer, &message.id).unwrap();
        assert_eq!(
            outbox.entry_kind(&peer, &message.id).unwrap(),
            EntryKind::Missing
        );
    }

    // 17
    #[test]
    fn a_stored_destination_that_does_not_decode_is_an_error_not_plain() {
        let (outbox, sibling) = (Outbox::memory().unwrap(), peer());
        let message = Message::new("oi".into(), None);
        outbox
            .enqueue_mirror(&sibling, &message, &identity())
            .unwrap();

        outbox.corrupt_destination(&sibling, &message.id);

        assert!(matches!(
            outbox.entry_kind(&sibling, &message.id),
            Err(Error::Malformed)
        ));
    }

    // 13
    #[test]
    fn a_mirror_entry_survives_reopening_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        let (sibling, message, bia) = (peer(), Message::new("oi".into(), None), identity());
        {
            let outbox = Outbox::open(&path).unwrap();
            outbox.enqueue_mirror(&sibling, &message, &bia).unwrap();
        }

        let outbox = Outbox::open(&path).unwrap();

        assert_eq!(
            outbox.entry_kind(&sibling, &message.id).unwrap(),
            EntryKind::Mirror(bia)
        );
    }

    // 14
    #[test]
    fn a_3_5_step_1_database_keeps_its_entries_and_gains_the_mirror_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        let (old, message) = (peer(), Message::new("antiga".into(), None));
        {
            // Esquema da etapa 1 da 3.5 (versão 1): por (peer, id), ainda sem `to_identity`.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE kin_outbox (
                     seq INTEGER PRIMARY KEY AUTOINCREMENT,
                     peer TEXT NOT NULL,
                     id BLOB NOT NULL,
                     parent BLOB,
                     sent_at_ms INTEGER NOT NULL,
                     text TEXT NOT NULL,
                     UNIQUE (peer, id));
                 PRAGMA user_version = 1;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kin_outbox (peer, id, parent, sent_at_ms, text) VALUES (?1, ?2, NULL, ?3, ?4)",
                params![
                    old.to_string(),
                    &message.id.as_bytes()[..],
                    message.sent_at_ms as i64,
                    message.text
                ],
            )
            .unwrap();
        }

        let outbox = Outbox::open(&path).unwrap();

        assert_eq!(
            outbox.entries(&old).unwrap(),
            std::slice::from_ref(&message)
        );
        assert_eq!(
            outbox.entry_kind(&old, &message.id).unwrap(),
            EntryKind::Plain
        );
        let (sibling, mirrored) = (peer(), Message::new("nova".into(), None));
        outbox
            .enqueue_mirror(&sibling, &mirrored, &identity())
            .unwrap();
        assert!(matches!(
            outbox.entry_kind(&sibling, &mirrored.id).unwrap(),
            EntryKind::Mirror(_)
        ));
    }

    // 15
    #[test]
    fn a_pre_3_5_database_ends_up_able_to_hold_mirror_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE kin_outbox (
                     seq INTEGER PRIMARY KEY AUTOINCREMENT,
                     peer TEXT NOT NULL,
                     id BLOB NOT NULL UNIQUE,
                     parent BLOB,
                     sent_at_ms INTEGER NOT NULL,
                     text TEXT NOT NULL);",
            )
            .unwrap();
        }
        let (sibling, message) = (peer(), Message::new("x".into(), None));

        let outbox = Outbox::open(&path).unwrap();
        outbox
            .enqueue_mirror(&sibling, &message, &identity())
            .unwrap();

        assert!(matches!(
            outbox.entry_kind(&sibling, &message.id).unwrap(),
            EntryKind::Mirror(_)
        ));
    }

    // 6
    #[test]
    fn a_database_without_the_times_table_opens_and_gains_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        let peer = peer();
        {
            let outbox = Outbox::open(&path).unwrap();
            deposited(&outbox, &peer, "antes", 100);
            outbox
                .conn
                .execute("DROP TABLE kin_deposit_times", [])
                .unwrap();
        }

        let outbox = Outbox::open(&path).unwrap();

        assert_eq!(time_rows(&outbox), 0);
        assert_eq!(outbox.expire_deposits(&peer, 1000).unwrap(), 0);
        assert_eq!(
            outbox.deposited_count(&peer).unwrap(),
            1,
            "dados antigos intactos"
        );
    }

    // 13
    #[test]
    fn trusted_identities_survive_reopening_and_are_not_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        let (ana, bia) = (identity(), identity());
        {
            let outbox = Outbox::open(&path).unwrap();
            assert!(outbox.trusted_identities().unwrap().is_empty());
            outbox.add_trusted(&ana).unwrap();
            outbox.add_trusted(&bia).unwrap();
            outbox.add_trusted(&ana).unwrap();
        }

        let outbox = Outbox::open(&path).unwrap();

        let mut got = outbox.trusted_identities().unwrap();
        got.sort_by_key(IdentityId::to_base58);
        let mut want = vec![ana, bia];
        want.sort_by_key(IdentityId::to_base58);
        assert_eq!(got, want);
    }

    #[test]
    fn distrusted_identities_survive_reopening_and_are_not_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        let (ana, bia) = (identity(), identity());
        {
            let outbox = Outbox::open(&path).unwrap();
            assert!(outbox.distrusted_identities().unwrap().is_empty());
            outbox.add_distrusted(&ana).unwrap();
            outbox.add_distrusted(&bia).unwrap();
            outbox.add_distrusted(&ana).unwrap();
        }

        let outbox = Outbox::open(&path).unwrap();

        let mut got = outbox.distrusted_identities().unwrap();
        let mut want = vec![ana, bia];
        got.sort_by_key(IdentityId::to_base58);
        want.sort_by_key(IdentityId::to_base58);
        assert_eq!(got, want);
    }

    #[test]
    fn removing_a_trust_or_a_distrust_touches_only_that_identity_and_is_idempotent() {
        let outbox = Outbox::memory().unwrap();
        let (ana, bia) = (identity(), identity());
        outbox.add_trusted(&ana).unwrap();
        outbox.add_trusted(&bia).unwrap();
        outbox.add_distrusted(&ana).unwrap();
        outbox.add_distrusted(&bia).unwrap();

        outbox.remove_trusted(&ana).unwrap();
        outbox.remove_trusted(&ana).unwrap();
        outbox.remove_distrusted(&bia).unwrap();
        outbox.remove_distrusted(&bia).unwrap();

        assert_eq!(outbox.trusted_identities().unwrap(), vec![bia]);
        assert_eq!(outbox.distrusted_identities().unwrap(), vec![ana]);
    }

    #[test]
    fn a_version_4_database_opens_gains_the_distrusted_table_and_keeps_the_trusted_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        let ana = identity();
        {
            let outbox = Outbox::open(&path).unwrap();
            outbox.add_trusted(&ana).unwrap();
            outbox
                .conn
                .execute_batch("DROP TABLE IF EXISTS kin_distrusted; PRAGMA user_version = 4;")
                .unwrap();
        }

        let outbox = Outbox::open(&path).unwrap();

        assert!(outbox.distrusted_identities().unwrap().is_empty());
        assert_eq!(outbox.trusted_identities().unwrap(), vec![ana]);
    }

    // 14
    #[test]
    fn a_version_2_database_opens_gains_the_trusted_table_and_keeps_its_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        let peer = peer();
        {
            let outbox = Outbox::open(&path).unwrap();
            deposited(&outbox, &peer, "antes", 100);
            outbox
                .conn
                .execute_batch("DROP TABLE kin_trusted; PRAGMA user_version = 2;")
                .unwrap();
        }

        let outbox = Outbox::open(&path).unwrap();

        assert!(outbox.trusted_identities().unwrap().is_empty());
        assert_eq!(outbox.deposited_count(&peer).unwrap(), 1);
    }

    // 18
    #[test]
    fn message_and_mirror_ids_are_remembered_separately_each_with_its_own_cap() {
        let (outbox, peer) = (Outbox::memory().unwrap(), peer());
        let shared = Message::new("mesmo id".into(), None).id;

        assert!(outbox.mark_seen(&peer, &shared, SeenKind::Message).unwrap());
        assert!(
            outbox.mark_seen(&peer, &shared, SeenKind::Mirror).unwrap(),
            "o mesmo id em outro tipo não é repetição"
        );
        assert!(!outbox.mark_seen(&peer, &shared, SeenKind::Message).unwrap());
        assert!(!outbox.mark_seen(&peer, &shared, SeenKind::Mirror).unwrap());

        for i in 0..SEEN_LIMIT + 5 {
            let id = Message::new(i.to_string(), None).id;
            outbox.mark_seen(&peer, &id, SeenKind::Mirror).unwrap();
        }
        assert_eq!(
            outbox.seen_count(&peer, SeenKind::Mirror).unwrap(),
            SEEN_LIMIT
        );
        assert_eq!(
            outbox.seen_count(&peer, SeenKind::Message).unwrap(),
            1,
            "os espelhos não tocam nas mensagens"
        );
    }

    // 19
    #[test]
    fn a_version_3_database_opens_gains_the_mirror_seen_table_and_keeps_its_seen_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("o.db");
        let (peer, id) = (peer(), Message::new("vista".into(), None).id);
        {
            let outbox = Outbox::open(&path).unwrap();
            assert!(outbox.mark_seen(&peer, &id, SeenKind::Message).unwrap());
            outbox
                .conn
                .execute_batch("DROP TABLE kin_seen_mirror; PRAGMA user_version = 3;")
                .unwrap();
        }

        let outbox = Outbox::open(&path).unwrap();

        assert_eq!(outbox.seen_count(&peer, SeenKind::Mirror).unwrap(), 0);
        assert!(
            !outbox.mark_seen(&peer, &id, SeenKind::Message).unwrap(),
            "o que já tinha sido visto continua visto"
        );
    }
}
