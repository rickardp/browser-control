//! Per-target liveness tracking for a CDP browser-level client.
//!
//! Subscribes to the broadcast event stream from [`crate::cdp::CdpClient`] and
//! maintains a `target_id → TargetStatus` map driven by:
//!
//! - `Target.attachedToTarget` — seed entries; remember the `sessionId` mapping.
//! - `Target.targetCrashed`    — mark target `Crashed`.
//! - `Target.targetDestroyed`  — mark target `Destroyed`.
//! - `Target.detachedFromTarget` — drop the session mapping.
//! - `Inspector.targetCrashed` — per-session crash; resolved to a `target_id`
//!   via the session map (this event is the most reliable crash signal but
//!   arrives on the child session, not the root, hence the routing step).
//!
//! Each target exposes a [`tokio::sync::watch`] channel so a `LockedSession`
//! op can `tokio::select!` its upstream send against "this target just died"
//! and resolve immediately instead of waiting for a per-op timeout.
//!
//! This registry does **not** cover the *alive-but-unresponsive* case
//! (renderer up, doesn't service `Runtime.evaluate` — the iLO failure mode).
//! That has no event signal and is handled by per-op `timeoutMs` higher up.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::json;
use tokio::sync::{watch, Mutex};

use crate::cdp::{CdpClient, CdpEvent};

/// Per-target liveness state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetStatus {
    /// Target is alive (no crash event seen). The default state for any new
    /// target observed via `Target.attachedToTarget` or
    /// `Target.targetCreated`. Does NOT imply the renderer is responsive — a
    /// wedged page that doesn't run JS will still be `Alive` here; use the
    /// per-op timeout to catch that case.
    Alive,
    /// Renderer crashed (CDP `Target.targetCrashed` or
    /// `Inspector.targetCrashed`). `reason` is the protocol's `status` string
    /// (e.g. `"crashed"`, `"killed"`, `"oom"`); `error_code` mirrors
    /// `errorCode`.
    Crashed { reason: String, error_code: i32 },
    /// Target was removed (tab closed, renderer cleaned up, etc.). Terminal.
    Destroyed,
}

struct TargetEntry {
    status_tx: watch::Sender<TargetStatus>,
    session_id: Option<String>,
}

#[derive(Default)]
struct RegistryState {
    targets: HashMap<String, TargetEntry>,
    /// `session_id` → `target_id` lookup, used to route per-session events
    /// (`Inspector.targetCrashed`, `Inspector.detached`) back to the target.
    session_to_target: HashMap<String, String>,
}

/// Tracks per-target liveness for a CDP browser-level client.
///
/// Construct with [`TargetRegistry::attach`], which:
///
/// 1. Subscribes to the client's broadcast event stream.
/// 2. Issues `Target.setDiscoverTargets(true)` and
///    `Target.setAutoAttach({autoAttach: true, flatten: true})` so the
///    browser starts emitting attach/crash/detach events for every page and
///    worker.
/// 3. Seeds the map with the current `Target.getTargets` snapshot.
/// 4. Spawns a background task that consumes the event stream.
///
/// The returned registry handle is `Clone` and cheap to hand around.
#[derive(Clone)]
pub struct TargetRegistry {
    state: Arc<Mutex<RegistryState>>,
    _reader_handle: Arc<tokio::task::JoinHandle<()>>,
}

impl TargetRegistry {
    /// Subscribe to `client` and start tracking target liveness.
    pub async fn attach(client: Arc<CdpClient>) -> Result<Self> {
        // Subscribe BEFORE issuing the discover/auto-attach calls so we don't
        // race their initial attachedToTarget bursts.
        let mut events = client.subscribe();

        let state: Arc<Mutex<RegistryState>> = Arc::new(Mutex::new(RegistryState::default()));

        // Seed: list current targets (best-effort; ignore failure).
        if let Ok(targets) = client.list_targets().await {
            let mut s = state.lock().await;
            for t in targets {
                if let Some(id) = t.get("targetId").and_then(|v| v.as_str()) {
                    insert_alive(&mut s, id);
                }
            }
        }

        // Turn on the firehose. setDiscoverTargets fires targetCreated /
        // targetDestroyed for every target; setAutoAttach(flatten=true) fires
        // attachedToTarget / detachedFromTarget plus per-session
        // Inspector.targetCrashed.
        let _ = client
            .send("Target.setDiscoverTargets", json!({ "discover": true }))
            .await;
        let _ = client
            .send(
                "Target.setAutoAttach",
                json!({
                    "autoAttach": true,
                    "waitForDebugger": false,
                    "flatten": true,
                }),
            )
            .await;

        let state_reader = state.clone();
        let reader_handle = tokio::spawn(async move {
            while let Ok(ev) = events.recv().await {
                handle_event(&state_reader, ev).await;
            }
        });

        Ok(Self {
            state,
            _reader_handle: Arc::new(reader_handle),
        })
    }

