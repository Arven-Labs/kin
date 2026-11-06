//! Chat 1:1 (Fase 1.6): formato de mensagem com `parent_message_id` (D4) e o fluxo que liga
//! `crypto` e `transport`.
//!
//! Estado MLS em SQLite com [`Chat::open`] (P14) ou só em memória com [`Chat::new`].

mod chat;
mod error;
mod invite;
mod mailbox;
mod message;
pub mod nostr;
mod outbox;
mod wire;

pub use chat::{Chat, ChatEvent};
pub use error::Error;
pub use invite::Invite;
pub use mailbox::{Deposit, DepositId, Mailbox, MailboxEvent, MemoryMailbox, MemoryRelay};
pub use message::{Message, MessageId};
