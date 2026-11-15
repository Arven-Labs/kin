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

    timeout(WAIT, async {
        loop {
            tokio::select! {
                _ = a.next_event() => {}
                _ = b.next_event() => {}
                _ = c.next_event() => {}
            }
            if c.is_revoked(&certificate).unwrap() {
                break;
            }
        }
    })
    .await
    .expect("o tombstone não chegou a C por repasse de B");

    assert!(b.is_revoked(&certificate).unwrap());
}
