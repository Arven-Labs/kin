//! P9 (c, ADR 0041): o ciclo completo da recuperação por `Stale`, com dois `Chat` reais.
//!
//! Um lado (`ana`) gira a chave e **perde o commit de propósito** (`rotate_keys_dropping_commit`); tudo o
//! que ela cifra depois fica numa época que `bia` não alcança. Com 33 mensagens, `bia` emite `Stale`,
//! abandona a conversa e fecha a conexão; ao reconectar, o `Hello` diz "não tenho conversa" e o P16 a
//! refaz com a **mesma identidade** (`ConversationReset`).
//!
//! Quem cria o grupo MLS é o de menor Peer ID e o sorteio muda a cada execução: o teste vale nas duas
//! ordens porque não depende de qual dos dois é. A outbox do lado atrasado tem teste unitário
//! (`stale_drops_the_conversation_and_keeps_the_outbox`).

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
    let (ana, bia) = (
        Person::new(dir.path(), "ana"),
        Person::new(dir.path(), "bia"),
    );
    let (mut chat_ana, mut chat_bia) = (ana.open(), bia.open());
    let addr = connect(&mut chat_ana, &mut chat_bia).await;
    let (ana_peer, bia_peer) = (ana.peer_id(), bia.peer_id());

    // `ana` avança de época sem que `bia` receba o commit, e manda o bastante para estourar o buffer dela.
    chat_ana.rotate_keys_dropping_commit(bia_peer).unwrap();
    for i in 0..FLOOD {
        chat_ana
            .send(bia_peer, format!("na época nova {i}"), None)
            .unwrap();
    }

    // `bia` percebe a divergência e abandona a conversa (e reconecta sozinha).
    let stale = timeout(WAIT, async {
        tokio::select! {
            found = wait_for(&mut chat_bia, |e| match e {
                ChatEvent::Stale { peer, recovering } => Some((peer, recovering)),
                _ => None,
            }) => found,
            _ = drive(&mut chat_ana) => unreachable!(),
        }
    })
    .await
    .expect("bia não emitiu Stale");
    assert_eq!(
        stale,
        (ana_peer, true),
        "o lado atrasado, conectado, abandona a conversa"
    );

    // A queda chega aos dois lados. Quem volta a discar é a rediscagem de fundo, mas ela só usa endereços
    // públicos (`dial_addrs` filtra loopback), então aqui o teste faz o papel do mDNS ou do `/dial`.
    timeout(WAIT, async {
        tokio::select! {
            () = wait_for(&mut chat_bia, |e| {
                matches!(e, ChatEvent::PeerDisconnected(p) if p == ana_peer).then_some(())
            }) => {}
            _ = drive(&mut chat_ana) => unreachable!(),
        }
    })
    .await
    .expect("bia não viu a queda que o Stale provoca");
    chat_bia.dial(addr).unwrap();

    // Na reconexão, o P16 refaz a conversa; os dois lados avisam a troca com a mesma identidade da
    // conversa antiga.
    let (seen_ana, seen_bia) = settle(&mut chat_ana, &mut chat_bia, Duration::from_secs(15)).await;
    let was_reset = |seen: &[ChatEvent], peer, identity| {
        seen.iter().any(|e| {
            matches!(e, ChatEvent::ConversationReset { peer: p, identity: i }
                if *p == peer && *i == identity)
        })
    };
    assert!(
        was_reset(&seen_bia, ana_peer, ana.identity.id()),
        "bia devia refazer a conversa com a identidade de ana\neventos de bia: {seen_bia:#?}"
    );
    assert!(
        was_reset(&seen_ana, bia_peer, bia.identity.id()),
        "ana devia ver a conversa refeita com a identidade de bia\neventos de ana: {seen_ana:#?}"
    );
    assert!(chat_bia.is_ready(&ana_peer) && chat_ana.is_ready(&bia_peer));

    // A conversa nova funciona nos dois sentidos.
    chat_bia
        .send(ana_peer, "depois da recuperação", None)
        .unwrap();
    let (_, message) = receive_while_driving(&mut chat_ana, &mut chat_bia).await;
    assert_eq!(message.text, "depois da recuperação");
    chat_ana.send(bia_peer, "de volta", None).unwrap();
    let (_, message) = receive_while_driving(&mut chat_bia, &mut chat_ana).await;
    assert_eq!(message.text, "de volta");
}
