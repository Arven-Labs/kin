//! P18 PR 3 (ADR 0039): com o portão ligado, a admissão também vale por **identidade autenticada**
//! (minha ou aceita por `trust`) e por **quem eu disquei**, sem token. O que não é nada disso segue
//! recusado.
//!
//! Quem cria o grupo MLS é o de menor Peer ID; por isso cada cenário roda nas duas ordens
//! (`ana_lower`), que exercitam o portão em `on_hello` (maior Peer ID) e em `on_key_package` (menor).

mod common;

use common::{connect_until_ready, listen, settle};
use kin_chat::Chat;
use kin_identity::{DeviceKey, IdentityId, StandaloneIdentity};
use kin_transport::{Multiaddr, NodeConfig, Protocol};
use std::time::Duration;

struct P {
    device: DeviceKey,
    chat: Chat,
}

fn config() -> NodeConfig {
    NodeConfig {
        mdns: false,
        ..Default::default()
    }
}

fn open(identity: &StandaloneIdentity, device: &DeviceKey) -> Chat {
    Chat::new(identity, device, config()).unwrap()
}

fn person() -> P {
    let device = DeviceKey::generate();
    let chat = open(&StandaloneIdentity::generate(), &device);
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

fn identity_of(p: &P) -> IdentityId {
    p.chat.certificate().identity().clone()
}

fn ready_between(a: &P, b: &P) -> bool {
    a.chat.is_ready(&b.device.peer_id()) || b.chat.is_ready(&a.device.peer_id())
}

const SHORT: Duration = Duration::from_secs(4);

/// Ana (portão ligado) aceitou a identidade da Bia antes de qualquer contato: a Bia disca a Ana sem
/// link nem token e a conversa abre.
#[tokio::test(flavor = "multi_thread")]
async fn a_trusted_identity_opens_a_conversation_without_a_token() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        ana.chat.set_require_invite(true);
        ana.chat.trust(identity_of(&bia)).unwrap();
        let addr = pinned_addr(&mut ana).await;

        bia.chat.dial(addr).unwrap();
        connect_until_ready(&mut ana.chat, &mut bia.chat).await;
        assert!(ready_between(&ana, &bia), "ana_lower={ana_lower}");
    }
}

/// `untrust` tira a identidade da lista: a Bia volta a ser desconhecida e sem token é recusada.
#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_identity_is_refused_again() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        ana.chat.set_require_invite(true);
        ana.chat.trust(identity_of(&bia)).unwrap();
        ana.chat.untrust(&identity_of(&bia)).unwrap();
        let addr = pinned_addr(&mut ana).await;

        bia.chat.dial(addr).unwrap();
        settle(&mut ana.chat, &mut bia.chat, SHORT).await;
        assert!(!ready_between(&ana, &bia), "ana_lower={ana_lower}");
    }
}

/// Confiar na identidade do Caio não admite a Bia.
#[tokio::test(flavor = "multi_thread")]
async fn trusting_someone_else_does_not_admit_a_stranger() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        let caio = person();
        ana.chat.set_require_invite(true);
        ana.chat.trust(identity_of(&caio)).unwrap();
        let addr = pinned_addr(&mut ana).await;

        bia.chat.dial(addr).unwrap();
        settle(&mut ana.chat, &mut bia.chat, SHORT).await;
        assert!(!ready_between(&ana, &bia), "ana_lower={ana_lower}");
    }
}

/// Segundo device da **mesma** identidade (pareamento e multi-device): passa sem token, sem a Ana
/// conhecer o Peer ID dele antes.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_device_of_my_own_identity_passes_without_a_token() {
    for first_lower in [true, false] {
        let identity = StandaloneIdentity::generate();
        let (d1, d2) = loop {
            let (d1, d2) = (DeviceKey::generate(), DeviceKey::generate());
            if (d1.peer_id() < d2.peer_id()) == first_lower {
                break (d1, d2);
            }
        };
        let copy = StandaloneIdentity::from_bytes(&identity.to_bytes().unwrap()).unwrap();
        let mut first = open(&identity, &d1);
        let mut second = open(&copy, &d2);
        first.set_require_invite(true);
        let addr = listen(&mut first).await.with(Protocol::P2p(d1.peer_id()));

        second.dial(addr).unwrap();
        connect_until_ready(&mut first, &mut second).await;
        assert!(
            first.is_ready(&d2.peer_id()) || second.is_ready(&d1.peer_id()),
            "first_lower={first_lower}"
        );
    }
}

/// O endereço de `p` com o Peer ID fixado (`/p2p/<id>`), como a CLI recebe em `--dial`.
async fn pinned_addr(p: &mut P) -> Multiaddr {
    let addr = listen(&mut p.chat).await;
    addr.with(Protocol::P2p(p.device.peer_id()))
}

/// Quem **eu** disco, com o Peer ID fixado no endereço, é admitido (discar é um ato meu): a Ana, com o
/// portão ligado, disca a Bia.
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_i_dialed_by_its_peer_id_is_admitted_without_a_token() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        ana.chat.set_require_invite(true);
        let addr = pinned_addr(&mut bia).await;

        ana.chat.dial(addr).unwrap();
        connect_until_ready(&mut ana.chat, &mut bia.chat).await;
        assert!(ready_between(&ana, &bia), "ana_lower={ana_lower}");
    }
}

/// Endereço **sem** `/p2p/<id>`: não há Peer ID para admitir, então não vale como autorização.
#[tokio::test(flavor = "multi_thread")]
async fn dialing_an_address_without_a_peer_id_admits_nobody() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        ana.chat.set_require_invite(true);
        let addr = listen(&mut bia.chat).await;

        ana.chat.dial(addr).unwrap();
        settle(&mut ana.chat, &mut bia.chat, SHORT).await;
        assert!(!ready_between(&ana, &bia), "ana_lower={ana_lower}");
    }
}

/// Discar não abre a porta no sentido contrário: quem me disca, desconhecido e sem token, segue recusado
/// mesmo que eu tenha discado outro peer.
#[tokio::test(flavor = "multi_thread")]
async fn dialing_someone_does_not_admit_a_different_stranger() {
    for ana_lower in [true, false] {
        let (mut ana, mut bia) = pair(ana_lower);
        let mut caio = person();
        ana.chat.set_require_invite(true);
        let caio_addr = pinned_addr(&mut caio).await;
        ana.chat.dial(caio_addr).unwrap();
        let addr = pinned_addr(&mut ana).await;

        bia.chat.dial(addr).unwrap();
        settle(&mut ana.chat, &mut bia.chat, SHORT).await;
        assert!(!ready_between(&ana, &bia), "ana_lower={ana_lower}");
    }
}
