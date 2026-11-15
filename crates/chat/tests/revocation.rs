//! P30 3c-ii (ADR 0025): um tombstone emitido pelo device A1 chega ao contato B e, por repasse de B, ao
//! outro device da mesma identidade, A2, que **não** está ligado a A1. Três `Chat` reais. É o caso que
//! importa: o dono revoga um device roubado e o resto da própria família soube por um contato.
//!
//! Regra de destino (auditoria 3c-ii, I1): quem aceita a identidade do tombstone. B aceita A (`trust`), A1
//! aceita B (por isso B está entre os destinos da emissão), e A2 é da própria identidade A (aceita a si).

mod common;

use std::time::Duration;

use common::{Person, WAIT, connect};
use kin_identity::{DeviceKey, IdentityProvider, StandaloneIdentity};
use tokio::time::timeout;

#[tokio::test]
async fn a_revocation_emitted_by_one_device_reaches_the_owners_other_device_through_a_contact() {
    let dir = tempfile::tempdir().unwrap();
    let (a1, bia) = (Person::new(dir.path(), "a1"), Person::new(dir.path(), "b"));
    // O segundo device de A: a mesma identidade (o pareamento copia a chave mestra), outra chave de device.
    let a2 = Person {
        identity: StandaloneIdentity::from_bytes(&a1.identity.to_bytes().unwrap()).unwrap(),
        device: DeviceKey::generate(),
        db: dir.path().join("a2.db"),
    };
    let (mut chat_a1, mut chat_b, mut chat_a2) = (a1.open(), bia.open(), a2.open());
    chat_b.trust(a1.identity.id()).unwrap();
    chat_a1.trust(bia.identity.id()).unwrap();
    chat_a2.trust(bia.identity.id()).unwrap();
    connect(&mut chat_a1, &mut chat_b).await;
    connect(&mut chat_a2, &mut chat_b).await;

    // Um device roubado de A e o tombstone que A assina para ele.
    let stolen = DeviceKey::generate();
    let certificate = a1
        .identity
        .authorize_device(
            &DeviceKey::generate().public_key(),
            &stolen.public_key(),
            &stolen.nostr_public_key(),
            1,
        )
        .unwrap();
    let revocation = a1.identity.revoke_device(&certificate, 2).unwrap();

    chat_a1.broadcast_revocation(&revocation).unwrap();
    assert!(
        chat_a1.is_revoked(&certificate).unwrap(),
        "A1 grava o que emite"
    );

    // Eventos vistos por chat (A1, B, A2), para o teste mostrar onde o tombstone parou se o prazo estourar.
    let mut seen: [Vec<kin_chat::ChatEvent>; 3] = Default::default();
    let result = timeout(WAIT, async {
        loop {
            tokio::select! {
                e = chat_a1.next_event() => seen[0].push(e),
                e = chat_b.next_event() => seen[1].push(e),
                e = chat_a2.next_event() => seen[2].push(e),
                // Receber um tombstone não emite evento: sem o tique, o `is_revoked` abaixo nunca rodaria.
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
            if chat_a2.is_revoked(&certificate).unwrap() {
                break;
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "o tombstone não chegou a A2 por repasse de B (B gravou: {})\neventos de A1: {:#?}\neventos de B: {:#?}\neventos de A2: {:#?}",
        chat_b.is_revoked(&certificate).unwrap(),
        seen[0],
        seen[1],
        seen[2]
    );

    assert!(chat_b.is_revoked(&certificate).unwrap());
}

/// P30 3c-ii-b (ADR 0026): quem conecta DEPOIS recebe o que já estava gravado, sem ninguém emitir de novo.
/// A grava 10 tombstones de devices seus sem ninguém conectado; B (que A aceita e que aceita A) conecta e
/// recebe só os 8 mais recentes, que são os que o reenvio manda.
#[tokio::test]
async fn a_peer_that_connects_later_gets_the_eight_most_recent_revocations() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    b.trust(pa.identity.id()).unwrap();
    a.trust(pb.identity.id()).unwrap();

    let certificates: Vec<_> = (0..10)
        .map(|_| {
            let device = DeviceKey::generate();
            let certificate = pa
                .identity
                .authorize_device(
                    &DeviceKey::generate().public_key(),
                    &device.public_key(),
                    &device.nostr_public_key(),
                    1,
                )
                .unwrap();
            a.broadcast_revocation(&pa.identity.revoke_device(&certificate, 2).unwrap())
                .unwrap();
            certificate
        })
        .collect();

    connect(&mut a, &mut b).await;

    let newest = &certificates[2..];
    let result = timeout(WAIT, async {
        loop {
            tokio::select! {
                _ = a.next_event() => {}
                _ = b.next_event() => {}
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
            if newest.iter().all(|c| b.is_revoked(c).unwrap()) {
                break;
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "B não recebeu os 8 mais recentes ao conectar"
    );
    // Dá tempo de um nono chegar, se o reenvio passasse do limite.
    let _ = timeout(Duration::from_millis(500), async {
        loop {
            tokio::select! {
                _ = a.next_event() => {}
                _ = b.next_event() => {}
            }
        }
    })
    .await;
    assert!(
        !b.is_revoked(&certificates[0]).unwrap() && !b.is_revoked(&certificates[1]).unwrap(),
        "os 2 mais antigos ficam de fora (limite de 8)"
    );
}
