//! Fase 2.4: convite por link com a identidade do convidante fixada.

mod common;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::{
    WAIT, connect_until_ready, listen, receive_while_driving, settle, start_relay, wait_for,
};
use kin_chat::{Chat, ChatEvent, DEFAULT_INVITE_TTL, Error, Invite, MAX_INVITE_TTL};
use kin_identity::{DeviceKey, IdentityProvider, StandaloneIdentity};
use kin_transport::{Multiaddr, NodeConfig};
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

/// Instante fixo dos testes de validade (segundos Unix).
const NOW: u64 = 1_800_000_000;
const DAY: u64 = 24 * 60 * 60;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Link da Ana, assinado pelo device dela, válido até `expires_at`.
fn signed_link(ana: &Person, expires_at: u64) -> String {
    ana.chat.invite(vec![]).to_link(&ana.device, expires_at)
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

    let link = invite.to_link(&ana.device, NOW + DAY);
    assert!(link.starts_with("kin://invite/"));
    assert!(link.len() < 1000, "cabe num QR: {} caracteres", link.len());
    let read = Invite::from_link_at(&link, NOW).unwrap();
    assert_eq!(read, invite);
    assert_eq!(read.identity(), &ana.identity.id());
    assert_eq!(read.peer_id(), ana.device.peer_id());
    assert_eq!(read.addrs(), [addr]);
    // Espaços em volta (colado de uma mensagem) não atrapalham.
    assert!(Invite::from_link_at(&format!("  {link}\n"), NOW).is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn damaged_links_are_rejected() {
    let ana = person();
    let link = signed_link(&ana, NOW + DAY);
    let bytes = bytes_of(&link);

    for bad in [
        String::new(),
        "kin://invite/".into(),
        "https://example.com/invite".into(),
        "kin://invite/@@@@".into(),
        link_of(&bytes[..bytes.len() - 1]),           // truncado
        link_of(&[bytes.clone(), vec![0]].concat()),  // sobra
        link_of(&[&[3u8][..], &bytes[1..]].concat()), // versão desconhecida
    ] {
        assert!(
            matches!(Invite::from_link_at(&bad, NOW), Err(Error::InvalidInvite)),
            "deveria recusar {bad:?}"
        );
    }

    // Qualquer byte alterado, em qualquer ponto (certificado, endereços, validade ou assinatura),
    // e qualquer truncamento quebram o link.
    for i in 1..bytes.len() {
        let mut tampered = bytes.clone();
        tampered[i] ^= 0x01;
        assert!(
            Invite::from_link_at(&link_of(&tampered), NOW).is_err(),
            "byte {i} alterado passou"
        );
        assert!(
            Invite::from_link_at(&link_of(&bytes[..i]), NOW).is_err(),
            "truncado em {i} passou"
        );
    }
}

/// A validade vale ao segundo: no instante `expires_at` o convite já venceu (P18).
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_link_is_refused_and_one_second_before_is_accepted() {
    let ana = person();
    let link = signed_link(&ana, NOW + DAY);

    assert!(Invite::from_link_at(&link, NOW + DAY - 1).is_ok());
    for late in [NOW + DAY, NOW + DAY + 1, NOW + 30 * DAY] {
        assert!(
            matches!(Invite::from_link_at(&link, late), Err(Error::ExpiredInvite)),
            "deveria estar vencido em {late}"
        );
    }
}

/// Quem tem o link só estende a validade se refizer a assinatura, e não tem a chave do device.
#[tokio::test(flavor = "multi_thread")]
async fn extending_the_expiry_without_the_device_key_breaks_the_link() {
    let ana = person();
    let mut bytes = bytes_of(&signed_link(&ana, NOW + DAY));
    // A validade é o u64 logo antes da assinatura: len16 (2) + 64 bytes de Ed25519.
    let at = bytes.len() - 2 - 64 - 8;
    let old = u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap());
    assert_eq!(old, NOW + DAY, "layout: validade antes da assinatura");
    bytes[at..at + 8].copy_from_slice(&(NOW + 3 * DAY).to_be_bytes());

    assert!(matches!(
        Invite::from_link_at(&link_of(&bytes), NOW),
        Err(Error::InvalidInvite)
    ));
}

fn addr(text: &str) -> Multiaddr {
    text.parse().unwrap()
}

/// Troca a assinatura do link (os últimos 2 + 64 bytes) por `signature` sobre `message`.
fn resigned(ana: &Person, link: &str, message: impl Fn(&[u8]) -> Vec<u8>) -> String {
    let bytes = bytes_of(link);
    let body = &bytes[..bytes.len() - 2 - 64];
    let signature = ana.device.keypair().sign(&message(body)).unwrap();
    let mut out = body.to_vec();
    out.extend_from_slice(&(signature.len() as u16).to_be_bytes());
    out.extend_from_slice(&signature);
    link_of(&out)
}

/// O domínio `kin/invite/v2` faz parte do que se assina: sem ele, ou com outro, o link é recusado.
#[tokio::test(flavor = "multi_thread")]
async fn the_signature_must_cover_the_invite_domain() {
    let ana = person();
    let link = signed_link(&ana, NOW + DAY);

    // Controle: refazer a assinatura com o domínio certo mantém o link válido.
    let ok = resigned(&ana, &link, |body| [&b"kin/invite/v2"[..], body].concat());
    assert!(Invite::from_link_at(&ok, NOW).is_ok());

    for (name, domain) in [
        ("sem domínio", &b""[..]),
        ("domínio v1", &b"kin/invite/v1"[..]),
        ("outro protocolo", &b"kin/pair/v1"[..]),
    ] {
        let bad = resigned(&ana, &link, |body| [domain, body].concat());
        assert!(
            matches!(Invite::from_link_at(&bad, NOW), Err(Error::InvalidInvite)),
            "assinatura {name} passou"
        );
    }
}

/// Com endereços no link, qualquer byte alterado e qualquer truncamento ainda quebram o link, e a lista
/// de endereços de um link não serve com a assinatura de outro (redirecionar a discagem).
#[tokio::test(flavor = "multi_thread")]
async fn the_addresses_are_covered_by_the_signature() {
    let ana = person();
    let one = ana
        .chat
        .invite(vec![
            addr("/ip4/203.0.113.7/tcp/4001"),
            addr("/ip4/198.51.100.9/udp/4001/quic-v1"),
        ])
        .to_link(&ana.device, NOW + DAY);
    let other = ana
        .chat
        .invite(vec![addr("/ip4/192.0.2.66/tcp/4001")])
        .to_link(&ana.device, NOW + DAY);
    assert!(Invite::from_link_at(&one, NOW).is_ok());
    let bytes = bytes_of(&one);

    for i in 1..bytes.len() {
        let mut tampered = bytes.clone();
        tampered[i] ^= 0x01;
        assert!(
            Invite::from_link_at(&link_of(&tampered), NOW).is_err(),
            "byte {i} alterado passou"
        );
        assert!(
            Invite::from_link_at(&link_of(&bytes[..i]), NOW).is_err(),
            "truncado em {i} passou"
        );
    }

    // Endereços de `other` com a assinatura de `one` (mesmo device, mesma validade).
    let (b_one, b_other) = (bytes_of(&one), bytes_of(&other));
    let tail = &b_one[b_one.len() - 2 - 64..];
    let spliced = [&b_other[..b_other.len() - 2 - 64], tail].concat();
    assert!(matches!(
        Invite::from_link_at(&link_of(&spliced), NOW),
        Err(Error::InvalidInvite)
    ));
}

/// `Chat::invite_link` nunca emite um link que o leitor recusa só pelo prazo pedido.
#[tokio::test(flavor = "multi_thread")]
async fn invite_link_clamps_the_ttl_to_what_a_reader_accepts() {
    let ana = person();

    for ttl in [MAX_INVITE_TTL, Duration::from_secs(u64::MAX), Duration::MAX] {
        let link = ana.chat.invite_link(vec![], ttl);
        assert!(
            Invite::from_link(&link).is_ok(),
            "ttl {ttl:?} gerou link ilegível"
        );
    }
    // Prazo zero: o link já nasce vencido (o chamador pediu isso), nunca inválido nem aceito.
    let link = ana.chat.invite_link(vec![], Duration::ZERO);
    assert!(matches!(
        Invite::from_link_at(&link, now() + 1),
        Err(Error::ExpiredInvite)
    ));
}

/// A Mallory assina o convite da Ana com a chave dela: o certificado é da Ana, a assinatura não.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_signed_by_another_device_is_refused() {
    let (ana, mallory) = (person(), person());
    let link = ana.chat.invite(vec![]).to_link(&mallory.device, NOW + DAY);

    assert!(matches!(
        Invite::from_link_at(&link, NOW),
        Err(Error::InvalidInvite)
    ));
}

