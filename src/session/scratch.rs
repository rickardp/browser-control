//! Scratch-tab recover-once wrapper. The architectural answer to the iLO
//! failure mode: lock-free ops (`eval`, `fetch` with no explicit tab) run
//! against a daemon-style scratch tab tracked in the `scratches` SQLite
//! row, not against the user's first page.
//!
//! Two reasons this is enough:
//!
//! 1. The scratch tab is daemon-owned `about:blank`. It can't be the
//!    weird-renderer that won't service `Runtime.evaluate`, because we
//!    created it for that purpose. If the user has an iLO admin tab open,
//!    nothing routes there by default.
//! 2. If the scratch tab *itself* goes bad (browser restarted between
//!    invocations and the cached `target_id` is stale; or a previous op
//!    legitimately wedged the renderer), the wrapper closes+recreates the
//!    scratch and retries the op once before escalating typed errors.
//!    This implements the "always proceed" rule on the direct path.

use std::sync::Arc;

use anyhow::Result;
use serde_json::json;

use crate::cdp::CdpClient;
use crate::errors::SessionError;
use crate::registry::Registry;

/// Run `op` against the daemon-style scratch tab for `browser_name`, with
/// one round of recover-and-retry on tab failures.
///
/// The op receives an active `(session_id, target_id)` pair and is
/// expected to return whatever the caller cares about. On a structured
/// failure that suggests the scratch tab is dead (`TabHung`,
/// `TabCrashed`, or a CDP "No target with given id" / "Session is gone"
/// protocol error), the wrapper:
///
/// 1. Best-effort closes the dead target via `Target.closeTarget`.
/// 2. Deletes the SQLite scratch row.
/// 3. Creates a fresh `about:blank` via `Target.createTarget` and
///    upserts the new row.
/// 4. Retries `op` once. If the retry also fails, escalates the typed
///    error to the caller.
///
/// Why one retry, not many: per ADR-002 follow-up policy ("retry once,
/// then `TabHung`") — a second failure usually means the browser itself
/// is sick, and unbounded retries would mask that.
pub async fn with_scratch_recovery<F, T, Fut>(
    client: Arc<CdpClient>,
    registry: &Registry,
    browser_name: &str,
    mut op: F,
) -> Result<T>
where
    F: FnMut(Arc<CdpClient>, String, String) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    // Resolve the scratch target id to use for attempt 1: reuse the existing
    // row if present, else create a fresh `about:blank` and register it.
    let attempt_one_target = match registry.scratch_get(browser_name)? {
        Some(row) => row.target_id,
        None => {
            let new_target = create_blank(&client).await?;
            registry.scratch_upsert(browser_name, &new_target)?;
            new_target
        }
    };

    match attach_and_run(client.clone(), &attempt_one_target, &mut op).await {
        Ok(value) => {
            let _ = registry.scratch_touch(browser_name);
            return Ok(value);
        }
        Err(e) if is_scratch_failure(&e) => {
            // Scratch tab is wedged / dead / vanished. Tear down + recreate +
            // single retry. Fall through.
            let _ = close_target(&client, &attempt_one_target).await;
            registry.scratch_delete(browser_name)?;
        }
        Err(e) => return Err(e),
    }

    // Attempt 2: fresh scratch tab. If this one also fails, escalate.
    let new_target = create_blank(&client).await?;
    registry.scratch_upsert(browser_name, &new_target)?;
    let result = attach_and_run(client.clone(), &new_target, &mut op).await;
    if result.is_ok() {
        let _ = registry.scratch_touch(browser_name);
    }
    result
}

/// Attach a CDP session to `target_id`, run `op`, detach. Surfaces the
/// op's `Result` verbatim so the caller can pattern-match `SessionError`.
async fn attach_and_run<F, T, Fut>(client: Arc<CdpClient>, target_id: &str, op: &mut F) -> Result<T>
where
    F: FnMut(Arc<CdpClient>, String, String) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let attach = client
        .send(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await?;
    let session_id = attach
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Target.attachToTarget returned no sessionId"))?
        .to_string();
    let result = op(client.clone(), session_id.clone(), target_id.to_string()).await;
    // Detach best-effort; failures here don't change the outcome.
    let _ = client
        .send(
            "Target.detachFromTarget",
            json!({ "sessionId": session_id }),
        )
        .await;
    result
}

async fn create_blank(client: &CdpClient) -> Result<String> {
    let v = client
        .send("Target.createTarget", json!({ "url": "about:blank" }))
        .await?;
    v.get("targetId")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Target.createTarget returned no targetId"))
}

async fn close_target(client: &CdpClient, target_id: &str) -> Result<()> {
    let _ = client
        .send("Target.closeTarget", json!({ "targetId": target_id }))
        .await?;
    Ok(())
}

