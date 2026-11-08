//! Estado local do chat que precisa sobreviver a reinícios (Fase 3.2): a fila de saída e os ids das
//! mensagens já vistas. Fica em SQLite próprio, separado do banco do `crypto`.
//!
//! A fila guarda o texto em claro até a mensagem ser confirmada (só é cifrada na hora de enviar, para
//! o ciphertext não envelhecer; ver ADR 0002). Por isso o arquivo é 0600, como o do `crypto` (P13).

use std::path::Path;

use kin_transport::PeerId;
use rusqlite::{Connection, params};

use crate::{Error, Message, MessageId};

/// Quantos ids de mensagem já vistas se lembra por peer; os mais antigos são esquecidos.
pub(crate) const SEEN_LIMIT: usize = 1024;

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
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS kin_outbox (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 peer TEXT NOT NULL,
                 id BLOB NOT NULL UNIQUE,
                 parent BLOB,
                 sent_at_ms INTEGER NOT NULL,
                 text TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS kin_outbox_peer ON kin_outbox (peer, seq);
             CREATE TABLE IF NOT EXISTS kin_deposited (
                 id BLOB PRIMARY KEY,
                 peer TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS kin_deposit_times (
                 id BLOB PRIMARY KEY,
                 at_secs INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS kin_peer_relays (
                 peer TEXT PRIMARY KEY,
                 relays TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS kin_seen (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 peer TEXT NOT NULL,
                 id BLOB NOT NULL,
                 UNIQUE (peer, id));",
        )
        .map_err(Error::storage)?;
        Ok(Self { conn })
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

    /// Tira a mensagem da fila (e a marca de depositada, se houver).
    pub(crate) fn remove(&self, id: &MessageId) -> Result<(), Error> {
        for table in ["kin_outbox", "kin_deposited", "kin_deposit_times"] {
            self.conn
                .execute(
                    &format!("DELETE FROM {table} WHERE id = ?1"),
                    [&id.as_bytes()[..]],
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
                "INSERT OR IGNORE INTO kin_deposited (id, peer) VALUES (?1, ?2)",
                params![&id.as_bytes()[..], peer.to_string()],
            )
            .map_err(Error::storage)?;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO kin_deposit_times (id, at_secs) VALUES (?1, ?2)",
                params![&id.as_bytes()[..], now as i64],
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
                 AND id IN (SELECT id FROM kin_deposit_times WHERE at_secs <= ?2)",
                params![peer.to_string(), cutoff as i64],
            )
            .map_err(Error::storage)?;
        // Horas sem depósito correspondente (acabaram de expirar) não servem mais.
        self.conn
            .execute(
                "DELETE FROM kin_deposit_times WHERE at_secs <= ?1
                 AND id NOT IN (SELECT id FROM kin_deposited)",
                [cutoff as i64],
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

    /// Registra que `peer` mandou a mensagem `id`; `false` se já tinha sido vista. Mantém só os
    /// [`SEEN_LIMIT`] ids mais recentes de cada peer.
    pub(crate) fn mark_seen(&self, peer: &PeerId, id: &MessageId) -> Result<bool, Error> {
        let peer = peer.to_string();
        let inserted = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO kin_seen (peer, id) VALUES (?1, ?2)",
                params![peer, &id.as_bytes()[..]],
            )
            .map_err(Error::storage)?;
        if inserted == 0 {
            return Ok(false);
        }
        // Apaga do (SEEN_LIMIT+1)º mais novo para trás; se não houver tantos, a subconsulta é NULL.
        self.conn
            .execute(
                "DELETE FROM kin_seen WHERE peer = ?1 AND seq <= (
                     SELECT seq FROM kin_seen WHERE peer = ?1
                     ORDER BY seq DESC LIMIT 1 OFFSET ?2)",
                params![peer, SEEN_LIMIT as i64],
            )
            .map_err(Error::storage)?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn seen_count(&self, peer: &PeerId) -> Result<usize, Error> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM kin_seen WHERE peer = ?1",
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
    use kin_identity::DeviceKey;

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

        outbox.remove(&id).unwrap();

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
}
