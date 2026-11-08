//! Fase 3.3 (ADR 0004): embrulhar o blob MLS num gift wrap NIP-17/59 e abri-lo. Sem rede.
//! Os eventos hostis são montados com o próprio crate `nostr`, nunca com JSON escrito à mão.

use kin_chat::nostr::{MAX_BLOB, WrapError, unwrap, wrap, wrap_with_ttl};
use kin_identity::DeviceKey;
use nostr::nips::nip59::GiftWrapBuilder;
use nostr::prelude::*;

fn keys_of(device: &DeviceKey) -> Keys {
    Keys::new(SecretKey::from_slice(&device.nostr_secret_bytes()).unwrap())
}

fn nostr_pk(device: &DeviceKey) -> PublicKey {
    PublicKey::from_slice(&device.nostr_public_key().to_bytes()).unwrap()
}

/// Gift wrap válido por fora, mas com o rumor e o seal que o teste escolher.
fn custom_wrap(
    sealer: &Keys,
    receiver: &DeviceKey,
    rumor_author: PublicKey,
    kind: Kind,
    content: String,
) -> Event {
    let rumor = UnsignedEvent::new(
        rumor_author,
        Timestamp::now(),
        kind,
        Vec::<Tag>::new(),
        content,
    );
    GiftWrapBuilder::new(nostr_pk(receiver), rumor)
        .finalize(sealer)
        .unwrap()
}

#[test]
fn a_blob_survives_wrap_and_unwrap_and_names_the_sender() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    for len in [1usize, 1024, MAX_BLOB] {
        let blob: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let event = wrap(&ana, &bia.nostr_public_key(), &blob).unwrap();
        let opened = unwrap(&bia, &event).unwrap();
        assert_eq!(opened.blob, blob, "blob de {len} bytes");
        assert_eq!(opened.sender, ana.nostr_public_key());
    }
}

#[test]
fn someone_else_cannot_open_the_event() {
    let (ana, bia, carol) = (
        DeviceKey::generate(),
        DeviceKey::generate(),
        DeviceKey::generate(),
    );
    let event = wrap(&ana, &bia.nostr_public_key(), b"so para a bia").unwrap();
    assert!(unwrap(&carol, &event).is_err());
}

#[test]
fn the_relay_sees_a_throwaway_key_and_the_recipient_never_the_sender() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let first = wrap(&ana, &bia.nostr_public_key(), b"mesmo blob").unwrap();
    let second = wrap(&ana, &bia.nostr_public_key(), b"mesmo blob").unwrap();

    assert_eq!(first.kind, Kind::GiftWrap);
    assert_ne!(first.id, second.id);
    assert_ne!(
        first.pubkey, second.pubkey,
        "chave de wrap nova a cada mensagem"
    );
    assert_ne!(
        first.pubkey,
        nostr_pk(&ana),
        "o wrap não é assinado pela Ana"
    );
    let json = first.as_json();
    assert!(
        json.contains(&nostr_pk(&bia).to_hex()),
        "tag p do destinatário"
    );
    assert!(
        !json.contains(&nostr_pk(&ana).to_hex()),
        "a Ana não aparece no evento"
    );
}

#[test]
fn a_blob_over_the_cap_is_refused_before_any_event_exists() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let too_big = vec![7u8; MAX_BLOB + 1];
    assert!(matches!(
        wrap(&ana, &bia.nostr_public_key(), &too_big),
        Err(WrapError::TooLarge)
    ));
}

#[test]
fn an_event_that_is_not_a_gift_wrap_is_an_error() {
    let bia = DeviceKey::generate();
    let note = EventBuilder::new(Kind::TextNote, "oi")
        .finalize(&Keys::generate())
        .unwrap();
    assert!(unwrap(&bia, &note).is_err());
}

#[test]
fn a_tampered_content_is_an_error() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let mut event = wrap(&ana, &bia.nostr_public_key(), b"intacta").unwrap();
    let flipped = if event.content.as_bytes()[10] == b'A' {
        "B"
    } else {
        "A"
    };
    event.content.replace_range(10..11, flipped);
    assert!(unwrap(&bia, &event).is_err());
}

