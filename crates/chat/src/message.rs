use std::time::{SystemTime, UNIX_EPOCH};

use crate::Error;

const VERSION: u8 = 1;
const ID_LEN: usize = 16;

/// Identificador de uma mensagem: 128 bits aleatórios, gerados por quem envia.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageId([u8; ID_LEN]);

impl MessageId {
    fn random() -> Self {
        Self(rand::random())
    }

    pub fn from_bytes(bytes: [u8; ID_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; ID_LEN] {
        &self.0
    }
}

impl std::fmt::Debug for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

/// Mensagem de chat, o que vai cifrado dentro do MLS. `parent` aponta para a mensagem a que esta
/// responde (thread, D4); `None` é uma mensagem de topo.
///
/// Formato (v1): `versão(1) | id(16) | tem_pai(1) | [pai(16)] | enviada_em_ms(8, BE) | texto UTF-8`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    pub parent: Option<MessageId>,
    /// Relógio de quem enviou, em ms Unix: informativo, não é prova de ordem.
    pub sent_at_ms: u64,
    pub text: String,
}

/// Uma linha do histórico local (ver `Chat::history`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Posição no histórico deste device: cresce a cada gravação e serve de cursor de paginação.
    pub seq: u64,
    /// `true` = enviei (deste device ou de um irmão); `false` = recebi.
    pub outgoing: bool,
    pub message: Message,
}

impl Message {
    pub(crate) fn new(text: String, parent: Option<MessageId>) -> Self {
        let sent_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        Self {
            id: MessageId::random(),
            parent,
            sent_at_ms,
            text,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        out.extend_from_slice(&self.id.0);
        match &self.parent {
            Some(parent) => {
                out.push(1);
                out.extend_from_slice(&parent.0);
            }
            None => out.push(0),
        }
        out.extend_from_slice(&self.sent_at_ms.to_be_bytes());
        out.extend_from_slice(self.text.as_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let (&version, rest) = bytes.split_first().ok_or(Error::Malformed)?;
        if version != VERSION {
            return Err(Error::Malformed);
        }
        let (id, rest) = take::<ID_LEN>(rest)?;
        let (&has_parent, rest) = rest.split_first().ok_or(Error::Malformed)?;
        let (parent, rest) = match has_parent {
            0 => (None, rest),
            1 => {
                let (parent, rest) = take::<ID_LEN>(rest)?;
                (Some(MessageId(parent)), rest)
            }
            _ => return Err(Error::Malformed),
        };
        let (sent_at, text) = take::<8>(rest)?;
        Ok(Self {
            id: MessageId(id),
            parent,
            sent_at_ms: u64::from_be_bytes(sent_at),
            text: String::from_utf8(text.to_vec()).map_err(|_| Error::Malformed)?,
        })
    }
}

fn take<const N: usize>(bytes: &[u8]) -> Result<([u8; N], &[u8]), Error> {
    let (head, rest) = bytes.split_at_checked(N).ok_or(Error::Malformed)?;
    Ok((head.try_into().expect("tamanho conferido"), rest))
}
