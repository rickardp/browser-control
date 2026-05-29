//! CDP renderer-crash detection.
//!
//! Background: the recover-once wrappers (`with_scratch_recovery`,
//! `with_named_tab_recovery`, bare-fetch recovery) classify
//! `SessionError::TabCrashed` as recoverable — but until this module, no
//! code path actually constructed that variant. The protocol-error
//! classifier converts post-mortem "no target with given id" responses
//! into `TargetGone`, which is fine for the *next* call but means an
//! in-flight `Runtime.evaluate` against a crashing renderer only ever
//! surfaces as `TabHung` after the timeout expires.
//!
//! This module fills the gap: while a CDP evaluate is in flight, watch
//! the event stream for the renderer-crash signal and short-circuit the
//! in-flight call with a typed `TabCrashed` immediately. The two
//! observable crash signals are:
//!
//! - `Target.targetCrashed` — browser-level event with
//!   `{targetId, status, errorCode}`. Requires
//!   `Target.setDiscoverTargets({discover:true})` on the connection.
//! - `Inspector.targetCrashed` — per-attached-session event (no params).
//!   Requires `Inspector.enable` on the attached session.
//!
//! We listen for both and match on `session_id` (Inspector) or
//! `targetId` (Target). BiDi has no equivalent protocol event — there,
//! a context crash surfaces as `no such frame/context` on the next
//! request, which the `TargetGone` classifier already handles.

use std::time::Duration;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::cdp::{CdpClient, CdpEvent};
use crate::errors::SessionError;

/// Run `fut` (a CDP request) while watching the client's event stream
/// for a renderer-crash signal that matches `target_id` (browser-level
/// `Target.targetCrashed`) or `session_id` (per-session
/// `Inspector.targetCrashed`). On a matching crash event, return
/// `SessionError::TabCrashed` instead of waiting for the request to
/// time out.
///
/// `timeout` bounds the whole operation; on expiry returns
/// `SessionError::TabHung`. Pass `None` to defer bounding to the caller
/// (matches the unbounded path in `PageSession::evaluate`).
pub async fn evaluate_with_crash_detection<Fut>(
    client: &CdpClient,
    target_id: &str,
    session_id: Option<&str>,
    fut: Fut,
    timeout: Option<Duration>,
) -> Result<Value>
where
    Fut: std::future::Future<Output = Result<Value>>,
{
    let events = client.subscribe();
    let crash_watch = watch_for_crash(events, target_id.to_string(), session_id.map(str::to_string));
    tokio::pin!(fut);
    tokio::pin!(crash_watch);

    let race = async {
        tokio::select! {
            biased;
            crash = &mut crash_watch => Err::<Value, anyhow::Error>(crash),
            result = &mut fut => result,
        }
    };

    match timeout {
        None => race.await,
        Some(d) => match tokio::time::timeout(d, race).await {
            Ok(r) => r,
            Err(_) => Err(SessionError::TabHung {
                target_id: Some(target_id.to_string()),
                url: None,
                timeout_ms: d.as_millis() as u64,
                hint: "op-timeout",
            }
            .into()),
        },
    }
}

/// Drain events from `rx` until we see a renderer-crash signal whose
/// scope matches our target or session. Returns the typed
/// `SessionError::TabCrashed` ready to escalate to the caller.
async fn watch_for_crash(
    mut rx: broadcast::Receiver<CdpEvent>,
    target_id: String,
    session_id: Option<String>,
) -> anyhow::Error {
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if matches_crash(&ev, &target_id, session_id.as_deref()) {
                    let reason = crash_reason(&ev.params);
                    return SessionError::TabCrashed {
                        target_id: target_id.clone(),
                        reason,
                    }
                    .into();
                }
            }
            // Lagged: an old event we'd care about may have been dropped.
            // Keep listening rather than fabricating a crash.
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            // Channel closed — the connection is dead. We have no crash
            // signal to report, and synthesising `TabCrashed` here would
            // mislabel a normal disconnect as a crash and (via the biased
            // select in `evaluate_with_crash_detection`) beat the real I/O
            // error. Instead, never resolve: let the in-flight future lose
            // its transport and surface the underlying error itself.
            Err(broadcast::error::RecvError::Closed) => {
                std::future::pending::<()>().await;
                unreachable!("pending future never resolves");
            }
        }
    }
}

