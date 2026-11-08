//! `NostrMailbox`: o backend do trait `Mailbox` sobre relays Nostr (Fase 3.3, ADR 0004).
//!
//! Cada relay tem um worker (tarefa tokio, ver `relay.rs`). O estado que eles dividem (diretório de
//! pares, ids já vistos, depósitos em andamento) fica em [`Shared`], atrás de um `Mutex` que nunca é
//! segurado durante I/O nem durante a decifragem.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::channel::mpsc::{UnboundedSender, unbounded};
use kin_identity::{DeviceKey, NostrPublicKey};
use kin_transport::PeerId;
use nostr::prelude::*;
use tokio::task::JoinHandle;

use super::relay::{self, Command};
use super::{MAX_BLOB, unwrap, wrap};
use crate::{DepositId, Mailbox, MailboxEvent};

/// Quantos ids de evento já vistos se lembra; os mais antigos são esquecidos.
const SEEN_LIMIT: usize = 4096;

/// Tempos do backend. Em produção use [`NostrConfig::default`]; os testes encurtam.
#[derive(Debug, Clone)]
pub struct NostrConfig {
    /// Primeira espera antes de reconectar; dobra a cada falha até `backoff_max` (com jitter de ±20%).
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// Quanto esperar o `OK` de um relay antes de avisar que ele está lento (`RelaySlow`). Não é
    /// falha: o depósito segue pendente e um `OK` tardio ainda conta.
    pub ack_timeout: Duration,
    /// Quanto esperar o `OK` de um relay antes de contá-lo como falha. Deve ser maior que
    /// `ack_timeout`. O nos.lol real levou 35 a 57 s (Sessão 11).
    pub ack_deadline: Duration,
    /// Quanto do passado pedir ao assinar. O NIP-59 sorteia o `created_at` do gift wrap até 2 dias
    /// atrás para esconder o horário real; 3 dias cobrem isso com folga de relógio.
    pub since_window: Duration,
}

