//! Minimal WebDriver BiDi WebSocket client.
//!
//! The socket / reader-task / writer-task / pending-correlation / timeout
//! machinery lives in the shared [`crate::transport`] (`WsRpc`); this module
//! supplies only the BiDi-specific framing/typing via the [`BidiProtocol`]
//! adapter and the convenience methods.

pub mod protocol;

use anyhow::{anyhow, Result};
use protocol::*;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};

use crate::errors::{is_bidi_target_gone, SessionError, TargetKind};
use crate::transport::{Decoded, Protocol, RequestError, WsRpc, REQUEST_TIMEOUT};

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

#[derive(Debug, Clone)]
pub struct BidiEvent {
    pub method: String,
    pub params: Value,
}

/// BiDi framing/typing adapter for the shared transport.
pub struct BidiProtocol;

impl Protocol for BidiProtocol {
    type ProtoError = BidiError;
    type Event = BidiEvent;

    fn encode_request(
        id: u64,
        method: &str,
        params: Value,
        _session_id: Option<&str>,
    ) -> Result<String> {
        let cmd = Command { id, method, params };
        Ok(serde_json::to_string(&cmd)?)
    }

    fn decode_frame(text: &str) -> Decoded<BidiError, BidiEvent> {
        match serde_json::from_str::<IncomingMessage>(text) {
            Ok(IncomingMessage::Success { id, result }) => Decoded::Reply {
                id,
                result: Ok(result),
            },
            Ok(IncomingMessage::Error { id, error, message }) => match id {
                Some(id) => Decoded::Reply {
                    id,
                    result: Err(BidiError {
                        code: error,
                        message,
                    }),
                },
                // Id-less error frames can't be correlated to a request.
                None => Decoded::Ignore,
            },
            Ok(IncomingMessage::Event { method, params }) => {
                Decoded::Event(BidiEvent { method, params })
            }
            Err(_) => Decoded::Ignore,
        }
    }

    fn closed_error() -> BidiError {
        BidiError {
            code: "connection closed".into(),
            message: "BiDi connection closed".into(),
        }
    }
}

pub struct BidiClient {
    rpc: WsRpc<BidiProtocol>,
    session_id: Mutex<Option<String>>,
}

impl BidiClient {
    pub async fn connect(ws_url: &str) -> Result<Self> {
        Ok(Self {
            rpc: WsRpc::connect(ws_url, "BiDi").await?,
            session_id: Mutex::new(None),
        })
    }

    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        match self.rpc.request(method, params, None).await {
            Ok(v) => Ok(v),
            Err(RequestError::Protocol(e)) => Err(classify_bidi_error(e)),
            Err(RequestError::Timeout) => Err(anyhow!(
                "BiDi request {method} timed out after {:?}",
                REQUEST_TIMEOUT
            )),
            Err(RequestError::Transport(e)) => Err(e),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BidiEvent> {
        self.rpc.subscribe()
    }

    /// Gracefully shut down the transport (flush writer, abort/join reader).
    /// Dropping the client also aborts the tasks via `WsRpc`'s `Drop`.
    pub async fn close(self) {
        self.rpc.close().await;
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

/// Convert a `BidiError` reply into a typed `SessionError::TargetGone` when
/// its code/message matches a known "gone" indicator (`no such frame`,
/// `no such context`, `invalid session id`), otherwise pass through as the
/// generic BiDi error.
fn classify_bidi_error(err: BidiError) -> anyhow::Error {
    if is_bidi_target_gone(&err.code, &err.message) {
        return SessionError::TargetGone {
            kind: TargetKind::Bidi,
            details: format!("BiDi error {}: {}", err.code, err.message),
        }
        .into();
    }
    err.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

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

    /// BiDi error with a "context gone" code surfaces as typed
    /// `SessionError::TargetGone`. Mirrors the CDP test so the same
    /// recovery wrappers can switch on the typed variant.
    #[tokio::test]
    async fn send_classifies_target_gone() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let v: Value = serde_json::from_str(&t).unwrap();
                let id = v["id"].as_u64().unwrap();
                let reply = json!({
                    "id": id,
                    "type": "error",
                    "error": "no such frame",
                    "message": "context C1 not found"
                });
                ws.send(Message::Text(reply.to_string())).await.unwrap();
            }
        });
        let client = BidiClient::connect(&format!("ws://{}", addr))
            .await
            .unwrap();
        let err = client
            .send("script.evaluate", json!({"target": {"context": "C1"}}))
            .await
            .expect_err("must error");
        let typed = err
            .downcast_ref::<crate::errors::SessionError>()
            .expect("typed SessionError");
        match typed {
            crate::errors::SessionError::TargetGone { kind, details } => {
                assert_eq!(*kind, crate::errors::TargetKind::Bidi);
                assert!(details.contains("no such frame"));
            }
            other => panic!("expected TargetGone, got {other:?}"),
        }
    }

    /// Unrelated BiDi errors (e.g. `invalid argument`) are NOT classified
    /// as `TargetGone` — they pass through as the regular `BidiError` so
    /// tab-recovery doesn't fire on schema mistakes.
    #[tokio::test]
    async fn send_does_not_classify_unrelated_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let v: Value = serde_json::from_str(&t).unwrap();
                let id = v["id"].as_u64().unwrap();
                let reply = json!({
                    "id": id,
                    "type": "error",
                    "error": "invalid argument",
                    "message": "missing required field"
                });
                ws.send(Message::Text(reply.to_string())).await.unwrap();
            }
        });
        let client = BidiClient::connect(&format!("ws://{}", addr))
            .await
            .unwrap();
        let err = client
            .send("script.evaluate", json!({}))
            .await
            .expect_err("must error");
        assert!(
            err.downcast_ref::<crate::errors::SessionError>().is_none(),
            "non-gone BiDi error must not classify as TargetGone"
        );
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
