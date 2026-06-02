//! Async WebDriver BiDi client.
//!
//! One WebSocket connection multiplexes many in-flight commands. Each command
//! gets an auto-incrementing `id`; the reader task matches responses back to the
//! waiting caller via a `oneshot`. Events (`type: "event"`) are fanned out to an
//! `events` channel for subscribers (e.g. waiting for navigation/load).

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// A live BiDi WebSocket connection.
pub struct BidiClient {
    next_id: AtomicU64,
    cmd_tx: mpsc::UnboundedSender<Message>,
    pending: Pending,
    /// Broadcast of every `type: "event"` message received. Reserved for
    /// event-driven waits (navigation/load), not yet wired into the CLI.
    #[allow(dead_code)]
    events_tx: tokio::sync::broadcast::Sender<Value>,
}

impl BidiClient {
    /// Connect to a BiDi endpoint (e.g. `ws://127.0.0.1:9222/session`).
    pub async fn connect(url: &str) -> Result<Arc<Self>> {
        let (ws, _) = connect_async(url)
            .await
            .map_err(|e| anyhow!("failed to connect to BiDi endpoint {url}: {e}"))?;
        let (mut write, mut read) = ws.split();

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Message>();
        let (events_tx, _) = tokio::sync::broadcast::channel::<Value>(1024);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        // Single writer task owns the sink.
        tokio::spawn(async move {
            while let Some(msg) = cmd_rx.recv().await {
                if write.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // Reader task routes responses and events.
        let pending_r = pending.clone();
        let events_r = events_tx.clone();
        tokio::spawn(async move {
            while let Some(next) = read.next().await {
                let msg = match next {
                    Ok(m) => m,
                    Err(_) => break,
                };
                if let Message::Text(txt) = msg {
                    if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                        route(v, &pending_r, &events_r).await;
                    }
                }
            }
            // Socket closed: fail everyone still waiting.
            let mut p = pending_r.lock().await;
            for (_, tx) in p.drain() {
                let _ = tx.send(Err("BiDi connection closed".into()));
            }
        });

        Ok(Arc::new(Self {
            next_id: AtomicU64::new(1),
            cmd_tx,
            pending,
            events_tx,
        }))
    }

    /// Subscribe to the raw event stream. Reserved for event-driven waits.
    #[allow(dead_code)]
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Value> {
        self.events_tx.subscribe()
    }

    /// Send a command and await its result, mapping BiDi errors to `Err`.
    pub async fn send(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let envelope = json!({ "id": id, "method": method, "params": params });
        self.cmd_tx
            .send(Message::Text(envelope.to_string().into()))
            .map_err(|_| "BiDi writer is gone".to_string())?;

        rx.await
            .map_err(|_| "BiDi response channel dropped".to_string())?
    }
}

/// Route one decoded message to either a pending command or the event stream.
async fn route(v: Value, pending: &Pending, events: &tokio::sync::broadcast::Sender<Value>) {
    match v.get("type").and_then(Value::as_str) {
        Some("success") => {
            if let Some(id) = v.get("id").and_then(Value::as_u64) {
                if let Some(tx) = pending.lock().await.remove(&id) {
                    let _ = tx.send(Ok(v.get("result").cloned().unwrap_or(Value::Null)));
                }
            }
        }
        Some("error") => {
            if let Some(id) = v.get("id").and_then(Value::as_u64) {
                if let Some(tx) = pending.lock().await.remove(&id) {
                    let kind = v.get("error").and_then(Value::as_str).unwrap_or("unknown error");
                    let msg = v.get("message").and_then(Value::as_str).unwrap_or("");
                    let _ = tx.send(Err(format!("{kind}: {msg}")));
                }
            }
        }
        Some("event") => {
            let _ = events.send(v);
        }
        _ => {}
    }
}
