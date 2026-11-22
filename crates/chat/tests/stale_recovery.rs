//! P9 (c, ADR 0041): o ciclo completo da recuperação por `Stale`, com dois `Chat` reais.
//!
//! Quem gira a chave do grupo é o de menor Peer ID (o committer, ADR 0001). Ele (`lead`) gira a chave e
//! **perde o commit de propósito** (`rotate_keys_dropping_commit`); tudo o que cifra depois fica numa
//! época que o outro (`lag`) não alcança. Com 33 mensagens, `lag` emite `Stale`, abandona a conversa e
//! fecha a conexão; ao reconectar, o `Hello` diz "não tenho conversa" e o P16 a refaz com a **mesma
//! identidade** (`ConversationReset`).
//!
//! Os papéis saem da ordem dos Peer IDs, sorteada a cada execução, então o teste vale nas duas ordens. A
//! outbox do lado atrasado tem teste unitário (`stale_drops_the_conversation_and_keeps_the_outbox`).

mod common;

use std::time::Duration;

use common::{Person, WAIT, connect, drive, receive_while_driving, settle, wait_for};
use kin_chat::ChatEvent;
use kin_identity::IdentityProvider;
use tokio::time::timeout;

/// Mensagens de época futura que estouram o buffer (`FUTURE_LIMIT` = 32, mais a que passa do teto).
const FLOOD: usize = 33;

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_conversation_is_rebuilt_with_the_same_identity() {
    let dir = tempfile::tempdir().unwrap();
    let (one, other) = (
        Person::new(dir.path(), "um"),
        Person::new(dir.path(), "outro"),
    );
    let (lead, lag) = if one.peer_id() < other.peer_id() {
        (one, other)
    } else {
        (other, one)
    };
    let (mut chat_lead, mut chat_lag) = (lead.open(), lag.open());
    let addr = connect(&mut chat_lead, &mut chat_lag).await;
    let (lead_peer, lag_peer) = (lead.peer_id(), lag.peer_id());

    // `lead` avança de época sem que `lag` receba o commit, e manda o bastante para estourar o buffer dele.
    chat_lead.rotate_keys_dropping_commit(lag_peer).unwrap();
    for i in 0..FLOOD {
        chat_lead
            .send(lag_peer, format!("na época nova {i}"), None)
            .unwrap();
    }

    // `lag` percebe a divergência e abandona a conversa.
    let stale = timeout(WAIT, async {
        tokio::select! {
            found = wait_for(&mut chat_lag, |e| match e {
                ChatEvent::Stale { peer, recovering } => Some((peer, recovering)),
                _ => None,
            }) => found,
            _ = drive(&mut chat_lead) => unreachable!(),
        }
    })
    .await
    .expect("lag não emitiu Stale");
    assert_eq!(
        stale,
        (lead_peer, true),
        "o lado atrasado, conectado, abandona a conversa"
    );

    // A queda chega aos dois lados. Quem volta a discar é a rediscagem de fundo, mas ela só usa endereços
    // públicos (`dial_addrs` filtra loopback), então aqui o teste faz o papel do mDNS ou do `/dial`.
    timeout(WAIT, async {
        tokio::select! {
            () = wait_for(&mut chat_lag, |e| {
                matches!(e, ChatEvent::PeerDisconnected(p) if p == lead_peer).then_some(())
            }) => {}
            _ = drive(&mut chat_lead) => unreachable!(),
        }
    })
    .await
    .expect("lag não viu a queda que o Stale provoca");
    chat_lag.dial(addr).unwrap();

    // Na reconexão, o P16 refaz a conversa; os dois lados avisam a troca com a mesma identidade da
    // conversa antiga.
    let (seen_lead, seen_lag) =
        settle(&mut chat_lead, &mut chat_lag, Duration::from_secs(15)).await;
    let was_reset = |seen: &[ChatEvent], peer, identity| {
        seen.iter().any(|e| {
            matches!(e, ChatEvent::ConversationReset { peer: p, identity: i }
                if *p == peer && *i == identity)
        })
    };
    assert!(
        was_reset(&seen_lag, lead_peer, lead.identity.id()),
        "lag devia refazer a conversa com a identidade de lead\neventos de lag: {seen_lag:#?}\neventos de lead: {seen_lead:#?}"
    );
    assert!(
        was_reset(&seen_lead, lag_peer, lag.identity.id()),
        "lead devia ver a conversa refeita com a identidade de lag\neventos de lead: {seen_lead:#?}\neventos de lag: {seen_lag:#?}"
    );
    assert!(chat_lag.is_ready(&lead_peer) && chat_lead.is_ready(&lag_peer));

    // A conversa nova funciona nos dois sentidos.
    chat_lag
        .send(lead_peer, "depois da recuperação", None)
        .unwrap();
    let (_, message) = receive_while_driving(&mut chat_lead, &mut chat_lag).await;
    assert_eq!(message.text, "depois da recuperação");
    chat_lead.send(lag_peer, "de volta", None).unwrap();
    let (_, message) = receive_while_driving(&mut chat_lag, &mut chat_lead).await;
    assert_eq!(message.text, "de volta");
}