    /// Current status of a target, or `None` if the registry has not seen it.
    pub async fn status(&self, target_id: &str) -> Option<TargetStatus> {
        let s = self.state.lock().await;
        s.targets
            .get(target_id)
            .map(|e| e.status_tx.borrow().clone())
    }

    /// Watch channel that fires when this target's status changes.
    ///
    /// Use with `tokio::select!` to race an upstream send against
    /// "this target just died." If the registry has not seen the target,
    /// returns `None` (caller should treat as unknown / not-attached).
    pub async fn watch(&self, target_id: &str) -> Option<watch::Receiver<TargetStatus>> {
        let s = self.state.lock().await;
        s.targets.get(target_id).map(|e| e.status_tx.subscribe())
    }

    /// Resolve a per-session event back to a target id.
    pub async fn target_for_session(&self, session_id: &str) -> Option<String> {
        let s = self.state.lock().await;
        s.session_to_target.get(session_id).cloned()
    }
}

fn insert_alive(state: &mut RegistryState, target_id: &str) {
    state
        .targets
        .entry(target_id.to_string())
        .or_insert_with(|| {
            let (tx, _rx) = watch::channel(TargetStatus::Alive);
            TargetEntry {
                status_tx: tx,
                session_id: None,
            }
        });
}

async fn handle_event(state: &Arc<Mutex<RegistryState>>, ev: CdpEvent) {
    let method = ev.method.as_str();
    let params = &ev.params;

    match method {
        "Target.attachedToTarget" => {
            let target_id = params
                .pointer("/targetInfo/targetId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let session_id = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(tid) = target_id {
                let mut s = state.lock().await;
                insert_alive(&mut s, &tid);
                if let Some(sid) = session_id {
                    if let Some(entry) = s.targets.get_mut(&tid) {
                        entry.session_id = Some(sid.clone());
                    }
                    s.session_to_target.insert(sid, tid);
                }
            }
        }
        "Target.targetCreated" => {
            if let Some(tid) = params
                .pointer("/targetInfo/targetId")
                .and_then(|v| v.as_str())
            {
                let mut s = state.lock().await;
                insert_alive(&mut s, tid);
            }
        }
        "Target.targetCrashed" => {
            let target_id = params.get("targetId").and_then(|v| v.as_str());
            let reason = params
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("crashed")
                .to_string();
            let error_code = params
                .get("errorCode")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;
            if let Some(tid) = target_id {
                mark(state, tid, TargetStatus::Crashed { reason, error_code }).await;
            }
        }
        "Inspector.targetCrashed" => {
            // Per-session crash. Route via sessionId in the envelope.
            let resolved = {
                let s = state.lock().await;
                ev.session_id
                    .as_deref()
                    .and_then(|sid| s.session_to_target.get(sid).cloned())
            };
            if let Some(tid) = resolved {
                mark(
                    state,
                    &tid,
                    TargetStatus::Crashed {
                        reason: "inspector_target_crashed".to_string(),
                        error_code: 0,
                    },
                )
                .await;
            }
        }
        "Target.detachedFromTarget" => {
            let session_id = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(sid) = session_id {
                let mut s = state.lock().await;
                if let Some(tid) = s.session_to_target.remove(&sid) {
                    if let Some(entry) = s.targets.get_mut(&tid) {
                        entry.session_id = None;
                    }
                }
            }
        }
        "Target.targetDestroyed" => {
            if let Some(tid) = params.get("targetId").and_then(|v| v.as_str()) {
                mark(state, tid, TargetStatus::Destroyed).await;
            }
        }
        _ => {}
    }
}

