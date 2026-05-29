//! Engine-agnostic tab backend used by the named-tab registry and the
//! scratch-recovery wrapper.
//!
//! Tab operations on Chromium-family browsers go through CDP
//! (`Target.*` + per-target session attach), and on Firefox via WebDriver
//! BiDi (`browsingContext.*` + `script.evaluate`). The named-tab CLI and
//! the scratch-tab recovery wrapper are engine-independent and just need
//! these four primitives:
//!
//! - **create** a fresh tab at a URL (`about:blank` if unspecified).
//! - **close** a tab by its engine-specific id.
//! - **navigate** an existing tab to a URL.
//! - **list** every live top-level tab id.
//!
//! Plus one more for the eval/fetch path:
//!
//! - **evaluate** a JS expression in a tab, returning the result value.
//!
//! `target_id` is an opaque `String` on both engines — CDP's `targetId`
//! and BiDi's `context` are both opaque ids the registry stores verbatim.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::bidi::BidiClient;
use crate::cdp::CdpClient;
use crate::cli::cookies::{normalize_bidi, normalize_cdp, NormalCookie};
use crate::errors::SessionError;
use crate::session::targets::{BidiContext, CdpTarget};

/// Wall-clock bound for `navigate`/`screenshot`. `evaluate` takes its
/// timeout from the caller (op-specific budgets), but navigate/screenshot
/// have no caller-supplied budget, so they default to this. Picked below
/// the 30s CDP `REQUEST_TIMEOUT` so a wedged op surfaces as a typed,
/// *recoverable* `TabHung`/`TabCrashed` before the client's generic
/// "CDP request timed out" string (which is not in the recoverable needle
/// list) can fire and defeat recover-once.
const NAV_OP_TIMEOUT: Duration = Duration::from_secs(20);

/// Engine-agnostic tab operations. Two variants because CDP and BiDi
/// have different protocols and clients; the methods abstract over the
/// difference.
#[derive(Clone)]
pub enum TabBackend {
    Cdp(Arc<CdpClient>),
    Bidi(Arc<BidiClient>),
}

/// Lightweight view of a live tab returned by [`TabBackend::live_targets`].
/// Used by `tab list --all` to merge the named-tab registry with the
/// browser's current target/context set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTarget {
    pub id: String,
    pub url: String,
    pub title: String,
}

impl TabBackend {
    /// Create a fresh top-level tab. Returns the engine-specific id
    /// (CDP `targetId`, BiDi `context`) the registry stores verbatim.
    /// `url` defaults to `about:blank`.
    pub async fn create_tab(&self, url: &str) -> Result<String> {
        let url = if url.is_empty() { "about:blank" } else { url };
        match self {
            TabBackend::Cdp(c) => {
                let v = c.send("Target.createTarget", json!({ "url": url })).await?;
                v.get("targetId")
                    .and_then(|x| x.as_str())
                    .map(String::from)
                    .ok_or_else(|| anyhow!("Target.createTarget returned no targetId"))
            }
            TabBackend::Bidi(c) => c.browsing_context_create(url).await,
        }
    }

    /// Close a tab by id. Best-effort — both CDP and BiDi handle a
    /// missing id gracefully, and the caller's intent ("this tab is
    /// gone") is satisfied either way.
    pub async fn close_tab(&self, target_id: &str) -> Result<()> {
        match self {
            TabBackend::Cdp(c) => {
                let _ = c
                    .send("Target.closeTarget", json!({ "targetId": target_id }))
                    .await?;
                Ok(())
            }
            TabBackend::Bidi(c) => c.browsing_context_close(target_id).await,
        }
    }

