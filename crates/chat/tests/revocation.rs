//! P30 3c-ii (ADR 0025): um tombstone emitido por A chega a B e, por repasse, a C, que só fala com B. Três
//! `Chat` reais. A é dona da identidade cujo device é revogado; B e C a aceitam (`trust`), e A e C aceitam B
//! (é o que faz o envio e o repasse terem destino).

mod common;

use common::{Person, WAIT, connect};
use kin_identity::{DeviceKey, IdentityProvider};
use tokio::time::timeout;

#[tokio::test]
async fn a_revocation_emitted_by_a_reaches_c_through_b() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb, pc) = (
        Person::new(dir.path(), "a"),
        Person::new(dir.path(), "b"),
        Person::new(dir.path(), "c"),
    );
    let (mut a, mut b, mut c) = (pa.open(), pb.open(), pc.open());
    b.trust(pa.identity.id()).unwrap();
    c.trust(pa.identity.id()).unwrap();
    a.trust(pb.identity.id()).unwrap();
    b.trust(pc.identity.id()).unwrap();
    c.trust(pb.identity.id()).unwrap();
    connect(&mut a, &mut b).await;
    connect(&mut c, &mut b).await;

    // Um device de A (o roubado) e o tombstone que A assina para ele.
    let stolen = DeviceKey::generate();
    let certificate = pa
        .identity
        .authorize_device(
            &DeviceKey::generate().public_key(),
            &stolen.public_key(),
            &stolen.nostr_public_key(),
            1,
        )
        .unwrap();
    let revocation = pa.identity.revoke_device(&certificate, 2).unwrap();

    a.broadcast_revocation(&revocation).unwrap();
    assert!(a.is_revoked(&certificate).unwrap(), "A grava o que emite");

    // Eventos vistos por chat (A, B, C), para o teste mostrar onde o tombstone parou se o prazo estourar.
    let mut seen: [Vec<kin_chat::ChatEvent>; 3] = Default::default();
    let result = timeout(WAIT, async {
        loop {
            tokio::select! {
                e = a.next_event() => seen[0].push(e),
                e = b.next_event() => seen[1].push(e),
                e = c.next_event() => seen[2].push(e),
            }
            if c.is_revoked(&certificate).unwrap() {
                break;
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "o tombstone não chegou a C por repasse de B (B gravou: {})\neventos de A: {:#?}\neventos de B: {:#?}\neventos de C: {:#?}",
        b.is_revoked(&certificate).unwrap(),
        seen[0],
        seen[1],
        seen[2]
    );

    assert!(b.is_revoked(&certificate).unwrap());
}
