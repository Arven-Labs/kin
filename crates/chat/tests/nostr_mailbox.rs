//! Fase 3.3 (ADR 0004): o `NostrMailbox` contra relays falsos em processo (127.0.0.1), sem rede externa.

mod common;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::fake_relay::{FakeRelay, Policy, dead_url};
use common::{Person, WAIT, connect, until_disconnected};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use kin_chat::nostr::{MAX_BLOB, NostrConfig, NostrError, NostrMailbox, wrap};
use kin_chat::{Chat, ChatEvent, Mailbox, MailboxEvent};
use kin_identity::DeviceKey;
use nostr::prelude::*;
use tokio::time::{sleep, timeout};

fn fast() -> NostrConfig {
    NostrConfig {
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        ack_timeout: Duration::from_millis(300),
        ack_deadline: Duration::from_secs(1),
        // Os testes antigos medem um relay de cada vez; o quórum tem testes próprios (`quorum`).
        min_replicas: 1,
        ..NostrConfig::default()
    }
}

/// Como `fast()`, mas exigindo `min` aceites antes de contar o depósito.
fn quorum(min: usize) -> NostrConfig {
    NostrConfig {
        min_replicas: min,
        ..fast()
    }
}

/// Liga `mailbox` e devolve o canal de eventos que o `Chat` leria.
fn attach(mailbox: &mut NostrMailbox) -> UnboundedReceiver<MailboxEvent> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    mailbox.attach(tx);
    rx
}

/// Próximo evento bruto, inclusive o diagnóstico por relay (`RelayOutcome`).
async fn next_raw(rx: &mut UnboundedReceiver<MailboxEvent>) -> MailboxEvent {
    timeout(Duration::from_secs(5), rx.next())
        .await
        .expect("nenhum evento da caixa postal em 5 s")
        .expect("canal fechado")
}

/// Próximo evento que não é diagnóstico por relay: os testes de agregação ignoram os `RelayOutcome`.
async fn next(rx: &mut UnboundedReceiver<MailboxEvent>) -> MailboxEvent {
    loop {
        let event = next_raw(rx).await;
        if !matches!(
            event,
            MailboxEvent::RelayOutcome { .. } | MailboxEvent::RelaySlow { .. }
        ) {
            return event;
        }
    }
}

/// Junta eventos brutos até ter `outcomes` resultados por relay; devolve (resultados, demais eventos).
async fn collect_outcomes(
    rx: &mut UnboundedReceiver<MailboxEvent>,
    outcomes: usize,
) -> (Vec<(String, bool, String)>, Vec<MailboxEvent>) {
    let (mut found, mut others) = (Vec::new(), Vec::new());
    while found.len() < outcomes {
        match next_raw(rx).await {
            MailboxEvent::RelayOutcome {
                relay,
                accepted,
                reason,
                ..
            } => found.push((relay, accepted, reason)),
            other => others.push(other),
        }
    }
    (found, others)
}

/// A URL do relay pode voltar normalizada (barra final); compara sem ela.
fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

async fn quiet(rx: &mut UnboundedReceiver<MailboxEvent>, ms: u64) {
    if let Ok(Some(event)) = timeout(Duration::from_millis(ms), rx.next()).await {
        panic!("não devia haver evento, veio {event:?}");
    }
}

/// Espera (até 3 s) uma condição que depende de tarefas em segundo plano.
async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(3);
    while !cond() {
        assert!(Instant::now() < end, "não aconteceu: {what}");
        sleep(Duration::from_millis(20)).await;
    }
}

fn mailbox<S: AsRef<str>>(device: &DeviceKey, urls: &[S]) -> NostrMailbox {
    NostrMailbox::with_config(device, urls, fast()).unwrap()
}