fn matches_crash(ev: &CdpEvent, target_id: &str, session_id: Option<&str>) -> bool {
    match ev.method.as_str() {
        // Browser-level: payload identifies the dead target.
        "Target.targetCrashed" => ev
            .params
            .get("targetId")
            .and_then(|v| v.as_str())
            .map(|t| t == target_id)
            .unwrap_or(false),
        // Per-session: event arrives on the session we attached to.
        "Inspector.targetCrashed" => match (session_id, ev.session_id.as_deref()) {
            (Some(want), Some(got)) => want == got,
            // No attached session to match against; conservatively skip.
            _ => false,
        },
        _ => false,
    }
}

fn crash_reason(params: &Value) -> String {
    let status = params.get("status").and_then(|v| v.as_str());
    let code = params.get("errorCode").and_then(|v| v.as_i64());
    match (status, code) {
        (Some(s), Some(c)) => format!("status={s} errorCode={c}"),
        (Some(s), None) => format!("status={s}"),
        (None, Some(c)) => format!("errorCode={c}"),
        _ => "renderer crash".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;

    /// Mock CDP server that:
    /// - answers `Inspector.enable` and `Target.setDiscoverTargets` immediately
    /// - holds `Runtime.evaluate` indefinitely (no response)
    /// - after `crash_delay`, pushes a `Target.targetCrashed` event for `target_id`
    async fn spawn_crashing_mock(
        target_id: &'static str,
        crash_delay: Duration,
    ) -> (String, oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // Push the crash event after `crash_delay`.
            let (tx_crash, mut rx_crash) = tokio::sync::mpsc::channel::<()>(1);
            tokio::spawn(async move {
                tokio::time::sleep(crash_delay).await;
                let _ = tx_crash.send(()).await;
            });
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    _ = rx_crash.recv() => {
                        let ev = json!({
                            "method": "Target.targetCrashed",
                            "params": {"targetId": target_id, "status": "crashed", "errorCode": 11},
                        });
                        ws.send(Message::Text(ev.to_string())).await.unwrap();
                    }
                    msg = ws.next() => {
                        let msg = match msg { Some(Ok(m)) => m, _ => break };
                        if let Message::Text(t) = msg {
                            let req: Value = serde_json::from_str(&t).unwrap();
                            let id = req["id"].as_u64().unwrap();
                            let method = req["method"].as_str().unwrap_or("");
                            // Hold Runtime.evaluate indefinitely; answer everything else.
                            if method == "Runtime.evaluate" { continue; }
                            let resp = json!({"id": id, "result": {}});
                            ws.send(Message::Text(resp.to_string())).await.unwrap();
                        }
                    }
                }
            }
        });
        (format!("ws://{addr}"), stop_tx)
    }

    #[tokio::test]
    async fn returns_tab_crashed_when_target_crashed_event_fires() {
        let (url, _stop) = spawn_crashing_mock("T1", Duration::from_millis(50)).await;
        let client = CdpClient::connect(&url).await.unwrap();
        let fut = async {
            client
                .send_with_session("Runtime.evaluate", json!({}), Some("S1"))
                .await
        };
        let err = evaluate_with_crash_detection(
            &client,
            "T1",
            Some("S1"),
            fut,
            Some(Duration::from_secs(2)),
        )
        .await
        .expect_err("must surface TabCrashed");
        match err.downcast_ref::<SessionError>() {
            Some(SessionError::TabCrashed { target_id, reason }) => {
                assert_eq!(target_id, "T1");
                assert!(reason.contains("crashed"), "reason: {reason}");
            }
            other => panic!("expected TabCrashed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ignores_crash_events_for_other_targets() {
        // Crash event is for T1, but we're watching T2 — must NOT trip.
        let (url, _stop) = spawn_crashing_mock("T1", Duration::from_millis(20)).await;
        let client = CdpClient::connect(&url).await.unwrap();
        let fut = async {
            client
                .send_with_session("Runtime.evaluate", json!({}), Some("Sx"))
                .await
        };
        let err = evaluate_with_crash_detection(
            &client,
            "T2",
            Some("Sx"),
            fut,
            Some(Duration::from_millis(200)),
        )
        .await
        .expect_err("times out, since we don't match the foreign crash");
        match err.downcast_ref::<SessionError>() {
            Some(SessionError::TabHung { .. }) => {}
            other => panic!("expected TabHung (foreign crash ignored), got {other:?}"),
        }
    }
}
