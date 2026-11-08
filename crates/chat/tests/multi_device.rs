//! 3.5: multi-device de ponta a ponta. Quatro `Chat` reais: Ana e Bia, cada uma com dois devices.
//! Só A1↔B1 se conhecem no começo; a descoberta (`DeviceList`) tem de ligar os outros pares, e uma
//! mensagem de A1 a Bia tem de chegar em B1 e B2 (mesmo `MessageId`) e aparecer em A2 como espelho.

mod common;

use std::path::Path;

use common::{WAIT, listen};
use futures::future::select_all;
use kin_chat::{Chat, ChatEvent, MessageId};
use kin_identity::{DeviceKey, IdentityId, IdentityProvider, StandaloneIdentity};
use kin_transport::{Multiaddr, NodeConfig, PeerId, Protocol};
use tokio::time::timeout;

const A1: usize = 0;
const A2: usize = 1;
const B1: usize = 2;
const B2: usize = 3;

struct World {
    chats: [Chat; 4],
    peers: [PeerId; 4],
    bia: IdentityId,
    /// Todos os eventos vistos, com o índice do chat que os produziu.
    log: Vec<(usize, ChatEvent)>,
}

/// O mesmo `StandaloneIdentity` em dois devices (o pareamento copia a chave mestra).
fn copy_of(identity: &StandaloneIdentity) -> StandaloneIdentity {
    StandaloneIdentity::from_bytes(&identity.to_bytes().unwrap()).unwrap()
}

fn open(identity: &StandaloneIdentity, device: &DeviceKey, dir: &Path, name: &str) -> Chat {
    Chat::open(
        identity,
        device,
        NodeConfig {
            mdns: false,
            ..Default::default()
        },
        &dir.join(format!("{name}.db")),
    )
    .unwrap()
}

impl World {
    /// Quatro chats escutando em loopback, cada um anunciando o próprio endereço (em loopback o nó não
    /// tem endereço "compartilhável", então fixamos à mão). Ninguém conhece ninguém ainda.
    async fn new(dir: &Path) -> (Self, [Multiaddr; 4]) {
        let (ana, bia) = (
            StandaloneIdentity::generate(),
            StandaloneIdentity::generate(),
        );
        let identities = [copy_of(&ana), copy_of(&ana), copy_of(&bia), copy_of(&bia)];
        let devices: [DeviceKey; 4] = std::array::from_fn(|_| DeviceKey::generate());
        let names = ["a1", "a2", "b1", "b2"];
        let mut chats: [Chat; 4] =
            std::array::from_fn(|i| open(&identities[i], &devices[i], dir, names[i]));
        let peers: [PeerId; 4] = std::array::from_fn(|i| devices[i].peer_id());
        let mut addrs = Vec::new();
        for (chat, peer) in chats.iter_mut().zip(&peers) {
            let addr = listen(chat).await;
            chat.set_advertised_addrs(vec![addr.clone().with(Protocol::P2p(*peer))]);
            addrs.push(addr);
        }
        let world = Self {
            chats,
            peers,
            bia: bia.id(),
            log: Vec::new(),
        };
        (world, addrs.try_into().unwrap())
    }

    /// Dirige os quatro chats juntos até `done` ficar verdadeiro (ou o prazo estourar).
    async fn pump_until(&mut self, what: &str, mut done: impl FnMut(&World) -> bool) {
        let run = async {
            while !done(self) {
                let (index, event) = {
                    let futures = self
                        .chats
                        .iter_mut()
                        .enumerate()
                        .map(|(i, chat)| Box::pin(async move { (i, chat.next_event().await) }));
                    select_all(futures).await.0
                };
                self.log.push((index, event));
            }
        };
        timeout(WAIT * 2, run)
            .await
            .unwrap_or_else(|_| panic!("não chegou a: {what}"));
    }

    fn knows(&self, a: usize, b: usize) -> bool {
        self.chats[a].ready_peers().contains(&self.peers[b])
    }

    fn all_events(&self, chat: usize) -> impl Iterator<Item = &ChatEvent> {
        self.log
            .iter()
            .filter(move |(i, _)| *i == chat)
            .map(|(_, e)| e)
    }

    /// Irmãos se conectam (A2→A1, B2→B1) e depois A1 disca B1: a única ligação entre as duas casas.
    async fn start(dir: &Path) -> Self {
        let (mut world, addrs) = Self::new(dir).await;
        world.chats[A2].dial(addrs[A1].clone()).unwrap();
        world.chats[B2].dial(addrs[B1].clone()).unwrap();
        world
            .pump_until("irmãos conectados", |w| w.knows(A1, A2) && w.knows(B1, B2))
            .await;
        world.chats[A1].dial(addrs[B1].clone()).unwrap();
        world
    }

    async fn converge(&mut self) {
        self.pump_until("todos conversando com todos", |w| {
            (0..4).all(|a| (0..4).all(|b| a == b || w.knows(a, b)))
        })
        .await;
    }
}

/// A descoberta liga os pares que nunca se discaram, sem ninguém pedir.
#[tokio::test(flavor = "multi_thread")]
async fn devices_of_two_households_discover_each_other_from_one_connection() {
    let dir = tempfile::tempdir().unwrap();
    let mut world = World::start(dir.path()).await;
    assert!(
        !world.knows(A2, B1) && !world.knows(A1, B2),
        "ponto de partida"
    );

    world.converge().await;

    let dropped: Vec<_> = world
        .log
        .iter()
        .filter(|(_, e)| matches!(e, ChatEvent::Dropped { .. }))
        .collect();
    assert!(dropped.is_empty(), "nada pode ser descartado: {dropped:?}");
}

/// Mensagem de A1 a Bia chega em B1 e B2 (mesmo id), e A2 a vê como "enviada por mim a Bia".
#[tokio::test(flavor = "multi_thread")]
async fn a_message_reaches_every_device_and_is_mirrored_to_my_other_device() {
    let dir = tempfile::tempdir().unwrap();
    let mut world = World::start(dir.path()).await;
    world.converge().await;

    let id: MessageId = world.chats[A1]
        .send_to(&world.bia.clone(), "oi, bia", None)
        .unwrap();

    world
        .pump_until("B1, B2 recebem e A2 espelha", |w| {
            let got = |chat: usize| {
                w.all_events(chat).any(|e| {
                    matches!(e, ChatEvent::Received { message, .. }
                        if message.id == id && message.text == "oi, bia")
                })
            };
            let mirrored = w.all_events(A2).any(|e| {
                matches!(e, ChatEvent::SentElsewhere { to, message, .. }
                    if message.id == id && *to == w.bia)
            });
            got(B1) && got(B2) && mirrored
        })
        .await;

    // A2 não vê a própria mensagem como recebida, e Bia não vê espelho nenhum.
    assert!(
        !world
            .all_events(A2)
            .any(|e| matches!(e, ChatEvent::Received { .. }))
    );
    for chat in [B1, B2] {
        assert!(
            !world
                .all_events(chat)
                .any(|e| matches!(e, ChatEvent::SentElsewhere { .. }))
        );
    }
}