fn p_tag(to: &DeviceKey) -> Tag {
    Tag::public_key(PublicKey::from_slice(&to.nostr_public_key().to_bytes()).unwrap())
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn a_deposit_reaches_every_relay_with_the_same_event_and_is_confirmed_by_the_first() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let (r1, r2) = (
        FakeRelay::start(Policy::Accept).await,
        FakeRelay::start(Policy::Accept).await,
    );
    let mut mb = mailbox(&ana, &[&r1.url, &r2.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    let id = mb.deposit(bia.peer_id(), b"oi bia".to_vec());

    assert_eq!(next(&mut rx).await, MailboxEvent::Deposited { id });
    eventually("os dois relays guardam o evento", || {
        r1.stored().len() == 1 && r2.stored().len() == 1
    })
    .await;
    assert_eq!(r1.stored()[0].id, r2.stored()[0].id);
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn one_relay_refusing_does_not_fail_the_deposit() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let refusing = FakeRelay::start(Policy::Reject("blocked: not in web of trust".into())).await;
    let accepting = FakeRelay::start(Policy::Accept).await;
    let mut mb = mailbox(&ana, &[&refusing.url, &accepting.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    assert_eq!(next(&mut rx).await, MailboxEvent::Deposited { id });
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn every_relay_refusing_fails_the_deposit_with_the_relay_reason() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let (r1, r2) = (
        FakeRelay::start(Policy::Reject("blocked: not in web of trust".into())).await,
        FakeRelay::start(Policy::Reject("blocked: not in web of trust".into())).await,
    );
    let mut mb = mailbox(&ana, &[&r1.url, &r2.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    match next(&mut rx).await {
        MailboxEvent::DepositFailed { id: got, reason } => {
            assert_eq!(got, id);
            assert!(reason.contains("web of trust"), "motivo: {reason}");
        }
        other => panic!("esperava DepositFailed, veio {other:?}"),
    }
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn no_relay_reachable_fails_the_deposit() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let mut mb = mailbox(&ana, &[&dead_url().await]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    assert!(matches!(
        next(&mut rx).await,
        MailboxEvent::DepositFailed { id: got, .. } if got == id
    ));
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_never_answers_fails_the_deposit_after_the_ack_timeout() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let silent = FakeRelay::start(Policy::Silent).await;
    let mut mb = mailbox(&ana, &[&silent.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());
    eventually("conexão feita", || silent.accepted_connections() >= 1).await;

    let started = Instant::now();
    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    assert!(matches!(
        next(&mut rx).await,
        MailboxEvent::DepositFailed { id: got, .. } if got == id
    ));
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "não esperou o ack"
    );
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_blob_or_an_unknown_peer_fails_without_touching_any_relay() {
    let (ana, bia, stranger) = (
        DeviceKey::generate(),
        DeviceKey::generate(),
        DeviceKey::generate(),
    );
    let relay = FakeRelay::start(Policy::Accept).await;
    let mut mb = mailbox(&ana, &[&relay.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    let big = mb.deposit(bia.peer_id(), vec![0u8; MAX_BLOB + 1]);
    let unknown = mb.deposit(stranger.peer_id(), b"x".to_vec());

    for expected in [big, unknown] {
        assert!(matches!(
            next(&mut rx).await,
            MailboxEvent::DepositFailed { id, .. } if id == expected
        ));
    }
    sleep(Duration::from_millis(200)).await;
    assert!(relay.stored().is_empty());
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn a_stored_event_is_retrieved_when_the_recipient_connects() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let relay = FakeRelay::start(Policy::Accept).await;
    relay.inject(wrap(&ana, &bia.nostr_public_key(), b"guardada").unwrap());

    let mut mb = mailbox(&bia, &[&relay.url]);
    mb.learn_peer(ana.peer_id(), ana.nostr_public_key());
    let mut rx = attach(&mut mb);

    assert_eq!(
        next(&mut rx).await,
        MailboxEvent::Retrieved {
            from: ana.peer_id(),
            blob: b"guardada".to_vec()
        }
    );
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn the_same_event_from_two_relays_is_retrieved_once() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let (r1, r2) = (
        FakeRelay::start(Policy::Accept).await,
        FakeRelay::start(Policy::Accept).await,
    );
    let event = wrap(&ana, &bia.nostr_public_key(), b"uma vez").unwrap();
    r1.inject(event.clone());
    r2.inject(event);

    let mut mb = mailbox(&bia, &[&r1.url, &r2.url]);
    mb.learn_peer(ana.peer_id(), ana.nostr_public_key());
    let mut rx = attach(&mut mb);

    assert!(matches!(
        next(&mut rx).await,
        MailboxEvent::Retrieved { .. }
    ));
    quiet(&mut rx, 700).await;
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn an_event_from_an_unknown_sender_is_dropped_and_the_worker_survives() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let relay = FakeRelay::start(Policy::Accept).await;
    let mut mb = mailbox(&bia, &[&relay.url]);
    let mut rx = attach(&mut mb);
    eventually("assinatura feita", || !relay.reqs().is_empty()).await;

    relay.inject(wrap(&ana, &bia.nostr_public_key(), b"quem e voce").unwrap());
    quiet(&mut rx, 500).await;

    // O worker continua vivo: depois de conhecer a Ana, o próximo evento chega.
    mb.learn_peer(ana.peer_id(), ana.nostr_public_key());
    relay.inject(wrap(&ana, &bia.nostr_public_key(), b"agora sim").unwrap());
    assert_eq!(
        next(&mut rx).await,
        MailboxEvent::Retrieved {
            from: ana.peer_id(),
            blob: b"agora sim".to_vec()
        }
    );
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn a_garbage_gift_wrap_is_ignored_and_the_next_valid_one_arrives() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let relay = FakeRelay::start(Policy::Accept).await;
    let garbage = EventBuilder::new(Kind::GiftWrap, "isto não é um gift wrap")
        .tag(p_tag(&bia))
        .finalize(&Keys::generate())
        .unwrap();
    relay.inject(garbage);
    relay.inject(wrap(&ana, &bia.nostr_public_key(), b"valida").unwrap());

    let mut mb = mailbox(&bia, &[&relay.url]);
    mb.learn_peer(ana.peer_id(), ana.nostr_public_key());
    let mut rx = attach(&mut mb);

    assert_eq!(
        next(&mut rx).await,
        MailboxEvent::Retrieved {
            from: ana.peer_id(),
            blob: b"valida".to_vec()
        }
    );
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_demands_nip42_serves_the_event_after_authentication() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let relay = FakeRelay::start(Policy::Accept).await;
    relay.require_auth();
    relay.inject(wrap(&ana, &bia.nostr_public_key(), b"so autenticado").unwrap());

    let mut mb = mailbox(&bia, &[&relay.url]);
    mb.learn_peer(ana.peer_id(), ana.nostr_public_key());
    let mut rx = attach(&mut mb);

    assert_eq!(
        next(&mut rx).await,
        MailboxEvent::Retrieved {
            from: ana.peer_id(),
            blob: b"so autenticado".to_vec()
        }
    );
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn after_a_dropped_connection_the_worker_reconnects_and_resubscribes() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let relay = FakeRelay::start(Policy::Accept).await;
    let mut mb = mailbox(&bia, &[&relay.url]);
    mb.learn_peer(ana.peer_id(), ana.nostr_public_key());
    let mut rx = attach(&mut mb);
    eventually("primeira assinatura", || relay.reqs().len() == 1).await;

    relay.drop_connections();
    relay.inject(wrap(&ana, &bia.nostr_public_key(), b"na lacuna").unwrap());

    assert_eq!(
        next(&mut rx).await,
        MailboxEvent::Retrieved {
            from: ana.peer_id(),
            blob: b"na lacuna".to_vec()
        }
    );
    assert!(relay.reqs().len() >= 2, "devia ter reassinado");
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn the_subscription_asks_for_the_last_ten_days() {
    let bia = DeviceKey::generate();
    let relay = FakeRelay::start(Policy::Accept).await;
    let mut mb = mailbox(&bia, &[&relay.url]);
    let _rx = attach(&mut mb);
    eventually("assinatura feita", || !relay.reqs().is_empty()).await;

    let filter = relay.reqs().remove(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let since = filter["since"].as_u64().expect("o REQ leva since");
    // 3.4: ttl (7 dias) + desvio do NIP-59 (2) + 1 dia de folga. Eram 3 dias, o que só achava
    // mensagens depositadas havia menos de 1 dia no pior sorteio.
    let expected = now - 10 * 24 * 3600;
    assert!(
        since.abs_diff(expected) <= 120,
        "since={since}, esperado ~{expected}"
    );
    assert_eq!(filter["kinds"], serde_json::json!([1059]));
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn bad_relay_urls_are_refused_and_tls_to_a_plain_server_fails_without_panicking() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    assert!(NostrMailbox::new(&ana, &["http://relay.example"]).is_err());
    assert!(NostrMailbox::new(&ana, &["ws://relay.example"]).is_err());
    assert!(NostrMailbox::new(&ana, &["ws://127.0.0.1:1"]).is_ok());

    // Servidor que aceita a conexão e fecha, sem falar TLS: o handshake do wss falha e só.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("wss://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { while listener.accept().await.is_ok() {} });
    let mut mb = mailbox(&ana, &[&url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());
    sleep(Duration::from_millis(300)).await;

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());
    assert!(matches!(
        next(&mut rx).await,
        MailboxEvent::DepositFailed { id: got, .. } if got == id
    ));
}

// 15
async fn until_deposited(chat: &mut Chat) {
    timeout(WAIT, async {
        while !matches!(chat.next_event().await, ChatEvent::Deposited { .. }) {}
    })
    .await
    .expect("o depósito não foi confirmado");
}

async fn until_received(chat: &mut Chat) -> String {
    timeout(WAIT, async {
        loop {
            if let ChatEvent::Received { message, .. } = chat.next_event().await {
                return message.text;
            }
        }
    })
    .await
    .expect("a mensagem não chegou pelo relay")
}

#[tokio::test(flavor = "multi_thread")]
async fn two_real_chats_exchange_a_message_through_a_relay_while_one_is_offline() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let relay = FakeRelay::start(Policy::Accept).await;
    let (mut a, mut b) = (pa.open(), pb.open());
    a.set_mailbox(mailbox(&pa.device, &[&relay.url]));
    connect(&mut a, &mut b).await;

    drop(b);
    until_disconnected(&mut a).await;
    a.send(pb.peer_id(), "pelo relay", None).unwrap();
    until_deposited(&mut a).await;
    assert_eq!(relay.stored().len(), 1);

    // A Bia volta sozinha, só com o relay: conversa recarregada do banco, sem novo handshake.
    let mut b = pb.open();
    b.set_mailbox(mailbox(&pb.device, &[&relay.url]));
    assert_eq!(until_received(&mut b).await, "pelo relay");
}

// 16
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_mailbox_closes_its_connections() {
    let bia = DeviceKey::generate();
    let relay = FakeRelay::start(Policy::Accept).await;
    let mut mb = mailbox(&bia, &[&relay.url]);
    let _rx = attach(&mut mb);
    eventually("conectou", || relay.open_connections() == 1).await;

    drop(mb);

    eventually("fechou", || relay.open_connections() == 0).await;
}

// 17
#[tokio::test(flavor = "multi_thread")]
async fn each_relay_reports_its_own_outcome_and_the_deposit_is_confirmed_once() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let refusing = FakeRelay::start(Policy::Reject("blocked: not in web of trust".into())).await;
    let accepting = FakeRelay::start(Policy::Accept).await;
    let mut mb = mailbox(&ana, &[&refusing.url, &accepting.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    let (outcomes, others) = collect_outcomes(&mut rx, 2).await;
    let of = |url: &str| outcomes.iter().find(|(r, ..)| same_url(r, url)).unwrap();
    assert!(
        of(&accepting.url).1,
        "o relay que aceitou deve constar como aceito"
    );
    let refused = of(&refusing.url);
    assert!(!refused.1);
    assert!(refused.2.contains("web of trust"), "motivo: {}", refused.2);
    let deposited = others
        .into_iter()
        .chain(next_or_none(&mut rx).await)
        .filter(|e| *e == MailboxEvent::Deposited { id })
        .count();
    assert_eq!(deposited, 1);
}

// 18
#[tokio::test(flavor = "multi_thread")]
async fn when_every_relay_refuses_each_outcome_is_reported_before_the_failure() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let (r1, r2) = (
        FakeRelay::start(Policy::Reject("blocked: a".into())).await,
        FakeRelay::start(Policy::Reject("blocked: b".into())).await,
    );
    let mut mb = mailbox(&ana, &[&r1.url, &r2.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    let (outcomes, others) = collect_outcomes(&mut rx, 2).await;
    assert!(outcomes.iter().all(|(_, accepted, _)| !accepted));
    let reasons: Vec<&str> = outcomes.iter().map(|(.., r)| r.as_str()).collect();
    assert!(reasons.contains(&"blocked: a") && reasons.contains(&"blocked: b"));
    let failed = others
        .into_iter()
        .chain(next_or_none(&mut rx).await)
        .any(|e| matches!(e, MailboxEvent::DepositFailed { id: got, .. } if got == id));
    assert!(failed);
}

// 19
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_relay_and_a_silent_relay_report_why() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let dead = dead_url().await;
    let silent = FakeRelay::start(Policy::Silent).await;
    let mut mb = mailbox(&ana, &[&dead, &silent.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());
    eventually("conexão feita", || silent.accepted_connections() >= 1).await;

    mb.deposit(bia.peer_id(), b"x".to_vec());

    let (outcomes, _) = collect_outcomes(&mut rx, 2).await;
    let of = |url: &str| outcomes.iter().find(|(r, ..)| same_url(r, url)).unwrap();
    assert!(!of(&dead).1 && of(&dead).2.contains("desconectado"));
    assert!(!of(&silent.url).1 && of(&silent.url).2.contains("a tempo"));
}

// 20
#[tokio::test(flavor = "multi_thread")]
async fn a_hostile_refusal_reason_is_one_short_line() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let hostile = format!("blocked: x\nrelay: ok\r\u{1b}[31m{}", "y".repeat(500));
    let relay = FakeRelay::start(Policy::Reject(hostile)).await;
    let mut mb = mailbox(&ana, &[&relay.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    mb.deposit(bia.peer_id(), b"x".to_vec());

    let (outcomes, _) = collect_outcomes(&mut rx, 1).await;
    let reason = &outcomes[0].2;
    assert!(reason.starts_with("blocked: x"), "motivo: {reason}");
    assert!(
        reason.chars().all(|c| !c.is_control()),
        "motivo: {reason:?}"
    );
    assert!(reason.chars().count() <= 200, "motivo: {reason}");
}

// 21
#[tokio::test(flavor = "multi_thread")]
async fn a_real_chat_surfaces_the_relay_outcome_next_to_the_deposit() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let relay = FakeRelay::start(Policy::Accept).await;
    let (mut a, mut b) = (pa.open(), pb.open());
    a.set_mailbox(mailbox(&pa.device, &[&relay.url]));
    connect(&mut a, &mut b).await;
    drop(b);
    until_disconnected(&mut a).await;

    a.send(pb.peer_id(), "oi", None).unwrap();

    let (mut deposited, mut outcome) = (false, None);
    timeout(WAIT, async {
        while !(deposited && outcome.is_some()) {
            match a.next_event().await {
                ChatEvent::Deposited { .. } => deposited = true,
                ChatEvent::MailboxRelay {
                    relay,
                    accepted,
                    reason,
                } => outcome = Some((relay, accepted, reason)),
                _ => {}
            }
        }
    })
    .await
    .expect("faltou o depósito ou o resultado do relay");
    let (url, accepted, _) = outcome.unwrap();
    assert!(same_url(&url, &relay.url) && accepted);
}

/// Todos os eventos que chegam em `window`, resumidos em uma palavra cada (a ordem importa).
async fn story(rx: &mut UnboundedReceiver<MailboxEvent>, window: Duration) -> Vec<String> {
    let end = tokio::time::Instant::now() + window;
    let mut tags = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout_at(end, rx.next()).await {
        tags.push(match event {
            MailboxEvent::RelaySlow { .. } => "slow".to_string(),
            MailboxEvent::RelayOutcome { accepted: true, .. } => "accepted".to_string(),
            MailboxEvent::RelayOutcome {
                accepted: false,
                reason,
                ..
            } => format!("rejected:{reason}"),
            MailboxEvent::Deposited { .. } => "deposited".to_string(),
            MailboxEvent::DepositFailed { .. } => "failed".to_string(),
            MailboxEvent::Retrieved { .. } => "retrieved".to_string(),
        });
    }
    tags
}

const TOO_LATE: &str = "rejected:o relay não respondeu a tempo";

/// Um depósito de `ana` para `bia` em `urls`, já com a conexão feita; devolve o canal de eventos.
async fn deposit_to(urls: &[&FakeRelay]) -> (NostrMailbox, UnboundedReceiver<MailboxEvent>) {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let addrs: Vec<&String> = urls.iter().map(|r| &r.url).collect();
    let mut mb = mailbox(&ana, &addrs);
    let rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());
    eventually("conexões feitas", || {
        urls.iter().all(|r| r.accepted_connections() >= 1)
    })
    .await;
    mb.deposit(bia.peer_id(), b"x".to_vec());
    (mb, rx)
}

// 22
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_relay_that_accepts_after_the_slow_warning_still_confirms_the_deposit() {
    let relay = FakeRelay::start(Policy::Slow(
        Duration::from_millis(600),
        Box::new(Policy::Accept),
    ))
    .await;
    let (_mb, mut rx) = deposit_to(&[&relay]).await;

    let tags = story(&mut rx, Duration::from_millis(1800)).await;

    assert_eq!(tags, ["slow", "accepted", "deposited"]);
}

// 23
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_relay_is_called_slow_first_and_only_fails_at_the_deadline() {
    let relay = FakeRelay::start(Policy::Silent).await;
    let (_mb, mut rx) = deposit_to(&[&relay]).await;

    let tags = story(&mut rx, Duration::from_millis(2000)).await;

    assert_eq!(tags, ["slow", TOO_LATE, "failed"]);
}

// 24
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_answers_before_the_slow_warning_is_never_called_slow() {
    let relay = FakeRelay::start(Policy::Accept).await;
    let (_mb, mut rx) = deposit_to(&[&relay]).await;

    let tags = story(&mut rx, Duration::from_millis(800)).await;

    assert_eq!(tags, ["accepted", "deposited"]);
}

// 25
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_relay_that_refuses_fails_the_deposit_with_its_own_reason() {
    let relay = FakeRelay::start(Policy::Slow(
        Duration::from_millis(600),
        Box::new(Policy::Reject("blocked: x".into())),
    ))
    .await;
    let (_mb, mut rx) = deposit_to(&[&relay]).await;

    let tags = story(&mut rx, Duration::from_millis(1800)).await;

    assert_eq!(tags, ["slow", "rejected:blocked: x", "failed"]);
}

// 26
#[tokio::test(flavor = "multi_thread")]
async fn an_ok_after_the_deadline_is_ignored_and_does_not_resurrect_the_deposit() {
    let relay = FakeRelay::start(Policy::Slow(
        Duration::from_millis(1500),
        Box::new(Policy::Accept),
    ))
    .await;
    let (_mb, mut rx) = deposit_to(&[&relay]).await;

    let tags = story(&mut rx, Duration::from_millis(2600)).await;

    assert_eq!(tags, ["slow", TOO_LATE, "failed"]);
}

// 27
#[tokio::test(flavor = "multi_thread")]
async fn a_fast_relay_confirms_once_and_the_slow_one_is_still_reported_when_it_answers() {
    let fast_relay = FakeRelay::start(Policy::Accept).await;
    let slow_relay = FakeRelay::start(Policy::Slow(
        Duration::from_millis(600),
        Box::new(Policy::Accept),
    ))
    .await;
    let (_mb, mut rx) = deposit_to(&[&fast_relay, &slow_relay]).await;

    let tags = story(&mut rx, Duration::from_millis(1800)).await;

    // O rápido aceita e confirma; o lento é chamado de lento e depois aceita; uma só confirmação.
    assert_eq!(tags, ["accepted", "deposited", "slow", "accepted"]);
}

// 28
#[tokio::test(flavor = "multi_thread")]
async fn a_real_chat_surfaces_the_slow_relay_before_the_deposit() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let relay = FakeRelay::start(Policy::Slow(
        Duration::from_millis(600),
        Box::new(Policy::Accept),
    ))
    .await;
    let (mut a, mut b) = (pa.open(), pb.open());
    a.set_mailbox(mailbox(&pa.device, &[&relay.url]));
    connect(&mut a, &mut b).await;
    drop(b);
    until_disconnected(&mut a).await;

    a.send(pb.peer_id(), "oi", None).unwrap();

    let mut order = Vec::new();
    timeout(WAIT, async {
        while !order.contains(&"deposited") {
            match a.next_event().await {
                ChatEvent::MailboxRelaySlow { relay: url } => {
                    assert!(same_url(&url, &relay.url));
                    order.push("slow");
                }
                ChatEvent::Deposited { .. } => order.push("deposited"),
                _ => {}
            }
        }
    })
    .await
    .expect("faltou o aviso de relay lento ou o depósito");
    assert_eq!(order, ["slow", "deposited"]);
}

// 29
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_refuses_our_auth_and_resends_the_same_challenge_is_answered_only_once() {
    let relay = FakeRelay::start(Policy::Accept).await;
    relay.refuse_auth();
    let ana = DeviceKey::generate();
    let mut mb = mailbox(&ana, &[&relay.url]);
    let _rx = attach(&mut mb);

    sleep(Duration::from_millis(1500)).await;

    // Antes da correção cada desafio repetido gerava um AUTH e um REQ novos, sem parar (257 voltas
    // em ~90 s contra o damus real). Agora: 1 AUTH e, no máximo, o REQ inicial mais o do AUTH.
    assert_eq!(
        relay.auth_attempts(),
        1,
        "AUTH repetido para o mesmo desafio"
    );
    assert!(
        relay.reqs().len() <= 2,
        "REQ em laço: {} pedidos",
        relay.reqs().len()
    );
}

// 30
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_refuses_auth_still_accepts_deposits() {
    let relay = FakeRelay::start(Policy::Accept).await;
    relay.refuse_auth();
    let (_mb, mut rx) = deposit_to(&[&relay]).await;

    let tags = story(&mut rx, Duration::from_millis(600)).await;

    assert_eq!(tags, ["accepted", "deposited"]);
}

// 31
#[tokio::test(flavor = "multi_thread")]
async fn after_a_reconnection_the_new_challenge_is_answered_once_again() {
    let relay = FakeRelay::start(Policy::Accept).await;
    relay.refuse_auth();
    let ana = DeviceKey::generate();
    let mut mb = mailbox(&ana, &[&relay.url]);
    let _rx = attach(&mut mb);
    eventually("primeiro AUTH", || relay.auth_attempts() == 1).await;

    relay.drop_connections();

    eventually("AUTH da nova conexão", || relay.auth_attempts() == 2).await;
    sleep(Duration::from_millis(500)).await;
    assert_eq!(relay.auth_attempts(), 2, "AUTH repetido na nova conexão");
}

// 32 (P23 PR 2)
#[tokio::test(flavor = "multi_thread")]
async fn with_the_contact_relays_known_the_deposit_goes_only_there() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let (own, theirs) = (
        FakeRelay::start(Policy::Accept).await,
        FakeRelay::start(Policy::Accept).await,
    );
    let mut mb = mailbox(&ana, &[&own.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());
    mb.learn_relays(bia.peer_id(), vec![theirs.url.clone()]);

    let id = mb.deposit(bia.peer_id(), b"para os relays dela".to_vec());

    assert_eq!(next(&mut rx).await, MailboxEvent::Deposited { id });
    eventually("o relay do contato guarda o evento", || {
        theirs.stored().len() == 1
    })
    .await;
    sleep(Duration::from_millis(200)).await;
    assert!(own.stored().is_empty(), "depositou no relay próprio");
}

// 33
#[tokio::test(flavor = "multi_thread")]
async fn without_a_contact_list_the_deposit_falls_back_to_the_own_relays() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let own = FakeRelay::start(Policy::Accept).await;
    let mut mb = mailbox(&ana, &[&own.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    let id = mb.deposit(bia.peer_id(), b"sem lista".to_vec());

    assert_eq!(next(&mut rx).await, MailboxEvent::Deposited { id });
    eventually("o relay próprio guarda o evento", || {
        own.stored().len() == 1
    })
    .await;
}

// 34
#[tokio::test(flavor = "multi_thread")]
async fn contact_relays_that_are_all_down_fail_the_deposit_without_falling_back() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let own = FakeRelay::start(Policy::Accept).await;
    let mut mb = mailbox(&ana, &[&own.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());
    mb.learn_relays(bia.peer_id(), vec![dead_url().await]);

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    assert!(matches!(
        next(&mut rx).await,
        MailboxEvent::DepositFailed { id: got, .. } if got == id
    ));
    sleep(Duration::from_millis(200)).await;
    assert!(own.stored().is_empty(), "caiu no relay próprio");
}

// 35
#[tokio::test(flavor = "multi_thread")]
async fn a_contact_relay_that_refuses_is_reported_and_one_that_accepts_confirms() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let (refusing, accepting) = (
        FakeRelay::start(Policy::Reject("blocked: not in web of trust".into())).await,
        FakeRelay::start(Policy::Accept).await,
    );
    let mut mb = mailbox(&ana, &["ws://127.0.0.1:1"]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());
    mb.learn_relays(
        bia.peer_id(),
        vec![refusing.url.clone(), accepting.url.clone()],
    );

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    let (outcomes, others) = collect_outcomes(&mut rx, 2).await;
    let refused = outcomes
        .iter()
        .find(|o| same_url(&o.0, &refusing.url))
        .unwrap();
    assert!(
        !refused.1 && refused.2.contains("web of trust"),
        "{refused:?}"
    );
    let accepted = outcomes
        .iter()
        .find(|o| same_url(&o.0, &accepting.url))
        .unwrap();
    assert!(accepted.1);
    assert!(
        others.contains(&MailboxEvent::Deposited { id })
            || next(&mut rx).await == MailboxEvent::Deposited { id }
    );
}

// 36
#[tokio::test(flavor = "multi_thread")]
async fn the_connection_to_a_contact_relay_only_publishes_and_then_closes() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let theirs = FakeRelay::start(Policy::Accept).await;
    let mut mb = mailbox(&ana, &["ws://127.0.0.1:1"]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());
    mb.learn_relays(bia.peer_id(), vec![theirs.url.clone()]);

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    assert_eq!(next(&mut rx).await, MailboxEvent::Deposited { id });
    eventually("conexão encerrada", || theirs.open_connections() == 0).await;
    assert_eq!(theirs.accepted_connections(), 1);
    assert!(
        theirs.reqs().is_empty(),
        "assinou eventos no relay do contato"
    );
}

// 37
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_contact_relay_warns_as_slow_then_fails_at_the_deadline() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let silent = FakeRelay::start(Policy::Silent).await;
    let mut mb = mailbox(&ana, &["ws://127.0.0.1:1"]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());
    mb.learn_relays(bia.peer_id(), vec![silent.url.clone()]);

    let started = Instant::now();
    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    assert!(matches!(
        next_raw(&mut rx).await,
        MailboxEvent::RelaySlow { id: got, .. } if got == id
    ));
    assert!(matches!(
        next(&mut rx).await,
        MailboxEvent::DepositFailed { id: got, .. } if got == id
    ));
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "não esperou o prazo"
    );
}

// 38
#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_contact_list_is_ignored_and_the_previous_one_stays() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let (own, theirs) = (
        FakeRelay::start(Policy::Accept).await,
        FakeRelay::start(Policy::Accept).await,
    );
    let mut mb = mailbox(&ana, &[&own.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());
    mb.learn_relays(bia.peer_id(), vec![theirs.url.clone()]);

    mb.learn_relays(bia.peer_id(), vec!["isso nao e url".to_string()]);
    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    assert_eq!(next(&mut rx).await, MailboxEvent::Deposited { id });
    eventually("depositou na lista anterior", || theirs.stored().len() == 1).await;
    assert!(own.stored().is_empty());
}

// 3.4: quórum de réplicas, TTL e janela de leitura.

/// Eventos brutos até o primeiro `Deposited` ou `DepositFailed`; devolve (aceites vistos antes, esse evento).
async fn until_verdict(rx: &mut UnboundedReceiver<MailboxEvent>) -> (usize, MailboxEvent) {
    let mut accepted = 0;
    loop {
        match next_raw(rx).await {
            MailboxEvent::RelayOutcome { accepted: true, .. } => accepted += 1,
            verdict @ (MailboxEvent::Deposited { .. } | MailboxEvent::DepositFailed { .. }) => {
                return (accepted, verdict);
            }
            _ => {}
        }
    }
}

fn quorum_mailbox(device: &DeviceKey, urls: &[&str], min: usize) -> NostrMailbox {
    NostrMailbox::with_config(device, urls, quorum(min)).unwrap()
}

// 39
#[tokio::test(flavor = "multi_thread")]
async fn with_two_replicas_required_the_deposit_waits_for_the_second_accept() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let (fast_relay, slow_relay) = (
        FakeRelay::start(Policy::Accept).await,
        FakeRelay::start(Policy::Slow(
            Duration::from_millis(400),
            Box::new(Policy::Accept),
        ))
        .await,
    );
    let mut mb = quorum_mailbox(&ana, &[&fast_relay.url, &slow_relay.url], 2);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    let (accepts_before, verdict) = until_verdict(&mut rx).await;
    assert_eq!(verdict, MailboxEvent::Deposited { id });
    assert_eq!(accepts_before, 2, "confirmou antes da segunda réplica");
}

// 40
#[tokio::test(flavor = "multi_thread")]
async fn one_accept_out_of_two_when_two_are_required_fails_and_says_how_many() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let (good, bad) = (
        FakeRelay::start(Policy::Accept).await,
        FakeRelay::start(Policy::Reject("blocked".into())).await,
    );
    let mut mb = quorum_mailbox(&ana, &[&good.url, &bad.url], 2);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    match until_verdict(&mut rx).await.1 {
        MailboxEvent::DepositFailed { id: got, reason } => {
            assert_eq!(got, id);
            assert!(reason.contains("1 de 2"), "motivo: {reason}");
            assert!(reason.contains("mínimo 2"), "motivo: {reason}");
        }
        other => panic!("esperava DepositFailed, veio {other:?}"),
    }
}

// 41
#[tokio::test(flavor = "multi_thread")]
async fn a_single_configured_relay_satisfies_a_quorum_of_two() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let only = FakeRelay::start(Policy::Accept).await;
    let mut mb = quorum_mailbox(&ana, &[&only.url], 2);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    assert_eq!(
        until_verdict(&mut rx).await.1,
        MailboxEvent::Deposited { id }
    );
}

// 42
#[tokio::test(flavor = "multi_thread")]
async fn two_accepts_out_of_three_are_enough() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let (a, b, c) = (
        FakeRelay::start(Policy::Accept).await,
        FakeRelay::start(Policy::Accept).await,
        FakeRelay::start(Policy::Reject("blocked".into())).await,
    );
    let mut mb = quorum_mailbox(&ana, &[&a.url, &b.url, &c.url], 2);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    let id = mb.deposit(bia.peer_id(), b"x".to_vec());

    assert_eq!(
        until_verdict(&mut rx).await.1,
        MailboxEvent::Deposited { id }
    );
}

// 43
#[tokio::test(flavor = "multi_thread")]
async fn the_quorum_also_applies_to_the_contact_relays() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let (good, bad) = (
        FakeRelay::start(Policy::Accept).await,
        FakeRelay::start(Policy::Reject("blocked".into())).await,
    );
    let mut mb = quorum_mailbox(&ana, &["ws://127.0.0.1:1"], 2);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());
    mb.learn_relays(bia.peer_id(), vec![good.url.clone(), bad.url.clone()]);

    let failing = mb.deposit(bia.peer_id(), b"x".to_vec());
    assert!(matches!(
        until_verdict(&mut rx).await.1,
        MailboxEvent::DepositFailed { id, .. } if id == failing
    ));

    mb.learn_relays(
        bia.peer_id(),
        vec![good.url.clone(), good.url.replace("127.0.0.1", "localhost")],
    );
    let passing = mb.deposit(bia.peer_id(), b"y".to_vec());
    assert_eq!(
        until_verdict(&mut rx).await.1,
        MailboxEvent::Deposited { id: passing }
    );
}

// 44
#[tokio::test(flavor = "multi_thread")]
async fn the_published_event_expires_after_at_least_the_ttl() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let relay = FakeRelay::start(Policy::Accept).await;
    let mut mb = mailbox(&ana, &[&relay.url]);
    let mut rx = attach(&mut mb);
    mb.learn_peer(bia.peer_id(), bia.nostr_public_key());

    mb.deposit(bia.peer_id(), b"x".to_vec());
    next(&mut rx).await;

    let event = relay.stored().remove(0);
    let expiration = event.tags.expiration().expect("sem expiração");
    let week = 7 * 24 * 3600;
    assert!(
        expiration.as_secs() >= Timestamp::now().as_secs() + week - 5,
        "expira antes do TTL de 7 dias"
    );
}

