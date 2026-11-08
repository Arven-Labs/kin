//! O que vai cifrado dentro de uma mensagem de aplicação MLS: uma mensagem de chat (v1, tag 1, o
//! formato de `Message`) ou uma mensagem de controle. A tag é o primeiro byte, então o formato v1
//! continua válido sem mudança.
//!
//! `RelayList` (tag 2): `2 | n(1) | n × [comprimento(2, BE) | URL UTF-8]`. Quem anuncia diz em quais
//! relays quer receber; o receptor valida a lista antes de usar (ver `nostr::validate_peer_relays`).

use crate::{Error, Message};

const TAG_RELAY_LIST: u8 = 2;

/// Teto de itens que o codec aceita ler. A política (quantos valem) é de `validate_peer_relays`; este
/// só evita alocar com base num número hostil.
const MAX_DECODED_RELAYS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Payload {
    Message(Message),
    RelayList(Vec<String>),
}

impl Payload {
    pub(crate) fn encode(&self) -> Vec<u8> {
        match self {
            Self::Message(message) => message.encode(),
            Self::RelayList(relays) => {
                let mut out = vec![TAG_RELAY_LIST, relays.len().min(u8::MAX as usize) as u8];
                for url in relays {
                    let bytes = url.as_bytes();
                    out.extend_from_slice(
                        &(bytes.len().min(u16::MAX as usize) as u16).to_be_bytes(),
                    );
                    out.extend_from_slice(&bytes[..bytes.len().min(u16::MAX as usize)]);
                }
                out
            }
        }
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        match bytes.first() {
            Some(&TAG_RELAY_LIST) => Self::decode_relay_list(&bytes[1..]),
            // Qualquer outra tag é do formato v1: `Message::decode` confere a versão.
            Some(_) => Ok(Self::Message(Message::decode(bytes)?)),
            None => Err(Error::Malformed),
        }
    }

    fn decode_relay_list(bytes: &[u8]) -> Result<Self, Error> {
        let (&count, mut rest) = bytes.split_first().ok_or(Error::Malformed)?;
        if usize::from(count) > MAX_DECODED_RELAYS {
            return Err(Error::Malformed);
        }
        let mut relays = Vec::with_capacity(count.into());
        for _ in 0..count {
            let (len, tail) = rest.split_at_checked(2).ok_or(Error::Malformed)?;
            let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
            let (url, tail) = tail.split_at_checked(len).ok_or(Error::Malformed)?;
            relays.push(String::from_utf8(url.to_vec()).map_err(|_| Error::Malformed)?);
            rest = tail;
        }
        if !rest.is_empty() {
            return Err(Error::Malformed);
        }
        Ok(Self::RelayList(relays))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, Message};

    // 1
    #[test]
    fn a_relay_list_survives_encode_and_decode() {
        let payload = Payload::RelayList(vec!["wss://a.example".into(), "wss://b.example".into()]);

        assert_eq!(Payload::decode(&payload.encode()).unwrap(), payload);
    }

    // 2
    #[test]
    fn a_v1_chat_message_still_decodes_as_a_message() {
        let message = Message::new("oi".into(), None);

        let decoded = Payload::decode(&message.encode()).unwrap();

        assert_eq!(decoded, Payload::Message(message));
    }

    // 3
    #[test]
    fn an_encoded_message_payload_is_byte_identical_to_the_v1_format() {
        let message = Message::new("oi".into(), None);

        assert_eq!(Payload::Message(message.clone()).encode(), message.encode());
    }

    // 4
    #[test]
    fn empty_input_and_unknown_tags_are_malformed() {
        for bytes in [&[][..], &[0][..], &[9, 1, 2][..], &[255][..]] {
            assert!(
                matches!(Payload::decode(bytes), Err(Error::Malformed)),
                "{bytes:?}"
            );
        }
    }

    // 5
    #[test]
    fn a_truncated_relay_list_is_malformed_at_every_cut() {
        let full =
            Payload::RelayList(vec!["wss://a.example".into(), "wss://b.example".into()]).encode();

        for cut in 1..full.len() {
            assert!(
                matches!(Payload::decode(&full[..cut]), Err(Error::Malformed)),
                "cortou em {cut}"
            );
        }
    }

    // 6
    #[test]
    fn trailing_bytes_after_the_list_are_malformed() {
        let mut bytes = Payload::RelayList(vec!["wss://a.example".into()]).encode();
        bytes.push(0);

        assert!(matches!(Payload::decode(&bytes), Err(Error::Malformed)));
    }

    // 7
    #[test]
    fn a_url_that_is_not_utf8_is_malformed() {
        // tag 2, 1 item, comprimento 2, bytes inválidos
        let bytes = [2, 1, 0, 2, 0xff, 0xfe];

        assert!(matches!(Payload::decode(&bytes), Err(Error::Malformed)));
    }

    // 8
    #[test]
    fn a_count_above_the_codec_limit_is_malformed_before_allocating() {
        // tag 2, 255 itens declarados, sem corpo
        let bytes = [2, 255];

        assert!(matches!(Payload::decode(&bytes), Err(Error::Malformed)));
    }

    // 9
    #[test]
    fn a_declared_length_larger_than_the_input_is_malformed() {
        // tag 2, 1 item, comprimento 65535, 3 bytes
        let bytes = [2, 1, 0xff, 0xff, b'a', b'b', b'c'];

        assert!(matches!(Payload::decode(&bytes), Err(Error::Malformed)));
    }
}
