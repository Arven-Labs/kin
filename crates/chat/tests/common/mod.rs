//! Helpers compartilhados pelos testes de integração do chat.
#![allow(dead_code)] // cada arquivo de teste usa só parte dos helpers

pub mod fake_relay;

use std::path::{Path, PathBuf};
use std::time::Duration;

use kin_chat::{Chat, ChatEvent, Message};
use kin_identity::{DeviceKey, IdentityId, StandaloneIdentity};
use kin_transport::{Multiaddr, Node, NodeConfig, NodeEvent, PeerId, Protocol, RelayLimits};
use tokio::time::timeout;

pub const WAIT: Duration = Duration::from_secs(20);

/// Uma pessoa com identidade, device e banco próprios; pode "reiniciar" reabrindo o mesmo banco.
pub struct Person {
    pub identity: StandaloneIdentity,
    pub device: DeviceKey,
    pub db: PathBuf,
}

impl Person {
    pub fn new(dir: &Path, name: &str) -> Self {
        Self {
            identity: StandaloneIdentity::generate(),
            device: DeviceKey::generate(),
            db: dir.join(format!("{name}.db")),
        }
    }

    pub fn peer_id(&self) -> PeerId {
        self.device.peer_id()
    }

    pub fn open(&self) -> Chat {
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
pub async fn connect(a: &mut Chat, b: &mut Chat) -> Multiaddr {
    let addr = listen(a).await;
    b.dial(addr.clone()).unwrap();
    connect_until_ready(a, b).await;
    addr
}

/// Dirige `a` até ele notar que um peer caiu.
pub async fn until_disconnected(a: &mut Chat) {
    timeout(
        WAIT,
        wait_for(a, |e| {
            matches!(e, ChatEvent::PeerDisconnected(_)).then_some(())
        }),
    )
    .await
    .expect("a queda do peer não foi notada");
}

/// Dirige os dois até `a` confirmar `delivered` entregas e `b` receber `received` mensagens;
/// devolve os textos recebidos por `b`, na ordem.
pub async fn until_delivered(
    a: &mut Chat,
    b: &mut Chat,
    delivered: usize,
    received: usize,
) -> Vec<String> {
    let (mut acks, mut texts) = (0, Vec::new());
    timeout(WAIT, async {
        while acks < delivered || texts.len() < received {
            tokio::select! {
                e = a.next_event() => if matches!(e, ChatEvent::Delivered { .. }) { acks += 1 },
                e = b.next_event() => if let ChatEvent::Received { message, .. } = e { texts.push(message.text) },
            }
        }
    })
    .await
    .expect("as mensagens da fila não chegaram");
    texts
}

/// Avança o chat até um evento que o filtro aceita; descarta os demais.
pub async fn wait_for<T>(chat: &mut Chat, mut pick: impl FnMut(ChatEvent) -> Option<T>) -> T {
    loop {
        if let Some(found) = pick(chat.next_event().await) {
            return found;
        }
    }
}

/// Dirige o chat para sempre (o swarm só progride quando polado); usar dentro de `select!`.
pub async fn drive(chat: &mut Chat) -> ! {
    loop {
        chat.next_event().await;
    }
}

pub async fn listen(chat: &mut Chat) -> Multiaddr {
    chat.listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap())
        .unwrap();
    wait_for(chat, |e| match e {
        ChatEvent::Listening(addr) => Some(addr),
        _ => None,
    })
    .await
}

pub async fn next_received(chat: &mut Chat) -> (IdentityId, Message) {
    wait_for(chat, |e| match e {
        ChatEvent::Received {
            sender, message, ..
        } => Some((sender, message)),
        _ => None,
    })
    .await
}

/// Dirige os dois chats juntos até ambos anunciarem `ConversationReady`.
pub async fn connect_until_ready(a: &mut Chat, b: &mut Chat) {
    connect_until_ready_within(a, b, WAIT).await;
}

/// Como [`connect_until_ready`], com prazo próprio; devolve os demais eventos vistos (`a`, `b`),
/// para o teste checar, por exemplo, que não houve `Dropped`.
pub async fn connect_until_ready_within(
    a: &mut Chat,
    b: &mut Chat,
    limit: Duration,
) -> (Vec<ChatEvent>, Vec<ChatEvent>) {
    let (mut ready_a, mut ready_b) = (false, false);
    let (mut seen_a, mut seen_b) = (Vec::new(), Vec::new());
    let result = timeout(limit, async {
        while !(ready_a && ready_b) {
            tokio::select! {
                e = a.next_event() => if matches!(e, ChatEvent::ConversationReady { .. }) { ready_a = true } else { seen_a.push(e) },
                e = b.next_event() => if matches!(e, ChatEvent::ConversationReady { .. }) { ready_b = true } else { seen_b.push(e) },
            }
        }
    })
    .await;
    // No prazo estourado, o que cada lado viu mostra onde o handshake travou.
    assert!(
        result.is_ok(),
        "conversa não ficou pronta (a pronto: {ready_a}, b pronto: {ready_b})\neventos de a: {seen_a:#?}\neventos de b: {seen_b:#?}"
    );
    (seen_a, seen_b)
}

/// Dirige os dois chats por `duration` e devolve os eventos vistos em cada um.
pub async fn settle(
    a: &mut Chat,
    b: &mut Chat,
    duration: Duration,
) -> (Vec<ChatEvent>, Vec<ChatEvent>) {
    let (mut seen_a, mut seen_b) = (Vec::new(), Vec::new());
    let _ = timeout(duration, async {
        loop {
            tokio::select! {
                e = a.next_event() => seen_a.push(e),
                e = b.next_event() => seen_b.push(e),
            }
        }
    })
    .await;
    (seen_a, seen_b)
}

/// Espera `receiver` receber uma mensagem enquanto `sender` é dirigido em paralelo.
pub async fn receive_while_driving(
    receiver: &mut Chat,
    sender: &mut Chat,
) -> (IdentityId, Message) {
    timeout(WAIT, async {
        tokio::select! { r = next_received(receiver) => r, _ = drive(sender) => unreachable!() }
    })
    .await
    .expect("sem mensagem")
}

/// Sobe um nó relay em segundo plano e devolve o endereço dele (com `/p2p/<id>`).
pub async fn start_relay() -> Multiaddr {
    let key = DeviceKey::generate();
    let mut relay = Node::new(
        key.keypair().clone(),
        NodeConfig {
            mdns: false,
            relay_server: Some(RelayLimits::default()),
            ..Default::default()
        },
    )
    .unwrap();
    relay
        .listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap())
        .unwrap();
    let addr = loop {
        if let NodeEvent::Listening(addr) = relay.next_event().await {
            break addr;
        }
    };
    tokio::spawn(async move {
        loop {
            relay.next_event().await;
        }
    });
    addr.with(Protocol::P2p(key.peer_id()))
}
