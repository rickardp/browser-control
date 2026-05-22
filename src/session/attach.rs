//! Attach to a page target and expose engine-agnostic high-level operations.
//!
//! [`PageSession`] hides the CDP/BiDi split behind a single async API
//! (`evaluate`, `navigate`, `screenshot`). The CLI subcommands instantiate
//! a fresh session per call; the MCP server may pre-build a session backed
//! by a long-lived BiDi client via [`PageSession::from_bidi_cache`].

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use regex::Regex;
use serde_json::{json, Value};

use crate::bidi::BidiClient;
use crate::cdp::CdpClient;
use crate::detect::Engine;
use crate::errors::SessionError;
use crate::session::targets::{open_bidi, open_cdp};

/// A bound page-level session. Variants are not constructed directly outside
/// this module; use [`PageSession::attach`].
pub enum PageSession {
    Cdp(CdpPage),
    /// A BiDi page session. The client is shared via `Arc` so the MCP server
    /// can keep a single persistent BiDi session across many tool calls
    /// (Firefox limits a browser to one BiDi session at a time).
    Bidi(BidiPage),
}

pub struct CdpPage {
    pub client: CdpClient,
    pub session_id: String,
    pub target_id: String,
}

pub struct BidiPage {
    pub client: Arc<BidiClient>,
    pub context: String,
    /// True when this `PageSession` opened the BiDi session (`session.new`)
    /// and is therefore responsible for ending it on close. False for
    /// sessions built from a shared, cached client (e.g. MCP server) where
    /// the lifetime is managed externally.
    owns_session: bool,
}

impl PageSession {
    /// Attach to a fresh page session over `engine`.
    ///
    /// If `url_regex` is `Some`, the first page target whose URL matches is
    /// selected; otherwise the first page (or top-level browsing context) is
    /// used.
    pub async fn attach(endpoint: &str, engine: Engine, url_regex: Option<&str>) -> Result<Self> {
        let pattern = url_regex.map(Regex::new).transpose()?;
        match engine {
            Engine::Cdp => {
                let client = open_cdp(endpoint).await?;
                let target_id = pick_cdp_page(&client, pattern.as_ref()).await?;
                let session_id = client.attach_to_target(&target_id).await?;
                Ok(PageSession::Cdp(CdpPage {
                    client,
                    session_id,
                    target_id,
                }))
            }
            Engine::Bidi => {
                let client = Arc::new(open_bidi(endpoint).await?);
                client.session_new().await?;
                let context = pick_bidi_context(&client, pattern.as_ref()).await?;
                Ok(PageSession::Bidi(BidiPage {
                    client,
                    context,
                    owns_session: true,
                }))
            }
        }
    }

    /// Build a BiDi session from a pre-opened, possibly cached client.
    ///
    /// The MCP server uses this to share one BiDi client across tool calls;
    /// `session.new` is invoked only when the client was freshly opened (the
    /// caller is expected to have done so).
    pub async fn from_bidi_cache(client: Arc<BidiClient>, url_regex: Option<&str>) -> Result<Self> {
        let pattern = url_regex.map(Regex::new).transpose()?;
        let context = pick_bidi_context(&client, pattern.as_ref()).await?;
        Ok(PageSession::Bidi(BidiPage {
            client,
            context,
            owns_session: false,
        }))
    }

    /// Attach to (or create) a page whose document origin matches `origin`.
    ///
    /// Strategy:
    /// 1. List existing page targets / browsing contexts.
    /// 2. If any has the same origin as `origin`, attach to it.
    /// 3. Otherwise create a new tab navigated to the origin's root and
    ///    attach to that tab.
    ///
    /// `origin` is parsed for its scheme, host, and port; path/query/fragment
    /// are ignored when comparing existing target URLs.
    pub async fn attach_for_origin(endpoint: &str, engine: Engine, origin: &str) -> Result<Self> {
        let want =
            url::Url::parse(origin).map_err(|e| anyhow!("invalid origin URL `{origin}`: {e}"))?;
        let origin_root = origin_root_url(&want);
        match engine {
            Engine::Cdp => {
                let client = open_cdp(endpoint).await?;
                let target_id = match find_cdp_target_for_origin(&client, &want).await? {
                    Some(id) => id,
                    None => create_cdp_tab(&client, &origin_root).await?,
                };
                let session_id = client.attach_to_target(&target_id).await?;
                Ok(PageSession::Cdp(CdpPage {
                    client,
                    session_id,
                    target_id,
                }))
            }
            Engine::Bidi => {
                let client = Arc::new(open_bidi(endpoint).await?);
                client.session_new().await?;
                let context = match find_bidi_context_for_origin(&client, &want).await? {
                    Some(c) => c,
                    None => create_bidi_tab(&client, &origin_root).await?,
                };
                Ok(PageSession::Bidi(BidiPage {
                    client,
                    context,
                    owns_session: true,
                }))
            }
        }
    }

