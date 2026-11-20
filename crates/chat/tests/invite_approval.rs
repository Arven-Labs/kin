//! P18 (d, ADR 0039): com o portão e `set_require_approval` ligados, quem chega com um token vivo vira
//! **pedido pendente**: o convidante vê a identidade e decide (`approve`/`deny`). Sem aprovação não nasce
//! conversa e o token não é gasto.
//!
//! Quem cria o grupo MLS é o de menor Peer ID; por isso cada cenário roda nas duas ordens (`ana_lower`):
//! com a Ana de menor ID o `KeyPackage` da Bia fica guardado até o `approve`; com a Ana de maior ID é ela
//! quem precisa mandar o próprio `KeyPackage` ao aprovar.

mod common;

use common::{listen, settle};
use kin_chat::{Chat, ChatEvent, DEFAULT_INVITE_TTL, Invite};
use kin_identity::{DeviceKey, IdentityId, IdentityProvider, StandaloneIdentity};
use kin_transport::{NodeConfig, PeerId};
use std::time::Duration;

struct P {
    identity: StandaloneIdentity,
    device: DeviceKey,
    chat: Chat,
}

impl P {
    fn peer(&self) -> PeerId {
        self.device.peer_id()
    }

    fn id(&self) -> IdentityId {
        self.identity.id()
    }
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
    P {
        identity,
        device,
        chat,
    }
}

/// Duas pessoas em que a Ana tem o menor Peer ID (`ana_lower`) ou o maior.
fn pair(ana_lower: bool) -> (P, P) {
    loop {
        let (ana, bia) = (person(), person());
        if (ana.peer() < bia.peer()) == ana_lower {
            return (ana, bia);
        }
    }
}

const SHORT: Duration = Duration::from_secs(4);
const LONG: Duration = Duration::from_secs(10);

fn ready_between(a: &P, b: &P) -> bool {
    a.chat.is_ready(&b.peer()) || b.chat.is_ready(&a.peer())
}

fn requests(seen: &[ChatEvent], peer: PeerId) -> Vec<IdentityId> {
    seen.iter()
        .filter_map(|e| match e {
            ChatEvent::AdmissionRequested { peer: p, identity } if *p == peer => {
                Some(identity.clone())
            }
            _ => None,
        })
        .collect()
}

fn not_admitted(seen: &[ChatEvent], peer: PeerId) -> usize {
    seen.iter()
        .filter(|e| matches!(e, ChatEvent::NotAdmitted { peer: p } if *p == peer))
        .count()
}

