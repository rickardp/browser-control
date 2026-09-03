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
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::bidi::BidiClient;
use crate::cdp::CdpClient;
use crate::cli::cookies::{normalize_bidi, normalize_cdp, NormalCookie};
use crate::errors::SessionError;
use crate::session::freshness;
use crate::session::input_bidi;
use crate::session::targets::{BidiContext, CdpTarget};

/// Wall-clock bound for `navigate`/`screenshot`. `evaluate` takes its
/// timeout from the caller (op-specific budgets), but navigate/screenshot
/// have no caller-supplied budget, so they default to this. Picked below
/// the 30s CDP `REQUEST_TIMEOUT` so a wedged op surfaces as a typed,
/// *recoverable* `TabHung`/`TabCrashed` before the client's generic
/// "CDP request timed out" string (which is not in the recoverable needle
/// list) can fire and defeat recover-once.
const NAV_OP_TIMEOUT: Duration = Duration::from_secs(20);

/// Output encoding for [`TabBackend::screenshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
}

impl ImageFormat {
    pub fn mime(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
        }
    }

    pub fn cdp_name(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpeg",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
        }
    }
}

/// JPEG quality used when the caller picks `jpeg` without a `quality`.
pub const DEFAULT_JPEG_QUALITY: u8 = 80;

/// Options for [`TabBackend::screenshot`]. The default is byte-for-byte
/// the previous behaviour: viewport PNG, no clip, no downscale.
#[derive(Debug, Clone, Default)]
pub struct ScreenshotOptions {
    pub full_page: bool,
    /// `{x, y, width, height}` in document coordinates.
    pub clip: Option<Value>,
    pub format: ImageFormat,
    /// JPEG only, 1-100.
    pub quality: Option<u8>,
    /// Downscale so the output is at most this many device pixels wide.
    pub max_width: Option<u32>,
}

/// Document-coordinate rectangle a downscaled capture covers when no clip
/// was given: the whole page for `full_page`, otherwise the layout
/// viewport. Built from `Page.getLayoutMetrics`.
fn capture_rect(metrics: &Value, full_page: bool) -> Value {
    let vp = metrics
        .get("cssLayoutViewport")
        .or_else(|| metrics.get("layoutViewport"))
        .cloned()
        .unwrap_or(Value::Null);
    if full_page {
        let cs = metrics
            .get("cssContentSize")
            .or_else(|| metrics.get("contentSize"))
            .cloned()
            .unwrap_or(Value::Null);
        json!({
            "x": 0,
            "y": 0,
            "width": cs["width"].as_f64().unwrap_or(0.0),
            "height": cs["height"].as_f64().unwrap_or(0.0),
        })
    } else {
        json!({
            "x": vp["pageX"].as_f64().unwrap_or(0.0),
            "y": vp["pageY"].as_f64().unwrap_or(0.0),
            "width": vp["clientWidth"].as_f64().unwrap_or(0.0),
            "height": vp["clientHeight"].as_f64().unwrap_or(0.0),
        })
    }
}

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

/// Bound a BiDi operation so a wedged context surfaces as recoverable
/// `TabHung` rather than the 30 s transport timeout.
async fn bidi_bounded<T>(
    target_id: &str,
    timeout: Duration,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_) => Err(SessionError::TabHung {
            target_id: Some(target_id.to_string()),
            url: None,
            timeout_ms: timeout.as_millis() as u64,
            hint: "op-timeout",
        }
        .into()),
    }
}

impl TabBackend {
    /// Release the engine session before the client goes away. Firefox
    /// does not end a BiDi session when its WebSocket closes, so a backend
    /// that is dropped without `session.end` leaves the browser refusing
    /// every later `session.new` ("Maximum number of active sessions").
    /// CDP has nothing to release. Best-effort and idempotent.
    pub async fn shutdown(&self) {
        if let TabBackend::Bidi(c) = self {
            let _ = c.session_end().await;
        }
    }