    /// Evaluate `expression` in the page's main world.
    ///
    /// `await_promise = true` mirrors `Runtime.evaluate({awaitPromise:true})`
    /// and is appropriate for fetch / promise-returning code. The returned
    /// value is the raw `result.value` from CDP / BiDi after `returnByValue`.
    ///
    /// Equivalent to [`evaluate_with_timeout`](Self::evaluate_with_timeout)
    /// with `timeout = None` (bounded only by the upstream client's protocol
    /// timeout, currently 30 s). Prefer the bounded form in any path where
    /// the renderer's responsiveness is uncertain — see the module docs.
    pub async fn evaluate(&self, expression: &str, await_promise: bool) -> Result<Value> {
        self.evaluate_with_timeout(expression, await_promise, None)
            .await
    }

    /// Bounded variant of [`evaluate`](Self::evaluate).
    ///
    /// If `timeout` is `Some`, the call races the upstream send against a
    /// `tokio::time::sleep`. On expiry, returns a typed
    /// [`SessionError::TabHung`] tagged with the target's id and URL — this
    /// is the catch-all for the alive-but-unresponsive renderer case that
    /// has no protocol event signal (service-worker-paused page, JS infinite
    /// loop, modal dialog, devtools-paused, embedded admin UIs whose
    /// renderer ignores `Runtime.evaluate`).
    ///
    /// If `timeout` is `None`, the call is bounded only by the underlying
    /// client's protocol timeout (CDP: 30 s, BiDi: 30 s).
    pub async fn evaluate_with_timeout(
        &self,
        expression: &str,
        await_promise: bool,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let target_id = self.target_id();
        let url = None;
        let inner = async {
            match self {
                PageSession::Cdp(p) => {
                    let v = p
                        .client
                        .send_with_session(
                            "Runtime.evaluate",
                            json!({
                                "expression": expression,
                                "returnByValue": true,
                                "awaitPromise": await_promise,
                            }),
                            Some(&p.session_id),
                        )
                        .await?;
                    Ok(v["result"]["value"].clone())
                }
                PageSession::Bidi(p) => {
                    let _ = await_promise; // BiDi always awaits per script_evaluate
                    let v = p.client.script_evaluate(&p.context, expression).await?;
                    Ok(v["result"]["value"].clone())
                }
            }
        };
        match timeout {
            None => inner.await,
            Some(d) => match tokio::time::timeout(d, inner).await {
                Ok(r) => r,
                Err(_) => Err(SessionError::TabHung {
                    target_id,
                    url,
                    timeout_ms: d.as_millis() as u64,
                    hint: "op-timeout",
                }
                .into()),
            },
        }
    }

    /// Engine-specific target id for diagnostics (CDP `targetId`, BiDi
    /// browsing context id).
    pub fn target_id(&self) -> Option<String> {
        match self {
            PageSession::Cdp(p) => Some(p.target_id.clone()),
            PageSession::Bidi(p) => Some(p.context.clone()),
        }
    }

    /// Navigate the current page to `url`.
    pub async fn navigate(&self, url: &str) -> Result<()> {
        match self {
            PageSession::Cdp(p) => {
                p.client
                    .send_with_session("Page.navigate", json!({"url": url}), Some(&p.session_id))
                    .await?;
                Ok(())
            }
            PageSession::Bidi(p) => {
                p.client.browsing_context_navigate(&p.context, url).await?;
                Ok(())
            }
        }
    }

