//! Trinco por identidade (P30 fatia 3e-ii-a, ADR 0031): depois que um par v4 (certificado de autoridade) de
//! uma identidade X foi **aceito**, um certificado v3 (da mestra) de X é recusado com `Error::Downgrade`.
//!
//! Personagens: `C` é o contato (identidade própria); `P` é um device pareado de X (v4); `M` é um device
//! de X em modo mestra (v3), isto é, o que um atacante com a mestra roubada consegue emitir.

use std::path::Path;

use kin_crypto::{Conversation, CryptoDevice, Error, Expected, PendingDevice};
use kin_identity::{
    AuthorityIdentity, AuthorityKey, DeviceKey, IdentityProvider, StandaloneIdentity,
};

fn primary_of(master: &StandaloneIdentity) -> AuthorityIdentity {
    let key = AuthorityKey::generate();
    let cert = master.certify_authority(&key.public_key(), 1_000).unwrap();
    AuthorityIdentity::new(key, cert).unwrap()
}

/// `P`: um device de X pareado por autoridade (v4), no banco `db`.
fn v4_device(db: &Path, primary: &AuthorityIdentity) -> CryptoDevice {
    let key = DeviceKey::generate();
    let pending = PendingDevice::open(db).unwrap();
    let request = pending.request(&key).unwrap();
    let sas = request.sas_code(&primary.id());
    let cert = primary.answer_pair_request(&request, &sas, 2_000).unwrap();
    pending
        .install_certificate(&key, &cert, &primary.id())
        .unwrap()
}

/// `M` ou `C`: um device em modo mestra (certificado v3), em memória.
fn v3_device(master: &StandaloneIdentity) -> CryptoDevice {
    let key = DeviceKey::generate();
    CryptoDevice::new(master, &key.public_key(), &key.nostr_public_key(), 1).unwrap()
}

fn dbs() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// `inviter` convida `guest` numa conversa nova.
fn invite(inviter: &CryptoDevice, guest: &CryptoDevice) -> Result<Vec<u8>, Error> {
    let mut conversation = Conversation::create(inviter).unwrap();
    conversation
        .invite(&guest.key_package().unwrap(), &Expected::default())
        .map(|invite| invite.welcome)
}

/// O contato `C` e os dois devices de X.
struct World {
    c: CryptoDevice,
    p: CryptoDevice,
    m: CryptoDevice,
    _dir: tempfile::TempDir,
}

fn world() -> World {
    let dir = dbs();
    let x = StandaloneIdentity::generate();
    let p = v4_device(&dir.path().join("p.db"), &primary_of(&x));
    World {
        c: v3_device(&StandaloneIdentity::generate()),
        m: v3_device(&x),
        p,
        _dir: dir,
    }
}

// ---- o trinco ----

/// `C` aceita o par v4 de X; depois recusa, ao convidar, um KeyPackage v3 de X.
#[test]
fn a_v3_key_package_of_an_identity_seen_as_v4_is_refused_on_invite() {
    let w = world();
    invite(&w.c, &w.p).unwrap();

    let result = invite(&w.c, &w.m);

    assert!(matches!(result, Err(Error::Downgrade)));
}

/// Mesmo trinco no outro sentido: `M` (v3) convida `C`, e `C` recusa o Welcome ao entrar.
#[test]
fn a_welcome_from_a_v3_device_of_an_identity_seen_as_v4_is_refused_on_join() {
    let w = world();
    invite(&w.c, &w.p).unwrap();
    let welcome = invite(&w.m, &w.c).unwrap();

    let result = Conversation::join(&w.c, &welcome, &Expected::default());

    assert!(matches!(result, Err(Error::Downgrade)));
}

/// Entrar numa conversa com um par v4 também vira o trinco (o caminho do `join`, não só do `invite`).
#[test]
fn joining_a_conversation_with_a_v4_device_turns_the_latch() {
    let w = world();
    let welcome = invite(&w.p, &w.c).unwrap();
    Conversation::join(&w.c, &welcome, &Expected::default()).unwrap();

    let result = invite(&w.c, &w.m);

    assert!(matches!(result, Err(Error::Downgrade)));
}

/// O trinco anda numa direção só e vale para o certificado, não para a identidade inteira.
#[test]
fn is_downgraded_tells_the_v3_certificate_of_a_latched_identity() {
    let w = world();
    assert!(!w.c.is_downgraded(w.m.certificate()).unwrap());
    invite(&w.c, &w.p).unwrap();

    assert!(w.c.is_downgraded(w.m.certificate()).unwrap());
    assert!(!w.c.is_downgraded(w.p.certificate()).unwrap());
}

/// P30 3e-ii-b: `learn_regime` é o backfill do `chat`; só um certificado v4 de outra identidade vira o trinco.
#[test]
fn learning_a_v4_certificate_turns_the_latch_but_a_v3_one_does_not() {
    let w = world();

    w.c.learn_regime(w.m.certificate()).unwrap();
    assert!(!w.c.is_downgraded(w.m.certificate()).unwrap());

    w.c.learn_regime(w.p.certificate()).unwrap();
    w.c.learn_regime(w.p.certificate()).unwrap();
    assert!(w.c.is_downgraded(w.m.certificate()).unwrap());
    assert!(!w.c.is_downgraded(w.p.certificate()).unwrap());
}

