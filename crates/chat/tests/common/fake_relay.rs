//! Relay Nostr falso, em processo e só para testes (127.0.0.1, porta livre). Guarda eventos, responde
//! `OK`, filtra `REQ` por `#p`, pode recusar por política, ficar mudo, exigir NIP-42 e derrubar as
//! conexões. Não valida nada além do que cada teste precisa observar.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use nostr::prelude::*;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Debug)]
pub enum Policy {
    Accept,
    /// Responde `OK false` com esta mensagem.
    Reject(String),
    /// Aceita a conexão e o `EVENT`, mas nunca responde.
    Silent,
}

#[derive(Default)]
struct State {
    events: Vec<Event>,
    reqs: Vec<Value>,
    policy: Option<Policy>,
    require_auth: bool,
}

pub struct FakeRelay {
    pub url: String,
    state: Arc<Mutex<State>>,
    live: broadcast::Sender<Event>,
    kick: broadcast::Sender<()>,
    open: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
}

impl FakeRelay {
    pub async fn start(policy: Policy) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            policy: Some(policy),
            ..Default::default()
        }));
        let (live, _) = broadcast::channel(64);
        let (kick, _) = broadcast::channel(8);
        let (open, accepted) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let relay = Self {
            url: url.clone(),
            state: state.clone(),
            live: live.clone(),
            kick: kick.clone(),
            open: open.clone(),
            accepted: accepted.clone(),
        };
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                accepted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(connection(
                    stream,
                    url.clone(),
                    state.clone(),
                    live.subscribe(),
                    kick.subscribe(),
                    open.clone(),
                ));
            }
        });
        relay
    }

    /// Exige NIP-42 antes de servir um `REQ`.
    pub fn require_auth(&self) {
        self.state.lock().unwrap().require_auth = true;
    }

    /// Guarda o evento e o entrega às assinaturas abertas (como se outro cliente o tivesse publicado).
    pub fn inject(&self, event: Event) {
        self.state.lock().unwrap().events.push(event.clone());
        let _ = self.live.send(event);
    }

    /// Fecha todas as conexões abertas; o relay continua aceitando novas.
    pub fn drop_connections(&self) {
        let _ = self.kick.send(());
    }

    pub fn stored(&self) -> Vec<Event> {
        self.state.lock().unwrap().events.clone()
    }

    /// Filtros dos `REQ` recebidos, na ordem.
    pub fn reqs(&self) -> Vec<Value> {
        self.state.lock().unwrap().reqs.clone()
    }

    pub fn open_connections(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    pub fn accepted_connections(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

/// URL de um endereço em que ninguém escuta.
pub async fn dead_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    format!("ws://{}", listener.local_addr().unwrap())
}

struct Open(Arc<AtomicUsize>);

impl Open {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn matches(filter: &Value, event: &Event) -> bool {
    let wanted = filter.get("#p").and_then(Value::as_array);
    let p_ok = wanted.is_none_or(|ps| {
        event.tags.iter().any(|t| {
            let t = t.as_slice();
            t.first().map(String::as_str) == Some("p")
                && t.get(1).is_some_and(|v| ps.iter().any(|p| p == v))
        })
    });
    let kind_ok = filter
        .get("kinds")
        .and_then(Value::as_array)
        .is_none_or(|ks| {
            ks.iter()
                .any(|k| k.as_u64() == Some(u64::from(event.kind.as_u16())))
        });
    p_ok && kind_ok
}

async fn connection(
    stream: tokio::net::TcpStream,
    url: String,
    state: Arc<Mutex<State>>,
    mut live: broadcast::Receiver<Event>,
    mut kick: broadcast::Receiver<()>,
    open: Arc<AtomicUsize>,
) {
    let _open = Open::new(open);
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut tx, mut rx) = ws.split();
    let challenge = format!("desafio-{}", rand::random::<u32>());
    if state.lock().unwrap().require_auth {
        let _ = tx
            .send(Message::text(json!(["AUTH", challenge]).to_string()))
            .await;
    }
    let mut authed: Option<String> = None;
    let mut subs: Vec<(String, Value)> = Vec::new();

    loop {
        tokio::select! {
            _ = kick.recv() => return,
            Ok(event) = live.recv() => {
                for (sub, filter) in &subs {
                    if matches(filter, &event) {
                        let _ = tx.send(Message::text(json!(["EVENT", sub, event]).to_string())).await;
                    }
                }
            }
            incoming = rx.next() => {
                let Some(Ok(Message::Text(text))) = incoming else { return };
                let Ok(Value::Array(msg)) = serde_json::from_str::<Value>(&text) else { continue };
                match msg.first().and_then(Value::as_str) {
                    Some("EVENT") => {
                        let Some(event) = msg.get(1).and_then(|v| Event::from_json(v.to_string()).ok()) else { continue };
                        let policy = state.lock().unwrap().policy.clone().unwrap_or(Policy::Accept);
                        match policy {
                            Policy::Silent => {}
                            Policy::Reject(why) => {
                                let _ = tx.send(Message::text(json!(["OK", event.id, false, why]).to_string())).await;
                            }
                            Policy::Accept => {
                                {
                                    let mut s = state.lock().unwrap();
                                    if !s.events.iter().any(|e| e.id == event.id) {
                                        s.events.push(event.clone());
                                    }
                                }
                                let _ = tx.send(Message::text(json!(["OK", event.id, true, ""]).to_string())).await;
                            }
                        }
                    }
                    Some("REQ") => {
                        let Some(sub) = msg.get(1).and_then(Value::as_str).map(str::to_owned) else { continue };
                        let filter = msg.get(2).cloned().unwrap_or(Value::Null);
                        state.lock().unwrap().reqs.push(filter.clone());
                        let need_auth = state.lock().unwrap().require_auth;
                        let wanted = filter.get("#p").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str).map(str::to_owned);
                        if need_auth && (authed.is_none() || authed != wanted) {
                            let _ = tx.send(Message::text(json!(["CLOSED", sub, "auth-required: autentique-se"]).to_string())).await;
                            continue;
                        }
                        subs.retain(|(s, _)| *s != sub);
                        let stored: Vec<Event> = state.lock().unwrap().events.iter().filter(|e| matches(&filter, e)).cloned().collect();
                        for event in stored {
                            let _ = tx.send(Message::text(json!(["EVENT", sub, event]).to_string())).await;
                        }
                        let _ = tx.send(Message::text(json!(["EOSE", sub]).to_string())).await;
                        subs.push((sub, filter));
                    }
                    Some("AUTH") => {
                        let Some(event) = msg.get(1).and_then(|v| Event::from_json(v.to_string()).ok()) else { continue };
                        let tag = |name: &str| event.tags.iter().find_map(|t| {
                            let t = t.as_slice();
                            (t.first().map(String::as_str) == Some(name)).then(|| t.get(1).cloned()).flatten()
                        });
                        let ok = event.kind == Kind::Authentication
                            && event.verify().is_ok()
                            && tag("challenge").as_deref() == Some(challenge.as_str())
                            && tag("relay").is_some_and(|r| r.trim_end_matches('/') == url.trim_end_matches('/'));
                        if ok { authed = Some(event.pubkey.to_hex()); }
                        let _ = tx.send(Message::text(json!(["OK", event.id, ok, if ok { "" } else { "auth inválido" }]).to_string())).await;
                    }
                    _ => {}
                }
            }
        }
    }
}