#[test]
fn a_tampered_outer_signature_is_an_error() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let mut event = wrap(&ana, &bia.nostr_public_key(), b"intacta").unwrap();
    event.sig = wrap(&ana, &bia.nostr_public_key(), b"outra").unwrap().sig;
    assert!(unwrap(&bia, &event).is_err());
}

#[test]
fn a_rumor_that_claims_another_author_than_the_seal_is_an_error() {
    let (ana, bia, mallory) = (
        DeviceKey::generate(),
        DeviceKey::generate(),
        DeviceKey::generate(),
    );
    // A Mallory sela, mas o rumor diz que o autor é a Ana.
    let forged = custom_wrap(
        &keys_of(&mallory),
        &bia,
        nostr_pk(&ana),
        Kind::PrivateDirectMessage,
        base64_of(b"mensagem forjada"),
    );
    assert!(unwrap(&bia, &forged).is_err());
}

#[test]
fn a_rumor_that_is_not_a_private_message_is_an_error() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let event = custom_wrap(
        &keys_of(&ana),
        &bia,
        nostr_pk(&ana),
        Kind::TextNote,
        base64_of(b"kind errado"),
    );
    assert!(unwrap(&bia, &event).is_err());
}

#[test]
fn a_rumor_whose_content_is_not_base64_is_an_error() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let event = custom_wrap(
        &keys_of(&ana),
        &bia,
        nostr_pk(&ana),
        Kind::PrivateDirectMessage,
        "isto não é base64 !!!".to_string(),
    );
    assert!(unwrap(&bia, &event).is_err());
}

#[test]
fn a_rumor_with_a_blob_over_the_cap_is_an_error() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let event = custom_wrap(
        &keys_of(&ana),
        &bia,
        nostr_pk(&ana),
        Kind::PrivateDirectMessage,
        base64_of(&vec![7u8; MAX_BLOB + 1]),
    );
    assert!(matches!(unwrap(&bia, &event), Err(WrapError::TooLarge)));
}

#[test]
fn an_event_with_a_huge_content_is_refused_before_decrypting() {
    let bia = DeviceKey::generate();
    let huge = EventBuilder::new(Kind::GiftWrap, "A".repeat(300_000))
        .finalize(&Keys::generate())
        .unwrap();
    assert!(matches!(unwrap(&bia, &huge), Err(WrapError::TooLarge)));
}

// 3.4: expiração NIP-40 no gift wrap.

const DAY: u64 = 24 * 3600;

fn expiration_of(event: &Event) -> Option<Timestamp> {
    event.tags.expiration()
}

#[test]
fn a_wrap_with_a_ttl_lives_at_least_the_ttl_even_with_the_worst_timestamp_tweak() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let ttl = std::time::Duration::from_secs(7 * DAY);

    let event = wrap_with_ttl(&ana, &bia.nostr_public_key(), b"x", Some(ttl)).unwrap();

    // A expiração é ancorada no `created_at` sorteado (até 2 dias atrás): a duração tem de somar os 2 dias.
    let expiration = expiration_of(&event).expect("sem tag de expiração");
    assert_eq!(
        expiration.as_secs() - event.created_at.as_secs(),
        9 * DAY,
        "ttl + 2 dias"
    );
    assert!(
        expiration.as_secs() >= Timestamp::now().as_secs() + 7 * DAY - 5,
        "vive menos que o TTL"
    );
}

#[test]
fn a_wrap_without_a_ttl_has_no_expiration() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());

    let event = wrap(&ana, &bia.nostr_public_key(), b"x").unwrap();

    assert!(expiration_of(&event).is_none());
}

#[test]
fn a_wrap_with_a_ttl_still_opens_for_the_recipient() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let ttl = std::time::Duration::from_secs(DAY);

    let event = wrap_with_ttl(&ana, &bia.nostr_public_key(), b"guardada", Some(ttl)).unwrap();

    let opened = unwrap(&bia, &event).unwrap();
    assert_eq!(opened.blob, b"guardada");
    assert_eq!(opened.sender, ana.nostr_public_key());
}

#[test]
fn the_blob_cap_still_applies_with_a_ttl() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let ttl = std::time::Duration::from_secs(DAY);

    let result = wrap_with_ttl(
        &ana,
        &bia.nostr_public_key(),
        &vec![0; MAX_BLOB + 1],
        Some(ttl),
    );

    assert!(matches!(result, Err(WrapError::TooLarge)));
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
