//! P30 3e-ii-b (ADR 0032): o `Chat` derruba as conversas com devices v3 (da mestra) de uma identidade que já
//! foi aceita em regime v4, na hora em que o v4 é aceito e na reabertura (com backfill do trinco).
//!
//! Personagens: `C` é o contato; `M` é um device de X em modo mestra (v3); `P` é um device pareado de X (v4);
//! `Y` é um device v3 de outra identidade.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Person, WAIT, connect, connect_until_ready_within, listen, settle, wait_for};
use kin_chat::{Chat, ChatEvent};
use kin_crypto::{Conversation, CryptoDevice, Expected, PendingDevice};
use kin_identity::{
    AuthorityIdentity, AuthorityKey, DeviceKey, IdentityId, IdentityProvider, StandaloneIdentity,
};
use kin_transport::NodeConfig;
use tokio::time::timeout;

fn config() -> NodeConfig {
    NodeConfig {
        mdns: false,
        ..Default::default()
    }
}

/// `P`: um device de X pareado por autoridade (v4), com o banco `db` já com o certificado instalado.
struct V4 {
    x: IdentityId,
    key: DeviceKey,
    db: PathBuf,
}

impl V4 {
    fn new(dir: &Path, name: &str, master: &StandaloneIdentity) -> Self {
        let authority_key = AuthorityKey::generate();
        let cert = master
            .certify_authority(&authority_key.public_key(), 1_000)
            .unwrap();
        let primary = AuthorityIdentity::new(authority_key, cert).unwrap();
        let (key, db) = (DeviceKey::generate(), dir.join(format!("{name}.db")));
        let pending = PendingDevice::open(&db).unwrap();
        let request = pending.request(&key).unwrap();
        let sas = request.sas_code(&primary.id());
        let cert = primary.answer_pair_request(&request, &sas, 2_000).unwrap();
        drop(
            pending
                .install_certificate(&key, &cert, &primary.id())
                .unwrap(),
        );
        Self {
            x: primary.id(),
            key,
            db,
        }
    }

    fn open(&self) -> Chat {
        Chat::open_paired(&self.x, &self.key, config(), &self.db).unwrap()
    }
}

fn downgraded(events: &[ChatEvent]) -> Vec<&ChatEvent> {
    events
        .iter()
        .filter(|e| matches!(e, ChatEvent::PeerDowngraded { .. }))
        .collect()
}

/// `P` disca `C` (que já tem conversas) e devolve os eventos que `C` viu até assentar.
async fn p_joins(chat_c: &mut Chat, chat_p: &mut Chat) -> Vec<ChatEvent> {
    let addr = listen(chat_c).await;
    chat_p.dial(addr).unwrap();
    let (mut seen, _) = connect_until_ready_within(chat_c, chat_p, WAIT).await;
    let (after, _) = settle(chat_c, chat_p, Duration::from_secs(1)).await;
    seen.extend(after);
    seen
}

/// Com o `Chat` de `C` fechado, cria no banco dele uma conversa com `P` pelo `crypto` (que marca X) e depois
/// apaga a marca: é o que um banco de antes do trinco existir tem em disco (conversas v3 e v4, sem
/// `kin_regime`).
fn legacy_conversation_with_v4(c: &Person, p: &V4) {
    let c_device = CryptoDevice::open(
        &c.identity,
        &c.device.public_key(),
        &c.device.nostr_public_key(),
        &c.db,
        1,
    )
    .unwrap();
    let p_device =
        CryptoDevice::open_paired(&p.x, &p.key.public_key(), &p.key.nostr_public_key(), &p.db)
            .unwrap();
    let mut conversation = Conversation::create(&c_device).unwrap();
    conversation
        .invite(&p_device.key_package().unwrap(), &Expected::default())
        .unwrap();
    c_device
        .remember(&p.key.peer_id().to_string(), &conversation.id())
        .unwrap();
    drop(conversation);
    drop(c_device);
    rusqlite::Connection::open(&c.db)
        .unwrap()
        .execute("DELETE FROM kin_regime", [])
        .unwrap();
}

// ---- ao aceitar o v4 ----

/// `C` conversa com `M` (v3 de X); quando aceita `P` (v4 de X), a conversa com `M` cai e a com `P` fica.
#[tokio::test(flavor = "multi_thread")]
async fn accepting_a_v4_device_drops_the_open_v3_conversation_of_the_same_identity() {
    let dir = tempfile::tempdir().unwrap();
    let (m, c) = (Person::new(dir.path(), "m"), Person::new(dir.path(), "c"));
    let p = V4::new(dir.path(), "p", &m.identity);
    let (mut chat_c, mut chat_m) = (c.open(), m.open());
    connect(&mut chat_c, &mut chat_m).await;
    assert!(chat_c.is_ready(&m.peer_id()));
    let mut chat_p = p.open();

    let seen = p_joins(&mut chat_c, &mut chat_p).await;

    assert_eq!(
        downgraded(&seen),
        [&ChatEvent::PeerDowngraded {
            peer: m.peer_id(),
            identity: m.identity.id()
        }]
    );
    assert!(!chat_c.is_ready(&m.peer_id()));
    assert!(chat_c.is_ready(&p.key.peer_id()));
}

