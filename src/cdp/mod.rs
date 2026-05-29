//! Minimal Chrome DevTools Protocol (CDP) WebSocket client.
//!
//! The socket / reader-task / writer-task / pending-correlation / timeout
//! machinery lives in the shared [`crate::transport`] (`WsRpc`); this module
//! supplies only the CDP-specific framing/typing via the [`CdpProtocol`]
//! adapter and the convenience methods.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tokio::sync::broadcast;

pub mod protocol;
use protocol::{CdpError, Request, Response};

use crate::errors::{is_cdp_target_gone, SessionError, TargetKind};
use crate::transport::{Decoded, Protocol, RequestError, WsRpc, CONNECT_TIMEOUT, REQUEST_TIMEOUT};

#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

/// CDP framing/typing adapter for the shared transport.
pub struct CdpProtocol;

impl Protocol for CdpProtocol {
    type ProtoError = CdpError;
    type Event = CdpEvent;

    fn encode_request(
        id: u64,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<String> {
        let req = Request {
            id,
            method,
            params,
            session_id: session_id.map(|s| s.to_string()),
        };
        Ok(serde_json::to_string(&req)?)
    }

    fn decode_frame(text: &str) -> Decoded<CdpError, CdpEvent> {
        let resp: Response = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(_) => return Decoded::Ignore,
        };
        if let Some(id) = resp.id {
            let result = if let Some(err) = resp.error {
                Err(err)
            } else {
                Ok(resp.result)
            };
            Decoded::Reply { id, result }
        } else if let Some(method) = resp.method {
            Decoded::Event(CdpEvent {
                method,
                params: resp.params,
                session_id: resp.session_id,
            })
        } else {
            Decoded::Ignore
        }
    }

    fn closed_error() -> CdpError {
        CdpError {
            code: -1,
            message: "connection closed".into(),
        }
    }
}

pub struct CdpClient {
    rpc: WsRpc<CdpProtocol>,
}

impl CdpClient {
    /// Connect by full WebSocket URL (ws:// or wss://).
    pub async fn connect(ws_url: &str) -> Result<Self> {
        Ok(Self {
            rpc: WsRpc::connect(ws_url, "CDP").await?,
        })
    }