    /// Create a fresh top-level tab. Returns the engine-specific id
    /// (CDP `targetId`, BiDi `context`) the registry stores verbatim.
    /// `url` defaults to `about:blank`.
    pub async fn create_tab(&self, url: &str) -> Result<String> {
        let url = if url.is_empty() { "about:blank" } else { url };
        match self {
            TabBackend::Cdp(c) => {
                let v = c
                    .send(
                        "Target.createTarget",
                        json!({ "url": url, "background": true }),
                    )
                    .await?;
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

    /// Make a tab visible and focused inside the browser window. This is
    /// intentionally explicit: normal automation creates/navigates tabs in
    /// the background so agents don't steal the user's foreground app unless
    /// they need interactive debugging or login.
    pub async fn show_tab(&self, target_id: &str) -> Result<()> {
        match self {
            TabBackend::Cdp(c) => {
                let _ = c
                    .send("Target.activateTarget", json!({ "targetId": target_id }))
                    .await?;
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
                    .send_with_session("Page.bringToFront", json!({}), Some(&session_id))
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
                let _ = c
                    .send("browsingContext.activate", json!({ "context": target_id }))
                    .await?;
                Ok(())
            }
        }
    }

    /// Return a tab suitable for `show`: prefer an existing live tab, create
    /// `about:blank` if the browser currently has none.
    pub async fn target_for_show(&self) -> Result<String> {
        if let Some(t) = self.live_targets().await?.into_iter().next() {
            return Ok(t.id);
        }
        self.create_tab("about:blank").await
    }

    /// Reload an old HTTP(S) tab before reading auth-sensitive page state.
    ///
    /// The age is measured from the document's `performance.timeOrigin`.
    /// Non-web pages such as `about:blank` are left untouched.
    pub async fn ensure_fresh(&self, target_id: &str, max_age: Duration) -> Result<()> {
        let info_value = self
            .evaluate(
                target_id,
                freshness::PAGE_FRESHNESS_EXPR,
                false,
                freshness::CHECK_TIMEOUT,
            )
            .await?;
        let info = freshness::parse_page_freshness(info_value)?;
        if !info.should_reload(max_age) {
            return Ok(());
        }

        tracing::info!(
            target = "session",
            target_id = %target_id,
            url = %info.href,
            age_ms = info.age_ms,
            max_age_ms = max_age.as_millis(),
            "reloading stale tab before reading page context"
        );
        self.navigate(target_id, &info.href).await?;
        self.wait_until_ready(target_id).await
    }

    async fn wait_until_ready(&self, target_id: &str) -> Result<()> {
        let deadline = Instant::now() + freshness::RELOAD_READY_TIMEOUT;
        loop {
            let value = self
                .evaluate(
                    target_id,
                    freshness::READY_STATE_EXPR,
                    false,
                    freshness::CHECK_TIMEOUT,
                )
                .await?;
            if freshness::is_ready(&value) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    target = "session",
                    target_id = %target_id,
                    "tab reload did not reach document.readyState=complete before continuing"
                );
                return Ok(());
            }
            tokio::time::sleep(freshness::READY_POLL_INTERVAL).await;
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
                    Ok(Ok(v)) => Ok(crate::bidi::remote_value_to_json(
                        &crate::bidi::unwrap_script_result(v)?,
                    )),
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

    /// Capture a screenshot of `target_id` and return base64-encoded bytes.
    ///
    /// CDP path attaches a transient session, calls `Page.captureScreenshot`,
    /// detaches. BiDi path calls `browsingContext.captureScreenshot` — the
    /// BiDi protocol always captures the viewport (no `full_page`
    /// equivalent) and has no downscale, so `full_page` and `max_width` are
    /// honoured only on CDP.
    ///
    /// When `opts.clip` is `Some({x, y, width, height})` (document
    /// coordinates, as produced by [`crate::dom::scripts::GET_CLIP_RECT_JS`])
    /// the capture is restricted to that rectangle, which takes precedence
    /// over `full_page`. `opts.max_width` downscales through `clip.scale`,
    /// which needs no emulation override and no restore step.
    pub async fn screenshot(&self, target_id: &str, opts: &ScreenshotOptions) -> Result<String> {
        match self {
            TabBackend::Cdp(c) => {
                let opts = opts.clone();
                crate::session::cdp_session::with_page_session(
                    c,
                    target_id,
                    NAV_OP_TIMEOUT,
                    |sid| async move {
                        // A clip rectangle lives outside the viewport in the
                        // general case (the element was scrolled into view by
                        // the caller, but may still be taller than the
                        // viewport), so force `captureBeyondViewport` whenever
                        // clipping.
                        let mut params = json!({
                            "format": opts.format.cdp_name(),
                            "captureBeyondViewport": opts.full_page || opts.clip.is_some(),
                        });
                        if opts.format == ImageFormat::Jpeg {
                            params["quality"] = json!(opts.quality.unwrap_or(DEFAULT_JPEG_QUALITY));
                        }
                        let mut clip = opts.clip.as_ref().map(|rect| {
                            json!({
                                "x": rect["x"],
                                "y": rect["y"],
                                "width": rect["width"],
                                "height": rect["height"],
                                "scale": 1,
                            })
                        });
                        if let Some(max_w) = opts.max_width {
                            let metrics = c
                                .send_with_session("Page.getLayoutMetrics", json!({}), Some(&sid))
                                .await?;
                            let dpr = c
                                .send_with_session(
                                    "Runtime.evaluate",
                                    json!({ "expression": "window.devicePixelRatio", "returnByValue": true }),
                                    Some(&sid),
                                )
                                .await
                                .ok()
                                .and_then(|v| v["result"]["value"].as_f64())
                                .filter(|d| *d > 0.0)
                                .unwrap_or(1.0);
                            let rect = match &clip {
                                Some(cl) => cl.clone(),
                                None => capture_rect(&metrics, opts.full_page),
                            };
                            let width = rect["width"].as_f64().unwrap_or(0.0);
                            if width > 0.0 {
                                let scale = (max_w as f64 / (width * dpr)).min(1.0);
                                if scale < 1.0 {
                                    let mut scaled = rect;
                                    scaled["scale"] = json!(scale);
                                    clip = Some(scaled);
                                    params["captureBeyondViewport"] = json!(true);
                                }
                            }
                        }
                        if let Some(cl) = clip {
                            params["clip"] = cl;
                        }
                        let v = c
                            .send_with_session("Page.captureScreenshot", params, Some(&sid))
                            .await?;
                        v["data"]
                            .as_str()
                            .map(|s| s.to_string())
                            .ok_or_else(|| anyhow!("Page.captureScreenshot returned no data"))
                    },
                )
                .await
            }
            TabBackend::Bidi(c) => {
                let format = match opts.format {
                    ImageFormat::Png => None,
                    ImageFormat::Jpeg => Some(json!({
                        "type": "image/jpeg",
                        "quality": f64::from(opts.quality.unwrap_or(DEFAULT_JPEG_QUALITY)) / 100.0,
                    })),
                };
                // BiDi has no full-page flag; a document-origin box clip of
                // the document's scroll size captures the whole page.
                // `max_width` has no BiDi equivalent (no `scale`) and is
                // ignored here.
                let full_page = opts.full_page && opts.clip.is_none();
                let clip = opts.clip.clone();
                bidi_bounded(target_id, NAV_OP_TIMEOUT, async move {
                    let clip = if full_page {
                        let (w, h) = input_bidi::document_size(c, target_id).await?;
                        Some(json!({ "x": 0, "y": 0, "width": w, "height": h }))
                    } else {
                        clip
                    };
                    c.browsing_context_capture_screenshot(target_id, clip, format)
                        .await
                })
                .await
            }
        }
    }

    // -----------------------------------------------------------------
    // Native accessibility + input (ref-based interaction).
    //
    // CDP uses the browser's accessibility tree and `Input.*` on a transient
    // session (`crate::session::input`); BiDi uses an injected DOM walker
    // with a page-side ref registry and `input.performActions`
    // (`crate::session::input_bidi`). Both feed the shared `crate::a11y`
    // renderer and ref table.
    // -----------------------------------------------------------------

    /// Full accessibility tree (`Accessibility.getFullAXTree`). `depth`
    /// bounds the tree the browser serialises; `None` means everything.
    pub async fn accessibility_tree(
        &self,
        target_id: &str,
        depth: Option<u32>,
        timeout: Duration,
    ) -> Result<Value> {
        match self {
            TabBackend::Cdp(c) => {
                crate::session::cdp_session::with_page_session(
                    c,
                    target_id,
                    timeout,
                    |sid| async move {
                        let _ = c
                            .send_with_session("Accessibility.enable", json!({}), Some(&sid))
                            .await;
                        let mut params = json!({});
                        if let Some(d) = depth {
                            params["depth"] = json!(d);
                        }
                        c.send_with_session("Accessibility.getFullAXTree", params, Some(&sid))
                            .await
                    },
                )
                .await
            }
            TabBackend::Bidi(c) => {
                bidi_bounded(
                    target_id,
                    timeout,
                    input_bidi::accessibility_tree(c, target_id),
                )
                .await
            }
        }
    }

    /// Identity of the current document (see [`crate::session::input::document_token`]).
    pub async fn document_token(&self, target_id: &str, timeout: Duration) -> Result<u64> {
        match self {
            TabBackend::Cdp(c) => {
                crate::session::cdp_session::with_page_session(
                    c,
                    target_id,
                    timeout,
                    |sid| async move { crate::session::input::document_token(c, &sid).await },
                )
                .await
            }
            TabBackend::Bidi(c) => {
                bidi_bounded(target_id, timeout, input_bidi::document_token(c, target_id)).await
            }
        }
    }

    /// Click the element with `backend_node_id`. Returns the viewport point
    /// that was clicked.
    pub async fn click_node(
        &self,
        target_id: &str,
        backend_node_id: u64,
        timeout: Duration,
    ) -> Result<crate::session::input::Point> {
        match self {
            TabBackend::Cdp(c) => crate::session::cdp_session::with_page_session(
                c,
                target_id,
                timeout,
                |sid| async move { crate::session::input::click(c, &sid, backend_node_id).await },
            )
            .await,
            TabBackend::Bidi(c) => {
                bidi_bounded(
                    target_id,
                    timeout,
                    input_bidi::click(c, target_id, backend_node_id),
                )
                .await
            }
        }
    }

    /// Hover the element with `backend_node_id`.
    pub async fn hover_node(
        &self,
        target_id: &str,
        backend_node_id: u64,
        timeout: Duration,
    ) -> Result<crate::session::input::Point> {
        match self {
            TabBackend::Cdp(c) => crate::session::cdp_session::with_page_session(
                c,
                target_id,
                timeout,
                |sid| async move { crate::session::input::hover(c, &sid, backend_node_id).await },
            )
            .await,
            TabBackend::Bidi(c) => {
                bidi_bounded(
                    target_id,
                    timeout,
                    input_bidi::hover(c, target_id, backend_node_id),
                )
                .await
            }
        }
    }

    /// Replace the element's content with `text` (see
    /// [`crate::session::input::type_text`]).
    pub async fn type_into_node(
        &self,
        target_id: &str,
        backend_node_id: u64,
        text: &str,
        press_sequentially: bool,
        submit: bool,
        timeout: Duration,
    ) -> Result<()> {
        match self {
            TabBackend::Cdp(c) => {
                let text = text.to_string();
                crate::session::cdp_session::with_page_session(
                    c,
                    target_id,
                    timeout,
                    |sid| async move {
                        crate::session::input::type_text(
                            c,
                            &sid,
                            backend_node_id,
                            &text,
                            press_sequentially,
                            submit,
                        )
                        .await
                    },
                )
                .await
            }
            TabBackend::Bidi(c) => {
                bidi_bounded(
                    target_id,
                    timeout,
                    input_bidi::type_text(
                        c,
                        target_id,
                        backend_node_id,
                        text,
                        press_sequentially,
                        submit,
                    ),
                )
                .await
            }
        }
    }

    /// Pointer drag from one element to another.
    pub async fn drag_nodes(
        &self,
        target_id: &str,
        from: u64,
        to: u64,
        timeout: Duration,
    ) -> Result<()> {
        match self {
            TabBackend::Cdp(c) => {
                crate::session::cdp_session::with_page_session(
                    c,
                    target_id,
                    timeout,
                    |sid| async move { crate::session::input::drag(c, &sid, from, to).await },
                )
                .await
            }
            TabBackend::Bidi(c) => {
                bidi_bounded(target_id, timeout, input_bidi::drag(c, target_id, from, to)).await
            }
        }
    }

    /// Border box of the element in document coordinates, for clipped
    /// screenshots.
    pub async fn node_clip_rect(
        &self,
        target_id: &str,
        backend_node_id: u64,
        timeout: Duration,
    ) -> Result<Value> {
        match self {
            TabBackend::Cdp(c) => {
                crate::session::cdp_session::with_page_session(
                    c,
                    target_id,
                    timeout,
                    |sid| async move {
                        crate::session::input::node_clip_rect(c, &sid, backend_node_id).await
                    },
                )
                .await
            }
            TabBackend::Bidi(c) => {
                bidi_bounded(
                    target_id,
                    timeout,
                    input_bidi::node_clip_rect(c, target_id, backend_node_id),
                )
                .await
            }
        }
    }

    /// Fetch the full cookie jar through this backend's *existing* client,
    /// normalised across engines. Unlike `cli::cookies::fetch_cookies`,
    /// this reuses the already-open session instead of opening a fresh
    /// one — required on Firefox, where BiDi permits only one session per
    /// browser, so a second `session.new` against a server-held browser
    /// fails or races. Cookies are browser-wide on both engines (CDP
    /// `Storage.getCookies` with legacy fallback / BiDi `storage.getCookies`),
    /// so no target id is needed.
    pub(crate) async fn cookies(&self) -> Result<Vec<NormalCookie>> {
        match self {
            TabBackend::Cdp(c) => {
                let v = c.get_all_cookies().await?;
                let arr = v
                    .get("cookies")
                    .and_then(|x| x.as_array())
                    .ok_or_else(|| anyhow!("CDP cookie export: missing `cookies` array"))?;
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

    /// Read the HTTP User-Agent exposed by the browser. When a target is
    /// supplied, evaluate in that document so per-target emulation overrides
    /// are preserved. Otherwise CDP can answer browser-wide; BiDi falls back
    /// to a live (or temporary) browsing context.
    pub(crate) async fn user_agent(&self, target_id: Option<&str>) -> Result<String> {
        if let Some(target_id) = target_id {
            let value = self
                .evaluate(
                    target_id,
                    "navigator.userAgent",
                    false,
                    Duration::from_secs(5),
                )
                .await?;
            return value
                .as_str()
                .map(String::from)
                .ok_or_else(|| anyhow!("navigator.userAgent returned a non-string value"));
        }

        if let TabBackend::Cdp(client) = self {
            let value = client.send("Browser.getVersion", json!({})).await?;
            return value
                .get("userAgent")
                .and_then(Value::as_str)
                .map(String::from)
                .ok_or_else(|| anyhow!("Browser.getVersion returned no userAgent"));
        }

        let (target_id, temporary) = match self.live_targets().await?.into_iter().next() {
            Some(target) => (target.id, false),
            None => (self.create_tab("about:blank").await?, true),
        };
        let result = self
            .evaluate(
                &target_id,
                "navigator.userAgent",
                false,
                Duration::from_secs(5),
            )
            .await;
        if temporary {
            let _ = self.close_tab(&target_id).await;
        }
        let value = result?;
        value
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("navigator.userAgent returned a non-string value"))
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
    use std::sync::Arc;
    use tokio::sync::{oneshot, Mutex};
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
        let (url, stop, _captures) = spawn_bidi_mock_with_captures().await;
        (url, stop)
    }

    /// Same mock, also returning the recorded `captureScreenshot` params.
    async fn spawn_bidi_mock_with_captures() -> (String, oneshot::Sender<()>, Arc<Mutex<Vec<Value>>>)
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let captures = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captures_task = captures.clone();
        tokio::spawn(async move {
            let captures = captures_task;
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
                                "script.evaluate"
                                    if req["params"]["expression"]
                                        .as_str()
                                        .is_some_and(|e| e.contains("scrollWidth")) =>
                                {
                                    json!({"type": "success", "result": {"type": "string", "value": "{\"width\":1000,\"height\":3000}"}, "realm": "R1"})
                                }
                                "script.evaluate" => json!({"type": "success", "result": {"type": "number", "value": 9}, "realm": "R1"}),
                                "browsingContext.captureScreenshot" => {
                                    captures.lock().await.push(req["params"].clone());
                                    json!({"data": "PNG"})
                                }
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
        (format!("ws://{addr}"), stop_tx, captures)
    }

    async fn spawn_cdp_freshness_mock() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let navigations = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn({
            let navigations = navigations.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let req: Value = serde_json::from_str(&t).unwrap();
                    let id = req["id"].as_u64().unwrap();
                    let method = req["method"].as_str().unwrap_or("");
                    let result = match method {
                        "Target.attachToTarget" => json!({"sessionId": "S1"}),
                        "Target.detachFromTarget" => json!({}),
                        "Inspector.enable" => json!({}),
                        "Runtime.evaluate" => {
                            let expression = req
                                .pointer("/params/expression")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let value = if expression == freshness::PAGE_FRESHNESS_EXPR {
                                json!({
                                    "href": "https://example.com/app",
                                    "ageMs": 700_000.0,
                                    "readyState": "complete"
                                })
                            } else if expression == freshness::READY_STATE_EXPR {
                                json!("complete")
                            } else {
                                json!(7)
                            };
                            json!({"result": {"value": value}})
                        }
                        "Page.navigate" => {
                            let url = req
                                .pointer("/params/url")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            navigations.lock().await.push(url);
                            json!({})
                        }
                        _ => json!({}),
                    };
                    let resp = json!({"id": id, "result": result});
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                }
            }
        });
        (format!("ws://{addr}"), navigations)
    }

    async fn spawn_cdp_recording_mock() -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        tokio::spawn({
            let seen = seen.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let req: Value = serde_json::from_str(&t).unwrap();
                    seen.lock().await.push(req.clone());
                    let id = req["id"].as_u64().unwrap();
                    let method = req["method"].as_str().unwrap_or("");
                    let result = match method {
                        "Target.createTarget" => json!({"targetId": "T1"}),
                        "Target.attachToTarget" => json!({"sessionId": "S1"}),
                        "Target.getTargets" => json!({"targetInfos": [
                            {"targetId": "T1", "type": "page", "url": "about:blank", "title": ""}
                        ]}),
                        _ => json!({}),
                    };
                    let resp = json!({"id": id, "result": result});
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                }
            }
        });
        (format!("ws://{addr}"), seen)
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
    async fn cdp_create_tab_requests_background_target() {
        let (url, seen) = spawn_cdp_recording_mock().await;
        let backend = open_backend(&url, crate::detect::Engine::Cdp)
            .await
            .unwrap();
        let tid = backend.create_tab("https://example.com/").await.unwrap();
        assert_eq!(tid, "T1");
        let calls = seen.lock().await;
        let create = calls
            .iter()
            .find(|v| v["method"] == "Target.createTarget")
            .expect("create call");
        assert_eq!(
            create.pointer("/params/url").and_then(Value::as_str),
            Some("https://example.com/")
        );
        assert_eq!(
            create
                .pointer("/params/background")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[tokio::test]
    async fn cdp_show_tab_activates_and_brings_to_front() {
        let (url, seen) = spawn_cdp_recording_mock().await;
        let backend = open_backend(&url, crate::detect::Engine::Cdp)
            .await
            .unwrap();
        backend.show_tab("T1").await.unwrap();
        let methods: Vec<String> = seen
            .lock()
            .await
            .iter()
            .filter_map(|v| v["method"].as_str().map(String::from))
            .collect();
        assert_eq!(
            methods,
            vec![
                "Target.activateTarget",
                "Target.attachToTarget",
                "Page.bringToFront",
                "Target.detachFromTarget"
            ]
        );
    }

    #[tokio::test]
    async fn ensure_fresh_reloads_old_http_page() {
        let (url, navigations) = spawn_cdp_freshness_mock().await;
        let backend = open_backend(&url, crate::detect::Engine::Cdp)
            .await
            .unwrap();
        backend
            .ensure_fresh("T1", Duration::from_secs(600))
            .await
            .unwrap();
        assert_eq!(
            *navigations.lock().await,
            vec!["https://example.com/app".to_string()]
        );
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

    #[tokio::test]
    async fn bidi_full_page_screenshot_uses_document_clip() {
        let (url, _stop, captures) = spawn_bidi_mock_with_captures().await;
        let backend = open_backend(&url, crate::detect::Engine::Bidi)
            .await
            .unwrap();
        let c1 = backend.create_tab("about:blank").await.unwrap();
        backend
            .screenshot(
                &c1,
                &ScreenshotOptions {
                    full_page: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        backend
            .screenshot(&c1, &ScreenshotOptions::default())
            .await
            .unwrap();
        let caps = captures.lock().await;
        assert_eq!(caps.len(), 2);
        assert_eq!(caps[0]["origin"], "document");
        assert_eq!(caps[0]["clip"]["type"], "box");
        assert_eq!(caps[0]["clip"]["width"], json!(1000.0));
        assert_eq!(caps[0]["clip"]["height"], json!(3000.0));
        assert!(caps[1].get("clip").is_none());
        backend.shutdown().await;
    }
}