impl Default for NostrConfig {
    fn default() -> Self {
        Self {
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            ack_timeout: Duration::from_secs(10),
            ack_deadline: Duration::from_secs(90),
            since_window: Duration::from_secs(3 * 24 * 3600),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NostrError {
    /// URL que não é `wss://` (ou `ws://` para o próprio computador) ou que não se lê.
    #[error("URL de relay recusada: {0}")]
    InvalidRelayUrl(String),
    #[error("chave do device ilegível: {0}")]
    Key(String),
    /// O arquivo de relays existe mas não deu para ler.
    #[error("não consegui ler o arquivo de relays: {0}")]
    RelayFile(String),
}

struct Pending {
    remaining: usize,
    accepted: bool,
    reason: String,
}

struct Seen {
    order: VecDeque<EventId>,
    ids: HashSet<EventId>,
}

impl Seen {
    fn insert(&mut self, id: EventId) -> bool {
        if !self.ids.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > SEEN_LIMIT
            && let Some(old) = self.order.pop_front()
        {
            self.ids.remove(&old);
        }
        true
    }
}

struct Inner {
    events: Option<UnboundedSender<MailboxEvent>>,
    peers: HashMap<PeerId, [u8; 32]>,
    by_nostr: HashMap<[u8; 32], PeerId>,
    /// Relays em que cada contato recebe (anunciados por ele, já validados pelo `Chat`).
    peer_relays: HashMap<PeerId, Vec<RelayUrl>>,
    seen: Seen,
    pending: HashMap<DepositId, Pending>,
}

/// Estado dividido entre o `NostrMailbox` e os workers.
pub(super) struct Shared {
    device: DeviceKey,
    inner: Mutex<Inner>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Um worker que entrou em pânico não deve derrubar os outros: o estado segue consistente.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Chaves Nostr deste device, para assinar (NIP-42) e abrir gift wraps.
    pub(super) fn keys(&self) -> Keys {
        let secret = SecretKey::from_slice(&self.device.nostr_secret_bytes())
            .expect("a chave Nostr do device já foi validada ao ser criada");
        Keys::new(secret)
    }

    fn emit(inner: &Inner, event: MailboxEvent) {
        if let Some(events) = &inner.events {
            let _ = events.unbounded_send(event);
        }
    }

    /// Um relay respondeu (ou falhou) a um depósito. `Deposited` sai na primeira aceitação;
    /// `DepositFailed` só quando todos os relays falharam.
    pub(super) fn ack(&self, id: DepositId, relay: &str, accepted: bool, reason: &str) {
        let mut inner = self.lock();
        if !inner.pending.contains_key(&id) {
            return;
        }
        // O motivo vem do relay (hostil): uma linha curta, sem caracteres de controle.
        let reason = one_line(reason);
        Self::emit(
            &inner,
            MailboxEvent::RelayOutcome {
                id,
                relay: relay.to_string(),
                accepted,
                reason: reason.clone(),
            },
        );
        let Some(pending) = inner.pending.get_mut(&id) else {
            return;
        };
        pending.remaining -= 1;
        let mut event = None;
        if accepted && !pending.accepted {
            pending.accepted = true;
            event = Some(MailboxEvent::Deposited { id });
        } else if !accepted {
            pending.reason = reason;
        }
        if pending.remaining == 0 {
            if !pending.accepted {
                event = Some(MailboxEvent::DepositFailed {
                    id,
                    reason: pending.reason.clone(),
                });
            }
            inner.pending.remove(&id);
        }
        if let Some(event) = event {
            Self::emit(&inner, event);
        }
    }

    /// O relay passou do `ack_timeout` sem responder ao depósito `id`. Só avisa; não conta como falha.
    pub(super) fn slow(&self, id: DepositId, relay: &str) {
        let inner = self.lock();
        if inner.pending.contains_key(&id) {
            Self::emit(
                &inner,
                MailboxEvent::RelaySlow {
                    id,
                    relay: relay.to_string(),
                },
            );
        }
    }

    /// Chegou um evento de um relay. Só vira `Retrieved` se abrir, vier de um par conhecido e ainda
    /// não tiver sido entregue (o mesmo gift wrap chega de vários relays e a cada reconexão).
    pub(super) fn on_event(&self, event: &Event) {
        if event.kind != Kind::GiftWrap || self.lock().seen.ids.contains(&event.id) {
            return;
        }
        // Decifrar fora do lock: é a parte cara e o conteúdo é hostil.
        let Ok(opened) = unwrap(&self.device, event) else {
            return;
        };
        let mut inner = self.lock();
        let Some(from) = inner.by_nostr.get(&opened.sender.to_bytes()).copied() else {
            // Sem `learn_peer` ainda: não marca como visto, para a reconexão tentar de novo.
            return;
        };
        if inner.seen.insert(event.id) {
            Self::emit(
                &inner,
                MailboxEvent::Retrieved {
                    from,
                    blob: opened.blob,
                },
            );
        }
    }
}

/// Quantos caracteres do motivo de um relay se mantêm.
const REASON_LIMIT: usize = 200;

/// Reduz um texto vindo do relay a uma linha curta: caracteres de controle (quebras, escapes) viram
/// espaço e o resto é cortado em `REASON_LIMIT`.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(REASON_LIMIT)
        .collect()
}

struct Worker {
    url: String,
    commands: UnboundedSender<Command>,
    task: JoinHandle<()>,
}

pub struct NostrMailbox {
    shared: Arc<Shared>,
    relays: Vec<RelayUrl>,
    /// As URLs como foram configuradas, para anunciar ao contato exatamente o que o usuário escreveu.
    announced: Vec<String>,
    config: NostrConfig,
    workers: Vec<Worker>,
    /// Publicações em relays de contatos (uma tarefa por relay e depósito); abortadas no `Drop`.
    ephemeral: Vec<JoinHandle<()>>,
    next_id: u64,
}

impl NostrMailbox {
    /// Configura o backend para os `relays` (`wss://...`; `ws://` só para o próprio computador). Nada
    /// de rede acontece aqui: as conexões sobem em [`Mailbox::attach`], dentro de um runtime tokio.
    pub fn new<S: AsRef<str>>(device: &DeviceKey, relays: &[S]) -> Result<Self, NostrError> {
        Self::with_config(device, relays, NostrConfig::default())
    }

    pub fn with_config<S: AsRef<str>>(
        device: &DeviceKey,
        relays: &[S],
        config: NostrConfig,
    ) -> Result<Self, NostrError> {
        let urls: Vec<String> = relays.iter().map(|u| u.as_ref().to_string()).collect();
        let relays = relays
            .iter()
            .map(|url| parse_relay(url.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        // `DeviceKey` não é `Clone`: copiamos pelo formato de arquivo, que já carrega as duas chaves.
        let device = DeviceKey::from_bytes(
            &device
                .to_bytes()
                .map_err(|e| NostrError::Key(e.to_string()))?,
        )
        .map_err(|e| NostrError::Key(e.to_string()))?;
        let shared = Arc::new(Shared {
            device,
            inner: Mutex::new(Inner {
                events: None,
                peers: HashMap::new(),
                by_nostr: HashMap::new(),
                peer_relays: HashMap::new(),
                seen: Seen {
                    order: VecDeque::new(),
                    ids: HashSet::new(),
                },
                pending: HashMap::new(),
            }),
        });
        Ok(Self {
            shared,
            relays,
            announced: urls,
            config,
            workers: Vec::new(),
            ephemeral: Vec::new(),
            next_id: 0,
        })
    }
}

pub(super) fn parse_relay(url: &str) -> Result<RelayUrl, NostrError> {
    let parsed = RelayUrl::parse(url).map_err(|e| NostrError::InvalidRelayUrl(e.to_string()))?;
    if parsed.scheme() == RelayUrlScheme::Ws && !parsed.is_localhost() {
        return Err(NostrError::InvalidRelayUrl(format!(
            "{url}: ws:// só vale para o próprio computador, use wss://"
        )));
    }
    Ok(parsed)
}

impl Mailbox for NostrMailbox {
    /// Sobe um worker por relay. Precisa ser chamado dentro de um runtime tokio.
    fn attach(&mut self, events: UnboundedSender<MailboxEvent>) {
        self.shared.lock().events = Some(events);
        for url in &self.relays {
            let (commands, receiver) = unbounded();
            let task = tokio::spawn(relay::run(
                url.clone(),
                self.shared.clone(),
                self.config.clone(),
                receiver,
            ));
            self.workers.push(Worker {
                url: url.to_string(),
                commands,
                task,
            });
        }
    }

    fn deposit(&mut self, to: PeerId, blob: Vec<u8>) -> DepositId {
        let id = DepositId(self.next_id);
        self.next_id += 1;
        let fail = |reason: &str| {
            let inner = self.shared.lock();
            Shared::emit(
                &inner,
                MailboxEvent::DepositFailed {
                    id,
                    reason: reason.to_string(),
                },
            );
        };

        if blob.len() > MAX_BLOB {
            fail("a mensagem passa do teto do backend Nostr");
            return id;
        }
        let Some(key) = self.shared.lock().peers.get(&to).copied() else {
            fail("a chave Nostr do par ainda não é conhecida");
            return id;
        };
        let wrapped = NostrPublicKey::from_bytes(&key)
            .map_err(|e| e.to_string())
            .and_then(|key| wrap(&self.shared.device, &key, &blob).map_err(|e| e.to_string()));
        let event = match wrapped {
            Ok(event) => event,
            Err(reason) => {
                fail(&reason);
                return id;
            }
        };
        // Com a lista do contato, deposita só lá (é onde ele escuta), por conexões de uma publicação.
        let theirs = self.shared.lock().peer_relays.get(&to).cloned();
        if let Some(theirs) = theirs {
            self.ephemeral.retain(|task| !task.is_finished());
            self.shared.lock().pending.insert(
                id,
                Pending {
                    remaining: theirs.len(),
                    accepted: false,
                    reason: String::new(),
                },
            );
            for url in theirs {
                self.ephemeral.push(tokio::spawn(relay::publish_once(
                    url,
                    self.shared.clone(),
                    self.config.clone(),
                    id,
                    event.clone(),
                )));
            }
            return id;
        }
        if self.workers.is_empty() {
            fail("a caixa postal Nostr ainda não foi ligada");
            return id;
        }

        self.shared.lock().pending.insert(
            id,
            Pending {
                remaining: self.workers.len(),
                accepted: false,
                reason: String::new(),
            },
        );
        for worker in &self.workers {
            let command = Command::Publish {
                id,
                event: event.clone(),
            };
            if worker.commands.unbounded_send(command).is_err() {
                self.shared
                    .ack(id, &worker.url, false, "o worker do relay encerrou");
            }
        }
        id
    }

    fn learn_peer(&mut self, peer: PeerId, nostr: NostrPublicKey) {
        let key = nostr.to_bytes();
        let mut inner = self.shared.lock();
        inner.peers.insert(peer, key);
        inner.by_nostr.insert(key, peer);
    }

    fn relays(&self) -> Vec<String> {
        self.announced.clone()
    }

    /// A política de segurança da lista (só `wss://`, sem IP, teto) é do `Chat`, que valida antes de
    /// chamar. Aqui só se exige que as URLs se leiam; lista vazia ou ilegível mantém a anterior.
    fn learn_relays(&mut self, peer: PeerId, relays: Vec<String>) {
        let parsed: Result<Vec<_>, _> = relays.iter().map(|u| parse_relay(u)).collect();
        if let Ok(parsed) = parsed
            && !parsed.is_empty()
        {
            self.shared.lock().peer_relays.insert(peer, parsed);
        }
    }
}

impl Drop for NostrMailbox {
    fn drop(&mut self) {
        for task in &self.ephemeral {
            task.abort();
        }
        for worker in &self.workers {
            worker.task.abort();
        }
    }
}