/// A Ana (portão e aprovação ligados) escuta e gera um link v3.
async fn ana_with_link(ana: &mut P, approval: bool) -> Invite {
    ana.chat.set_require_invite(true);
    ana.chat.set_require_approval(approval);
    let addr = listen(&mut ana.chat).await;
    let link = ana
        .chat
        .invite_link(vec![addr], DEFAULT_INVITE_TTL)
        .unwrap();
    Invite::from_link(&link).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_holder_waits_for_approval_and_no_conversation_opens() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        let invite = ana_with_link(&mut ana, true).await;

        bia.chat.accept(&invite).unwrap();
        let (seen_ana, seen_bia) = settle(&mut ana.chat, &mut bia.chat, SHORT).await;

        // Um pedido, com a identidade da Bia (que veio no `Hello`), e nenhum `NotAdmitted` por cima.
        assert_eq!(
            requests(&seen_ana, bia.peer()),
            vec![bia.id()],
            "ana_lower={ana_lower}: {seen_ana:?}"
        );
        assert_eq!(
            not_admitted(&seen_ana, bia.peer()),
            0,
            "ana_lower={ana_lower}"
        );
        assert!(requests(&seen_bia, ana.peer()).is_empty());
        assert!(
            !ready_between(&ana, &bia),
            "sem aprovar (ana_lower={ana_lower})"
        );
        // O pedido pendente não gasta o token.
        assert_eq!(ana.chat.invites().len(), 1, "ana_lower={ana_lower}");
        assert_eq!(
            ana.chat.pending_approvals(),
            vec![(bia.peer(), bia.id())],
            "ana_lower={ana_lower}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn approving_opens_the_conversation_and_spends_the_token() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        let invite = ana_with_link(&mut ana, true).await;
        bia.chat.accept(&invite).unwrap();
        settle(&mut ana.chat, &mut bia.chat, SHORT).await;
        assert!(!ready_between(&ana, &bia));

        assert!(
            ana.chat.approve(bia.peer()).unwrap(),
            "ana_lower={ana_lower}"
        );
        settle(&mut ana.chat, &mut bia.chat, LONG).await;

        assert!(
            ana.chat.is_ready(&bia.peer()) && bia.chat.is_ready(&ana.peer()),
            "aprovado deveria abrir nos dois lados (ana_lower={ana_lower})"
        );
        assert!(
            ana.chat.invites().is_empty(),
            "uso único (ana_lower={ana_lower})"
        );
        assert!(ana.chat.pending_approvals().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn denying_leaves_no_conversation_keeps_the_token_and_does_not_ask_again() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        let invite = ana_with_link(&mut ana, true).await;
        bia.chat.accept(&invite).unwrap();
        settle(&mut ana.chat, &mut bia.chat, SHORT).await;

        assert!(ana.chat.deny(bia.peer()), "ana_lower={ana_lower}");
        assert!(ana.chat.pending_approvals().is_empty());
        let (seen_ana, _) = settle(&mut ana.chat, &mut bia.chat, SHORT).await;

        assert!(!ready_between(&ana, &bia), "negado (ana_lower={ana_lower})");
        // Negar não queima o link: ele segue vivo para o convidado de verdade.
        assert_eq!(ana.chat.invites().len(), 1, "ana_lower={ana_lower}");
        // Nenhum pedido novo na mesma conexão.
        assert!(
            requests(&seen_ana, bia.peer()).is_empty(),
            "ana_lower={ana_lower}: {seen_ana:?}"
        );
    }
}

/// Guarda: sem `set_require_approval`, o portão de convite segue como antes (ADR 0039).
#[tokio::test(flavor = "multi_thread")]
async fn without_approval_a_token_holder_opens_the_conversation_as_before() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        let invite = ana_with_link(&mut ana, false).await;

        bia.chat.accept(&invite).unwrap();
        let (seen_ana, _) = settle(&mut ana.chat, &mut bia.chat, LONG).await;

        assert!(
            ana.chat.is_ready(&bia.peer()) && bia.chat.is_ready(&ana.peer()),
            "ana_lower={ana_lower}"
        );
        assert!(requests(&seen_ana, bia.peer()).is_empty());
        assert!(ana.chat.invites().is_empty());
    }
}

/// Guarda: sem token vivo (link revogado), não há o que aprovar; vale o `NotAdmitted` de sempre.
#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_link_is_not_a_pending_request() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        let invite = ana_with_link(&mut ana, true).await;
        let id = ana.chat.invites()[0].id.clone();
        assert!(ana.chat.revoke_invite(&id).unwrap());

        bia.chat.accept(&invite).unwrap();
        let (seen_ana, _) = settle(&mut ana.chat, &mut bia.chat, SHORT).await;

        assert!(
            requests(&seen_ana, bia.peer()).is_empty(),
            "ana_lower={ana_lower}"
        );
        assert_eq!(
            not_admitted(&seen_ana, bia.peer()),
            1,
            "ana_lower={ana_lower}"
        );
        assert!(!ready_between(&ana, &bia));
    }
}

/// Guarda: aprovar quem não pediu não faz nada (e não adianta o pedido de outro).
#[tokio::test(flavor = "multi_thread")]
async fn approving_or_denying_an_unknown_peer_changes_nothing() {
    let mut ana = person();
    ana.chat.set_require_invite(true);
    ana.chat.set_require_approval(true);
    let stranger = PeerId::random();

    assert!(!ana.chat.approve(stranger).unwrap());
    assert!(!ana.chat.deny(stranger));
    assert!(ana.chat.pending_approvals().is_empty());
}
