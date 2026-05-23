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

/// Bound on `connect_async` during initial BiDi bringup; see the matching
/// constant in `crate::cdp` for rationale.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Recognise the BiDi error returned when a fresh `session.new` is rejected
/// because a session already exists on the browser. Firefox reports this as
/// `session not created` with a "Maximum number of active sessions" message.
fn is_session_already_active(err: &anyhow::Error) -> bool {
    if let Some(b) = err.downcast_ref::<BidiError>() {
        let msg = b.message.to_ascii_lowercase();
        return b.code == "session not created"
            && (msg.contains("maximum number of active sessions")
                || msg.contains("session is already created"));
    }
    false
}

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
        let (ws, _resp) =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(ws_url))
                .await
                .map_err(|_| {
                    anyhow!(
                        "BiDi WebSocket connect to {ws_url} timed out after {:?}",
                        CONNECT_TIMEOUT
                    )
                })??;
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
        let v = match self.send("session.new", json!({"capabilities": {}})).await {
            Ok(v) => v,
            Err(e) if is_session_already_active(&e) => {
                // A previous BiDi session is still active on this browser
                // (e.g. a prior CLI run exited without calling session.end).
                // Firefox limits a browser to one session at a time, so end
                // the stuck one and retry once before giving up.
                tracing::warn!(
                    target = "bidi",
                    "session.new rejected (active session exists); ending and retrying",
                );
                let _ = self.send("session.end", json!({})).await;
                self.send("session.new", json!({"capabilities": {}}))
                    .await?
            }
            Err(e) => return Err(e),
        };
        let sid = v["sessionId"]
            .as_str()
            .ok_or_else(|| anyhow!("no sessionId"))?
            .to_string();
        *self.session_id.lock().await = Some(sid.clone());
        Ok(sid)
    }

    pub async fn session_end(&self) -> Result<()> {
        // Best effort: ignore errors if no session is active.
        let _ = self.send("session.end", json!({})).await;
        *self.session_id.lock().await = None;
        Ok(())
    }

    pub async fn browsing_context_navigate(&self, context: &str, url: &str) -> Result<Value> {
        self.send(
            "browsingContext.navigate",
            json!({"context": context, "url": url, "wait": "complete"}),
        )
        .await
    }

    /// `browsingContext.create({type: "tab"})` — opens a fresh top-level
    /// browsing context. If `url` is non-empty, navigates after create so
    /// the returned context lands at the desired URL.
    pub async fn browsing_context_create(&self, url: &str) -> Result<String> {
        let v = self
            .send("browsingContext.create", json!({"type": "tab"}))
            .await?;
        let context = v["context"]
            .as_str()
            .ok_or_else(|| anyhow!("browsingContext.create returned no context"))?
            .to_string();
        if !url.is_empty() && url != "about:blank" {
            self.browsing_context_navigate(&context, url).await?;
        }
        Ok(context)
    }

    /// `browsingContext.close({context})`. Idempotent against an already-
    /// closed context (BiDi returns an error but the caller's intent is
    /// satisfied).
    pub async fn browsing_context_close(&self, context: &str) -> Result<()> {
        let _ = self
            .send("browsingContext.close", json!({"context": context}))
            .await;
        Ok(())
    }

    /// `browsingContext.getTree()` flattened to a set of all live top-level
    /// context ids. Used by the engine-agnostic tab registry for
    /// sweep-on-read.
    pub async fn browsing_context_ids(&self) -> Result<std::collections::HashSet<String>> {
        let v = self.send("browsingContext.getTree", json!({})).await?;
        let contexts = v
            .get("contexts")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(contexts
            .iter()
            .filter_map(|c| c.get("context").and_then(|x| x.as_str()).map(String::from))
            .collect())
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

    #[test]
    fn detects_firefox_active_session_error() {
        let e: anyhow::Error = BidiError {
            code: "session not created".to_string(),
            message: "Maximum number of active sessions.".to_string(),
        }
        .into();
        assert!(is_session_already_active(&e));

        let other: anyhow::Error = BidiError {
            code: "invalid argument".to_string(),
            message: "Maximum number of active sessions".to_string(),
        }
        .into();
        assert!(!is_session_already_active(&other));

        let unrelated: anyhow::Error = anyhow!("not a bidi error");
        assert!(!is_session_already_active(&unrelated));
    }

    #[tokio::test]
    async fn session_new_retries_after_active_session_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let attempts = AtomicUsize::new(0);
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let v: Value = serde_json::from_str(&text).unwrap();
                let id = v["id"].as_u64().unwrap();
                let method = v["method"].as_str().unwrap();
                let reply = match method {
                    "session.new" => {
                        let n = attempts.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            json!({
                                "id": id,
                                "type": "error",
                                "error": "session not created",
                                "message": "Maximum number of active sessions."
                            })
                        } else {
                            json!({
                                "id": id,
                                "type": "success",
                                "result": {"sessionId": "S2"}
                            })
                        }
                    }
                    "session.end" => json!({"id": id, "type": "success", "result": {}}),
                    _ => json!({"id": id, "type": "success", "result": {}}),
                };
                ws.send(Message::Text(reply.to_string())).await.unwrap();
            }
        });
        let client = BidiClient::connect(&format!("ws://{}", addr))
            .await
            .unwrap();
        let sid = client.session_new().await.unwrap();
        assert_eq!(sid, "S2");
    }
}