// 45
#[test]
fn the_defaults_are_a_week_of_ttl_two_replicas_and_a_window_that_covers_the_timestamp_tweak() {
    let config = NostrConfig::default();

    assert_eq!(config.ttl, Duration::from_secs(7 * 24 * 3600));
    assert_eq!(config.min_replicas, 2);
    assert!(
        config.since_window >= config.ttl + Duration::from_secs(2 * 24 * 3600),
        "a janela de leitura não cobre TTL + o desvio de 2 dias do NIP-59"
    );
    assert!(NostrMailbox::new(&DeviceKey::generate(), &["wss://a.example"]).is_ok());
}

// 46
#[test]
fn a_read_window_shorter_than_ttl_plus_two_days_is_refused() {
    let config = NostrConfig {
        ttl: Duration::from_secs(7 * 24 * 3600),
        since_window: Duration::from_secs(8 * 24 * 3600),
        ..NostrConfig::default()
    };

    let result = NostrMailbox::with_config(&DeviceKey::generate(), &["wss://a.example"], config);

    assert!(matches!(result, Err(NostrError::Config(_))));
}

// 47
#[test]
fn zero_required_replicas_is_refused() {
    let config = NostrConfig {
        min_replicas: 0,
        ..NostrConfig::default()
    };

    let result = NostrMailbox::with_config(&DeviceKey::generate(), &["wss://a.example"], config);

    assert!(matches!(result, Err(NostrError::Config(_))));
}

