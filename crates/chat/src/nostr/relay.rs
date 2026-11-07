//! Worker de um relay Nostr: uma tarefa tokio que mantém a conexão (com reconexão e espera crescente),
//! assina os gift wraps endereçados a este device, publica o que o `NostrMailbox` manda e responde ao
//! NIP-42. Tudo o que vem do relay é entrada hostil: o limite de mensagem é aplicado no websocket e o
//! conteúdo só é aceito depois de `unwrap` (que tem os próprios tetos).

use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::{SinkExt, StreamExt};
use nostr::nips::nip42::ClientAuthentication;
use nostr::prelude::*;
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

use super::mailbox::{NostrConfig, Shared};
use crate::DepositId;

/// Tamanho máximo de uma mensagem do relay (bytes). Um gift wrap de `MAX_BLOB` cabe com folga.
const MAX_MESSAGE: usize = 1 << 20;

/// Quanto esperar o handshake do websocket.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) enum Command {
    Publish { id: DepositId, event: Event },
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub(super) async fn run(
    url: RelayUrl,
    shared: Arc<Shared>,
    config: NostrConfig,
    mut commands: UnboundedReceiver<Command>,
) {
    let mut backoff = config.backoff_min;
    loop {
        if let Some(socket) = connect(&url).await {
            backoff = config.backoff_min;
            if !session(&url, &shared, &config, socket, &mut commands).await {
                return;
            }
        }
        // Desconectado: o que for pedido agora falha rápido (a outbox repete no próximo gatilho).
        let jitter = 0.8 + rand::random::<f64>() * 0.4;
        let wait = backoff.mul_f64(jitter);
        backoff = (backoff * 2).min(config.backoff_max);
        let pause = sleep(wait);
        tokio::pin!(pause);
        loop {
            tokio::select! {
                _ = &mut pause => break,
                command = commands.next() => match command {
                    Some(Command::Publish { id, .. }) => shared.ack(id, url.as_str(), false, "relay desconectado"),
                    None => return,
                },
            }
        }
    }
}

async fn connect(url: &RelayUrl) -> Option<Socket> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let attempt = connect_async_with_config(url.as_str(), Some(config), false);
    timeout(CONNECT_TIMEOUT, attempt)
        .await
        .ok()?
        .ok()
        .map(|(socket, _)| socket)
}

/// Uma conexão. Devolve `false` quando é para encerrar de vez (o `NostrMailbox` foi descartado).
async fn session(
    url: &RelayUrl,
    shared: &Arc<Shared>,
    config: &NostrConfig,
    socket: Socket,
    commands: &mut UnboundedReceiver<Command>,
) -> bool {
    let (mut sink, mut stream) = socket.split();
    let keys = shared.keys();
    let subscription = SubscriptionId::new("kin");
    let filter = Filter::new()
        .kind(Kind::GiftWrap)
        .pubkey(keys.public_key())
        .since(Timestamp::now() - config.since_window);
    let request = ClientMessage::req(subscription, filter);
    if send(&mut sink, &request).await.is_err() {
        return true;
    }

    // Publicações à espera de `OK`.
    struct Waiting {
        event_id: EventId,
        deposit: DepositId,
        /// Quando avisar que o relay está lento; `None` depois de avisar.
        slow_at: Option<Instant>,
        /// Quando contar o relay como falha.
        deadline: Instant,
    }
    let mut waiting: Vec<Waiting> = Vec::new();
    let keep = loop {
        let next_deadline = waiting
            .iter()
            .flat_map(|w| w.slow_at.into_iter().chain([w.deadline]))
            .min();
        tokio::select! {
            incoming = stream.next() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break true,
                    Some(Ok(_)) => continue,
                };
                let Ok(message) = serde_json::from_str::<RelayMessage>(text.as_str()) else {
                    continue;
                };
                match message {
                    RelayMessage::Event { event, .. } => shared.on_event(&event),
                    RelayMessage::Ok { event_id, status, message } => {
                        if let Some(at) = waiting.iter().position(|w| w.event_id == event_id) {
                            let done = waiting.swap_remove(at);
                            shared.ack(done.deposit, url.as_str(), status, &message);
                        }
                    }
                    RelayMessage::Auth { challenge } => {
                        // NIP-42: prova que controlamos a chave Nostr e pede a assinatura de novo.
                        let auth = ClientAuthentication::new(challenge.as_ref(), url.clone())
                            .into_event_builder()
                            .finalize(&keys);
                        let Ok(auth) = auth else { continue };
                        if send(&mut sink, &ClientMessage::auth(auth)).await.is_err()
                            || send(&mut sink, &request).await.is_err()
                        {
                            break true;
                        }
                    }
                    _ => {}
                }
            }
            command = commands.next() => match command {
                Some(Command::Publish { id, event }) => {
                    let event_id = event.id;
                    if send(&mut sink, &ClientMessage::event(event)).await.is_err() {
                        shared.ack(id, url.as_str(), false, "falha ao enviar ao relay");
                        break true;
                    }
                    let now = Instant::now();
                    waiting.push(Waiting {
                        event_id,
                        deposit: id,
                        slow_at: Some(now + config.ack_timeout),
                        deadline: now + config.ack_deadline,
                    });
                }
                None => break false,
            },
            _ = async { sleep_until(next_deadline.unwrap()).await }, if next_deadline.is_some() => {
                let now = Instant::now();
                let (late, rest): (Vec<_>, Vec<_>) = waiting.drain(..).partition(|w| w.deadline <= now);
                waiting = rest;
                for w in late {
                    shared.ack(w.deposit, url.as_str(), false, "o relay não respondeu a tempo");
                }
                for w in &mut waiting {
                    if w.slow_at.is_some_and(|at| at <= now) {
                        w.slow_at = None;
                        shared.slow(w.deposit, url.as_str());
                    }
                }
            }
        }
    };
    for w in waiting {
        shared.ack(
            w.deposit,
            url.as_str(),
            false,
            "conexão perdida antes da resposta",
        );
    }
    keep
}

async fn send<S>(sink: &mut S, message: &ClientMessage<'_>) -> Result<(), ()>
where
    S: futures::Sink<Message> + Unpin,
{
    let json = serde_json::to_string(message).map_err(|_| ())?;
    sink.send(Message::text(json)).await.map_err(|_| ())
}
