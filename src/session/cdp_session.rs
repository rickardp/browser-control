//! Transient flat-session helper for CDP page operations.
//!
//! Every CDP page op in `browser-control` follows the same shape: attach a
//! flat session to the target, enable `Inspector` so renderer crashes are
//! observable, run the op under the crash-detecting timeout, and detach
//! regardless of outcome. This helper factors that shape so new native
//! operations (accessibility tree, input dispatch) do not copy the block
//! from `TabBackend::evaluate` a fourth time.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use crate::cdp::CdpClient;
use crate::session::crash::evaluate_with_crash_detection;

/// Attach to `target_id`, run `f(session_id)` bounded by `timeout` with
/// crash detection, then detach. Detach is best-effort: the target may
/// already be gone, and the caller's result is what matters.
pub async fn with_page_session<T, F, Fut>(
    client: &CdpClient,
    target_id: &str,
    timeout: Duration,
    f: F,
) -> Result<T>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let session_id = client.attach_to_target(target_id).await?;
    // Best-effort, same rationale as `TabBackend::evaluate`: a failed
    // enable would only mute crash detection.
    let _ = client
        .send_with_session("Inspector.enable", json!({}), Some(&session_id))
        .await;
    let fut = f(session_id.clone());
    let result =
        evaluate_with_crash_detection(client, target_id, Some(&session_id), fut, Some(timeout))
            .await;
    let _ = client
        .send(
            "Target.detachFromTarget",
            json!({ "sessionId": session_id }),
        )
        .await;
    result
}
