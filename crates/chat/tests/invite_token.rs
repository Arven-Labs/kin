//! P18 (ADR 0039): o link de convite carrega um token de uso único; quem o gerou, com a exigência
//! ligada, só abre conversa com peer desconhecido que o apresente, e pode revogá-lo antes do uso.
//!
//! Quem cria o grupo MLS é o de menor Peer ID, não quem gerou o link; por isso cada cenário roda
//! nas duas ordens (`ana_lower`), que exercitam o portão em `on_hello` e em `on_key_package`.

mod common;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::{Person, connect, connect_until_ready, listen, receive_while_driving, settle};
use kin_chat::{Chat, DEFAULT_INVITE_TTL, Error, Invite};
use kin_identity::{DeviceKey, StandaloneIdentity};
use kin_transport::NodeConfig;
use std::time::Duration;

struct P {
    device: DeviceKey,
    chat: Chat,
}

fn person() -> P {
    let identity = StandaloneIdentity::generate();
    let device = DeviceKey::generate();
    let chat = Chat::new(
        &identity,
        &device,
        NodeConfig {
            mdns: false,
            ..Default::default()
        },
    )
    .unwrap();
    P { device, chat }
}

/// Duas pessoas em que a Ana tem o menor Peer ID (`ana_lower`) ou o maior.
fn pair(ana_lower: bool) -> (P, P) {
    loop {
        let (ana, bia) = (person(), person());
        if (ana.device.peer_id() < bia.device.peer_id()) == ana_lower {
            return (ana, bia);
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

const SHORT: Duration = Duration::from_secs(4);

fn ready_between(a: &P, b: &P) -> bool {
    a.chat.is_ready(&b.device.peer_id()) || b.chat.is_ready(&a.device.peer_id())
}

/// A Ana (portão ligado) escuta e gera um link v3; devolve o endereço e o `Invite` lido do link.
async fn ana_with_link(ana: &mut P) -> Invite {
    ana.chat.set_require_invite(true);
    let addr = listen(&mut ana.chat).await;
    let link = ana
        .chat
        .invite_link(vec![addr], DEFAULT_INVITE_TTL)
        .unwrap();
    Invite::from_link(&link).unwrap()
}

/// Dirige três chats juntos por `duration`.
async fn settle3(a: &mut Chat, b: &mut Chat, c: &mut Chat, duration: Duration) {
    let _ = tokio::time::timeout(duration, async {
        loop {
            tokio::select! {
                _ = a.next_event() => {},
                _ = b.next_event() => {},
                _ = c.next_event() => {},
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_carries_a_token_and_each_link_has_its_own() {
    let mut ana = person();
    let first =
        Invite::from_link(&ana.chat.invite_link(vec![], DEFAULT_INVITE_TTL).unwrap()).unwrap();
    let second =
        Invite::from_link(&ana.chat.invite_link(vec![], DEFAULT_INVITE_TTL).unwrap()).unwrap();
    let (first, second) = (
        first.token().expect("link v3 sem token"),
        second.token().unwrap(),
    );
    assert_ne!(first, second);
    assert_eq!(ana.chat.invites().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn accepting_a_token_link_opens_the_conversation_and_spends_the_token() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        let invite = ana_with_link(&mut ana).await;
        assert_eq!(ana.chat.invites().len(), 1, "ana_lower={ana_lower}");

        bia.chat.accept(&invite).unwrap();
        connect_until_ready(&mut ana.chat, &mut bia.chat).await;

        assert!(
            ana.chat.invites().is_empty(),
            "uso único (ana_lower={ana_lower})"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spent_link_cannot_be_used_by_someone_else() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        let invite = ana_with_link(&mut ana).await;
        bia.chat.accept(&invite).unwrap();
        connect_until_ready(&mut ana.chat, &mut bia.chat).await;

        let mut caio = person();
        caio.chat.accept(&invite).unwrap();
        settle(&mut ana.chat, &mut caio.chat, SHORT).await;
        assert!(
            !ready_between(&ana, &caio),
            "link já usado (ana_lower={ana_lower})"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_link_is_refused_and_leaves_no_trace_in_the_list() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        let invite = ana_with_link(&mut ana).await;
        let id = ana.chat.invites()[0].id.clone();

        assert!(ana.chat.revoke_invite(&id).unwrap());
        assert!(ana.chat.invites().is_empty());

        bia.chat.accept(&invite).unwrap();
        settle(&mut ana.chat, &mut bia.chat, SHORT).await;
        assert!(
            !ready_between(&ana, &bia),
            "link revogado (ana_lower={ana_lower})"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_needs_a_prefix_that_matches_exactly_one_live_invite() {
    let mut ana = person();
    ana.chat.invite_link(vec![], DEFAULT_INVITE_TTL).unwrap();
    ana.chat.invite_link(vec![], DEFAULT_INVITE_TTL).unwrap();

    assert!(
        matches!(ana.chat.revoke_invite(""), Err(Error::InvalidInvite)),
        "vazio revogaria tudo"
    );
    assert!(!ana.chat.revoke_invite("zzzz").unwrap(), "nenhum bate");
    assert_eq!(ana.chat.invites().len(), 2, "nada foi revogado por engano");
}

/// Peer desconhecido sem token (convite sem token, como o v1 da DeviceList ou um endereço qualquer).
#[tokio::test(flavor = "multi_thread")]
async fn a_stranger_without_a_token_is_refused_when_the_gate_is_on() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        ana.chat.set_require_invite(true);
        let addr = listen(&mut ana.chat).await;

        bia.chat.accept(&ana.chat.invite(vec![addr])).unwrap();
        settle(&mut ana.chat, &mut bia.chat, SHORT).await;
        assert!(
            !ready_between(&ana, &bia),
            "sem token (ana_lower={ana_lower})"
        );
    }
}

/// Padrão da biblioteca: aberta, como antes do P18 (testes e mDNS continuam funcionando).
#[tokio::test(flavor = "multi_thread")]
async fn a_stranger_without_a_token_gets_in_when_the_gate_is_off() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        let addr = listen(&mut ana.chat).await;

        bia.chat.accept(&ana.chat.invite(vec![addr])).unwrap();
        connect_until_ready(&mut ana.chat, &mut bia.chat).await;
    }
}

/// Um token válido na forma (e assinado pela Ana) que ela nunca emitiu não abre nada.
#[tokio::test(flavor = "multi_thread")]
async fn a_signed_token_that_ana_never_issued_is_refused() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        ana.chat.set_require_invite(true);
        let addr = listen(&mut ana.chat).await;
        let link = ana.chat.invite(vec![addr]).to_link_with_token(
            &ana.device.invite_signer(),
            now() + 3600,
            [9; 16],
        );

        bia.chat.accept(&Invite::from_link(&link).unwrap()).unwrap();
        settle(&mut ana.chat, &mut bia.chat, SHORT).await;
        assert!(
            !ready_between(&ana, &bia),
            "token não emitido (ana_lower={ana_lower})"
        );
    }
}

/// O token vence no banco mesmo que o link ainda pareça válido ao leitor (relógio dele atrasado).
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_token_is_refused_and_swept() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        ana.chat.set_require_invite(true);
        let addr = listen(&mut ana.chat).await;
        let link = ana
            .chat
            .invite_link(vec![addr], Duration::from_secs(1))
            .unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        // O leitor vê o link no passado; o token, no banco da Ana, já venceu.
        let invite = Invite::from_link_at(&link, now() - 10).unwrap();
        bia.chat.accept(&invite).unwrap();
        settle(&mut ana.chat, &mut bia.chat, SHORT).await;
        assert!(
            !ready_between(&ana, &bia),
            "token vencido (ana_lower={ana_lower})"
        );
        assert!(ana.chat.invites().is_empty(), "vencido sai da lista");
    }
}

/// Dois convidados com o mesmo link ao mesmo tempo: só um vira conversa.
#[tokio::test(flavor = "multi_thread")]
async fn two_peers_racing_for_one_token_leave_only_one_conversation() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        let mut caio = person();
        let invite = ana_with_link(&mut ana).await;

        bia.chat.accept(&invite).unwrap();
        caio.chat.accept(&invite).unwrap();
        settle3(
            &mut ana.chat,
            &mut bia.chat,
            &mut caio.chat,
            Duration::from_secs(8),
        )
        .await;

        let ready = [&bia, &caio]
            .iter()
            .filter(|p| ana.chat.is_ready(&p.device.peer_id()))
            .count();
        assert_eq!(ready, 1, "uso único (ana_lower={ana_lower})");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn there_is_a_ceiling_on_live_invites() {
    let mut ana = person();
    for n in 0..16 {
        ana.chat
            .invite_link(vec![], DEFAULT_INVITE_TTL)
            .unwrap_or_else(|e| panic!("o convite {n} deveria caber: {e}"));
    }
    assert!(matches!(
        ana.chat.invite_link(vec![], DEFAULT_INVITE_TTL),
        Err(Error::TooManyInvites)
    ));
    assert_eq!(ana.chat.invites().len(), 16);
}

/// Entrada externa: qualquer bit trocado no link v3 (certificado, endereços, token, validade ou
/// assinatura) é recusado; o token está na parte assinada.
#[tokio::test(flavor = "multi_thread")]
async fn flipping_any_bit_of_a_v3_link_breaks_it() {
    let mut ana = person();
    let link = ana.chat.invite_link(vec![], DEFAULT_INVITE_TTL).unwrap();
    assert!(
        Invite::from_link(&link).unwrap().token().is_some(),
        "precisa ser v3"
    );

    let bytes = URL_SAFE_NO_PAD
        .decode(link.strip_prefix("kin://invite/").unwrap())
        .unwrap();
    for i in 0..bytes.len() {
        let mut mutated = bytes.clone();
        mutated[i] ^= 0x01;
        let forged = format!("kin://invite/{}", URL_SAFE_NO_PAD.encode(mutated));
        assert!(
            Invite::from_link(&forged).is_err(),
            "byte {i} trocado foi aceito"
        );
    }
}

/// Compatibilidade: um link v2 (sem token) ainda é lido e, com o portão desligado, abre conversa.
#[tokio::test(flavor = "multi_thread")]
async fn an_old_v2_link_is_still_read_and_works_while_the_gate_is_off() {
    let (mut ana, mut bia) = pair(true);
    let addr = listen(&mut ana.chat).await;
    let link = ana
        .chat
        .invite(vec![addr])
        .to_link(&ana.device.invite_signer(), now() + 3600);

    let invite = Invite::from_link(&link).unwrap();
    assert_eq!(invite.token(), None);
    bia.chat.accept(&invite).unwrap();
    connect_until_ready(&mut ana.chat, &mut bia.chat).await;
}

/// Quem já tem conversa continua falando com a Ana depois de ela reiniciar com o portão ligado.
#[tokio::test(flavor = "multi_thread")]
async fn a_known_contact_keeps_talking_after_the_gate_is_turned_on() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (
        Person::new(dir.path(), "ana"),
        Person::new(dir.path(), "bia"),
    );
    {
        let (mut ana, mut bia) = (a.open(), b.open());
        connect(&mut ana, &mut bia).await;
    }

    let (mut ana, mut bia) = (a.open(), b.open());
    ana.set_require_invite(true);
    let addr = listen(&mut ana).await;
    bia.dial(addr).unwrap();
    bia.send(a.peer_id(), "voltei", None).unwrap();
    let (_, message) = receive_while_driving(&mut ana, &mut bia).await;
    assert_eq!(message.text, "voltei");
}

/// Os tokens vivos ficam no banco: sobrevivem a reiniciar (schema v9).
#[tokio::test(flavor = "multi_thread")]
async fn live_tokens_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let a = Person::new(dir.path(), "ana");
    {
        let mut ana = a.open();
        ana.invite_link(vec![], DEFAULT_INVITE_TTL).unwrap();
        assert_eq!(ana.invites().len(), 1);
    }
    let ana = a.open();
    assert_eq!(ana.invites().len(), 1, "o token sumiu ao reiniciar");
}