    /// Connect by HTTP base URL (e.g. http://127.0.0.1:9222). Fetches /json/version to discover the WS URL.
    pub async fn connect_http(base_url: &str) -> Result<Self> {
        let base = base_url.trim_end_matches('/');
        let url = format!("{base}/json/version");
        let client = reqwest::Client::builder()
            .timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| anyhow!("building reqwest client: {e}"))?;
        // Check the HTTP status before parsing: a non-2xx (wrong port / a
        // non-CDP server answering) otherwise surfaces as a confusing serde
        // error or "webSocketDebuggerUrl missing" instead of the real cause.
        let http_resp = client
            .get(&url)
            .send()
            .await?
            .error_for_status()
            .map_err(|e| anyhow!("fetching {url}: {e}"))?;
        let resp: Value = http_resp.json().await?;
        let ws_url = resp
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("webSocketDebuggerUrl missing from {url}"))?
            .to_string();
        Self::connect(&ws_url).await
    }

    /// Send a method on the root browser-level session.
    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        self.send_with_session(method, params, None).await
    }

    pub async fn send_with_session(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value> {
        match self.rpc.request(method, params, session_id).await {
            Ok(v) => Ok(v),
            Err(RequestError::Protocol(e)) => Err(classify_cdp_error(e, session_id.is_some())),
            Err(RequestError::Timeout) => match session_id {
                Some(sid) => Err(anyhow!(
                    "CDP request {method} (session {sid}) timed out after {:?}",
                    REQUEST_TIMEOUT
                )),
                None => Err(anyhow!(
                    "CDP request {method} timed out after {:?}",
                    REQUEST_TIMEOUT
                )),
            },
            Err(RequestError::Transport(e)) => Err(e),
        }
    }

    /// Subscribe to all events. Drop the receiver to unsubscribe.
    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.rpc.subscribe()
    }

    /// Attach to a target via Target.attachToTarget(flatten=true) and return the session id.
    pub async fn attach_to_target(&self, target_id: &str) -> Result<String> {
        let v = self
            .send(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        v.get("sessionId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("sessionId missing from Target.attachToTarget response"))
    }

    /// Convenience: list targets via Target.getTargets.
    pub async fn list_targets(&self) -> Result<Vec<Value>> {
        let v = self.send("Target.getTargets", Value::Null).await?;
        match v.get("targetInfos") {
            Some(Value::Array(a)) => Ok(a.clone()),
            _ => Ok(vec![]),
        }
    }

    /// Gracefully shut down. Dropping the client also aborts the tasks via
    /// `WsRpc`'s `Drop`, so this is the explicit-flush path.
    pub async fn close(self) {
        self.rpc.close().await;
    }
}

/// Convert a `CdpError` reply into a typed `SessionError::TargetGone` if
/// its message matches a known "gone" indicator, otherwise pass through as
/// the generic CDP error. `attached` is true when the call carried a
/// `sessionId` — only attached-session failures are classified, because
/// browser-session errors typically mean "bad request," not "target gone."
fn classify_cdp_error(err: CdpError, attached: bool) -> anyhow::Error {
    if attached && is_cdp_target_gone(&err.message) {
        return SessionError::TargetGone {
            kind: TargetKind::Cdp,
            details: format!("CDP error {}: {}", err.code, err.message),
        }
        .into();
    }
    anyhow!(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;

    #[tokio::test]
    async fn round_trip_request_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(t) = msg {
                    let req: Value = serde_json::from_str(&t).unwrap();
                    let id = req["id"].as_u64().unwrap();
                    let resp = json!({"id": id, "result": {"ok": true, "echo": req["method"]}});
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                }
            }
        });
        let url = format!("ws://{}", addr);
        let client = CdpClient::connect(&url).await.unwrap();
        let v = client
            .send("Page.navigate", json!({"url": "about:blank"}))
            .await
            .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["echo"], "Page.navigate");
        client.close().await;
    }

    #[tokio::test]
    async fn broadcast_event_to_subscriber() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // Wait until the test confirms it has subscribed before pushing event.
            let _ = ready_rx.await;
            let evt = json!({
                "method": "Target.targetCreated",
                "params": {"targetInfo": {"targetId": "abc"}},
                "sessionId": "S1"
            });
            ws.send(Message::Text(evt.to_string())).await.unwrap();
            // Keep socket alive briefly.
            while let Some(Ok(_)) = ws.next().await {}
        });

        let url = format!("ws://{}", addr);
        let client = CdpClient::connect(&url).await.unwrap();
        let mut rx = client.subscribe();
        ready_tx.send(()).unwrap();

        let evt = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("event timeout")
            .expect("event recv");
        assert_eq!(evt.method, "Target.targetCreated");
        assert_eq!(evt.session_id.as_deref(), Some("S1"));
        assert_eq!(evt.params["targetInfo"]["targetId"], "abc");
        client.close().await;
    }

    /// Attached-session CDP error matching a "target gone" indicator
    /// surfaces as a typed `SessionError::TargetGone`. The recovery
    /// wrappers rely on the typed variant to skip substring matching.
    #[tokio::test]
    async fn send_with_session_classifies_target_gone() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let req: Value = serde_json::from_str(&t).unwrap();
                let id = req["id"].as_u64().unwrap();
                let resp = json!({
                    "id": id,
                    "error": {"code": -32000, "message": "No target with given id found: T42"}
                });
                ws.send(Message::Text(resp.to_string())).await.unwrap();
            }
        });
        let client = CdpClient::connect(&format!("ws://{addr}")).await.unwrap();
        let err = client
            .send_with_session("Runtime.evaluate", json!({}), Some("S1"))
            .await
            .expect_err("must error");
        let typed = err
            .downcast_ref::<crate::errors::SessionError>()
            .expect("typed SessionError");
        match typed {
            crate::errors::SessionError::TargetGone { kind, details } => {
                assert_eq!(*kind, crate::errors::TargetKind::Cdp);
                assert!(details.contains("No target with given id"));
            }
            other => panic!("expected TargetGone, got {other:?}"),
        }
        client.close().await;
    }

    /// Browser-session CDP errors (no `sessionId`) are NOT classified —
    /// they pass through as generic anyhow errors. `Target.attachToTarget`
    /// failing with "no such target" is a routing problem at the browser
    /// level, not a wedged renderer, and shouldn't trigger tab recovery.
    #[tokio::test]
    async fn send_root_does_not_classify_target_gone() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let req: Value = serde_json::from_str(&t).unwrap();
                let id = req["id"].as_u64().unwrap();
                let resp = json!({
                    "id": id,
                    "error": {"code": -32000, "message": "No target with given id found: T42"}
                });
                ws.send(Message::Text(resp.to_string())).await.unwrap();
            }
        });
        let client = CdpClient::connect(&format!("ws://{addr}")).await.unwrap();
        let err = client
            .send("Target.attachToTarget", json!({}))
            .await
            .expect_err("must error");
        assert!(
            err.downcast_ref::<crate::errors::SessionError>().is_none(),
            "root-session error must NOT classify as TargetGone"
        );
        client.close().await;
    }

    /// Test #15: connect-side timeout fires when the WS upgrade hangs.
    ///
    /// The TCP listener accepts the connection but never writes the HTTP
    /// upgrade response, so `tokio_tungstenite::connect_async` would wait
    /// indefinitely without the bound.
    #[tokio::test]
    async fn connect_times_out_when_upgrade_hangs() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Hold the accepted connection forever (no HTTP response).
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let url = format!("ws://{addr}");
        let start = std::time::Instant::now();
        let err = match CdpClient::connect(&url).await {
            Ok(_) => panic!("connect must fail when upgrade hangs"),
            Err(e) => e,
        };
        let elapsed = start.elapsed();

        // Must fail within the bound + a generous slack for CI variance.
        assert!(
            elapsed < CONNECT_TIMEOUT + Duration::from_secs(2),
            "connect did not honour the 5s bound (took {elapsed:?})"
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains("timed out"),
            "error should mention timeout, got: {msg}"
        );
    }
}
