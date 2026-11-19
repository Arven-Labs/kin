//! Fase 1.7: cenários de vida real entre dois nós (mDNS, queda, reconexão, peer offline).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{
    WAIT, connect_until_ready, connect_until_ready_within, drive, listen, next_received,
    receive_while_driving, settle, wait_for,
};
use kin_chat::{Chat, ChatEvent};
use kin_identity::{DeviceKey, StandaloneIdentity};
use kin_transport::{Multiaddr, NodeConfig, PeerId};
use tokio::time::timeout;

/// Uma pessoa com identidade, device e banco próprios; pode "reiniciar" reabrindo o mesmo banco.
struct Person {
    identity: StandaloneIdentity,
    device: DeviceKey,
    db: PathBuf,
}

impl Person {
    fn new(dir: &Path, name: &str) -> Self {
        Self {
            identity: StandaloneIdentity::generate(),
            device: DeviceKey::generate(),
            db: dir.join(format!("{name}.db")),
        }
    }

    fn peer_id(&self) -> PeerId {
        self.device.peer_id()
    }

    fn open(&self) -> Chat {
        Chat::open(
            &self.identity,
            &self.device,
            NodeConfig {
                mdns: false,
                ..Default::default()
            },
            &self.db,
        )
        .unwrap()
    }
}

/// `a` escuta, `b` disca, e a conversa fica pronta nos dois lados.
async fn connect(a: &mut Chat, b: &mut Chat) -> Multiaddr {
    let addr = listen(a).await;
    b.dial(addr.clone()).unwrap();
    connect_until_ready(a, b).await;
    addr
}

/// Uma mensagem em cada sentido, para provar que o canal cifrado funciona nos dois lados.
async fn exchange(a: &mut Chat, b: &mut Chat, b_peer: PeerId, a_peer: PeerId, tag: &str) {
    let sent = a.send(b_peer, format!("{tag}: a→b"), None).unwrap();
    let (_, message) = receive_while_driving(b, a).await;
    assert_eq!((message.id, message.text), (sent, format!("{tag}: a→b")));

    let sent = b.send(a_peer, format!("{tag}: b→a"), None).unwrap();
    let (_, message) = receive_while_driving(a, b).await;
    assert_eq!((message.id, message.text), (sent, format!("{tag}: b→a")));
}

fn has_dropped(events: &[ChatEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, ChatEvent::Dropped { .. }))
}

/// 1. Duas instâncias se acham sozinhas na LAN e conversam, sem ninguém discar.
///
/// Tem de ser o ÚNICO teste de mDNS deste binário e rodar uma rodada só: dois testes de mDNS em
/// paralelo no mesmo host descobrem os chats um do outro, e rodadas seguidas no mesmo processo
/// discam os peers mortos das rodadas anteriores (o mDNS os anuncia até o TTL expirar).
#[tokio::test(flavor = "multi_thread")]
async fn chats_find_each_other_via_mdns_and_talk() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let mk = |p: &Person| Chat::open(&p.identity, &p.device, NodeConfig::default(), &p.db).unwrap();
    let (mut a, mut b) = (mk(&pa), mk(&pb));
    a.listen().unwrap();
    b.listen().unwrap();

    // Mesmo prazo do teste de mDNS do transporte (lan.rs).
    connect_until_ready_within(&mut a, &mut b, Duration::from_secs(30)).await;
    exchange(&mut a, &mut b, pb.peer_id(), pa.peer_id(), "mdns").await;
}

/// 2. Com a outbox (3.2), enviar a um peer que caiu deixa a mensagem na fila (`Queued`), nunca
/// `Delivered`, e não a perde.
#[tokio::test(flavor = "multi_thread")]
async fn send_to_offline_peer_is_queued_not_lost() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    connect(&mut a, &mut b).await;

    drop(b);
    // Ainda há conversa (ela vem do banco), então o envio é aceito e só falha na rede.
    let sent = a.send(pb.peer_id(), "ninguém ouve", None).unwrap();
    let queued = timeout(
        WAIT,
        wait_for(&mut a, |e| match e {
            ChatEvent::Delivered { .. } => panic!("entregue a um peer offline"),
            ChatEvent::SendFailed { .. } => panic!("a mensagem não pode ser dada como perdida"),
            ChatEvent::Queued { message, .. } => Some(message),
            _ => None,
        }),
    )
    .await
    .expect("a mensagem não foi para a fila");
    assert_eq!(queued, sent);
    assert_eq!(a.queued(&pb.peer_id()), 1);
}

