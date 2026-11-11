//! O que vai cifrado dentro de uma mensagem de aplicação MLS: uma mensagem de chat (v1, tag 1, o
//! formato de `Message`) ou uma mensagem de controle. A tag é o primeiro byte, então o formato v1
//! continua válido sem mudança.
//!
//! `RelayList` (tag 2): `2 | n(1) | n × [comprimento(2, BE) | URL UTF-8]`. Quem anuncia diz em quais
//! relays quer receber; o receptor valida a lista antes de usar (ver `nostr::validate_peer_relays`).
//!
//! `Sent` (tag 3), o espelho multi-device (3.5): `3 | len16 | identidade de destino (protobuf) | Message`.
//! Um device da minha identidade diz ao irmão "enviei isto a essa identidade". Quem lê precisa conferir
//! que o autor é da própria identidade (o `chat` faz; o codec só lê bytes).
//!
//! `DeviceList` (tag 4): `4 | n(1) | n × [len16 | Invite]`. Os devices de uma identidade, com certificado
//! e endereços, para o contato achá-los (3.5). Cada certificado é conferido ao ler (assinatura); que a
//! identidade seja a do remetente é política do `chat`.

use kin_identity::{IdentityId, PublicKey};

use crate::{Error, Invite, Message};

const TAG_RELAY_LIST: u8 = 2;
const TAG_SENT: u8 = 3;
const TAG_DEVICE_LIST: u8 = 4;
const TAG_HISTORY_REQUEST: u8 = 5;
const TAG_HISTORY_CHUNK: u8 = 6;

/// Teto de itens por `HistoryChunk`, na leitura e na escrita: evita alocar com base num número hostil.
#[allow(dead_code)]
pub(crate) const MAX_DECODED_HISTORY: usize = 256;

/// Orçamento em bytes de um `HistoryChunk` (texto claro, antes do MLS): folga de sobra sob o limite de
/// 64 KiB do transporte.
#[allow(dead_code)]
pub(crate) const HISTORY_CHUNK_BUDGET: usize = 32 * 1024;

/// Uma linha do histórico a caminho de um irmão: a conversa (identidade do contato), a direção vista de
/// quem a enviou e a mensagem.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryItem {
    pub(crate) contact: IdentityId,
    pub(crate) outgoing: bool,
    pub(crate) message: Message,
}

/// Teto de devices por lista, na leitura e na escrita: evita alocar com base num número hostil.
pub(crate) const MAX_DECODED_DEVICES: usize = 8;