    /// Navigate an existing tab to `url`. CDP requires attaching a
    /// transient session; BiDi takes the context id directly.
    pub async fn navigate(&self, target_id: &str, url: &str) -> Result<()> {
        match self {
            TabBackend::Cdp(c) => {
                let attach = c
                    .send(
                        "Target.attachToTarget",
                        json!({ "targetId": target_id, "flatten": true }),
                    )
                    .await?;
                let session_id = attach
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("attachToTarget returned no sessionId"))?
                    .to_string();
                // Enable the Inspector domain so `Inspector.targetCrashed`
                // is delivered while the navigate is in flight. Best-effort,
                // same rationale as `evaluate`.
                let _ = c
                    .send_with_session("Inspector.enable", json!({}), Some(&session_id))
                    .await;
                let inner = async {
                    c.send_with_session("Page.navigate", json!({ "url": url }), Some(&session_id))
                        .await
                };
                // Bound by timeout + renderer-crash detection so a wedged
                // navigate surfaces as recoverable `TabHung`/`TabCrashed`
                // (recover-once), not a 30s non-recoverable client timeout.
                let result = crate::session::crash::evaluate_with_crash_detection(
                    c,
                    target_id,
                    Some(&session_id),
                    inner,
                    Some(NAV_OP_TIMEOUT),
                )
                .await;
                let _ = c
                    .send(
                        "Target.detachFromTarget",
                        json!({ "sessionId": session_id }),
                    )
                    .await;
                result?;
                Ok(())
            }
            TabBackend::Bidi(c) => {
                // BiDi has no crash event; a wedged navigate must still be
                // bounded so it surfaces as recoverable `TabHung` rather
                // than the 30s client `SEND_TIMEOUT`. A dead context comes
                // back as `no such context` which the `TargetGone`
                // classifier already treats as recoverable.
                let fut = c.browsing_context_navigate(target_id, url);
                match tokio::time::timeout(NAV_OP_TIMEOUT, fut).await {
                    Ok(r) => r.map(|_| ()),
                    Err(_) => Err(SessionError::TabHung {
                        target_id: Some(target_id.to_string()),
                        url: Some(url.to_string()),
                        timeout_ms: NAV_OP_TIMEOUT.as_millis() as u64,
                        hint: "op-timeout",
                    }
                    .into()),
                }
            }
        }
    }

    /// Snapshot of every live top-level tab id in the browser.
    /// Used by the registry's sweep-on-read to drop rows whose target
    /// no longer exists.
    pub async fn live_target_ids(&self) -> Result<HashSet<String>> {
        Ok(self
            .live_targets()
            .await?
            .into_iter()
            .map(|t| t.id)
            .collect())
    }

    /// Snapshot of every live top-level tab with id + URL + title. Used by
    /// `tab list --all` to merge the named-tab registry with the
    /// browser's view of the world. CDP filters to `type == "page"`; BiDi
    /// returns every top-level browsing context.
    pub async fn live_targets(&self) -> Result<Vec<LiveTarget>> {
        match self {
            TabBackend::Cdp(c) => {
                let v: Value = c.send("Target.getTargets", json!({})).await?;
                let arr = v
                    .get("targetInfos")
                    .and_then(|x| x.as_array())
                    .cloned()
                    .unwrap_or_default();
                Ok(CdpTarget::pages(&arr)
                    .map(|t| LiveTarget {
                        id: t.id,
                        url: t.url,
                        title: t.title,
                    })
                    .collect())
            }
            TabBackend::Bidi(c) => {
                let v: Value = c.send("browsingContext.getTree", json!({})).await?;
                // BiDi getTree doesn't expose page titles directly on the
                // context node; leave blank for now.
                Ok(BidiContext::from_tree(&v)
                    .into_iter()
                    .map(|ctx| LiveTarget {
                        id: ctx.context,
                        url: ctx.url,
                        title: String::new(),
                    })
                    .collect())
            }
        }
    }

    /// Resolve a target whose document origin matches `url`'s origin,
    /// reusing a live tab already on that origin if one exists and creating
    /// one rooted at the origin otherwise. Returns the engine-specific id.
    ///
    /// This is the routing primitive for `browser_fetch`: running the
    /// in-page fetch from a same-origin document is what lets cookies and
    /// credentials propagate and lets the response bypass CORS. Routing a
    /// fetch through an `about:blank` scratch tab (this backend's default
    /// active tab) gives it an opaque origin, which silently breaks
    /// authenticated and CORS-sensitive requests — see `cli::fetch`'s
    /// origin-bound path for the same contract.
    pub async fn resolve_or_create_for_origin(&self, url: &str) -> Result<String> {
        let want = url::Url::parse(url).map_err(|e| anyhow!("invalid fetch URL `{url}`: {e}"))?;
        for t in self.live_targets().await? {
            if let Ok(parsed) = url::Url::parse(&t.url) {
                if crate::session::attach::same_origin(&parsed, &want) {
                    return Ok(t.id);
                }
            }
        }
        let root = crate::session::attach::origin_root_url(&want);
        self.create_tab(&root).await
    }

    /// Evaluate `expression` in `target_id`'s main world, returning the
    /// raw result value (after `returnByValue`). Bounded by `timeout`;
    /// expiry returns typed [`SessionError::TabHung`].
    ///
    /// CDP path attaches a transient session, calls `Runtime.evaluate`,
    /// detaches. BiDi path calls `script.evaluate` against the context.
    /// On BiDi, `await_promise` is ignored — BiDi always awaits per
    /// `script.evaluate` semantics.
    pub async fn evaluate(
        &self,
        target_id: &str,
        expression: &str,
        await_promise: bool,
        timeout: Duration,
    ) -> Result<Value> {
        match self {
            TabBackend::Cdp(c) => {
                let attach = c
                    .send(
                        "Target.attachToTarget",
                        json!({ "targetId": target_id, "flatten": true }),
                    )
                    .await?;
                let session_id = attach
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("attachToTarget returned no sessionId"))?
                    .to_string();
                // Enable the Inspector domain on the attached session so
                // `Inspector.targetCrashed` is delivered while the
                // evaluate is in flight. Best-effort: older Chromium
                // builds and headless variants may answer with an
                // empty result but never raise — failing the enable
                // would silently mute crash detection, so we proceed.
                let _ = c
                    .send_with_session("Inspector.enable", json!({}), Some(&session_id))
                    .await;
                let inner = async {
                    let v = c
                        .send_with_session(
                            "Runtime.evaluate",
                            json!({
                                "expression": expression,
                                "returnByValue": true,
                                "awaitPromise": await_promise,
                            }),
                            Some(&session_id),
                        )
                        .await?;
                    Ok::<Value, anyhow::Error>(v["result"]["value"].clone())
                };
                let value = crate::session::crash::evaluate_with_crash_detection(
                    c,
                    target_id,
                    Some(&session_id),
                    inner,
                    Some(timeout),
                )
                .await;
                let _ = c
                    .send(
                        "Target.detachFromTarget",
                        json!({ "sessionId": session_id }),
                    )
                    .await;
                value
            }
            TabBackend::Bidi(c) => {
                let _ = await_promise; // BiDi always awaits
                let fut = c.script_evaluate(target_id, expression);
                match tokio::time::timeout(timeout, fut).await {
                    Ok(Ok(v)) => Ok(v["result"]["value"].clone()),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(SessionError::TabHung {
                        target_id: Some(target_id.to_string()),
                        url: None,
                        timeout_ms: timeout.as_millis() as u64,
                        hint: "op-timeout",
                    }
                    .into()),
                }
            }
        }
    }

    /// Capture a PNG screenshot of `target_id` and return base64-encoded
    /// bytes.
    ///
    /// CDP path attaches a transient session, calls
    /// `Page.captureScreenshot({format:"png", captureBeyondViewport:full_page})`,
    /// detaches. BiDi path calls `browsingContext.captureScreenshot` —
    /// the BiDi protocol always captures the viewport (no `full_page`
    /// equivalent), so `full_page` is honoured only on CDP.
    ///
    /// When `clip` is `Some({x, y, width, height})` (document coordinates, as
    /// produced by [`crate::dom::scripts::GET_CLIP_RECT_JS`]) the capture is
    /// restricted to that rectangle, which takes precedence over `full_page`.
    pub async fn screenshot(
        &self,
        target_id: &str,
        full_page: bool,
        clip: Option<Value>,
    ) -> Result<String> {
        match self {
            TabBackend::Cdp(c) => {
                let attach = c
                    .send(
                        "Target.attachToTarget",
                        json!({ "targetId": target_id, "flatten": true }),
                    )
                    .await?;
                let session_id = attach
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("attachToTarget returned no sessionId"))?
                    .to_string();
                // Enable the Inspector domain so `Inspector.targetCrashed`
                // is delivered while the capture is in flight. Best-effort,
                // same rationale as `evaluate`.
                let _ = c
                    .send_with_session("Inspector.enable", json!({}), Some(&session_id))
                    .await;
                // A clip rectangle lives outside the viewport in the general
                // case (the element was scrolled into view by the caller, but
                // may still be taller than the viewport), so force
                // `captureBeyondViewport` whenever clipping.
                let mut params = json!({
                    "format": "png",
                    "captureBeyondViewport": full_page || clip.is_some(),
                });
                if let Some(rect) = &clip {
                    params["clip"] = json!({
                        "x": rect["x"],
                        "y": rect["y"],
                        "width": rect["width"],
                        "height": rect["height"],
                        "scale": 1,
                    });
                }
                let inner = async {
                    c.send_with_session("Page.captureScreenshot", params, Some(&session_id))
                        .await
                };
                // Bound by timeout + renderer-crash detection so a wedged
                // capture surfaces as recoverable `TabHung`/`TabCrashed`,
                // not a 30s non-recoverable client timeout.
                let v = crate::session::crash::evaluate_with_crash_detection(
                    c,
                    target_id,
                    Some(&session_id),
                    inner,
                    Some(NAV_OP_TIMEOUT),
                )
                .await;
                let _ = c
                    .send(
                        "Target.detachFromTarget",
                        json!({ "sessionId": session_id }),
                    )
                    .await;
                let v = v?;
                v["data"]
                    .as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| anyhow!("Page.captureScreenshot returned no data"))
            }
            TabBackend::Bidi(c) => {
                let _ = full_page; // BiDi captures the viewport by default
                let fut = c.browsing_context_capture_screenshot(target_id, clip);
                match tokio::time::timeout(NAV_OP_TIMEOUT, fut).await {
                    Ok(r) => r,
                    Err(_) => Err(SessionError::TabHung {
                        target_id: Some(target_id.to_string()),
                        url: None,
                        timeout_ms: NAV_OP_TIMEOUT.as_millis() as u64,
                        hint: "op-timeout",
                    }
                    .into()),
                }
            }
        }
    }

    /// Fetch the full cookie jar through this backend's *existing* client,
    /// normalised across engines. Unlike `cli::cookies::fetch_cookies`,
    /// this reuses the already-open session instead of opening a fresh
    /// one — required on Firefox, where BiDi permits only one session per
    /// browser, so a second `session.new` against a server-held browser
    /// fails or races. Cookies are browser-wide on both engines (CDP
    /// `Network.getAllCookies` / BiDi `storage.getCookies`), so no target
    /// id is needed.
    pub(crate) async fn cookies(&self) -> Result<Vec<NormalCookie>> {
        match self {
            TabBackend::Cdp(c) => {
                let v = c.send("Network.getAllCookies", json!({})).await?;
                let arr = v
                    .get("cookies")
                    .and_then(|x| x.as_array())
                    .ok_or_else(|| anyhow!("CDP Network.getAllCookies: missing `cookies` array"))?;
                Ok(arr.iter().map(normalize_cdp).collect())
            }
            TabBackend::Bidi(c) => {
                let v = c.send("storage.getCookies", json!({})).await?;
                let arr = v
                    .get("cookies")
                    .and_then(|x| x.as_array())
                    .ok_or_else(|| anyhow!("BiDi storage.getCookies: missing `cookies` array"))?;
                Ok(arr.iter().map(normalize_bidi).collect())
            }
        }
    }
}