/// Validade além de 7 dias no futuro (relógio errado ou forjado) é recusada; 7 dias exatos passam.
#[tokio::test(flavor = "multi_thread")]
async fn a_validity_beyond_the_maximum_horizon_is_refused() {
    let ana = person();
    let max = MAX_INVITE_TTL.as_secs();

    assert!(Invite::from_link_at(&signed_link(&ana, NOW + max), NOW).is_ok());
    assert!(matches!(
        Invite::from_link_at(&signed_link(&ana, NOW + max + 1), NOW),
        Err(Error::InvalidInvite)
    ));
    assert!(matches!(
        Invite::from_link_at(&signed_link(&ana, u64::MAX), NOW),
        Err(Error::InvalidInvite)
    ));
}

/// O formato v1 (sem validade) não é mais aceito: era o problema do P18.
#[tokio::test(flavor = "multi_thread")]
async fn the_old_v1_link_without_expiry_is_refused() {
    let ana = person();
    let mut bytes = bytes_of(&signed_link(&ana, NOW + DAY));
    bytes[0] = 1;

    assert!(matches!(
        Invite::from_link_at(&link_of(&bytes), NOW),
        Err(Error::InvalidInvite)
    ));
}

/// `Chat::invite_link` assina com o próprio device e usa o prazo pedido a partir de agora.
#[tokio::test(flavor = "multi_thread")]
async fn chat_invite_link_is_signed_by_the_own_device_and_lasts_the_requested_ttl() {
    let ana = person();
    let before = now();
    let link = ana.chat.invite_link(vec![], DEFAULT_INVITE_TTL);
    let after = now();

    let invite = Invite::from_link(&link).unwrap();
    assert_eq!(invite.peer_id(), ana.device.peer_id());
    let ttl = DEFAULT_INVITE_TTL.as_secs();
    assert!(Invite::from_link_at(&link, before + ttl - 1).is_ok());
    assert!(matches!(
        Invite::from_link_at(&link, after + ttl),
        Err(Error::ExpiredInvite)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn accepting_an_invite_connects_and_pins_the_identity() {
    let (mut ana, mut bia) = (person(), person());
    let addr = listen(&mut ana.chat).await;
    let link = ana.chat.invite_link(vec![addr], DEFAULT_INVITE_TTL);
    let invite = Invite::from_link(&link).unwrap();

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
    let mallory_device = DeviceKey::generate();
    let addr = listen(&mut ana.chat).await;

    let forged_cert = mallory
        .authorize_device(
            &DeviceKey::generate().public_key(),
            &ana.device.public_key(), // reivindica o Peer ID da Ana
            &DeviceKey::generate().nostr_public_key(),
            1,
        )
        .unwrap();
    let forged = Invite::new(forged_cert, vec![addr]);
    // Como link, a falsificação já morre na assinatura: a Mallory não tem a chave de rede da Ana.
    assert!(matches!(
        Invite::from_link_at(&forged.to_link(&mallory_device, NOW + DAY), NOW),
        Err(Error::InvalidInvite)
    ));
    // Já com o objeto `Invite` na mão (sem link), `accept` ainda exige a identidade no handshake.
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