/// Teto de itens que o codec aceita ler. A política (quantos valem) é de `validate_peer_relays`; este
/// só evita alocar com base num número hostil.
const MAX_DECODED_RELAYS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Payload {
    Message(Message),
    RelayList(Vec<String>),
    /// Espelho: enviei `message` a `to` (outro device meu mandou; este só registra).
    Sent {
        to: IdentityId,
        message: Message,
    },
    DeviceList(Vec<Invite>),
    /// Um irmão pede o histórico depois de `cursor` (o `seq` do respondente; 0 = do começo).
    #[allow(dead_code)]
    HistoryRequest {
        cursor: u64,
    },
    /// Uma página do histórico: `next` é o `seq` da última linha examinada, `done` = não há mais.
    #[allow(dead_code)]
    HistoryChunk {
        items: Vec<HistoryItem>,
        next: u64,
        done: bool,
    },
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
            Self::Sent { to, message } => {
                let mut out = vec![TAG_SENT];
                put(&mut out, &to.public_key().encode_protobuf());
                out.extend_from_slice(&message.encode());
                out
            }
            Self::DeviceList(invites) => {
                // Item que não cabe em 64 KiB fica de fora (endereços absurdos), em vez de panicar.
                let items: Vec<Vec<u8>> = invites
                    .iter()
                    .map(Invite::encode)
                    .filter(|item| u16::try_from(item.len()).is_ok())
                    .take(MAX_DECODED_DEVICES)
                    .collect();
                let mut out = vec![TAG_DEVICE_LIST, items.len() as u8];
                for item in &items {
                    put(&mut out, item);
                }
                out
            }
            // STUB (testes primeiro, ADR 0009 do workspace): o codec real vem depois da prova.
            Self::HistoryRequest { .. } | Self::HistoryChunk { .. } => {
                let _ = (TAG_HISTORY_REQUEST, TAG_HISTORY_CHUNK);
                Vec::new()
            }
        }
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        match bytes.first() {
            Some(&TAG_RELAY_LIST) => Self::decode_relay_list(&bytes[1..]),
            Some(&TAG_SENT) => Self::decode_sent(&bytes[1..]),
            Some(&TAG_DEVICE_LIST) => Self::decode_device_list(&bytes[1..]),
            // Qualquer outra tag é do formato v1: `Message::decode` confere a versão.
            Some(_) => Ok(Self::Message(Message::decode(bytes)?)),
            None => Err(Error::Malformed),
        }
    }

    fn decode_sent(bytes: &[u8]) -> Result<Self, Error> {
        let (identity, message) = take(bytes)?;
        let to = PublicKey::try_decode_protobuf(identity).map_err(|_| Error::Malformed)?;
        Ok(Self::Sent {
            to: IdentityId::new(to),
            message: Message::decode(message)?,
        })
    }

    fn decode_device_list(bytes: &[u8]) -> Result<Self, Error> {
        let (&count, mut rest) = bytes.split_first().ok_or(Error::Malformed)?;
        if usize::from(count) > MAX_DECODED_DEVICES {
            return Err(Error::Malformed);
        }
        let mut invites = Vec::with_capacity(count.into());
        for _ in 0..count {
            let (item, tail) = take(rest)?;
            invites.push(Invite::decode(item).map_err(|_| Error::Malformed)?);
            rest = tail;
        }
        if !rest.is_empty() {
            return Err(Error::Malformed);
        }
        Ok(Self::DeviceList(invites))
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

/// Escreve `field` com prefixo de tamanho de 2 bytes (BE). O chamador garante que cabe em `u16`.
fn put(out: &mut Vec<u8>, field: &[u8]) {
    let len = u16::try_from(field.len()).expect("campo conferido pelo chamador");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(field);
}

/// Lê um campo com prefixo de 2 bytes e devolve `(campo, resto)`.
fn take(bytes: &[u8]) -> Result<(&[u8], &[u8]), Error> {
    let (len, rest) = bytes.split_at_checked(2).ok_or(Error::Malformed)?;
    let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
    rest.split_at_checked(len).ok_or(Error::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, Message, MessageId};

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

    use kin_identity::{DeviceCertificate, DeviceKey, IdentityProvider, StandaloneIdentity};
    use kin_transport::Multiaddr;

    use crate::Invite;

    fn addr(text: &str) -> Multiaddr {
        text.parse().unwrap()
    }

    /// Certificado válido de um device novo de `identity`.
    fn certificate(identity: &StandaloneIdentity) -> DeviceCertificate {
        let device = DeviceKey::generate();
        identity
            .authorize_device(
                &DeviceKey::generate().public_key(),
                &device.public_key(),
                &device.nostr_public_key(),
                1,
            )
            .unwrap()
    }

    fn invite(identity: &StandaloneIdentity, addrs: Vec<Multiaddr>) -> Invite {
        Invite::new(certificate(identity), addrs)
    }

    fn sent(text: &str, parent: Option<crate::MessageId>) -> (Payload, Message) {
        let message = Message::new(text.into(), parent);
        let to = StandaloneIdentity::generate().id();
        (
            Payload::Sent {
                to,
                message: message.clone(),
            },
            message,
        )
    }

    // 10
    #[test]
    fn a_sent_mirror_survives_encode_and_decode_with_its_thread_parent() {
        let parent = Message::new("pai".into(), None).id;
        let (payload, _) = sent("oi, bia", Some(parent));

        assert_eq!(Payload::decode(&payload.encode()).unwrap(), payload);
    }

    // 11
    #[test]
    fn a_sent_mirror_with_an_empty_text_is_malformed_at_every_cut() {
        // Texto vazio: a mensagem já é o menor possível, então qualquer corte a deixa incompleta
        // (com texto, cortar o fim só encurtaria o texto e ainda seria válido).
        let (payload, _) = sent("", None);
        let full = payload.encode();

        for cut in 1..full.len() {
            assert!(
                matches!(Payload::decode(&full[..cut]), Err(Error::Malformed)),
                "cortou em {cut}"
            );
        }
        assert_eq!(Payload::decode(&full).unwrap(), payload);
    }

    // 12
    #[test]
    fn a_sent_mirror_with_an_unreadable_identity_is_malformed() {
        let (payload, _) = sent("x", None);
        let mut bytes = payload.encode();
        // tag(1) + len16(2): o primeiro byte da identidade é o tipo de chave do protobuf
        bytes[3] = 0xff;

        assert!(matches!(Payload::decode(&bytes), Err(Error::Malformed)));
    }

    // 13
    #[test]
    fn a_sent_mirror_with_an_unknown_message_version_is_malformed() {
        let (payload, message) = sent("x", None);
        let mut bytes = payload.encode();
        let body = bytes.len() - message.encode().len();
        bytes[body] = 99;

        assert!(matches!(Payload::decode(&bytes), Err(Error::Malformed)));
    }

    // 14
    #[test]
    fn a_device_list_survives_encode_and_decode_with_addresses() {
        let identity = StandaloneIdentity::generate();
        let payload = Payload::DeviceList(vec![
            invite(&identity, vec![addr("/ip4/127.0.0.1/tcp/4001")]),
            invite(&identity, vec![]),
        ]);

        assert_eq!(Payload::decode(&payload.encode()).unwrap(), payload);
    }

    // 15
    #[test]
    fn a_device_list_cut_at_any_point_is_malformed() {
        let identity = StandaloneIdentity::generate();
        let full = Payload::DeviceList(vec![
            invite(&identity, vec![addr("/ip4/10.0.0.2/tcp/9")]),
            invite(&identity, vec![]),
        ])
        .encode();

        for cut in 1..full.len() {
            assert!(
                matches!(Payload::decode(&full[..cut]), Err(Error::Malformed)),
                "cortou em {cut}"
            );
        }
    }

    // 16
    #[test]
    fn trailing_bytes_after_a_device_list_are_malformed() {
        let identity = StandaloneIdentity::generate();
        let mut bytes = Payload::DeviceList(vec![invite(&identity, vec![])]).encode();
        bytes.push(0);

        assert!(matches!(Payload::decode(&bytes), Err(Error::Malformed)));
    }

    // 17
    #[test]
    fn a_device_count_above_the_codec_limit_is_malformed_before_allocating() {
        // tag 4, 9 e 255 itens declarados, sem corpo
        for count in [9, 255] {
            assert!(matches!(
                Payload::decode(&[4, count]),
                Err(Error::Malformed)
            ));
        }
    }

    // 18
    #[test]
    fn a_device_list_with_a_tampered_certificate_is_malformed() {
        // Certificado de uma identidade que diz pertencer a outra: a assinatura não bate.
        let mut forged = certificate(&StandaloneIdentity::generate());
        forged.identity = StandaloneIdentity::generate().id();

        let bytes = Payload::DeviceList(vec![Invite::new(forged, vec![])]).encode();

        assert!(matches!(Payload::decode(&bytes), Err(Error::Malformed)));
    }

    // 20
    #[test]
    fn a_device_list_at_the_limit_decodes_and_encoding_never_goes_past_it() {
        let identity = StandaloneIdentity::generate();
        let nine: Vec<Invite> = (0..=MAX_DECODED_DEVICES)
            .map(|_| invite(&identity, vec![]))
            .collect();

        let decoded = Payload::decode(&Payload::DeviceList(nine.clone()).encode()).unwrap();

        // O 9º fica de fora na escrita, para o receptor não recusar a lista inteira.
        assert_eq!(
            decoded,
            Payload::DeviceList(nine[..MAX_DECODED_DEVICES].to_vec())
        );
    }

    // 19
    #[test]
    fn an_empty_device_list_is_a_valid_empty_list() {
        let payload = Payload::DeviceList(vec![]);

        assert_eq!(Payload::decode(&payload.encode()).unwrap(), payload);
    }

    // História (P10a, fatia 2)

    fn item(outgoing: bool, text: &str, parent: Option<MessageId>) -> HistoryItem {
        HistoryItem {
            contact: StandaloneIdentity::generate().id(),
            outgoing,
            message: Message::new(text.into(), parent),
        }
    }

    fn chunk(items: Vec<HistoryItem>, next: u64, done: bool) -> Payload {
        Payload::HistoryChunk { items, next, done }
    }

    #[test]
    fn a_history_request_survives_encode_and_decode() {
        let payload = Payload::HistoryRequest {
            cursor: 0x0102_0304_0506_0708,
        };

        let bytes = payload.encode();

        assert_eq!(bytes.len(), 9, "tag + cursor de 8 bytes");
        assert_eq!(Payload::decode(&bytes).unwrap(), payload);
    }

    #[test]
    fn a_history_chunk_survives_encode_and_decode_with_direction_and_parent() {
        let parent = MessageId::from_bytes([7; 16]);
        let payload = chunk(
            vec![
                item(true, "enviei", None),
                item(false, "recebi", Some(parent)),
            ],
            77,
            false,
        );

        assert_eq!(Payload::decode(&payload.encode()).unwrap(), payload);
    }

    #[test]
    fn an_empty_final_history_chunk_is_valid() {
        let payload = chunk(vec![], 5, true);

        assert_eq!(Payload::decode(&payload.encode()).unwrap(), payload);
    }

    #[test]
    fn a_truncated_history_chunk_is_malformed_at_every_cut() {
        let full = chunk(
            vec![item(true, "um", None), item(false, "dois", None)],
            9,
            false,
        )
        .encode();
        assert!(!full.is_empty());

        for cut in 1..full.len() {
            assert!(
                matches!(Payload::decode(&full[..cut]), Err(Error::Malformed)),
                "corte em {cut} de {}",
                full.len()
            );
        }
    }

    #[test]
    fn a_truncated_history_request_is_malformed() {
        let full = Payload::HistoryRequest { cursor: 1 }.encode();
        assert!(!full.is_empty());

        for cut in 1..full.len() {
            assert!(matches!(
                Payload::decode(&full[..cut]),
                Err(Error::Malformed)
            ));
        }
        let mut longer = full.clone();
        longer.push(0);
        assert!(matches!(Payload::decode(&longer), Err(Error::Malformed)));
    }

    #[test]
    fn a_history_chunk_with_trailing_bytes_is_malformed() {
        let mut bytes = chunk(vec![item(true, "um", None)], 1, true).encode();
        assert!(!bytes.is_empty());
        bytes.push(0);

        assert!(matches!(Payload::decode(&bytes), Err(Error::Malformed)));
    }

    #[test]
    fn a_history_chunk_announcing_more_than_the_cap_is_malformed() {
        // tag | done | next(8) | n = 257 (acima do teto), sem itens: recusa pelo número, sem alocar.
        let mut bytes = vec![TAG_HISTORY_CHUNK, 0];
        bytes.extend_from_slice(&0u64.to_be_bytes());
        bytes.extend_from_slice(&((MAX_DECODED_HISTORY + 1) as u16).to_be_bytes());

        assert!(matches!(Payload::decode(&bytes), Err(Error::Malformed)));
    }

    #[test]
    fn a_history_chunk_with_invalid_utf8_text_or_a_bad_contact_key_is_malformed() {
        let valid = chunk(vec![item(true, "texto", None)], 1, true).encode();
        assert!(!valid.is_empty());

        // Último byte = último byte do texto: UTF-8 inválido.
        let mut bad_text = valid.clone();
        *bad_text.last_mut().unwrap() = 0xFF;
        assert!(matches!(Payload::decode(&bad_text), Err(Error::Malformed)));

        // tag(1) done(1) next(8) n(2) len16(2) → o 1º byte do contato fica na posição 14.
        let mut bad_contact = valid;
        bad_contact[14] = 0xFF;
        assert!(matches!(
            Payload::decode(&bad_contact),
            Err(Error::Malformed)
        ));
    }

    #[test]
    fn encoding_a_history_chunk_never_goes_past_the_item_cap() {
        let many: Vec<HistoryItem> = (0..=MAX_DECODED_HISTORY)
            .map(|n| item(true, &n.to_string(), None))
            .collect();

        let decoded = Payload::decode(&chunk(many.clone(), 1, false).encode()).unwrap();

        // O 257º fica de fora na escrita, para o receptor não recusar a página inteira.
        assert_eq!(
            decoded,
            chunk(many[..MAX_DECODED_HISTORY].to_vec(), 1, false)
        );
    }

    #[test]
    fn a_history_item_too_big_for_its_field_is_left_out_of_the_chunk() {
        let huge = item(true, &"x".repeat(70_000), None);
        let small = item(false, "pequena", None);

        let decoded = Payload::decode(&chunk(vec![huge, small.clone()], 2, true).encode()).unwrap();

        assert_eq!(decoded, chunk(vec![small], 2, true));
    }
}