/// 3. O peer que discou cai e volta (outra porta, mesmo banco); quem ficou rodando segue de onde parou.
#[tokio::test(flavor = "multi_thread")]
async fn dialer_restarts_and_conversation_resumes_without_new_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    let addr = connect(&mut a, &mut b).await;
    exchange(&mut a, &mut b, pb.peer_id(), pa.peer_id(), "antes").await;

    drop(b);
    timeout(
        WAIT,
        wait_for(&mut a, |e| {
            matches!(e, ChatEvent::PeerDisconnected(p) if p == pb.peer_id()).then_some(())
        }),
    )
    .await
    .expect("a não percebeu a queda");

    let mut b = pb.open();
    assert!(b.is_ready(&pa.peer_id()), "a conversa deveria vir do banco");
    b.dial(addr).unwrap();
    let (seen_a, seen_b) = connect_until_ready_within(&mut a, &mut b, WAIT).await;
    assert!(
        !has_dropped(&seen_a) && !has_dropped(&seen_b),
        "não deveria haver novo handshake: {seen_a:?} / {seen_b:?}"
    );
    exchange(&mut a, &mut b, pb.peer_id(), pa.peer_id(), "depois").await;
}

/// 4. Quem escuta cai e volta numa porta nova; o peer que ficou rodando redisca.
#[tokio::test(flavor = "multi_thread")]
async fn listener_restarts_and_dialer_redials() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    connect(&mut a, &mut b).await;
    exchange(&mut a, &mut b, pb.peer_id(), pa.peer_id(), "antes").await;

    drop(a);
    timeout(
        WAIT,
        wait_for(&mut b, |e| {
            matches!(e, ChatEvent::PeerDisconnected(p) if p == pa.peer_id()).then_some(())
        }),
    )
    .await
    .expect("b não percebeu a queda");

    let mut a = pa.open();
    let addr = listen(&mut a).await;
    b.dial(addr).unwrap();
    let (seen_a, seen_b) = connect_until_ready_within(&mut a, &mut b, WAIT).await;
    assert!(
        !has_dropped(&seen_a) && !has_dropped(&seen_b),
        "não deveria haver novo handshake: {seen_a:?} / {seen_b:?}"
    );
    exchange(&mut a, &mut b, pb.peer_id(), pa.peer_id(), "depois").await;
}

/// 5. Mensagens em voo quando o peer cai não são reentregues (não há outbox); só as novas chegam,
/// cada uma uma vez, e a conversa continua decifrando apesar do salto na sequência.
#[tokio::test(flavor = "multi_thread")]
async fn in_flight_messages_are_not_replayed_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let (mut a, mut b) = (pa.open(), pb.open());
    let addr = connect(&mut a, &mut b).await;

    let first = a.send(pb.peer_id(), "m1", None).unwrap();
    a.send(pb.peer_id(), "m2", None).unwrap();
    a.send(pb.peer_id(), "m3", None).unwrap();
    // Deixa só a primeira chegar e derruba b com as outras possivelmente em voo.
    let (_, message) = receive_while_driving(&mut b, &mut a).await;
    assert_eq!(message.id, first);
    drop(b);

    let mut b = pb.open();
    b.dial(addr).unwrap();
    connect_until_ready(&mut a, &mut b).await;
    let fresh = a.send(pb.peer_id(), "novo", None).unwrap();

    let mut texts = Vec::new();
    timeout(WAIT, async {
        let collect = async {
            loop {
                let (_, message) = next_received(&mut b).await;
                let done = message.id == fresh;
                texts.push(message.text);
                if done {
                    return;
                }
            }
        };
        tokio::select! { _ = collect => (), _ = drive(&mut a) => unreachable!() }
    })
    .await
    .expect("a mensagem nova não chegou");

    assert_eq!(texts.last().map(String::as_str), Some("novo"));
    assert!(
        !texts.contains(&"m1".to_string()),
        "m1 foi reentregue: {texts:?}"
    );
    let mut unique = texts.clone();
    unique.dedup();
    assert_eq!(unique, texts, "mensagem duplicada: {texts:?}");
}