/// Controle: o trinco é por identidade; a conversa com um v3 de **outra** identidade fica.
#[tokio::test(flavor = "multi_thread")]
async fn a_v3_conversation_of_another_identity_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let (m, y, c) = (
        Person::new(dir.path(), "m"),
        Person::new(dir.path(), "y"),
        Person::new(dir.path(), "c"),
    );
    let p = V4::new(dir.path(), "p", &m.identity);
    let (mut chat_c, mut chat_m, mut chat_y) = (c.open(), m.open(), y.open());
    connect(&mut chat_c, &mut chat_m).await;
    connect(&mut chat_c, &mut chat_y).await;
    let mut chat_p = p.open();

    let seen = p_joins(&mut chat_c, &mut chat_p).await;

    assert_eq!(downgraded(&seen).len(), 1);
    assert!(!chat_c.is_ready(&m.peer_id()));
    assert!(chat_c.is_ready(&y.peer_id()));
}

// ---- na reabertura ----

/// Um banco de antes do trinco, com conversas v3 e v4 de X e sem `kin_regime`: a reabertura faz o backfill
/// (marca X pela conversa v4) e só então varre, derrubando a v3.
#[tokio::test(flavor = "multi_thread")]
async fn reopening_backfills_the_latch_and_drops_the_v3_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let (m, c) = (Person::new(dir.path(), "m"), Person::new(dir.path(), "c"));
    let p = V4::new(dir.path(), "p", &m.identity);
    {
        let (mut chat_c, mut chat_m) = (c.open(), m.open());
        connect(&mut chat_c, &mut chat_m).await;
    }
    legacy_conversation_with_v4(&c, &p);

    let mut reopened = c.open();

    assert!(!reopened.is_ready(&m.peer_id()));
    assert!(reopened.is_ready(&p.key.peer_id()));
    let event = timeout(
        WAIT,
        wait_for(&mut reopened, |e| {
            matches!(e, ChatEvent::PeerDowngraded { .. }).then_some(e)
        }),
    )
    .await
    .expect("sem PeerDowngraded na reabertura");
    assert_eq!(
        event,
        ChatEvent::PeerDowngraded {
            peer: m.peer_id(),
            identity: m.identity.id()
        }
    );
}

/// Controle (confiança no primeiro uso): sem nenhum v4 de X aceito, a conversa v3 volta ao reabrir.
#[tokio::test(flavor = "multi_thread")]
async fn a_v3_conversation_is_kept_while_no_v4_of_the_identity_was_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let (m, c) = (Person::new(dir.path(), "m"), Person::new(dir.path(), "c"));
    {
        let (mut chat_c, mut chat_m) = (c.open(), m.open());
        connect(&mut chat_c, &mut chat_m).await;
    }

    let mut reopened = c.open();

    assert!(reopened.is_ready(&m.peer_id()));
    let event = timeout(
        Duration::from_millis(500),
        wait_for(&mut reopened, |e| {
            matches!(e, ChatEvent::PeerDowngraded { .. }).then_some(e)
        }),
    )
    .await;
    assert!(event.is_err(), "derrubou sem v4 aceito: {event:?}");
}

/// Falha fechada: se o store do trinco não pode ser lido, a reabertura falha, em vez de abrir com uma
/// conversa que talvez devesse ter caído.
#[tokio::test(flavor = "multi_thread")]
async fn reopening_with_an_unreadable_regime_store_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (m, c) = (Person::new(dir.path(), "m"), Person::new(dir.path(), "c"));
    {
        let (mut chat_c, mut chat_m) = (c.open(), m.open());
        connect(&mut chat_c, &mut chat_m).await;
    }
    rusqlite::Connection::open(&c.db)
        .unwrap()
        // Só apagar a tabela não basta: `Provider::open` a recria (`CREATE TABLE IF NOT EXISTS`). Uma tabela
        // com outro formato sobrevive à abertura e quebra a consulta.
        .execute_batch("DROP TABLE kin_regime; CREATE TABLE kin_regime (other TEXT);")
        .unwrap();

    let result = Chat::open(&c.identity, &c.device, config(), &c.db);

    assert!(result.is_err(), "abriu com o store do trinco quebrado");
}

/// Controle: a identidade **própria** nunca cai. `M` (v3) e `P` (v4) são do mesmo X e continuam conversando
/// depois de reabrir (a convivência v3/v4 da mesma identidade vale até a migração; limite I1 do ADR 0031).
#[tokio::test(flavor = "multi_thread")]
async fn siblings_of_the_same_identity_are_never_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let m = Person::new(dir.path(), "m");
    let p = V4::new(dir.path(), "p", &m.identity);
    {
        let (mut chat_m, mut chat_p) = (m.open(), p.open());
        connect(&mut chat_m, &mut chat_p).await;
    }

    let (mut reopened_m, mut reopened_p) = (m.open(), p.open());

    assert!(reopened_m.is_ready(&p.key.peer_id()));
    assert!(reopened_p.is_ready(&m.peer_id()));
    let (seen_m, seen_p) =
        settle(&mut reopened_m, &mut reopened_p, Duration::from_millis(500)).await;
    assert!(downgraded(&seen_m).is_empty() && downgraded(&seen_p).is_empty());
}