/// O estado é gravado no banco: reabrir o device de `C` mantém o trinco.
#[test]
fn the_latch_survives_reopening_the_database() {
    let dir = dbs();
    let x = StandaloneIdentity::generate();
    let p = v4_device(&dir.path().join("p.db"), &primary_of(&x));
    let m = v3_device(&x);
    let c_id = StandaloneIdentity::generate();
    let key = DeviceKey::generate();
    let db = dir.path().join("c.db");
    let c = CryptoDevice::open(&c_id, &key.public_key(), &key.nostr_public_key(), &db, 1).unwrap();
    invite(&c, &p).unwrap();
    drop(c);

    let reopened =
        CryptoDevice::open(&c_id, &key.public_key(), &key.nostr_public_key(), &db, 1).unwrap();

    assert!(matches!(invite(&reopened, &m), Err(Error::Downgrade)));
}

// ---- o que o trinco não pode fazer ----

/// Sem nenhum v4 visto antes, o v3 de X é aceito (confiança no primeiro uso): o trinco só anda a partir
/// do primeiro v4.
#[test]
fn a_v3_device_is_accepted_while_no_v4_of_the_identity_was_seen() {
    let w = world();

    assert!(invite(&w.c, &w.m).is_ok());
    // aceitar um v3 não vira o trinco: ele só anda com um v4 aceito
    assert!(!w.c.is_downgraded(w.m.certificate()).unwrap());
}

/// Outro device v4 de X continua aceito depois do trinco.
#[test]
fn another_v4_device_of_the_latched_identity_is_still_accepted() {
    let dir = dbs();
    let x = StandaloneIdentity::generate();
    let primary = primary_of(&x);
    let (p1, p2) = (
        v4_device(&dir.path().join("p1.db"), &primary),
        v4_device(&dir.path().join("p2.db"), &primary),
    );
    let c = v3_device(&StandaloneIdentity::generate());
    invite(&c, &p1).unwrap();

    assert!(invite(&c, &p2).is_ok());
}

/// O trinco é por identidade: o v3 de **outra** identidade Y não é afetado.
#[test]
fn a_v3_device_of_another_identity_is_not_affected() {
    let w = world();
    invite(&w.c, &w.p).unwrap();
    let y = v3_device(&StandaloneIdentity::generate());

    assert!(invite(&w.c, &y).is_ok());
}

/// A identidade **própria** nunca é barrada nem marcada: `M` (mestra de X) convive com os devices v4 de
/// X, que é o estado até a migração (o `pairing.rs` já faz isso entre `P` e um device normal).
#[test]
fn a_device_never_latches_or_refuses_its_own_identity() {
    let dir = dbs();
    let x = StandaloneIdentity::generate();
    let primary = primary_of(&x);
    let (p1, p2) = (
        v4_device(&dir.path().join("p1.db"), &primary),
        v4_device(&dir.path().join("p2.db"), &primary),
    );
    let m = v3_device(&x);
    invite(&m, &p1).unwrap();
    invite(&p2, &m).unwrap();

    // `m` viu um v4 da própria identidade e ainda aceita um v3 dela; `p2` idem.
    assert!(invite(&m, &v3_device(&x)).is_ok());
    assert!(invite(&p2, &v3_device(&x)).is_ok());
    assert!(!m.is_downgraded(m.certificate()).unwrap());
}

/// O mesmo no `join`: um Welcome v4 recusado pela identidade esperada não vira o trinco (auditoria S1).
#[test]
fn a_refused_v4_welcome_does_not_turn_the_latch() {
    let w = world();
    let welcome = invite(&w.p, &w.c).unwrap();
    let someone_else = StandaloneIdentity::generate().id();
    let refused = Conversation::join(
        &w.c,
        &welcome,
        &Expected {
            identity: Some(&someone_else),
            peer: None,
        },
    );
    assert!(refused.is_err());

    assert!(invite(&w.c, &w.m).is_ok());
    assert!(!w.c.is_downgraded(w.m.certificate()).unwrap());
}

/// "Ao aceitar": um KeyPackage v4 cujo convite é **recusado** (identidade esperada diferente) não vira o
/// trinco; senão um estranho mexeria no estado permanente só por apresentar uma credencial.
#[test]
fn a_refused_v4_key_package_does_not_turn_the_latch() {
    let w = world();
    let someone_else = StandaloneIdentity::generate().id();
    let mut conversation = Conversation::create(&w.c).unwrap();
    let refused = conversation.invite(
        &w.p.key_package().unwrap(),
        &Expected {
            identity: Some(&someone_else),
            peer: None,
        },
    );
    assert!(refused.is_err());

    assert!(invite(&w.c, &w.m).is_ok());
    assert!(!w.c.is_downgraded(w.m.certificate()).unwrap());
}