/// Does this error suggest the scratch tab is dead and we should retry on
/// a fresh one? We treat three categories as recoverable:
///
/// - `SessionError::TabHung` — per-op timeout fired with no reply. Catches
///   the wedged-renderer case (iLO and friends).
/// - `SessionError::TabCrashed` — renderer crash event reached us.
/// - CDP protocol errors mentioning a missing target/session — the stored
///   `target_id` no longer exists in the browser (typical after a browser
///   restart between CLI invocations).
fn is_scratch_failure(err: &anyhow::Error) -> bool {
    if let Some(se) = err.downcast_ref::<SessionError>() {
        return matches!(
            se,
            SessionError::TabHung { .. } | SessionError::TabCrashed { .. }
        );
    }
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("no target with given id")
        || msg.contains("session is gone")
        || msg.contains("no session with given id")
        || msg.contains("target closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;

    /// Mock CDP server with configurable per-method behaviour. Returns
    /// (ws_url, recreate-count-handle, stop).
    ///
    /// `eval_behaviour` controls `Runtime.evaluate`:
    /// - `Wedge(n)`: drop the first `n` evaluate requests on the floor
    ///   (forcing client-side timeouts), then answer normally.
    /// - `Always`: always answer with `{result: {value: 42}}`.
    async fn spawn_mock(
        eval_behaviour: EvalBehaviour,
    ) -> (String, Arc<AtomicU32>, oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let create_count = Arc::new(AtomicU32::new(0));
        let cc = create_count.clone();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut next_target = 0u32;
            let mut next_session = 0u32;
            let mut evals_seen = 0u32;
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
                            // Runtime.evaluate may be dropped on the floor
                            // to simulate a wedged renderer.
                            if method == "Runtime.evaluate" {
                                evals_seen += 1;
                                if eval_behaviour.should_drop(evals_seen) {
                                    continue;
                                }
                            }
                            let result = match method {
                                "Target.createTarget" => {
                                    next_target += 1;
                                    cc.fetch_add(1, Ordering::SeqCst);
                                    serde_json::json!({"targetId": format!("T{next_target}")})
                                }
                                "Target.closeTarget" => serde_json::json!({"success": true}),
                                "Target.attachToTarget" => {
                                    next_session += 1;
                                    serde_json::json!({"sessionId": format!("S{next_session}")})
                                }
                                "Target.detachFromTarget" => serde_json::json!({}),
                                "Runtime.evaluate" => {
                                    serde_json::json!({"result": {"value": 42}})
                                }
                                _ => serde_json::json!({}),
                            };
                            let resp = serde_json::json!({"id": id, "result": result});
                            ws.send(Message::Text(resp.to_string())).await.unwrap();
                        }
                    }
                }
            }
        });
        (format!("ws://{addr}"), create_count, stop_tx)
    }

    #[derive(Copy, Clone)]
    enum EvalBehaviour {
        Always,
        Wedge(u32),
    }

    impl EvalBehaviour {
        fn should_drop(self, eval_index: u32) -> bool {
            match self {
                EvalBehaviour::Always => false,
                EvalBehaviour::Wedge(n) => eval_index <= n,
            }
        }
    }

    /// Op closure that runs `Runtime.evaluate("1")` against the given
    /// session with a tight timeout, mirroring what a real lock-free
    /// op would do.
    async fn eval_op(
        client: Arc<CdpClient>,
        session_id: String,
        _target_id: String,
    ) -> Result<Value> {
        let inner = client.send_with_session(
            "Runtime.evaluate",
            json!({"expression": "1", "returnByValue": true, "awaitPromise": false}),
            Some(&session_id),
        );
        match tokio::time::timeout(std::time::Duration::from_millis(200), inner).await {
            Ok(Ok(v)) => Ok(v["result"]["value"].clone()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(SessionError::TabHung {
                target_id: None,
                url: None,
                timeout_ms: 200,
                hint: "test",
            }
            .into()),
        }
    }

    #[tokio::test]
    async fn first_call_creates_scratch_and_returns_value() {
        let (url, create_count, _stop) = spawn_mock(EvalBehaviour::Always).await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = Registry::open_in_memory().unwrap();
        let v = with_scratch_recovery(client, &reg, "brave-twilight", eval_op)
            .await
            .unwrap();
        assert_eq!(v, json!(42));
        assert_eq!(create_count.load(Ordering::SeqCst), 1, "one scratch tab");
        let row = reg.scratch_get("brave-twilight").unwrap().unwrap();
        assert_eq!(row.target_id, "T1");
    }

    #[tokio::test]
    async fn second_call_reuses_scratch_row() {
        let (url, create_count, _stop) = spawn_mock(EvalBehaviour::Always).await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = Registry::open_in_memory().unwrap();
        with_scratch_recovery(client.clone(), &reg, "b", eval_op)
            .await
            .unwrap();
        with_scratch_recovery(client, &reg, "b", eval_op)
            .await
            .unwrap();
        assert_eq!(
            create_count.load(Ordering::SeqCst),
            1,
            "second call reused the row, no new target"
        );
    }

    /// First evaluate wedges → wrapper closes + recreates + retries once →
    /// retry succeeds. Caller sees a value, not an error.
    #[tokio::test]
    async fn recovers_after_one_wedge() {
        let (url, create_count, _stop) = spawn_mock(EvalBehaviour::Wedge(1)).await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = Registry::open_in_memory().unwrap();
        let v = with_scratch_recovery(client, &reg, "b", eval_op)
            .await
            .unwrap();
        assert_eq!(v, json!(42));
        assert_eq!(
            create_count.load(Ordering::SeqCst),
            2,
            "second target created after the wedge"
        );
        let row = reg.scratch_get("b").unwrap().unwrap();
        assert_eq!(row.target_id, "T2");
    }

    /// Both attempts wedge → caller sees typed `TabHung`. We do NOT keep
    /// retrying forever.
    #[tokio::test]
    async fn escalates_after_second_wedge() {
        let (url, create_count, _stop) = spawn_mock(EvalBehaviour::Wedge(99)).await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = Registry::open_in_memory().unwrap();
        let err = with_scratch_recovery(client, &reg, "b", eval_op)
            .await
            .expect_err("must escalate");
        let typed = err
            .downcast_ref::<SessionError>()
            .expect("typed SessionError");
        assert!(
            matches!(typed, SessionError::TabHung { .. }),
            "expected TabHung, got {typed:?}"
        );
        // We tried twice: initial + one retry after recovery.
        assert_eq!(create_count.load(Ordering::SeqCst), 2);
    }
}
