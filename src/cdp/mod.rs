//! Minimal Chrome DevTools Protocol (CDP) WebSocket client.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

pub mod protocol;
use protocol::{CdpError, Request, Response};

use crate::errors::{is_cdp_target_gone, SessionError, TargetKind};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Bound on `connect_async` / HTTP discovery during initial CDP bringup.
///
/// A dead browser process or a stale `/devtools/browser/<GUID>` can otherwise
/// stall the WebSocket upgrade (or the underlying TCP connect) for the OS's
/// connect timeout — multiple seconds to over a minute on macOS/Linux. Five
/// seconds matches the `--version` probe in `crate::detect` and is short
/// enough that agents don't perceive a hang.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

type PendingMap = HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>;

#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

pub struct CdpClient {
    next_id: Mutex<u64>,
    pending: Arc<Mutex<PendingMap>>,
    events_tx: broadcast::Sender<CdpEvent>,
    write_tx: mpsc::UnboundedSender<String>,
    reader_handle: tokio::task::JoinHandle<()>,
    writer_handle: tokio::task::JoinHandle<()>,
}

impl CdpClient {
    /// Connect by full WebSocket URL (ws:// or wss://).
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let (ws_stream, _) =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(ws_url))
                .await
                .map_err(|_| {
                    anyhow!(
                        "CDP WebSocket connect to {ws_url} timed out after {:?}",
                        CONNECT_TIMEOUT
                    )
                })??;
        let (mut ws_sink, mut ws_stream) = ws_stream.split();

        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();

        let writer_handle = tokio::spawn(async move {
            while let Some(text) = write_rx.recv().await {
                if ws_sink.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            let _ = ws_sink.close().await;
        });

        let pending_r = pending.clone();
        let events_r = events_tx.clone();
        let reader_handle = tokio::spawn(async move {
            while let Some(msg) = ws_stream.next().await {
                let text = match msg {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Binary(b)) => match String::from_utf8(b) {
                        Ok(s) => s,
                        Err(_) => continue,
                    },
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                let resp: Response = match serde_json::from_str(&text) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if let Some(id) = resp.id {
                    let mut p = pending_r.lock().await;
                    if let Some(tx) = p.remove(&id) {
                        let res = if let Some(err) = resp.error {
                            Err(err)
                        } else {
                            Ok(resp.result)
                        };
                        let _ = tx.send(res);
                    }
                } else if let Some(method) = resp.method {
                    let _ = events_r.send(CdpEvent {
                        method,
                        params: resp.params,
                        session_id: resp.session_id,
                    });
                }
            }
            // Reader closed: fail all pending requests.
            let mut p = pending_r.lock().await;
            for (_, tx) in p.drain() {
                let _ = tx.send(Err(CdpError {
                    code: -1,
                    message: "connection closed".into(),
                }));
            }
        });

        Ok(Self {
            next_id: Mutex::new(1),
            pending,
            events_tx,
            write_tx,
            reader_handle,
            writer_handle,
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
        let resp: Value = client.get(&url).send().await?.json().await?;
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
        let id = {
            let mut n = self.next_id.lock().await;
            let id = *n;
            *n += 1;
            id
        };

        let req = Request {
            id,
            method,
            params,
            session_id: session_id.map(|s| s.to_string()),
        };
        let text = serde_json::to_string(&req)?;

        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().await;
            p.insert(id, tx);
        }

        if self.write_tx.send(text).is_err() {
            let mut p = self.pending.lock().await;
            p.remove(&id);
            return Err(anyhow!("writer task closed"));
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(classify_cdp_error(e, session_id.is_some())),
            Ok(Err(_)) => Err(anyhow!("response channel dropped")),
            Err(_) => {
                let mut p = self.pending.lock().await;
                p.remove(&id);
                Err(anyhow!("CDP request timed out after {:?}", REQUEST_TIMEOUT))
            }
        }
    }

    /// Subscribe to all events. Drop the receiver to unsubscribe.
    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events_tx.subscribe()
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

    /// Gracefully shut down.
    pub async fn close(self) {
        drop(self.write_tx);
        let _ = self.writer_handle.await;
        self.reader_handle.abort();
        let _ = self.reader_handle.await;
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
