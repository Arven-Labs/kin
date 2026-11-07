//! Caixa postal de terceiros (Fase 3.1, D1): um lugar onde o remetente deposita a mensagem já cifrada
//! para o destinatário buscar depois, sem os dois estarem online ao mesmo tempo.
//!
//! O `Chat` só chama [`Mailbox::deposit`]; o resultado e o que chega para este device voltam por um
//! canal de [`MailboxEvent`] entregue em [`Mailbox::attach`]. Assim o backend pode ser síncrono (o de
//! memória daqui) ou assíncrono, baseado em tarefas (websocket de um relay Nostr, 3.3). O envio
//! direto (libp2p) não passa por aqui.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::UnboundedSender;
use kin_identity::NostrPublicKey;
use kin_transport::PeerId;

/// Identifica um depósito, para casar com [`MailboxEvent::Deposited`] / [`MailboxEvent::DepositFailed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DepositId(pub u64);

/// O que um backend de mailbox informa ao `Chat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxEvent {
    /// A caixa postal aceitou o depósito. Não é entrega: o destinatário ainda precisa buscar.
    Deposited {
        id: DepositId,
    },
    DepositFailed {
        id: DepositId,
        reason: String,
    },
    /// Diagnóstico: um relay respondeu ao depósito `id` (aceitou ou recusou, e por quê). Vem além de
    /// `Deposited`/`DepositFailed`, que são o resultado agregado; `reason` é texto do relay (já
    /// reduzido a uma linha curta). Backends sem relays não emitem.
    RelayOutcome {
        id: DepositId,
        relay: String,
        accepted: bool,
        reason: String,
    },
    /// Diagnóstico: o relay ainda não respondeu ao depósito `id` depois do `ack_timeout`. Não é
    /// falha: o depósito segue pendente e uma resposta tardia ainda conta. Cada relay é avisado no
    /// máximo uma vez por depósito; o resultado final vem em `RelayOutcome`.
    RelaySlow {
        id: DepositId,
        relay: String,
    },
    /// Chegou algo para este device. `from` é afirmado pelo backend; quem de fato enviou é
    /// verificado na decifragem (credencial MLS), então um `from` falso vira `Dropped`.
    Retrieved {
        from: PeerId,
        blob: Vec<u8>,
    },
}

/// Backend de caixa postal. `deposit` não bloqueia: o resultado chega como evento.
pub trait Mailbox {
    /// Chamado uma vez, em `Chat::set_mailbox`: o backend guarda `events` e envia por ele tudo o que
    /// acontecer (resultados de depósito e mensagens para este device, inclusive as que já
    /// estavam esperando).
    fn attach(&mut self, events: UnboundedSender<MailboxEvent>);

    /// Deposita `blob` (bytes opacos, já cifrados) para `to`. O backend sabe como chegar até ele.
    fn deposit(&mut self, to: PeerId, blob: Vec<u8>) -> DepositId;

    /// O `Chat` informa a chave Nostr de `peer`, lida do certificado dele (assinado pela identidade e
    /// validado pelo MLS), sempre que uma conversa fica pronta ou é recarregada do banco. Backends que
    /// endereçam por outra chave que não o Peer ID (Nostr, ADR 0004) guardam o par; os demais ignoram.
    fn learn_peer(&mut self, _peer: PeerId, _nostr: NostrPublicKey) {}
}

/// Um depósito registrado pelo [`MemoryRelay`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deposit {
    pub from: PeerId,
    pub to: PeerId,
    pub blob: Vec<u8>,
}

#[derive(Default)]
struct Inbox {
    sender: Option<UnboundedSender<MailboxEvent>>,
    stored: VecDeque<(PeerId, Vec<u8>)>,
}

#[derive(Default)]
struct State {
    inboxes: HashMap<PeerId, Inbox>,
    deposits: Vec<Deposit>,
    failing: bool,
}

/// "Servidor" em memória compartilhado por vários [`MemoryMailbox`]: guarda o que não tem quem
/// receba e entrega quando o destinatário se conecta. Backend de referência e de teste.
#[derive(Clone, Default)]
pub struct MemoryRelay(Arc<Mutex<State>>);

impl MemoryRelay {
    /// Todos os depósitos aceitos, na ordem.
    pub fn deposits(&self) -> Vec<Deposit> {
        self.0.lock().unwrap().deposits.clone()
    }

    /// Enquanto `true`, todo depósito é recusado (`DepositFailed`).
    pub fn fail_deposits(&self, failing: bool) {
        self.0.lock().unwrap().failing = failing;
    }
}

/// A ponta de um device no [`MemoryRelay`].
pub struct MemoryMailbox {
    owner: PeerId,
    relay: MemoryRelay,
    next_id: u64,
    events: Option<UnboundedSender<MailboxEvent>>,
}

impl MemoryMailbox {
    pub fn new(relay: &MemoryRelay, owner: PeerId) -> Self {
        Self {
            owner,
            relay: relay.clone(),
            next_id: 0,
            events: None,
        }
    }

    fn emit(&self, event: MailboxEvent) {
        if let Some(events) = &self.events {
            let _ = events.unbounded_send(event);
        }
    }
}

impl Mailbox for MemoryMailbox {
    fn attach(&mut self, events: UnboundedSender<MailboxEvent>) {
        self.events = Some(events.clone());
        let mut state = self.relay.0.lock().unwrap();
        let inbox = state.inboxes.entry(self.owner).or_default();
        inbox.sender = Some(events.clone());
        while let Some((from, blob)) = inbox.stored.pop_front() {
            let event = MailboxEvent::Retrieved {
                from,
                blob: blob.clone(),
            };
            if events.unbounded_send(event).is_err() {
                inbox.stored.push_front((from, blob));
                break;
            }
        }
    }

    fn deposit(&mut self, to: PeerId, blob: Vec<u8>) -> DepositId {
        let id = DepositId(self.next_id);
        self.next_id += 1;
        let mut state = self.relay.0.lock().unwrap();
        if state.failing {
            drop(state);
            self.emit(MailboxEvent::DepositFailed {
                id,
                reason: "o relay recusou o depósito".into(),
            });
            return id;
        }
        state.deposits.push(Deposit {
            from: self.owner,
            to,
            blob: blob.clone(),
        });
        let inbox = state.inboxes.entry(to).or_default();
        // Sem receptor vivo (nunca ligou ou o `Chat` foi descartado), o relay guarda para depois.
        let delivered = inbox.sender.as_ref().is_some_and(|tx| {
            tx.unbounded_send(MailboxEvent::Retrieved {
                from: self.owner,
                blob: blob.clone(),
            })
            .is_ok()
        });
        if !delivered {
            inbox.sender = None;
            inbox.stored.push_back((self.owner, blob));
        }
        drop(state);
        self.emit(MailboxEvent::Deposited { id });
        id
    }
}