/// 6. P16: se um lado (`b`) perde o banco (mesma identidade e device, banco novo), o outro (`a`) o percebe
/// pelo `Hello` ("não tenho conversa com você") e **refaz** a conversa sozinho, avisando com
/// `ConversationReset`. Nas duas ordens de Peer ID: quem cria o grupo MLS é o de menor Peer ID.
#[tokio::test(flavor = "multi_thread")]
async fn peer_that_lost_its_database_is_rehandshaked() {
    for a_lower in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (pa, mut pb) = loop {
            let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
            if (pa.peer_id() < pb.peer_id()) == a_lower {
                break (pa, pb);
            }
        };
        let (mut a, mut b) = (pa.open(), pb.open());
        let addr = connect(&mut a, &mut b).await;
        drop(b);
        timeout(
            WAIT,
            wait_for(&mut a, |e| {
                matches!(e, ChatEvent::PeerDisconnected(_)).then_some(())
            }),
        )
        .await
        .expect("a não percebeu a queda");

        // Mesma identidade e device, banco novo: b não lembra de conversa nenhuma.
        pb.db = dir.path().join("b-novo.db");
        let mut b = pb.open();
        assert!(!b.is_ready(&pa.peer_id()));
        b.dial(addr).unwrap();
        // `settle`, e não `connect_until_ready_within`: ao reconectar, `a` reanuncia a conversa antiga (o
        // `ConversationReady` "enganoso"), então esperar os dois `Ready` volta antes de `a` processar o
        // `Welcome` e emitir o `ConversationReset`.
        let (seen_a, seen_b) = settle(&mut a, &mut b, Duration::from_secs(4)).await;
        let b_id = pb.peer_id();
        assert!(
            seen_a
                .iter()
                .any(|e| matches!(e, ChatEvent::ConversationReset { peer, .. } if *peer == b_id)),
            "a não avisou a refeita (a_lower={a_lower}): {seen_a:?}"
        );
        assert!(
            !seen_b
                .iter()
                .any(|e| matches!(e, ChatEvent::ConversationReset { .. })),
            "b não tinha o que refazer (a_lower={a_lower}): {seen_b:?}"
        );
        assert!(a.is_ready(&pb.peer_id()) && b.is_ready(&pa.peer_id()));

        // E a conversa nova funciona nos dois sentidos.
        a.send(pb.peer_id(), "depois do reset", None).unwrap();
        let (_, message) = receive_while_driving(&mut b, &mut a).await;
        assert_eq!(message.text, "depois do reset", "a_lower={a_lower}");
        b.send(pa.peer_id(), "e de volta", None).unwrap();
        let (_, message) = receive_while_driving(&mut a, &mut b).await;
        assert_eq!(message.text, "e de volta", "a_lower={a_lower}");
    }
}

/// P16 (auditoria, médio 1): o que `a` tinha na fila para o `b` que perdeu o banco **chega**. Antes, a fila
/// era despejada no grupo antigo ao reconectar (o transporte confirma, o `a` marca `Delivered`, o `b`
/// descarta); agora ela espera o `Hello` do `b` e sai pela conversa nova.
#[tokio::test(flavor = "multi_thread")]
async fn messages_queued_for_a_peer_that_lost_its_database_still_arrive() {
    for a_lower in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (pa, mut pb) = loop {
            let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
            if (pa.peer_id() < pb.peer_id()) == a_lower {
                break (pa, pb);
            }
        };
        let (mut a, mut b) = (pa.open(), pb.open());
        let addr = connect(&mut a, &mut b).await;
        drop(b);
        timeout(
            WAIT,
            wait_for(&mut a, |e| {
                matches!(e, ChatEvent::PeerDisconnected(_)).then_some(())
            }),
        )
        .await
        .expect("a não percebeu a queda");

        a.send(pb.peer_id(), "na fila", None).unwrap();
        pb.db = dir.path().join("b-novo.db");
        let mut b = pb.open();
        b.dial(addr).unwrap();

        let (_, message) = receive_while_driving(&mut b, &mut a).await;
        assert_eq!(message.text, "na fila", "a_lower={a_lower}");
    }
}

/// P16: sem perda nenhuma, uma conversa nova (ou uma que continua) **não** gera `ConversationReset`.
#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_or_surviving_conversation_is_never_reported_as_reset() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    {
        let (mut a, mut b) = (pa.open(), pb.open());
        let addr = listen(&mut a).await;
        b.dial(addr).unwrap();
        let (seen_a, seen_b) = connect_until_ready_within(&mut a, &mut b, WAIT).await;
        assert!(!has_reset(&seen_a) && !has_reset(&seen_b), "conversa nova");
    }
    // Os dois reabrem com os bancos de antes: a conversa continua, sem refazer.
    let (mut a, mut b) = (pa.open(), pb.open());
    let addr = listen(&mut a).await;
    b.dial(addr).unwrap();
    let (seen_a, seen_b) = settle(&mut a, &mut b, Duration::from_secs(3)).await;
    assert!(!has_reset(&seen_a) && !has_reset(&seen_b), "conversa que sobreviveu");
    assert!(a.is_ready(&pb.peer_id()) && b.is_ready(&pa.peer_id()));
}

fn has_reset(events: &[ChatEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, ChatEvent::ConversationReset { .. }))
}
