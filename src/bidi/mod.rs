//! Minimal WebDriver BiDi WebSocket client.

pub mod protocol;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use protocol::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

const SEND_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_CHANNEL_CAPACITY: usize = 256;

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, BidiError>>>>>;

#[derive(Debug, Clone)]
pub struct BidiEvent {
    pub method: String,
    pub params: Value,
}

pub struct BidiClient {
    next_id: Mutex<u64>,
    pending: PendingMap,
    events_tx: broadcast::Sender<BidiEvent>,
    write_tx: mpsc::UnboundedSender<String>,
    session_id: Mutex<Option<String>>,
}

impl BidiClient {
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let (ws, _resp) = tokio_tungstenite::connect_async(ws_url).await?;
        let (mut sink, mut stream) = ws.split();

        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();
        let (events_tx, _) = broadcast::channel::<BidiEvent>(EVENT_CHANNEL_CAPACITY);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        // Writer task.
        tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                if sink.send(Message::Text(msg)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        // Reader task.
        let pending_reader = pending.clone();
        let events_reader = events_tx.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Binary(b) => match String::from_utf8(b) {
                        Ok(s) => s,
                        Err(_) => continue,
                    },
                    Message::Close(_) => break,
                    _ => continue,
                };
                let parsed: Result<IncomingMessage, _> = serde_json::from_str(&text);
                match parsed {
                    Ok(IncomingMessage::Success { id, result }) => {
                        if let Some(tx) = pending_reader.lock().await.remove(&id) {
                            let _ = tx.send(Ok(result));
                        }
                    }
                    Ok(IncomingMessage::Error { id, error, message }) => {
                        if let Some(id) = id {
                            if let Some(tx) = pending_reader.lock().await.remove(&id) {
                                let _ = tx.send(Err(BidiError {
                                    code: error,
                                    message,
                                }));
                            }
                        }
                    }
                    Ok(IncomingMessage::Event { method, params }) => {
                        let _ = events_reader.send(BidiEvent { method, params });
                    }
                    Err(_) => continue,
                }
            }
            pending_reader.lock().await.clear();
        });

        Ok(Self {
            next_id: Mutex::new(1),
            pending,
            events_tx,
            write_tx,
            session_id: Mutex::new(None),
        })
    }

    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        let id = {
            let mut guard = self.next_id.lock().await;
            let id = *guard;
            *guard += 1;
            id
        };
        let cmd = Command { id, method, params };
        let text = serde_json::to_string(&cmd)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        self.write_tx
            .send(text)
            .map_err(|_| anyhow!("BiDi connection closed"))?;

        match tokio::time::timeout(SEND_TIMEOUT, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(e.into()),
            Ok(Err(_)) => Err(anyhow!("BiDi response channel cancelled")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(anyhow!("BiDi send timed out after {:?}", SEND_TIMEOUT))
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BidiEvent> {
        self.events_tx.subscribe()
    }

    pub async fn session_new(&self) -> Result<String> {
        let v = self
            .send("session.new", json!({"capabilities": {}}))
            .await?;
        let sid = v["sessionId"]
            .as_str()
            .ok_or_else(|| anyhow!("no sessionId"))?
            .to_string();
        *self.session_id.lock().await = Some(sid.clone());
        Ok(sid)
    }

    pub async fn browsing_context_navigate(&self, context: &str, url: &str) -> Result<Value> {
        self.send(
            "browsingContext.navigate",
            json!({"context": context, "url": url, "wait": "complete"}),
        )
        .await
    }

    pub async fn script_evaluate(&self, context: &str, expression: &str) -> Result<Value> {
        self.send(
            "script.evaluate",
            json!({
                "expression": expression,
                "target": {"context": context},
                "awaitPromise": true,
                "resultOwnership": "none"
            }),
        )
        .await
    }

    pub async fn browsing_context_capture_screenshot(&self, context: &str) -> Result<String> {
        let v = self
            .send(
                "browsingContext.captureScreenshot",
                json!({"context": context}),
            )
            .await?;
        Ok(v["data"]
            .as_str()
            .ok_or_else(|| anyhow!("no data"))?
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    async fn spawn_echo_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let mut ws = accept_async(stream).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(text) = msg {
                        let v: Value = serde_json::from_str(&text).unwrap();
                        let id = v["id"].as_u64().unwrap();
                        let method = v["method"].as_str().unwrap().to_string();
                        let reply = json!({
                            "id": id,
                            "type": "success",
                            "result": {"echoed": method}
                        });
                        ws.send(Message::Text(reply.to_string())).await.unwrap();
                    }
                }
            }
        });
        format!("ws://{}", addr)
    }

    #[tokio::test]
    async fn send_receives_success_result() {
        let url = spawn_echo_server().await;
        let client = BidiClient::connect(&url).await.unwrap();
        let result = client.send("session.status", json!({})).await.unwrap();
        assert_eq!(result["echoed"], "session.status");
    }

    #[tokio::test]
    async fn subscriber_receives_event() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let event = json!({
                "type": "event",
                "method": "log.entryAdded",
                "params": {"text": "hello"}
            });
            ws.send(Message::Text(event.to_string())).await.unwrap();
            while ws.next().await.is_some() {}
        });
        let url = format!("ws://{}", addr);
        let client = BidiClient::connect(&url).await.unwrap();
        let mut rx = client.subscribe();
        let evt = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.method, "log.entryAdded");
        assert_eq!(evt.params["text"], "hello");
    }
}
