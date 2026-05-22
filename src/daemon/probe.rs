//! Cheap pre-flight liveness probe for CDP targets.
//!
//! A bound `LockedSession` is committed to a single target for its lifetime,
//! so the moment of binding is when we want to discover wedged renderers —
//! before the agent issues an op that would block for `timeoutMs`. The probe
//! is a single `Runtime.evaluate("1")` over the target's session, raced
//! against a tight bound (default 500 ms). A target that does not reply in
//! that window is `TabHung`; we mark it stuck in the registry, exclude it
//! from default selection, and return a typed error so the agent can choose
//! a different tab or reload.
//!
//! This is the *only* catch for the iLO-style failure mode (alive renderer
//! that never services JS) — there is no protocol event for that case.

use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use crate::cdp::CdpClient;

/// Default probe budget. Generous enough that a healthy tab on a loaded
/// machine still replies, tight enough that a wedged tab fails fast.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Probe outcome.
#[derive(Debug)]
pub enum ProbeResult {
    /// Target replied to `Runtime.evaluate("1")` within the bound.
    Responsive,
    /// Bound expired without a reply. Most likely: service-worker-paused
    /// page, JS infinite loop, or an admin UI that ignores `Runtime.evaluate`
    /// (e.g. iLO). Renderer may be alive.
    Hung,
    /// Protocol returned an error (target id rejected, session lost, etc.).
    /// Treated by the caller the same as Hung for "don't bind a session" —
    /// but distinguished here so the agent gets a more accurate message.
    Errored(String),
}

/// Probe `target_id` over the given CDP client. Attaches a transient session,
/// sends `Runtime.evaluate("1")`, detaches.
///
/// On a healthy tab this typically completes in single-digit milliseconds on
/// localhost. On a wedged renderer it returns `Hung` after `timeout`.
pub async fn probe_target(
    client: &CdpClient,
    target_id: &str,
    timeout: Duration,
) -> Result<ProbeResult> {
    // Attach a transient session for the probe. We use flatten=true so the
    // session_id propagates on the same WS.
    let attach = match tokio::time::timeout(
        timeout,
        client.send(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        ),
    )
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Ok(ProbeResult::Errored(format!("attach: {e}"))),
        Err(_) => return Ok(ProbeResult::Hung),
    };
    let session_id = match attach.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return Ok(ProbeResult::Errored("no sessionId from attach".to_string())),
    };

    // Race Runtime.evaluate against the remaining budget. We give it the
    // full timeout — attach is cheap and the relevant bound is for the
    // evaluate call where wedged renderers actually drop replies.
    let eval = client.send_with_session(
        "Runtime.evaluate",
        json!({
            "expression": "1",
            "returnByValue": true,
            "awaitPromise": false,
        }),
        Some(&session_id),
    );
    let result = match tokio::time::timeout(timeout, eval).await {
        Ok(Ok(_)) => ProbeResult::Responsive,
        Ok(Err(e)) => ProbeResult::Errored(format!("evaluate: {e}")),
        Err(_) => ProbeResult::Hung,
    };

    // Detach best-effort; we don't fail the probe over a failed detach.
    let _ = client
        .send(
            "Target.detachFromTarget",
            json!({ "sessionId": session_id }),
        )
        .await;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use std::sync::Arc;
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;

    /// Mock that accepts `Target.attachToTarget` but never replies to
    /// `Runtime.evaluate`. Simulates the iLO wedge.
    async fn spawn_mock_evaluate_hangs() -> (String, oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    msg = ws.next() => {
                        let msg = match msg {
                            Some(Ok(m)) => m,
                            _ => break,
                        };
                        if let Message::Text(t) = msg {
                            let req: Value = serde_json::from_str(&t).unwrap();
                            let id = req["id"].as_u64().unwrap();
                            let method = req["method"].as_str().unwrap_or("");
                            if method == "Runtime.evaluate" {
                                continue; // drop on floor
                            }
                            let result = match method {
                                "Target.attachToTarget" => json!({"sessionId": "PROBE_S"}),
                                "Target.detachFromTarget" => json!({}),
                                _ => json!({}),
                            };
                            let resp = json!({"id": id, "result": result});
                            ws.send(Message::Text(resp.to_string())).await.unwrap();
                        }
                    }
                }
            }
        });
        (format!("ws://{addr}"), stop_tx)
    }

    /// Mock that responds to `Runtime.evaluate` with `{result: {value: 1}}`.
    async fn spawn_mock_responsive() -> (String, oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    msg = ws.next() => {
                        let msg = match msg {
                            Some(Ok(m)) => m,
                            _ => break,
                        };
                        if let Message::Text(t) = msg {
                            let req: Value = serde_json::from_str(&t).unwrap();
                            let id = req["id"].as_u64().unwrap();
                            let method = req["method"].as_str().unwrap_or("");
                            let result = match method {
                                "Target.attachToTarget" => json!({"sessionId": "S1"}),
                                "Runtime.evaluate" => json!({"result": {"value": 1}}),
                                "Target.detachFromTarget" => json!({}),
                                _ => json!({}),
                            };
                            let resp = json!({"id": id, "result": result});
                            ws.send(Message::Text(resp.to_string())).await.unwrap();
                        }
                    }
                }
            }
        });
        (format!("ws://{addr}"), stop_tx)
    }

    /// Test #2: probe rejects a stuck tab within ~timeout.
    #[tokio::test]
    async fn probe_returns_hung_for_unresponsive_evaluate() {
        let (url, _stop) = spawn_mock_evaluate_hangs().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let start = std::time::Instant::now();
        let res = probe_target(&client, "T1", Duration::from_millis(300))
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(800),
            "probe overshot bound: {elapsed:?}"
        );
        assert!(
            matches!(res, ProbeResult::Hung),
            "expected Hung, got {res:?}"
        );
    }

    /// Responsive tab probes within milliseconds.
    #[tokio::test]
    async fn probe_returns_responsive_for_healthy_tab() {
        let (url, _stop) = spawn_mock_responsive().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let res = probe_target(&client, "T1", Duration::from_millis(500))
            .await
            .unwrap();
        assert!(
            matches!(res, ProbeResult::Responsive),
            "expected Responsive, got {res:?}"
        );
    }
}