// 48
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_gift_wrap_is_ignored_and_the_next_valid_one_arrives() {
    let (ana, bia) = (DeviceKey::generate(), DeviceKey::generate());
    let relay = FakeRelay::start(Policy::Accept).await;
    relay.inject(expired_wrap(&ana, &bia, b"vencida"));
    relay.inject(wrap(&ana, &bia.nostr_public_key(), b"valida").unwrap());
    let mut mb = mailbox(&bia, &[&relay.url]);
    mb.learn_peer(ana.peer_id(), ana.nostr_public_key());
    let mut rx = attach(&mut mb);

    assert_eq!(
        next(&mut rx).await,
        MailboxEvent::Retrieved {
            from: ana.peer_id(),
            blob: b"valida".to_vec()
        }
    );
    quiet(&mut rx, 500).await;
}

/// Gift wrap válido por fora, mas cuja expiração (NIP-40) já passou.
fn expired_wrap(from: &DeviceKey, to: &DeviceKey, blob: &[u8]) -> Event {
    use base64::Engine as _;
    let keys = Keys::new(SecretKey::from_slice(&from.nostr_secret_bytes()).unwrap());
    let rumor = UnsignedEvent::new(
        keys.public_key(),
        Timestamp::now(),
        Kind::PrivateDirectMessage,
        Vec::<Tag>::new(),
        base64::engine::general_purpose::STANDARD.encode(blob),
    );
    nostr::nips::nip59::GiftWrapBuilder::new(
        PublicKey::from_slice(&to.nostr_public_key().to_bytes()).unwrap(),
        rumor,
    )
    .extra_tags([Tag::expiration(Timestamp::now() - 3600)])
    .finalize(&keys)
    .unwrap()
}

/// Próximo evento bruto, se vier logo (o `Deposited` pode sair depois dos resultados por relay).
async fn next_or_none(rx: &mut UnboundedReceiver<MailboxEvent>) -> Option<MailboxEvent> {
    timeout(Duration::from_millis(300), rx.next())
        .await
        .ok()
        .flatten()
}