async fn mark(state: &Arc<Mutex<RegistryState>>, target_id: &str, status: TargetStatus) {
    let mut s = state.lock().await;
    let entry = s.targets.entry(target_id.to_string()).or_insert_with(|| {
        let (tx, _rx) = watch::channel(status.clone());
        TargetEntry {
            status_tx: tx,
            session_id: None,
        }
    });
    // send_replace returns the prior value; we ignore it.
    let _ = entry.status_tx.send(status);
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;

    /// Build a mock CDP server that:
    /// - responds to `Target.getTargets`, `Target.setDiscoverTargets`,
    ///   `Target.setAutoAttach` with empty results
    /// - holds the connection open until `evt_rx` fires a closure that
    ///   pushes an event message into the socket
    async fn spawn_mock_pushing<F>(
        targets: Vec<Value>,
        event_pusher: F,
    ) -> (String, oneshot::Sender<()>)
    where
        F: FnOnce(
                &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
            ) -> futures_util::future::BoxFuture<'_, ()>
            + Send
            + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (go_tx, go_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            // Drain initial requests until the test signals "go push event".
            // We'll handle a fixed set of bring-up calls inline.
            let mut got_targets = false;
            let mut got_discover = false;
            let mut got_auto_attach = false;
            loop {
                let msg = match ws.next().await {
                    Some(Ok(m)) => m,
                    _ => return,
                };
                if let Message::Text(t) = msg {
                    let req: Value = serde_json::from_str(&t).unwrap();
                    let id = req["id"].as_u64().unwrap();
                    let method = req["method"].as_str().unwrap_or("");
                    let result = match method {
                        "Target.getTargets" => {
                            got_targets = true;
                            json!({"targetInfos": targets.clone()})
                        }
                        "Target.setDiscoverTargets" => {
                            got_discover = true;
                            json!({})
                        }
                        "Target.setAutoAttach" => {
                            got_auto_attach = true;
                            json!({})
                        }
                        _ => json!({}),
                    };
                    let resp = json!({"id": id, "result": result});
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                    if got_targets && got_discover && got_auto_attach {
                        break;
                    }
                }
            }

            // Wait for the test to tell us to push the event.
            let _ = go_rx.await;
            event_pusher(&mut ws).await;

            // Keep the connection open for a bit so the registry has time
            // to ingest the event.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });
        (format!("ws://{addr}"), go_tx)
    }

    /// Test #12 (crash subset): Inspector.targetCrashed flips the watch
    /// channel to `Crashed` for the right target via session routing.
    #[tokio::test]
    async fn inspector_target_crashed_routes_via_session() {
        let targets = vec![json!({"targetId": "T1", "type": "page", "url": "https://x"})];
        let (url, go) = spawn_mock_pushing(targets, |ws| {
            Box::pin(async move {
                // First emit attachedToTarget so the registry learns the
                // sessionId → targetId mapping.
                let attached = json!({
                    "method": "Target.attachedToTarget",
                    "params": {
                        "sessionId": "S1",
                        "targetInfo": {"targetId": "T1", "type": "page"},
                        "waitingForDebugger": false,
                    },
                });
                ws.send(Message::Text(attached.to_string())).await.unwrap();
                // Then the per-session crash.
                let crashed = json!({
                    "method": "Inspector.targetCrashed",
                    "params": {},
                    "sessionId": "S1",
                });
                ws.send(Message::Text(crashed.to_string())).await.unwrap();
            })
        })
        .await;

        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = TargetRegistry::attach(client).await.unwrap();
        let mut watch_rx = reg.watch("T1").await.expect("T1 seeded from getTargets");
        go.send(()).unwrap();

        // Wait for the watcher to flip.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if !matches!(*watch_rx.borrow_and_update(), TargetStatus::Alive) {
                break;
            }
            tokio::select! {
                _ = watch_rx.changed() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
            }
            if std::time::Instant::now() > deadline {
                panic!("watch channel never flipped: {:?}", *watch_rx.borrow());
            }
        }

        let status = reg.status("T1").await.unwrap();
        match status {
            TargetStatus::Crashed { reason, .. } => {
                assert_eq!(reason, "inspector_target_crashed");
            }
            other => panic!("expected Crashed, got {other:?}"),
        }
    }

    /// `Target.targetDestroyed` marks the target Destroyed.
    #[tokio::test]
    async fn target_destroyed_marks_destroyed() {
        let targets = vec![json!({"targetId": "T2", "type": "page", "url": "https://x"})];
        let (url, go) = spawn_mock_pushing(targets, |ws| {
            Box::pin(async move {
                let destroyed = json!({
                    "method": "Target.targetDestroyed",
                    "params": {"targetId": "T2"},
                });
                ws.send(Message::Text(destroyed.to_string())).await.unwrap();
            })
        })
        .await;

        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = TargetRegistry::attach(client).await.unwrap();
        let mut watch_rx = reg.watch("T2").await.unwrap();
        go.send(()).unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if matches!(*watch_rx.borrow_and_update(), TargetStatus::Destroyed) {
                break;
            }
            tokio::select! {
                _ = watch_rx.changed() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
            }
            if std::time::Instant::now() > deadline {
                panic!("watch did not flip to Destroyed");
            }
        }
    }
}
