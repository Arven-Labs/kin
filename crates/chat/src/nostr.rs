//! Camada de protocolo do backend Nostr (Fase 3.3, ADR 0004), sem rede: embrulha um blob MLS num
//! gift wrap NIP-17/59 para um destinatário e abre o que chega. A cifra (NIP-44), o seal e o wrap
//! vêm do crate `nostr`; aqui só decidimos o formato do rumor, o teto de tamanho e o que recusar.
//!
//! O que sai daqui **não é confiável por si só**: `sender` é a chave que selou a mensagem, e quem de
//! fato enviou continua sendo a credencial MLS na decifragem (ADR 0003). O Nostr é só transporte.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kin_identity::{DeviceKey, NostrPublicKey};
use nostr::nips::nip59::{GiftWrapBuilder, UnwrappedGift};
use nostr::prelude::*;

/// Teto de um blob MLS (bytes) por mensagem. Política nossa, não limite do protocolo: relays com web
/// of trust já recusaram eventos de 16 KB nos testes (ADR 0004).
pub const MAX_BLOB: usize = 30_000;

/// Teto do `content` de um gift wrap recebido (bytes), checado antes de decifrar: um blob de
/// `MAX_BLOB` vira bem menos que isto, e evento maior só pode ser lixo ou ataque.
const MAX_WRAP_CONTENT: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WrapError {
    /// Blob (ou evento recebido) maior que o teto.
    #[error("maior que o teto permitido")]
    TooLarge,
    /// O evento não é um gift wrap válido para este device, ou o conteúdo não segue o formato do Kin.
    #[error("gift wrap recusado: {0}")]
    Rejected(String),
}

impl WrapError {
    fn rejected(e: impl std::fmt::Display) -> Self {
        Self::Rejected(e.to_string())
    }
}

/// O que um gift wrap aberto contém.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unwrapped {
    /// Chave Nostr que selou a mensagem. Afirmação do transporte, não prova de autoria (ver o topo).
    pub sender: NostrPublicKey,
    pub blob: Vec<u8>,
}

/// Embrulha `blob` para `recipient`: rumor kind 14 (conteúdo em base64) → seal assinado por
/// `sender` → gift wrap kind 1059 assinado por uma chave descartável nova a cada chamada.
pub fn wrap(
    sender: &DeviceKey,
    recipient: &NostrPublicKey,
    blob: &[u8],
) -> Result<Event, WrapError> {
    if blob.len() > MAX_BLOB {
        return Err(WrapError::TooLarge);
    }
    let keys = keys_of(sender)?;
    let receiver = PublicKey::from_slice(&recipient.to_bytes()).map_err(WrapError::rejected)?;
    let rumor = UnsignedEvent::new(
        keys.public_key(),
        Timestamp::now(),
        Kind::PrivateDirectMessage,
        Vec::<Tag>::new(),
        STANDARD.encode(blob),
    );
    GiftWrapBuilder::new(receiver, rumor)
        .finalize(&keys)
        .map_err(WrapError::rejected)
}

/// Abre um gift wrap endereçado a `own`. O crate `nostr` confere o kind 1059, a assinatura do wrap e
/// do seal e que o autor do rumor é o do seal; aqui exigimos ainda rumor kind 14, base64 válido e
/// blob dentro do teto.
pub fn unwrap(own: &DeviceKey, event: &Event) -> Result<Unwrapped, WrapError> {
    if event.content.len() > MAX_WRAP_CONTENT {
        return Err(WrapError::TooLarge);
    }
    let gift = UnwrappedGift::from_gift_wrap(&keys_of(own)?, event).map_err(WrapError::rejected)?;
    if gift.rumor.kind != Kind::PrivateDirectMessage {
        return Err(WrapError::Rejected(
            "o rumor não é uma mensagem privada".into(),
        ));
    }
    let blob = STANDARD
        .decode(gift.rumor.content.as_bytes())
        .map_err(WrapError::rejected)?;
    if blob.len() > MAX_BLOB {
        return Err(WrapError::TooLarge);
    }
    let sender =
        NostrPublicKey::from_bytes(&gift.sender.to_bytes()).map_err(WrapError::rejected)?;
    Ok(Unwrapped { sender, blob })
}

fn keys_of(device: &DeviceKey) -> Result<Keys, WrapError> {
    let secret =
        SecretKey::from_slice(&device.nostr_secret_bytes()).map_err(WrapError::rejected)?;
    Ok(Keys::new(secret))
}
