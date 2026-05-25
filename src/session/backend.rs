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
use crate::errors::SessionError;

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
                let result = c
                    .send_with_session("Page.navigate", json!({ "url": url }), Some(&session_id))
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
                c.browsing_context_navigate(target_id, url).await?;
                Ok(())
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
                Ok(arr
                    .iter()
                    .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
                    .map(|t| LiveTarget {
                        id: t
                            .get("targetId")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        url: t
                            .get("url")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        title: t
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    })
                    .collect())
            }
            TabBackend::Bidi(c) => {
                let v: Value = c.send("browsingContext.getTree", json!({})).await?;
                let contexts = v
                    .get("contexts")
                    .and_then(|x| x.as_array())
                    .cloned()
                    .unwrap_or_default();
                Ok(contexts
                    .iter()
                    .filter_map(|ctx| {
                        let id = ctx.get("context").and_then(|v| v.as_str())?.to_string();
                        let url = ctx
                            .get("url")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        // BiDi getTree doesn't expose page titles directly
                        // on the context node; leave blank for now.
                        Some(LiveTarget {
                            id,
                            url,
                            title: String::new(),
                        })
                    })
                    .collect())
            }
        }
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
                let inner = c.send_with_session(
                    "Runtime.evaluate",
                    json!({
                        "expression": expression,
                        "returnByValue": true,
                        "awaitPromise": await_promise,
                    }),
                    Some(&session_id),
                );
                let value = match tokio::time::timeout(timeout, inner).await {
                    Ok(Ok(v)) => Ok(v["result"]["value"].clone()),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(SessionError::TabHung {
                        target_id: Some(target_id.to_string()),
                        url: None,
                        timeout_ms: timeout.as_millis() as u64,
                        hint: "op-timeout",
                    }
                    .into()),
                };
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
                                "Target.createTarget" => {
                                    next_target += 1;
                                    let tid = format!("T{next_target}");
                                    live.insert(tid.clone());
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
                                    json!({"sessionId": format!("S{next_session}")})
                                }
                                "Target.detachFromTarget" => json!({}),
                                "Page.navigate" => json!({}),
                                "Runtime.evaluate" => json!({"result": {"value": 7}}),
                                "Target.getTargets" => {
                                    let infos: Vec<Value> = live
                                        .iter()
                                        .map(|tid| json!({"targetId": tid, "type": "page", "url": ""}))
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