    /// Capture a PNG screenshot of the current page; returns base64 data.
    pub async fn screenshot(&self, full_page: bool) -> Result<String> {
        match self {
            PageSession::Cdp(p) => {
                let v = p
                    .client
                    .send_with_session(
                        "Page.captureScreenshot",
                        json!({
                            "format": "png",
                            "captureBeyondViewport": full_page,
                        }),
                        Some(&p.session_id),
                    )
                    .await?;
                v["data"]
                    .as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| anyhow!("no screenshot data"))
            }
            PageSession::Bidi(p) => {
                let _ = full_page; // BiDi captures the viewport by default
                p.client
                    .browsing_context_capture_screenshot(&p.context)
                    .await
            }
        }
    }

    /// Engine this session is bound to.
    pub fn engine(&self) -> Engine {
        match self {
            PageSession::Cdp(_) => Engine::Cdp,
            PageSession::Bidi(_) => Engine::Bidi,
        }
    }

    /// Release the underlying connection. For BiDi sessions that this
    /// `PageSession` opened, also calls `session.end` so that Firefox (which
    /// enforces one BiDi session per browser) accepts a fresh `session.new`
    /// on the next invocation.
    pub async fn close(self) {
        match self {
            PageSession::Cdp(p) => p.client.close().await,
            PageSession::Bidi(p) => {
                if p.owns_session {
                    let _ = p.client.session_end().await;
                }
            }
        }
    }
}

async fn pick_cdp_page(client: &CdpClient, pattern: Option<&Regex>) -> Result<String> {
    let targets = client.list_targets().await?;
    let mut pages = targets
        .iter()
        .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"));
    let pick = if let Some(re) = pattern {
        pages
            .find(|t| {
                t.get("url")
                    .and_then(|v| v.as_str())
                    .is_some_and(|u| re.is_match(u))
            })
            .ok_or_else(|| anyhow!("no CDP page target matched URL regex"))?
    } else {
        pages
            .next()
            .ok_or_else(|| anyhow!("no page target found"))?
    };
    pick.get("targetId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("targetId missing from page target"))
}

async fn pick_bidi_context(client: &BidiClient, pattern: Option<&Regex>) -> Result<String> {
    let tree = client.send("browsingContext.getTree", json!({})).await?;
    let contexts = tree
        .get("contexts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no contexts in browsingContext.getTree"))?;
    if let Some(re) = pattern {
        for c in contexts {
            let url = c.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if re.is_match(url) {
                return c
                    .get("context")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| anyhow!("no context id"));
            }
        }
        Err(anyhow!("no BiDi context matched URL regex"))
    } else {
        contexts
            .first()
            .and_then(|c| c.get("context").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("no top-level browsing context"))
    }
}

