//! Fase 2.4: convite por link com a identidade do convidante fixada.

mod common;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::{
    WAIT, connect_until_ready, listen, receive_while_driving, settle, start_relay, wait_for,
};
use kin_chat::{Chat, ChatEvent, Error, Invite};
use kin_identity::{DeviceKey, IdentityProvider, StandaloneIdentity};
use kin_transport::NodeConfig;
use std::time::Duration;
use tokio::time::timeout;

struct Person {
    identity: StandaloneIdentity,
    device: DeviceKey,
    chat: Chat,
}

fn person_with(config: NodeConfig) -> Person {
    let identity = StandaloneIdentity::generate();
    let device = DeviceKey::generate();
    let chat = Chat::new(&identity, &device, config).unwrap();
    Person {
        identity,
        device,
        chat,
    }
}

fn person() -> Person {
    person_with(NodeConfig {
        mdns: false,
        ..Default::default()
    })
}

fn bytes_of(link: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD
        .decode(link.strip_prefix("kin://invite/").unwrap())
        .unwrap()
}

fn link_of(bytes: &[u8]) -> String {
    format!("kin://invite/{}", URL_SAFE_NO_PAD.encode(bytes))
}

#[tokio::test(flavor = "multi_thread")]
async fn link_roundtrips_and_carries_identity_peer_and_addresses() {
    let mut ana = person();
    let addr = listen(&mut ana.chat).await;
    let invite = ana.chat.invite(vec![addr.clone()]);

    let link = invite.to_link();
    assert!(link.starts_with("kin://invite/"));
    assert!(link.len() < 1000, "cabe num QR: {} caracteres", link.len());
    let read = Invite::from_link(&link).unwrap();
    assert_eq!(read, invite);
    assert_eq!(read.identity(), &ana.identity.id());
    assert_eq!(read.peer_id(), ana.device.peer_id());
    assert_eq!(read.addrs(), [addr]);
    // Espaços em volta (colado de uma mensagem) não atrapalham.
    assert!(Invite::from_link(&format!("  {link}\n")).is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn damaged_links_are_rejected() {
    let ana = person();
    let link = ana.chat.invite(vec![]).to_link();
    let bytes = bytes_of(&link);

    for bad in [
        String::new(),
        "kin://invite/".into(),
        "https://example.com/invite".into(),
        "kin://invite/@@@@".into(),
        link_of(&bytes[..bytes.len() - 1]),           // truncado
        link_of(&[bytes.clone(), vec![0]].concat()),  // sobra
        link_of(&[&[2u8][..], &bytes[1..]].concat()), // versão desconhecida
    ] {
        assert!(
            matches!(Invite::from_link(&bad), Err(Error::InvalidInvite)),
            "deveria recusar {bad:?}"
        );
    }

    // Qualquer byte alterado dentro do certificado quebra a assinatura (ou o formato).
    for i in 4..bytes.len().min(150) {
        let mut tampered = bytes.clone();
        tampered[i] ^= 0x01;
        assert!(
            Invite::from_link(&link_of(&tampered)).is_err(),
            "byte {i} alterado passou"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn accepting_an_invite_connects_and_pins_the_identity() {
    let (mut ana, mut bia) = (person(), person());
    let addr = listen(&mut ana.chat).await;
    let invite = Invite::from_link(&ana.chat.invite(vec![addr]).to_link()).unwrap();

    bia.chat.accept(&invite).unwrap();
    connect_until_ready(&mut ana.chat, &mut bia.chat).await;

    let sent = bia
        .chat
        .send(ana.device.peer_id(), "oi, vim pelo link", None)
        .unwrap();
    let (sender, message) = receive_while_driving(&mut ana.chat, &mut bia.chat).await;
    assert_eq!((sender, message.id), (bia.identity.id(), sent));
    let sent = ana
        .chat
        .send(bia.device.peer_id(), "bem-vinda", None)
        .unwrap();
    let (sender, message) = receive_while_driving(&mut bia.chat, &mut ana.chat).await;
    assert_eq!((sender, message.id), (ana.identity.id(), sent));
}

/// O atacante forja um convite válido (assinado por ele) que reivindica o Peer ID da Ana. Quem aceita
/// disca a Ana de verdade, mas a identidade que ela apresenta não é a do convite: nada de conversa.
#[tokio::test(flavor = "multi_thread")]
async fn invite_claiming_another_identity_for_the_same_peer_is_refused() {
    let (mut ana, mut bia) = (person(), person());
    let mallory = StandaloneIdentity::generate();
    let addr = listen(&mut ana.chat).await;

    let forged_cert = mallory
        .authorize_device(
            &DeviceKey::generate().public_key(),
            &ana.device.public_key(), // reivindica o Peer ID da Ana
            &DeviceKey::generate().nostr_public_key(),
            1,
        )
        .unwrap();
    let forged = Invite::from_link(&Invite::new(forged_cert, vec![addr]).to_link()).unwrap();
    assert_eq!(forged.peer_id(), ana.device.peer_id());
    assert_ne!(forged.identity(), &ana.identity.id());

    bia.chat.accept(&forged).unwrap();
    let (seen_ana, seen_bia) = settle(&mut ana.chat, &mut bia.chat, Duration::from_secs(4)).await;

    let dropped = |events: &[ChatEvent]| {
        events.iter().any(
            |e| matches!(e, ChatEvent::Dropped { reason, .. } if reason.contains("identidade")),
        )
    };
    assert!(
        dropped(&seen_ana) || dropped(&seen_bia),
        "deveria descartar por identidade: {seen_ana:?} / {seen_bia:?}"
    );
    // Quem fixou a identidade (Bia) nunca fica com conversa. A Ana, que não exigiu nada, pode ter
    // criado a dela se coube a ela convidar no MLS: um estado de um lado só, que a Bia nunca usa.
    assert!(!bia.chat.is_ready(&ana.device.peer_id()));
}

#[tokio::test(flavor = "multi_thread")]
async fn accepting_your_own_invite_is_an_error() {
    let mut ana = person();
    let own = ana.chat.invite(vec![]);
    assert!(matches!(ana.chat.accept(&own), Err(Error::InvalidInvite)));
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_with_a_relay_circuit_address_works_without_a_direct_path() {
    let relay = start_relay().await;
    let mut ana = person_with(NodeConfig {
        mdns: false,
        relays: vec![relay.clone()],
        ..Default::default()
    });
    let mut bia = person();
    timeout(
        WAIT,
        wait_for(&mut ana.chat, |e| {
            matches!(e, ChatEvent::RelayReserved { .. }).then_some(())
        }),
    )
    .await
    .expect("sem reserva no relay");

    // Ana não escuta em lugar nenhum: o único caminho é o circuito.
    let invite = ana.chat.invite(vec![ana.chat.circuit_address(&relay)]);
    bia.chat.accept(&invite).unwrap();
    connect_until_ready(&mut ana.chat, &mut bia.chat).await;
    assert!(ana.chat.is_ready(&bia.device.peer_id()));
}