/// Open the right [`TabBackend`] for a resolved browser endpoint, taking
/// care of BiDi's `session.new` handshake. The returned backend is `Clone`
/// and owns its underlying client via `Arc`.
pub async fn open_backend(endpoint: &str, engine: crate::detect::Engine) -> Result<TabBackend> {
    match engine {
        crate::detect::Engine::Cdp => {
            let client = if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
                CdpClient::connect(endpoint).await?
            } else {
                CdpClient::connect_http(endpoint).await?
            };
            Ok(TabBackend::Cdp(Arc::new(client)))
        }
        crate::detect::Engine::Bidi => {
            let client = if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
                BidiClient::connect(endpoint).await?
            } else {
                // HTTP discovery for BiDi: fetch /json/version, extract
                // webSocketDebuggerUrl, then connect. Firefox geckodriver
                // exposes /session via WebDriver classic but BiDi sessions
                // need the WS URL — same flow as CDP.
                let base = endpoint.trim_end_matches('/');
                let url = format!("{base}/json/version");
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()?;
                let resp: Value = client.get(&url).send().await?.json().await?;
                let ws = resp
                    .get("webSocketDebuggerUrl")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| anyhow!("webSocketDebuggerUrl missing from {url}"))?
                    .to_string();
                BidiClient::connect(&ws).await?
            };
            // BiDi requires session.new before any other call. Use the
            // existing helper which handles "session already active" via
            // session.end + retry.
            client.session_new().await?;
            Ok(TabBackend::Bidi(Arc::new(client)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;

    // CDP and BiDi each have their own mock-server tests in lower-level
    // modules; these tests focus on the engine-agnostic behaviour of the
    // backend wrapper.

    async fn spawn_cdp_mock() -> (String, oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut next_target = 0u32;
            let mut next_session = 0u32;
            // target id -> last-known url, so getTargets can report a URL
            // and origin resolution has something to match against.
            let mut live = std::collections::HashMap::<String, String>::new();
            // Sessions attach to a target; remember which so navigate can
            // update the right target's url.
            let mut sessions = std::collections::HashMap::<String, String>::new();
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
                                "Target.createTarget" => {
                                    next_target += 1;
                                    let tid = format!("T{next_target}");
                                    let url = req
                                        .pointer("/params/url")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    live.insert(tid.clone(), url);
                                    json!({"targetId": tid})
                                }
                                "Target.closeTarget" => {
                                    if let Some(tid) = req
                                        .pointer("/params/targetId")
                                        .and_then(|v| v.as_str())
                                    {
                                        live.remove(tid);
                                    }
                                    json!({"success": true})
                                }
                                "Target.attachToTarget" => {
                                    next_session += 1;
                                    let sid = format!("S{next_session}");
                                    if let Some(tid) = req
                                        .pointer("/params/targetId")
                                        .and_then(|v| v.as_str())
                                    {
                                        sessions.insert(sid.clone(), tid.to_string());
                                    }
                                    json!({"sessionId": sid})
                                }
                                "Target.detachFromTarget" => json!({}),
                                "Page.navigate" => {
                                    // Update the attached target's url so a
                                    // later getTargets reflects the navigation.
                                    if let (Some(sid), Some(url)) = (
                                        req.pointer("/sessionId").and_then(|v| v.as_str()),
                                        req.pointer("/params/url").and_then(|v| v.as_str()),
                                    ) {
                                        if let Some(tid) = sessions.get(sid) {
                                            live.insert(tid.clone(), url.to_string());
                                        }
                                    }
                                    json!({})
                                }
                                "Runtime.evaluate" => json!({"result": {"value": 7}}),
                                "Target.getTargets" => {
                                    let infos: Vec<Value> = live
                                        .iter()
                                        .map(|(tid, url)| json!({"targetId": tid, "type": "page", "url": url}))
                                        .collect();
                                    json!({"targetInfos": infos})
                                }
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

    async fn spawn_bidi_mock() -> (String, oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut next_ctx = 0u32;
            let mut live = std::collections::HashSet::<String>::new();
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
                                "session.new" => json!({"sessionId": "S1", "capabilities": {}}),
                                "browsingContext.create" => {
                                    next_ctx += 1;
                                    let c = format!("C{next_ctx}");
                                    live.insert(c.clone());
                                    json!({"context": c})
                                }
                                "browsingContext.close" => {
                                    if let Some(c) = req
                                        .pointer("/params/context")
                                        .and_then(|v| v.as_str())
                                    {
                                        live.remove(c);
                                    }
                                    json!({})
                                }
                                "browsingContext.navigate" => json!({"navigation": "N1"}),
                                "script.evaluate" => json!({"result": {"value": 9}}),
                                "browsingContext.getTree" => {
                                    let contexts: Vec<Value> = live
                                        .iter()
                                        .map(|c| json!({"context": c, "url": "", "children": []}))
                                        .collect();
                                    json!({"contexts": contexts})
                                }
                                _ => json!({}),
                            };
                            // BiDi wire format uses {type, id, result} —
                            // not JSON-RPC `{id, result}` — per spec.
                            let resp = json!({"type": "success", "id": id, "result": result});
                            ws.send(Message::Text(resp.to_string())).await.unwrap();
                        }
                    }
                }
            }
        });
        (format!("ws://{addr}"), stop_tx)
    }

    #[tokio::test]
    async fn cdp_backend_create_close_navigate_list_evaluate() {
        let (url, _stop) = spawn_cdp_mock().await;
        let backend = open_backend(&url, crate::detect::Engine::Cdp)
            .await
            .unwrap();
        let t1 = backend.create_tab("about:blank").await.unwrap();
        assert_eq!(t1, "T1");
        backend.navigate(&t1, "https://example.com/").await.unwrap();
        let live = backend.live_target_ids().await.unwrap();
        assert!(live.contains(&t1));
        let v = backend
            .evaluate(&t1, "1+1", false, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(v, json!(7));
        backend.close_tab(&t1).await.unwrap();
        let live = backend.live_target_ids().await.unwrap();
        assert!(!live.contains(&t1));
    }

    #[tokio::test]
    async fn resolve_for_origin_reuses_same_origin_tab() {
        let (url, _stop) = spawn_cdp_mock().await;
        let backend = open_backend(&url, crate::detect::Engine::Cdp)
            .await
            .unwrap();
        // Open a tab and navigate it onto the target origin.
        let t1 = backend.create_tab("about:blank").await.unwrap();
        backend
            .navigate(&t1, "https://example.com/login")
            .await
            .unwrap();
        // A fetch to a different path on the same origin must reuse t1,
        // not spin up a fresh tab.
        let resolved = backend
            .resolve_or_create_for_origin("https://example.com/api/v1")
            .await
            .unwrap();
        assert_eq!(resolved, t1);
    }

    #[tokio::test]
    async fn resolve_for_origin_creates_tab_when_no_match() {
        let (url, _stop) = spawn_cdp_mock().await;
        let backend = open_backend(&url, crate::detect::Engine::Cdp)
            .await
            .unwrap();
        let t1 = backend.create_tab("about:blank").await.unwrap();
        backend.navigate(&t1, "https://other.test/").await.unwrap();
        // No live tab on example.com → a new one is created, rooted at the
        // origin so the in-page fetch inherits that origin.
        let resolved = backend
            .resolve_or_create_for_origin("https://example.com/api")
            .await
            .unwrap();
        assert_ne!(resolved, t1);
        let live = backend.live_target_ids().await.unwrap();
        assert!(live.contains(&resolved));
    }

    #[tokio::test]
    async fn bidi_backend_create_close_navigate_list_evaluate() {
        let (url, _stop) = spawn_bidi_mock().await;
        let backend = open_backend(&url, crate::detect::Engine::Bidi)
            .await
            .unwrap();
        let c1 = backend.create_tab("about:blank").await.unwrap();
        assert_eq!(c1, "C1");
        backend.navigate(&c1, "https://example.com/").await.unwrap();
        let live = backend.live_target_ids().await.unwrap();
        assert!(live.contains(&c1));
        let v = backend
            .evaluate(&c1, "1+1", false, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(v, json!(9));
        backend.close_tab(&c1).await.unwrap();
        let live = backend.live_target_ids().await.unwrap();
        assert!(!live.contains(&c1));
    }
}