/// True when both URLs share scheme, host, and effective port.
pub(crate) fn same_origin(a: &url::Url, b: &url::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Strip everything after the origin: e.g. `https://x/y?z` → `https://x/`.
pub(crate) fn origin_root_url(u: &url::Url) -> String {
    let scheme = u.scheme();
    let host = u.host_str().unwrap_or("");
    match (u.port(), u.port_or_known_default()) {
        // Only emit a port when it's non-default for the scheme.
        (Some(p), _) => format!("{scheme}://{host}:{p}/"),
        (None, _) => format!("{scheme}://{host}/"),
    }
}

async fn find_cdp_target_for_origin(client: &CdpClient, want: &url::Url) -> Result<Option<String>> {
    let targets = client.list_targets().await?;
    Ok(targets
        .iter()
        .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
        .find_map(|t| {
            let u = t.get("url").and_then(|v| v.as_str())?;
            let parsed = url::Url::parse(u).ok()?;
            if same_origin(&parsed, want) {
                t.get("targetId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        }))
}

async fn create_cdp_tab(client: &CdpClient, url: &str) -> Result<String> {
    let v = client
        .send("Target.createTarget", json!({ "url": url }))
        .await?;
    v.get("targetId")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Target.createTarget did not return targetId"))
}

async fn find_bidi_context_for_origin(
    client: &BidiClient,
    want: &url::Url,
) -> Result<Option<String>> {
    let tree = client.send("browsingContext.getTree", json!({})).await?;
    let contexts = tree
        .get("contexts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(contexts.iter().find_map(|c| {
        let u = c.get("url").and_then(|v| v.as_str())?;
        let parsed = url::Url::parse(u).ok()?;
        if same_origin(&parsed, want) {
            c.get("context")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        } else {
            None
        }
    }))
}

async fn create_bidi_tab(client: &BidiClient, url: &str) -> Result<String> {
    let v = client
        .send("browsingContext.create", json!({ "type": "tab" }))
        .await?;
    let ctx = v
        .get("context")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("browsingContext.create did not return context"))?
        .to_string();
    client.browsing_context_navigate(&ctx, url).await?;
    Ok(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    async fn spawn_cdp_mock(targets: Vec<Value>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let req: Value = serde_json::from_str(&t).unwrap();
                let id = req["id"].as_u64().unwrap();
                let method = req["method"].as_str().unwrap_or("");
                let result = match method {
                    "Target.getTargets" => json!({"targetInfos": targets.clone()}),
                    "Target.attachToTarget" => json!({"sessionId": "S1"}),
                    "Target.createTarget" => json!({"targetId": "NEW"}),
                    "Runtime.evaluate" => json!({"result": {"value": "ok"}}),
                    "Page.navigate" => json!({}),
                    "Page.captureScreenshot" => json!({"data": "PNGDATA"}),
                    _ => json!({}),
                };
                let resp = json!({"id": id, "result": result});
                ws.send(Message::Text(resp.to_string())).await.unwrap();
            }
        });
        format!("ws://{addr}")
    }

    #[test]
    fn same_origin_basic() {
        let a = url::Url::parse("https://example.com/path?q=1").unwrap();
        let b = url::Url::parse("https://example.com/other").unwrap();
        let c = url::Url::parse("https://other.test/path").unwrap();
        let d = url::Url::parse("http://example.com/").unwrap();
        assert!(same_origin(&a, &b));
        assert!(!same_origin(&a, &c));
        assert!(!same_origin(&a, &d));
    }

    #[test]
    fn origin_root_strips_path_and_default_port() {
        let u = url::Url::parse("https://example.com/foo/bar?x=1#z").unwrap();
        assert_eq!(origin_root_url(&u), "https://example.com/");
        let u2 = url::Url::parse("http://localhost:8080/foo").unwrap();
        assert_eq!(origin_root_url(&u2), "http://localhost:8080/");
    }

    #[tokio::test]
    async fn attach_for_origin_reuses_matching_tab() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://other.test/x"}),
            json!({"targetId":"b","type":"page","url":"https://example.com/login"}),
        ])
        .await;
        let s = PageSession::attach_for_origin(&url, Engine::Cdp, "https://example.com/api/v1")
            .await
            .unwrap();
        match s {
            PageSession::Cdp(p) => assert_eq!(p.target_id, "b"),
            _ => panic!("expected CDP"),
        }
    }

    #[tokio::test]
    async fn attach_for_origin_creates_tab_when_no_match() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://other.test/"}),
        ])
        .await;
        let s = PageSession::attach_for_origin(&url, Engine::Cdp, "https://example.com/api")
            .await
            .unwrap();
        match s {
            PageSession::Cdp(p) => assert_eq!(p.target_id, "NEW"),
            _ => panic!("expected CDP"),
        }
    }

    #[tokio::test]
    async fn attach_cdp_picks_first_page_when_no_regex() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/"}),
            json!({"targetId":"b","type":"page","url":"https://other.test/"}),
        ])
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, None).await.unwrap();
        match s {
            PageSession::Cdp(p) => {
                assert_eq!(p.target_id, "a");
                assert_eq!(p.session_id, "S1");
            }
            _ => panic!("expected CDP"),
        }
    }

    #[tokio::test]
    async fn attach_cdp_url_regex_selects_matching() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/"}),
            json!({"targetId":"b","type":"page","url":"https://other.test/"}),
        ])
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, Some(r"other"))
            .await
            .unwrap();
        match s {
            PageSession::Cdp(p) => assert_eq!(p.target_id, "b"),
            _ => panic!("expected CDP"),
        }
    }

    #[tokio::test]
    async fn attach_cdp_url_regex_no_match_errors() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/"}),
        ])
        .await;
        let err = match PageSession::attach(&url, Engine::Cdp, Some("nomatch")).await {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("no CDP page target matched"));
    }

    #[tokio::test]
    async fn evaluate_round_trip_cdp() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/"}),
        ])
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, None).await.unwrap();
        let v = s.evaluate("1+1", false).await.unwrap();
        assert_eq!(v, json!("ok"));
        s.close().await;
    }

    #[tokio::test]
    async fn screenshot_round_trip_cdp() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/"}),
        ])
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, None).await.unwrap();
        let b64 = s.screenshot(false).await.unwrap();
        assert_eq!(b64, "PNGDATA");
        s.close().await;
    }

    /// Spawn a CDP mock that answers `Target.getTargets` / `attachToTarget`
    /// normally but **never replies to `Runtime.evaluate`** — simulating the
    /// iLO-style wedge where the renderer is alive but refuses to service JS.
    async fn spawn_cdp_mock_eval_hangs(targets: Vec<Value>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let req: Value = serde_json::from_str(&t).unwrap();
                let id = req["id"].as_u64().unwrap();
                let method = req["method"].as_str().unwrap_or("");
                if method == "Runtime.evaluate" {
                    // Drop the request on the floor. No response, ever.
                    continue;
                }
                let result = match method {
                    "Target.getTargets" => json!({"targetInfos": targets.clone()}),
                    "Target.attachToTarget" => json!({"sessionId": "S1"}),
                    _ => json!({}),
                };
                let resp = json!({"id": id, "result": result});
                ws.send(Message::Text(resp.to_string())).await.unwrap();
            }
        });
        format!("ws://{addr}")
    }

    /// Test #1: the iLO-style wedge. `evaluate_with_timeout` returns a typed
    /// `TabHung` within the bound — not the 30 s upstream `REQUEST_TIMEOUT`.
    #[tokio::test]
    async fn evaluate_with_timeout_returns_tab_hung_on_no_reply() {
        let url = spawn_cdp_mock_eval_hangs(vec![
            json!({"targetId":"iLO","type":"page","url":"https://192.168.2.28/"}),
        ])
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, None).await.unwrap();
        let start = std::time::Instant::now();
        let err = s
            .evaluate_with_timeout("1+1", false, Some(Duration::from_millis(300)))
            .await
            .expect_err("must return TabHung");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "did not honour 300ms bound, took {elapsed:?}"
        );
        let downcast = err.downcast_ref::<SessionError>().expect("typed error");
        match downcast {
            SessionError::TabHung {
                target_id,
                timeout_ms,
                hint,
                ..
            } => {
                assert_eq!(target_id.as_deref(), Some("iLO"));
                assert_eq!(*timeout_ms, 300);
                assert_eq!(*hint, "op-timeout");
            }
            other => panic!("expected TabHung, got {other:?}"),
        }
        s.close().await;
    }

    /// Test #16 (partial): a stuck eval on one PageSession does not block a
    /// concurrent eval on a sibling PageSession sharing the same browser. We
    /// model the "sibling" by opening a second mock — same protocol, two
    /// CdpClient instances. The point of the test is to verify that the
    /// timeout/error path on one session is isolated from the other.
    #[tokio::test]
    async fn stuck_eval_does_not_block_sibling_session() {
        let bad = spawn_cdp_mock_eval_hangs(vec![
            json!({"targetId":"BAD","type":"page","url":"https://192.168.2.28/"}),
        ])
        .await;
        let good = spawn_cdp_mock(vec![
            json!({"targetId":"GOOD","type":"page","url":"https://example.com/"}),
        ])
        .await;

        let s_bad = PageSession::attach(&bad, Engine::Cdp, None).await.unwrap();
        let s_good = PageSession::attach(&good, Engine::Cdp, None).await.unwrap();

        // Run both concurrently. The bad one should fast-fail; the good one
        // should succeed independently.
        let bad_fut = s_bad.evaluate_with_timeout("1+1", false, Some(Duration::from_millis(200)));
        let good_fut = s_good.evaluate_with_timeout("1+1", false, Some(Duration::from_secs(5)));
        let (bad_res, good_res) = tokio::join!(bad_fut, good_fut);

        assert!(bad_res.is_err(), "bad session must surface TabHung");
        assert_eq!(good_res.unwrap(), json!("ok"));

        s_bad.close().await;
        s_good.close().await;
    }
}
