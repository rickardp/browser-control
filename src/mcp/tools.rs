//! MCP tools exposed by the `browser-control mcp` server.
//!
//! The tool surface is Playwright-shaped (`browser_*` prefix) plus
//! browser-control extensions (`browser_get_html`, `browser_fetch`,
//! `browser_select_element`, `browser_cookies`, `browser_storage_*`,
//! `browser_wait_for_cookie`) and the legacy CDP-shaped `list_targets`
//! kept for info-dense diagnostics.
//!
//! Tools that operate against a single tab accept optional `tab` (named)
//! and `target` (URL regex) arguments. The two are mutually exclusive;
//! omitting both routes to the server's in-memory active tab
//! (`current_tab`).

use anyhow::{anyhow, Result};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cli::storage::{build_get_expr, build_set_expr, ns_global};
use crate::cli::wait_for_cookie::cookie_matches;
use crate::detect::Engine;
use crate::dom::scripts::{FETCH_JS, GET_CLIP_RECT_JS, GET_DOM_JS, SELECT_ELEMENT_JS};
use crate::mcp::server::{RegisteredTool, ServerState, ToolHandler, ToolRegistry};
use crate::session::freshness;
use crate::session::targets::TargetInfo;

/// Per-op timeout for read tools (`browser_get_html`,
/// `browser_select_element` short path, storage). 10 s is generous for
/// legitimate DOM work and tight enough that a wedged renderer
/// fast-fails.
const MCP_OP_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-op timeout for `browser_fetch`. Slow HTTP fetches over real
/// networks can take many seconds; 60 s matches the CLI `fetch
/// --timeout-ms` default.
const MCP_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-op timeout for `browser_select_element`. The overlay waits for a
/// human click, so the bound has to be much longer than for automated
/// tools. Five minutes is plenty for an interactive selection without
/// leaking forever if the page is left abandoned.
const MCP_SELECT_ELEMENT_TIMEOUT: Duration = Duration::from_secs(300);

/// Probe budget for `browser_tab_select`: how long we give the selected
/// tab to answer `Runtime.evaluate("1")` / `script.evaluate("1")` before
/// returning `TabHung`. Matches `session::attach::PICK_PROBE_TIMEOUT`.
const TAB_SELECT_PROBE: Duration = Duration::from_millis(500);

/// Register the standard tool set onto the given registry.
pub fn register_all(registry: &ToolRegistry) {
    // Renamed-from-Playwright tools.
    registry.register(make_navigate());
    registry.register(make_get_html());
    registry.register(make_take_screenshot());
    registry.register(make_fetch());
    registry.register(make_select_element());
    registry.register(make_cookies());
    registry.register(make_storage_get());
    registry.register(make_storage_set());
    registry.register(make_wait_for_cookie());
    // Diagnostic enumeration (kept).
    registry.register(make_list_targets());
    // New tab-management tools.
    registry.register(make_tab_list());
    registry.register(make_tab_new());
    registry.register(make_tab_select());
    registry.register(make_tab_close());
    // New browser-management tools.
    registry.register(make_browser_select());
    registry.register(make_browser_list());
    registry.register(make_browser_show());
    // Playwright-only interaction tools — Chromium-family only (route
    // through the Node sidecar). Each errors with `EngineUnsupported`
    // when the active browser is BiDi.
    registry.register(make_snapshot());
    registry.register(make_click());
    registry.register(make_type());
    registry.register(make_hover());
    registry.register(make_drag());
    registry.register(make_press_key());
    registry.register(make_wait_for());
    registry.register(make_pdf_save());
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn text_content(text: impl Into<String>) -> Value {
    json!({ "content": [ { "type": "text", "text": text.into() } ] })
}

fn image_content(data: String) -> Value {
    json!({
        "content": [ { "type": "image", "data": data, "mimeType": "image/png" } ]
    })
}

fn handler<F>(f: F) -> ToolHandler
where
    F: Fn(ServerState, Value) -> futures_util::future::BoxFuture<'static, Result<Value>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(f)
}

/// Schema fragment for optional `tab` / `target` args. Inlined into
/// every per-tab tool's input schema so the agent-facing contract is
/// consistent.
fn tab_args_schema() -> Value {
    json!({
        "tab": {
            "type": "string",
            "description": "Optional named tab; mutually exclusive with `target`."
        },
        "target": {
            "type": "string",
            "description": "Optional URL regex selecting an existing tab; mutually exclusive with `tab`."
        }
    })
}

/// Canonical builder for a per-tab tool's `properties` object: the shared
/// `tab` / `target` schema merged with tool-specific `extra` fields. The
/// merge result is order-independent — `serde_json::Map` serializes keys
/// sorted — so callers may pass `extra` in any shape.
fn tab_args_properties(extra: Value) -> Value {
    let mut obj = extra.as_object().cloned().unwrap_or_default();
    if let Some(ta) = tab_args_schema().as_object() {
        for (k, v) in ta {
            obj.insert(k.clone(), v.clone());
        }
    }
    Value::Object(obj)
}

/// Canonical extraction of the optional `tab` (named) / `target` (URL
/// regex) routing args from a tool's `args`. Mirrors the parse in
/// [`ServerState::resolve_target_for_args`]; used by tools that need to
/// branch on whether explicit routing was given before resolving.
fn extract_tab_target(args: &Value) -> (Option<String>, Option<String>) {
    let tab = args.get("tab").and_then(|v| v.as_str()).map(String::from);
    let target = args
        .get("target")
        .and_then(|v| v.as_str())
        .map(String::from);
    (tab, target)
}

fn max_age_arg(args: &Value) -> Result<Duration> {
    match args.get("max_age") {
        None | Some(Value::Null) => Ok(freshness::DEFAULT_MAX_AGE),
        Some(Value::String(s)) => freshness::parse_max_age(s),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(Duration::from_secs)
            .ok_or_else(|| anyhow!("`max_age` number must be non-negative seconds")),
        Some(_) => Err(anyhow!(
            "`max_age` must be a duration string, e.g. `10m` or `1h`"
        )),
    }
}

// ---------------------------------------------------------------------------
// browser_navigate
// ---------------------------------------------------------------------------

fn make_navigate() -> RegisteredTool {
    RegisteredTool {
        name: "browser_navigate".into(),
        description: "Navigate the active page to a URL.".into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({ "url": { "type": "string" } })),
            "required": ["url"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let url = args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing 'url'"))?
                    .to_string();
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                backend.navigate(&target_id, &url).await?;
                Ok(text_content(format!("Navigated to {url}")))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// browser_get_html
// ---------------------------------------------------------------------------

fn make_get_html() -> RegisteredTool {
    RegisteredTool {
        name: "browser_get_html".into(),
        description: "Get the rendered DOM as HTML, with shadow roots serialized when supported."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "selector": {
                    "type": "string",
                    "description": "Optional CSS selector; defaults to the document element."
                }
            })),
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let selector_arg = args.get("selector").and_then(|v| v.as_str());
                let selector_literal = match selector_arg {
                    Some(s) => serde_json::to_string(s)?,
                    None => "null".to_string(),
                };
                let expr = format!("({GET_DOM_JS})({selector_literal})");
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                let value = backend
                    .evaluate(&target_id, &expr, false, MCP_OP_TIMEOUT)
                    .await?;
                let html = value.as_str().unwrap_or("").to_string();
                Ok(text_content(html))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// browser_take_screenshot
// ---------------------------------------------------------------------------

fn make_take_screenshot() -> RegisteredTool {
    RegisteredTool {
        name: "browser_take_screenshot".into(),
        description: "Capture a PNG screenshot of the active page.".into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "full_page": { "type": "boolean", "default": false },
                "selector": { "type": "string" }
            })),
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let full_page = args
                    .get("full_page")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let selector = args.get("selector").and_then(|v| v.as_str());
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                // A selector clips the capture to that element's bounding box.
                let clip = match selector {
                    Some(sel) => {
                        let sel_literal = serde_json::to_string(sel)?;
                        let expr = format!("({GET_CLIP_RECT_JS})({sel_literal})");
                        let rect = backend
                            .evaluate(&target_id, &expr, false, MCP_OP_TIMEOUT)
                            .await?;
                        if rect.is_null() {
                            return Err(anyhow!("selector matched no visible element: {sel}"));
                        }
                        Some(rect)
                    }
                    None => None,
                };
                let b64 = backend.screenshot(&target_id, full_page, clip).await?;
                Ok(image_content(b64))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// browser_fetch
// ---------------------------------------------------------------------------

fn make_fetch() -> RegisteredTool {
    RegisteredTool {
        name: "browser_fetch".into(),
        description:
            "Perform an HTTP request from the page context (preserves cookies, bypasses CORS)."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "url": { "type": "string" },
                "method": { "type": "string" },
                "headers": { "type": "object" },
                "body": { "type": "string" },
                "max_age": {
                    "type": "string",
                    "description": "Reload the page first if its document is older than this duration (default 10m)."
                }
            })),
            "required": ["url"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                if args.get("url").and_then(|v| v.as_str()).is_none() {
                    return Err(anyhow!("missing 'url'"));
                }
                // Strip routing args before forwarding to the JS shim.
                let mut for_js = args.clone();
                if let Some(obj) = for_js.as_object_mut() {
                    obj.remove("tab");
                    obj.remove("target");
                    obj.remove("max_age");
                }
                let max_age = max_age_arg(&args)?;
                let args_json = serde_json::to_string(&for_js)?;
                let args_literal = serde_json::to_string(&args_json)?;
                let expr = format!("({FETCH_JS})({args_literal})");
                // Explicit `tab`/`target` routing is honoured verbatim. With
                // neither, route to a tab on the URL's origin rather than the
                // server's `about:blank` active tab — an opaque-origin fetch
                // silently drops cookies/credentials and trips CORS. Mirrors
                // `cli::fetch`'s origin-bound default path.
                let (tab, target) = extract_tab_target(&args);
                let has_route = tab.is_some() || target.is_some();
                let (backend, target_id) = if has_route {
                    state.resolve_target_for_args(&args).await?
                } else {
                    let url = args.get("url").and_then(|v| v.as_str()).unwrap();
                    state.resolve_or_create_for_origin(url).await?
                };
                backend.ensure_fresh(&target_id, max_age).await?;
                let value = backend
                    .evaluate(&target_id, &expr, true, MCP_FETCH_TIMEOUT)
                    .await?;
                let raw = value.as_str().unwrap_or("").to_string();
                let parsed: Value = serde_json::from_str(&raw)
                    .map_err(|e| anyhow!("invalid fetch response JSON: {e}"))?;
                let pretty = serde_json::to_string_pretty(&parsed)?;
                Ok(text_content(pretty))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// browser_select_element
// ---------------------------------------------------------------------------

fn make_select_element() -> RegisteredTool {
    RegisteredTool {
        name: "browser_select_element".into(),
        description:
            "Show an interactive overlay; resolve with the CSS selector for the clicked element."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({})),
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let expr = SELECT_ELEMENT_JS.to_string();
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                // select_element shows an interactive overlay that the
                // human clicks — extend the bound generously so the
                // human has time to click.
                let value = backend
                    .evaluate(&target_id, &expr, true, MCP_SELECT_ELEMENT_TIMEOUT)
                    .await?;
                let selector = value.as_str().unwrap_or("").to_string();
                Ok(text_content(selector))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// list_targets (legacy, CDP-shaped info-dense diagnostic)
// ---------------------------------------------------------------------------

fn make_list_targets() -> RegisteredTool {
    RegisteredTool {
        name: "list_targets".into(),
        description: "List open page targets, optionally filtered by an unanchored URL regex. \
                      CDP-shaped diagnostic; agents typically want `browser_tab_list`."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "description": "Optional unanchored URL regex."
                }
            },
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let filter_re = args
                    .get("filter")
                    .and_then(|v| v.as_str())
                    .map(Regex::new)
                    .transpose()
                    .map_err(|e| anyhow!("invalid `filter` regex: {e}"))?;
                // Route through the server-owned backend rather than opening
                // a fresh BiDi session (which would fail/race on Firefox).
                // `live_targets` is the same primitive `browser_tab_list`
                // uses; re-shape it into the legacy CDP-style `TargetInfo`.
                let backend = state.ensure_backend().await?;
                let kind = match state.browser_snapshot().await.engine {
                    Engine::Cdp => "page",
                    Engine::Bidi => "context",
                };
                let targets: Vec<TargetInfo> = backend
                    .live_targets()
                    .await?
                    .into_iter()
                    .filter(|t| filter_re.as_ref().map_or(true, |re| re.is_match(&t.url)))
                    .map(|t| TargetInfo {
                        id: t.id,
                        url: t.url,
                        title: t.title,
                        kind: kind.to_string(),
                    })
                    .collect();
                Ok(text_content(serde_json::to_string_pretty(&targets)?))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// browser_cookies
// ---------------------------------------------------------------------------

fn make_cookies() -> RegisteredTool {
    RegisteredTool {
        name: "browser_cookies".into(),
        description: "Fetch cookies from the active browser. Returns full values (MCP is a \
                      trusted local channel). Optional unanchored regex filters."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "domain": { "type": "string", "description": "Unanchored regex on cookie domain." },
                "name":   { "type": "string", "description": "Unanchored regex on cookie name." }
            },
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let domain_re = args
                    .get("domain")
                    .and_then(|v| v.as_str())
                    .map(Regex::new)
                    .transpose()
                    .map_err(|e| anyhow!("invalid `domain` regex: {e}"))?;
                let name_re = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(Regex::new)
                    .transpose()
                    .map_err(|e| anyhow!("invalid `name` regex: {e}"))?;
                // Route through the server-owned backend (reuses the open
                // session) instead of `fetch_cookies`, which opens a fresh
                // BiDi session and would fail/race on Firefox.
                let backend = state.ensure_backend().await?;
                let all = backend.cookies().await?;
                let filtered: Vec<_> = all
                    .into_iter()
                    .filter(|c| {
                        domain_re.as_ref().map_or(true, |re| re.is_match(&c.domain))
                            && name_re.as_ref().map_or(true, |re| re.is_match(&c.name))
                    })
                    .collect();
                Ok(text_content(serde_json::to_string_pretty(&filtered)?))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// browser_storage_get / browser_storage_set
// ---------------------------------------------------------------------------

fn make_storage_get() -> RegisteredTool {
    RegisteredTool {
        name: "browser_storage_get".into(),
        description: "Read a value from localStorage or sessionStorage on the active page.".into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "key": { "type": "string" },
                "namespace": {
                    "type": "string",
                    "enum": ["local", "session"],
                    "default": "local"
                },
                "max_age": {
                    "type": "string",
                    "description": "Reload the page first if its document is older than this duration (default 10m)."
                }
            })),
            "required": ["key"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let key = args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing 'key'"))?
                    .to_string();
                let namespace = args
                    .get("namespace")
                    .and_then(|v| v.as_str())
                    .unwrap_or("local");
                let ns = ns_global(namespace)?;
                let expr = build_get_expr(ns, &key);
                let max_age = max_age_arg(&args)?;
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                backend.ensure_fresh(&target_id, max_age).await?;
                let value = backend
                    .evaluate(&target_id, &expr, true, MCP_OP_TIMEOUT)
                    .await?;
                // `build_get_expr` wraps the result in JSON.stringify, so the
                // evaluator returns a JSON string. Unwrap one layer to surface
                // the raw value (or `null` when the key is absent).
                let text = match value {
                    Value::String(s) => s,
                    Value::Null => "null".to_string(),
                    other => other.to_string(),
                };
                Ok(text_content(text))
            })
        }),
    }
}

fn make_storage_set() -> RegisteredTool {
    RegisteredTool {
        name: "browser_storage_set".into(),
        description: "Write a value to localStorage or sessionStorage on the active page.".into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "key": { "type": "string" },
                "value": { "type": "string" },
                "namespace": {
                    "type": "string",
                    "enum": ["local", "session"],
                    "default": "local"
                }
            })),
            "required": ["key", "value"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let key = args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing 'key'"))?
                    .to_string();
                let value = args
                    .get("value")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing 'value'"))?
                    .to_string();
                let namespace = args
                    .get("namespace")
                    .and_then(|v| v.as_str())
                    .unwrap_or("local");
                let ns = ns_global(namespace)?;
                let expr = build_set_expr(ns, &key, &value);
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                let _ = backend
                    .evaluate(&target_id, &expr, true, MCP_OP_TIMEOUT)
                    .await?;
                Ok(text_content("ok"))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// browser_wait_for_cookie
// ---------------------------------------------------------------------------

fn make_wait_for_cookie() -> RegisteredTool {
    RegisteredTool {
        name: "browser_wait_for_cookie".into(),
        description: "Poll the browser until a cookie matching the regex filters appears, or \
                      timeout elapses."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "domain": { "type": "string", "description": "Unanchored regex on cookie domain." },
                "name":   { "type": "string", "description": "Unanchored regex on cookie name." },
                "timeout_seconds": { "type": "number", "default": 120 },
                "poll_interval_seconds": { "type": "number", "default": 1 }
            },
            "required": ["domain", "name"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let domain = args
                    .get("domain")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing 'domain'"))?;
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing 'name'"))?;
                let domain_re =
                    Regex::new(domain).map_err(|e| anyhow!("invalid `domain` regex: {e}"))?;
                let name_re = Regex::new(name).map_err(|e| anyhow!("invalid `name` regex: {e}"))?;
                let timeout_s = args
                    .get("timeout_seconds")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(120.0)
                    .max(0.0);
                let interval_s = args
                    .get("poll_interval_seconds")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1.0)
                    .max(0.001);
                let deadline = Instant::now() + Duration::from_secs_f64(timeout_s);
                let interval = Duration::from_secs_f64(interval_s);
                // Acquire the server-owned backend once; reuse it each poll
                // rather than opening a fresh BiDi session per iteration
                // (which would fail/race on Firefox).
                let backend = state.ensure_backend().await?;
                loop {
                    let cookies = backend.cookies().await?;
                    if let Some(c) = cookies
                        .into_iter()
                        .find(|c| cookie_matches(c, &domain_re, &name_re))
                    {
                        return Ok(text_content(c.name));
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(anyhow!("timed out waiting for cookie"));
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    let nap = std::cmp::min(interval, remaining);
                    if nap.is_zero() {
                        return Err(anyhow!("timed out waiting for cookie"));
                    }
                    tokio::time::sleep(nap).await;
                }
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// browser_tab_list / browser_tab_new / browser_tab_select / browser_tab_close
// ---------------------------------------------------------------------------

fn make_tab_list() -> RegisteredTool {
    RegisteredTool {
        name: "browser_tab_list".into(),
        description: "List open tabs in the active browser, Playwright-shaped \
                      (`[{target_id, url, title, active}]`)."
            .into(),
        input_schema: json!({"type": "object", "properties": {}}),
        handler: handler(|state, _args| {
            Box::pin(async move {
                let v = tab_list_value(&state).await?;
                Ok(text_content(serde_json::to_string_pretty(&v)?))
            })
        }),
    }
}

/// Build the `[{target_id, url, title, active}]` value for the current
/// browser. Shared between `browser_tab_list` and `browser_select`'s
/// response.
async fn tab_list_value(state: &ServerState) -> Result<Value> {
    let backend = state.ensure_backend().await?;
    let targets = backend.live_targets().await?;
    let active = state.active_target_id.lock().await.clone();
    let arr: Vec<Value> = targets
        .into_iter()
        .map(|t| {
            json!({
                "target_id": t.id,
                "url": t.url,
                "title": t.title,
                "active": active.as_deref() == Some(t.id.as_str()),
            })
        })
        .collect();
    Ok(Value::Array(arr))
}

fn make_tab_new() -> RegisteredTool {
    RegisteredTool {
        name: "browser_tab_new".into(),
        description: "Create a new tab and make it the active tab. Defaults to about:blank.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Optional URL; defaults to about:blank." }
            },
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let url = args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("about:blank")
                    .to_string();
                let backend = state.ensure_backend().await?;
                let tid = backend.create_tab(&url).await?;
                *state.active_target_id.lock().await = Some(tid.clone());
                Ok(text_content(serde_json::to_string_pretty(&json!({
                    "target_id": tid,
                    "url": url,
                    "active": true,
                }))?))
            })
        }),
    }
}

fn make_tab_select() -> RegisteredTool {
    RegisteredTool {
        name: "browser_tab_select".into(),
        description: "Set the active tab. Probe-and-iterate: errors `TabHung` if the selected \
                      tab doesn't respond to a 500ms probe (agent should pick another or call \
                      `browser_tab_new`)."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "target_id": { "type": "string" }
            },
            "required": ["target_id"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                use crate::errors::SessionError;
                let tid = args
                    .get("target_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing 'target_id'"))?
                    .to_string();
                let backend = state.ensure_backend().await?;
                let live = backend.live_target_ids().await?;
                if !live.contains(&tid) {
                    return Err(SessionError::TabNotFound {
                        browser: state
                            .registered_browser_name()
                            .await
                            .unwrap_or_else(|_| "<external>".to_string()),
                        name: tid,
                    }
                    .into());
                }
                // Probe the tab. We don't auto-recreate on hang — the
                // agent asked for THIS tab; bubble up `TabHung` so they
                // can choose to `browser_tab_new` or pick a different
                // tab.
                let probed = tokio::time::timeout(
                    TAB_SELECT_PROBE,
                    backend.evaluate(&tid, "1", false, TAB_SELECT_PROBE),
                )
                .await;
                let ok = matches!(probed, Ok(Ok(_)));
                if !ok {
                    return Err(SessionError::TabHung {
                        target_id: Some(tid),
                        url: None,
                        timeout_ms: TAB_SELECT_PROBE.as_millis() as u64,
                        hint: "selected-tab-hung",
                    }
                    .into());
                }
                *state.active_target_id.lock().await = Some(tid.clone());
                Ok(text_content(serde_json::to_string_pretty(&json!({
                    "target_id": tid,
                    "active": true,
                }))?))
            })
        }),
    }
}

fn make_tab_close() -> RegisteredTool {
    RegisteredTool {
        name: "browser_tab_close".into(),
        description: "Close a tab. Defaults to the active tab; clears the active pointer if the \
                      closed tab was active."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "target_id": { "type": "string", "description": "Optional; defaults to active tab." }
            },
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let backend = state.ensure_backend().await?;
                let explicit = args
                    .get("target_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let active = state.active_target_id.lock().await.clone();
                let tid = match (explicit, &active) {
                    (Some(e), _) => e,
                    (None, Some(a)) => a.clone(),
                    (None, None) => {
                        return Err(anyhow!("no `target_id` given and no active tab to close"));
                    }
                };
                backend.close_tab(&tid).await?;
                // If we just closed the active tab, clear the pointer.
                let mut ptr = state.active_target_id.lock().await;
                if ptr.as_deref() == Some(tid.as_str()) {
                    *ptr = None;
                }
                Ok(text_content(serde_json::to_string_pretty(&json!({
                    "closed": tid,
                }))?))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// browser_select / browser_list
// ---------------------------------------------------------------------------

fn make_browser_select() -> RegisteredTool {
    RegisteredTool {
        name: "browser_select".into(),
        description: "Switch the active browser by registered name (or any selector accepted by \
                      the CLI, e.g. `chrome`, `firefox-pikachu`). The switch is committed before \
                      Firefox BiDi lock preparation; if preparation fails, the new browser remains \
                      active and the caller decides whether to retry, switch elsewhere, or switch back."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing 'name'"))?
                    .to_string();
                let selector = crate::cli::env_resolver::parse(&name)?;
                let resolved = crate::mcp::server::resolve_browser_send(selector).await?;
                let resolved_clone = resolved.clone();
                state.switch_browser(resolved).await?;
                let tabs = tab_list_value(&state).await?;
                Ok(text_content(serde_json::to_string_pretty(&json!({
                    "name": match &resolved_clone.source {
                        crate::cli::env_resolver::Source::Registered { name } => name.as_str(),
                        crate::cli::env_resolver::Source::External => "<external>",
                    },
                    "engine": match resolved_clone.engine {
                        crate::detect::Engine::Cdp => "cdp",
                        crate::detect::Engine::Bidi => "bidi",
                    },
                    "endpoint": resolved_clone.endpoint,
                    "tabs": tabs,
                }))?))
            })
        }),
    }
}

fn make_browser_list() -> RegisteredTool {
    RegisteredTool {
        name: "browser_list".into(),
        description: "List live registered browsers with `[{name, kind, engine, endpoint, alive}]`; dead-process rows are pruned."
            .into(),
        input_schema: json!({"type": "object", "properties": {}}),
        handler: handler(|_state, _args| {
            Box::pin(async move {
                // `Registry` is `!Send`; do the read on a blocking thread.
                let arr = tokio::task::spawn_blocking(|| -> Result<Vec<Value>> {
                    let registry = crate::registry::Registry::open()?;
                    let rows = registry.list_alive()?;
                    Ok(rows
                        .into_iter()
                        .map(|r| {
                            json!({
                                "name": r.name,
                                "kind": r.kind.as_str(),
                                "engine": match r.engine {
                                    crate::detect::Engine::Cdp => "cdp",
                                    crate::detect::Engine::Bidi => "bidi",
                                },
                                "endpoint": r.endpoint,
                                "alive": true,
                            })
                        })
                        .collect())
                })
                .await??;
                Ok(text_content(serde_json::to_string_pretty(&Value::Array(
                    arr,
                ))?))
            })
        }),
    }
}

fn make_browser_show() -> RegisteredTool {
    RegisteredTool {
        name: "browser_show".into(),
        description: "Explicitly reveal the active browser window for login or debugging. \
                      Normal automation keeps new tabs in the background."
            .into(),
        input_schema: json!({"type": "object", "properties": {}}),
        handler: handler(|state, _args| {
            Box::pin(async move {
                let backend = state.ensure_backend().await?;
                let target_id = backend.target_for_show().await?;
                let resolved = state.browser_snapshot().await;
                let source = resolved.source.clone();
                let os_activated = tokio::task::spawn_blocking(move || -> Result<bool> {
                    let registry = crate::registry::Registry::open()?;
                    crate::cli::show::activate_resolved_app(&registry, &source)
                })
                .await??;
                backend.show_tab(&target_id).await?;
                Ok(text_content(serde_json::to_string_pretty(&json!({
                    "target_id": target_id,
                    "os_activated": os_activated,
                }))?))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Playwright-only interaction tools (routed through the Node sidecar).
// ---------------------------------------------------------------------------
//
// Each tool:
//   1. Resolves the target tab via `state.resolve_target_for_args(args)`.
//   2. Acquires the sidecar via `state.ensure_sidecar(tool_name)`. On
//      BiDi browsers this errors with `EngineUnsupported`.
//   3. Forwards to the sidecar with `target_id` + tool-specific params.

/// Forward a sidecar call. Resolves the target, ensures the sidecar is
/// up, sends the RPC with `target_id` merged into the params.
async fn forward_to_sidecar(
    state: &ServerState,
    tool_name: &str,
    args: &Value,
    sidecar_method: &str,
    mut params: serde_json::Map<String, Value>,
) -> Result<Value> {
    // Preflight: check engine support before resolving the target.
    // `ensure_sidecar` returns `EngineUnsupported` on BiDi browsers;
    // calling it first avoids opening a backend / creating a tab
    // only to discard it.
    let sc = state.ensure_sidecar(tool_name).await?;
    let (_, target_id) = state.resolve_target_for_args(args).await?;
    params.insert("target_id".into(), Value::String(target_id));
    sc.call(sidecar_method, Value::Object(params)).await
}

fn make_snapshot() -> RegisteredTool {
    RegisteredTool {
        name: "browser_snapshot".into(),
        description: "Capture an accessibility-tree snapshot (YAML) of the active page. \
                      Chromium-only via Playwright sidecar."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_schema(),
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let v = forward_to_sidecar(
                    &state,
                    "browser_snapshot",
                    &args,
                    "snapshot",
                    serde_json::Map::new(),
                )
                .await?;
                let yaml = v
                    .get("snapshot")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default();
                Ok(text_content(yaml))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Table-driven sidecar interaction tools.
//
// click / type / hover / drag / press_key / wait_for all share one shape:
// build a param map from a fixed set of args, forward to the sidecar, return a
// fixed success string. Previously each tool declared its params *twice* — once
// in the input schema (`tab_args_properties`) and once in the handler (`copy_arg` per
// param) — with no compiler link, so a schema param missing a matching
// `copy_arg` was silently dropped before reaching the sidecar.
//
// `SidecarTool` is the single source of truth: each param's name + schema +
// required-ness is declared once in `params`, and BOTH the input schema and the
// param-forwarding are derived from it, so a param can't be in the schema but
// missing from the wire (or vice versa).
// ---------------------------------------------------------------------------

/// One sidecar-forwarded parameter, declared once. Drives both the JSON schema
/// (`schema`, `required`) and the runtime forwarding (`name`).
struct SidecarParam {
    name: &'static str,
    schema: Value,
    required: bool,
}

/// Declarative spec for a sidecar interaction tool. Both the input schema and
/// the param-forwarding are derived from the single `params` slice.
struct SidecarTool {
    name: &'static str,
    description: &'static str,
    /// The sidecar RPC method (e.g. `"click"`).
    method: &'static str,
    params: Vec<SidecarParam>,
    /// Fixed success message returned as text content.
    success: &'static str,
}

impl SidecarTool {
    fn build(self) -> RegisteredTool {
        let SidecarTool {
            name,
            description,
            method,
            params,
            success,
        } = self;

        // Schema: shared tab/target args plus this tool's params, with the
        // `required` list derived from the same table.
        let extra = Value::Object(
            params
                .iter()
                .map(|p| (p.name.to_string(), p.schema.clone()))
                .collect(),
        );
        let required: Vec<&str> = params
            .iter()
            .filter(|p| p.required)
            .map(|p| p.name)
            .collect();
        let mut input_schema = json!({
            "type": "object",
            "properties": tab_args_properties(extra),
        });
        if !required.is_empty() {
            input_schema["required"] = json!(required);
        }

        // Forwarding: copy exactly the params declared above — no second list
        // to drift out of sync.
        let param_names: Vec<&'static str> = params.iter().map(|p| p.name).collect();
        RegisteredTool {
            name: name.into(),
            description: description.into(),
            input_schema,
            handler: handler(move |state, args| {
                let param_names = param_names.clone();
                Box::pin(async move {
                    let mut params = serde_json::Map::new();
                    for key in &param_names {
                        copy_arg(&args, key, &mut params);
                    }
                    forward_to_sidecar(&state, name, &args, method, params).await?;
                    Ok(text_content(success))
                })
            }),
        }
    }
}

fn make_click() -> RegisteredTool {
    SidecarTool {
        name: "browser_click",
        description: "Click an element matched by CSS selector. Chromium-only.",
        method: "click",
        params: vec![
            SidecarParam {
                name: "selector",
                schema: json!({"type": "string"}),
                required: true,
            },
            SidecarParam {
                name: "timeout_ms",
                schema: json!({"type": "integer"}),
                required: false,
            },
        ],
        success: "clicked",
    }
    .build()
}

fn make_type() -> RegisteredTool {
    SidecarTool {
        name: "browser_type",
        description: "Type text into an input matched by CSS selector. \
                      `press_sequentially=true` simulates keystrokes; default uses fast `fill`. \
                      Chromium-only.",
        method: "type",
        params: vec![
            SidecarParam {
                name: "selector",
                schema: json!({"type": "string"}),
                required: true,
            },
            SidecarParam {
                name: "text",
                schema: json!({"type": "string"}),
                required: true,
            },
            SidecarParam {
                name: "press_sequentially",
                schema: json!({"type": "boolean"}),
                required: false,
            },
            SidecarParam {
                name: "timeout_ms",
                schema: json!({"type": "integer"}),
                required: false,
            },
        ],
        success: "typed",
    }
    .build()
}

fn make_hover() -> RegisteredTool {
    SidecarTool {
        name: "browser_hover",
        description: "Hover an element matched by CSS selector. Chromium-only.",
        method: "hover",
        params: vec![
            SidecarParam {
                name: "selector",
                schema: json!({"type": "string"}),
                required: true,
            },
            SidecarParam {
                name: "timeout_ms",
                schema: json!({"type": "integer"}),
                required: false,
            },
        ],
        success: "hovered",
    }
    .build()
}

fn make_drag() -> RegisteredTool {
    SidecarTool {
        name: "browser_drag",
        description: "Drag from one CSS-selected element to another. Chromium-only.",
        method: "drag",
        params: vec![
            SidecarParam {
                name: "source_selector",
                schema: json!({"type": "string"}),
                required: true,
            },
            SidecarParam {
                name: "target_selector",
                schema: json!({"type": "string"}),
                required: true,
            },
        ],
        success: "dragged",
    }
    .build()
}

fn make_press_key() -> RegisteredTool {
    SidecarTool {
        name: "browser_press_key",
        description: "Press a keyboard key (Playwright key name, e.g. 'Enter', 'Control+A'). \
                      Chromium-only.",
        method: "press_key",
        params: vec![SidecarParam {
            name: "key",
            schema: json!({"type": "string"}),
            required: true,
        }],
        success: "pressed",
    }
    .build()
}

fn make_wait_for() -> RegisteredTool {
    SidecarTool {
        name: "browser_wait_for",
        description: "Wait for a condition: a selector reaching `state`, a URL matching \
                      `url_regex`, or the page reaching `load_state` (`load` / \
                      `domcontentloaded` / `networkidle`). Chromium-only.",
        method: "wait_for",
        params: vec![
            SidecarParam { name: "selector", schema: json!({"type": "string"}), required: false },
            SidecarParam { name: "state", schema: json!({"type": "string", "enum": ["attached", "detached", "visible", "hidden"]}), required: false },
            SidecarParam { name: "url_regex", schema: json!({"type": "string"}), required: false },
            SidecarParam { name: "load_state", schema: json!({"type": "string", "enum": ["load", "domcontentloaded", "networkidle"]}), required: false },
            SidecarParam { name: "timeout_ms", schema: json!({"type": "integer"}), required: false },
        ],
        success: "ok",
    }
    .build()
}

fn make_pdf_save() -> RegisteredTool {
    RegisteredTool {
        name: "browser_pdf_save".into(),
        description: "Render the active page to PDF (base64 in `pdf_base64`). Chromium-only."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_schema(),
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let v = forward_to_sidecar(
                    &state,
                    "browser_pdf_save",
                    &args,
                    "pdf",
                    serde_json::Map::new(),
                )
                .await?;
                let b64 = v
                    .get("pdf_base64")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default();
                Ok(json!({
                    "content": [{
                        "type": "resource",
                        "resource": { "mimeType": "application/pdf", "blob": b64 }
                    }]
                }))
            })
        }),
    }
}

/// Helper: copy a key from `args` into `dst` if present.
fn copy_arg(args: &Value, key: &str, dst: &mut serde_json::Map<String, Value>) {
    if let Some(v) = args.get(key) {
        dst.insert(key.into(), v.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::Mutex;
    use tokio_tungstenite::tungstenite::Message;

    /// All tools the registry exposes after `register_all`. Mirrors the
    /// registration order in `register_all`.
    const EXPECTED_TOOLS: &[&str] = &[
        "browser_navigate",
        "browser_get_html",
        "browser_take_screenshot",
        "browser_fetch",
        "browser_select_element",
        "browser_cookies",
        "browser_storage_get",
        "browser_storage_set",
        "browser_wait_for_cookie",
        "list_targets",
        "browser_tab_list",
        "browser_tab_new",
        "browser_tab_select",
        "browser_tab_close",
        "browser_select",
        "browser_list",
        "browser_show",
        "browser_snapshot",
        "browser_click",
        "browser_type",
        "browser_hover",
        "browser_drag",
        "browser_press_key",
        "browser_wait_for",
        "browser_pdf_save",
    ];

    fn schema_for(name: &str) -> Value {
        let registry = ToolRegistry::new();
        register_all(&registry);
        registry
            .list()
            .into_iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("tool {name} not registered"))["inputSchema"]
            .clone()
    }

    fn tool_description(name: &str) -> String {
        let registry = ToolRegistry::new();
        register_all(&registry);
        registry
            .list()
            .into_iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("tool {name} not registered"))["description"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    struct ScreenshotMock {
        endpoint: String,
        capture_params: Arc<Mutex<Vec<Value>>>,
    }

    async fn spawn_screenshot_mock(selector_rect: Value) -> ScreenshotMock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let capture_params = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn({
            let capture_params = capture_params.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let mut next_session = 0u32;
                let mut eval_count = 0u32;
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let req: Value = serde_json::from_str(&t).unwrap();
                    let id = req["id"].as_u64().unwrap();
                    let method = req["method"].as_str().unwrap_or("");
                    let result = match method {
                        "Target.getTargets" => json!({
                            "targetInfos": [{
                                "targetId": "T1",
                                "type": "page",
                                "url": "https://example.com/",
                                "title": "Example",
                            }]
                        }),
                        "Target.attachToTarget" => {
                            next_session += 1;
                            json!({"sessionId": format!("S{next_session}")})
                        }
                        "Target.detachFromTarget" => json!({}),
                        "Inspector.enable" => json!({}),
                        "Runtime.evaluate" => {
                            eval_count += 1;
                            if eval_count == 1 {
                                json!({"result": {"value": 1}})
                            } else {
                                json!({"result": {"value": selector_rect.clone()}})
                            }
                        }
                        "Page.captureScreenshot" => {
                            capture_params.lock().await.push(req["params"].clone());
                            json!({"data": "PNGDATA"})
                        }
                        _ => json!({}),
                    };
                    let resp = json!({"id": id, "result": result});
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                }
            }
        });
        ScreenshotMock {
            endpoint: format!("ws://{addr}"),
            capture_params,
        }
    }

    #[test]
    fn register_all_includes_expected_set() {
        let registry = ToolRegistry::new();
        register_all(&registry);
        let list = registry.list();
        let names: Vec<&str> = list.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in EXPECTED_TOOLS {
            assert!(
                names.contains(expected),
                "missing tool {expected} in {names:?}"
            );
        }
        assert_eq!(
            list.len(),
            EXPECTED_TOOLS.len(),
            "extra tools present: {names:?}"
        );
    }

    #[test]
    fn every_tool_has_object_input_schema() {
        let registry = ToolRegistry::new();
        register_all(&registry);
        for t in registry.list() {
            let schema = &t["inputSchema"];
            assert!(schema.is_object(), "schema not object: {schema}");
            assert_eq!(
                schema["type"], "object",
                "schema type != object for {}: {schema}",
                t["name"]
            );
        }
    }

    #[test]
    fn list_targets_schema_has_optional_filter() {
        let schema = schema_for("list_targets");
        assert_eq!(schema["properties"]["filter"]["type"], "string");
        assert!(
            schema.get("required").is_none() || schema["required"].as_array().unwrap().is_empty()
        );
    }

    #[test]
    fn browser_cookies_schema_has_optional_filters() {
        let schema = schema_for("browser_cookies");
        assert_eq!(schema["properties"]["domain"]["type"], "string");
        assert_eq!(schema["properties"]["name"]["type"], "string");
        assert!(
            schema.get("required").is_none() || schema["required"].as_array().unwrap().is_empty()
        );
    }

    #[test]
    fn browser_storage_get_requires_key() {
        let schema = schema_for("browser_storage_get");
        let required = schema["required"].as_array().expect("required array");
        assert!(required.iter().any(|v| v == "key"));
        assert_eq!(schema["properties"]["key"]["type"], "string");
        assert_eq!(schema["properties"]["namespace"]["type"], "string");
    }

    #[test]
    fn browser_storage_set_requires_key_and_value() {
        let schema = schema_for("browser_storage_set");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"key"));
        assert!(required.contains(&"value"));
        assert_eq!(schema["properties"]["value"]["type"], "string");
    }

    #[test]
    fn browser_wait_for_cookie_requires_domain_and_name() {
        let schema = schema_for("browser_wait_for_cookie");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"domain"));
        assert!(required.contains(&"name"));
        assert_eq!(schema["properties"]["timeout_seconds"]["type"], "number");
        assert_eq!(
            schema["properties"]["poll_interval_seconds"]["type"],
            "number"
        );
    }

    #[test]
    fn browser_navigate_schema_has_tab_and_target() {
        // Per-tab tools expose optional `tab`/`target` for routing.
        let schema = schema_for("browser_navigate");
        assert_eq!(schema["properties"]["tab"]["type"], "string");
        assert_eq!(schema["properties"]["target"]["type"], "string");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"url"));
        assert!(!required.contains(&"tab"));
        assert!(!required.contains(&"target"));
    }

    #[test]
    fn browser_tab_select_requires_target_id() {
        let schema = schema_for("browser_tab_select");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"target_id"));
    }

    #[test]
    fn browser_tab_close_target_id_is_optional() {
        // Default = close active tab; no required args.
        let schema = schema_for("browser_tab_close");
        assert!(
            schema.get("required").is_none() || schema["required"].as_array().unwrap().is_empty()
        );
        assert_eq!(schema["properties"]["target_id"]["type"], "string");
    }

    #[test]
    fn browser_select_requires_name() {
        let schema = schema_for("browser_select");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"name"));
    }

    #[test]
    fn browser_select_description_documents_failed_lock_contract() {
        let desc = tool_description("browser_select");
        assert!(desc.contains("committed before"));
        assert!(desc.contains("new browser remains active"));
        assert!(desc.contains("switch back"));
    }

    #[test]
    fn browser_list_has_no_args() {
        let schema = schema_for("browser_list");
        assert_eq!(schema["properties"], json!({}));
    }

    #[test]
    fn browser_cookies_schema_has_no_tab_arg() {
        // Cookies are browser-wide; no per-tab routing.
        let schema = schema_for("browser_cookies");
        assert!(schema["properties"].get("tab").is_none());
        assert!(schema["properties"].get("target").is_none());
    }

    /// Sidecar-routed tools expose `tab`/`target` for the same routing
    /// surface as the other per-tab tools.
    #[test]
    fn sidecar_tools_expose_tab_and_target() {
        for name in &[
            "browser_snapshot",
            "browser_click",
            "browser_type",
            "browser_hover",
            "browser_drag",
            "browser_press_key",
            "browser_wait_for",
            "browser_pdf_save",
        ] {
            let schema = schema_for(name);
            assert_eq!(
                schema["properties"]["tab"]["type"], "string",
                "{name} missing tab arg"
            );
            assert_eq!(
                schema["properties"]["target"]["type"], "string",
                "{name} missing target arg"
            );
        }
    }

    /// `browser_click` / `browser_type` etc. require their selector
    /// args; `browser_snapshot` / `browser_pdf_save` / `browser_wait_for`
    /// don't (snapshot is page-wide, wait_for has multiple alternative
    /// conditions, pdf is page-wide).
    #[test]
    fn sidecar_tools_required_args() {
        let click = schema_for("browser_click");
        let req: Vec<&str> = click["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(req.contains(&"selector"));

        let t = schema_for("browser_type");
        let req: Vec<&str> = t["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(req.contains(&"selector"));
        assert!(req.contains(&"text"));

        // No required args on these.
        let snap = schema_for("browser_snapshot");
        assert!(snap.get("required").is_none() || snap["required"].as_array().unwrap().is_empty());
        let pdf = schema_for("browser_pdf_save");
        assert!(pdf.get("required").is_none() || pdf["required"].as_array().unwrap().is_empty());
    }

    /// Sidecar tool against a BiDi browser must error with
    /// `EngineUnsupported` BEFORE attempting to spawn the sidecar — so
    /// even systems without Node/Bun get a clean message.
    #[tokio::test]
    async fn sidecar_tool_on_bidi_returns_engine_unsupported() {
        use crate::cli::env_resolver::{ResolvedBrowser, Source};
        use crate::detect::Engine;
        use crate::errors::SessionError;

        // ServerState bound to a BiDi browser. Endpoint never gets hit
        // because the engine check short-circuits.
        let resolved = ResolvedBrowser {
            engine: Engine::Bidi,
            endpoint: "ws://127.0.0.1:0".into(),
            source: Source::External,
        };
        let state = ServerState::new(resolved);

        let err = match state.ensure_sidecar("browser_snapshot").await {
            Ok(_) => panic!("BiDi must error"),
            Err(e) => e,
        };
        let typed = err.downcast_ref::<SessionError>().expect("typed error");
        match typed {
            SessionError::EngineUnsupported { tool, hint, .. } => {
                assert_eq!(tool, "browser_snapshot");
                assert!(!hint.contains(concat!("browser_", "evaluate")));
                assert!(hint.contains("browser_get_html"));
                assert!(hint.contains("browser_select"));
            }
            other => panic!("expected EngineUnsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn screenshot_selector_sends_cdp_clip() {
        let mock = spawn_screenshot_mock(json!({
            "x": 12.5,
            "y": 34.0,
            "width": 56.0,
            "height": 78.0,
        }))
        .await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: mock.endpoint,
            source: Source::External,
        });
        let h = handler_for("browser_take_screenshot");
        let out = h(
            state,
            json!({
                "target": "example\\.com",
                "selector": "#main",
            }),
        )
        .await
        .unwrap();
        assert_eq!(out["content"][0]["type"], "image");
        assert_eq!(out["content"][0]["data"], "PNGDATA");

        let captures = mock.capture_params.lock().await;
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0]["format"], "png");
        assert_eq!(captures[0]["captureBeyondViewport"], true);
        assert_eq!(captures[0]["clip"]["x"], json!(12.5));
        assert_eq!(captures[0]["clip"]["y"], json!(34.0));
        assert_eq!(captures[0]["clip"]["width"], json!(56.0));
        assert_eq!(captures[0]["clip"]["height"], json!(78.0));
        assert_eq!(captures[0]["clip"]["scale"], json!(1));
    }

    #[tokio::test]
    async fn screenshot_selector_null_rect_errors_clearly() {
        let mock = spawn_screenshot_mock(Value::Null).await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: mock.endpoint,
            source: Source::External,
        });
        let h = handler_for("browser_take_screenshot");
        let err = h(
            state,
            json!({
                "target": "example\\.com",
                "selector": "#missing",
            }),
        )
        .await
        .expect_err("null selector rect must error");
        assert!(
            err.to_string()
                .contains("selector matched no visible element: #missing"),
            "got: {err:#}"
        );
        assert!(mock.capture_params.lock().await.is_empty());
    }

    // -- Behavioral handler arg-validation -----------------------------------
    //
    // These invoke the real handler closures (not just the static schema)
    // against a `ServerState` whose endpoint is never reached, because the
    // arg-validation / mutual-exclusion checks fire *before* any backend
    // connection. No browser required.

    use crate::cli::env_resolver::{ResolvedBrowser, Source};
    use crate::detect::Engine;

    /// Fetch a registered tool's handler by name.
    fn handler_for(name: &str) -> ToolHandler {
        let registry = ToolRegistry::new();
        register_all(&registry);
        registry
            .handler(name)
            .unwrap_or_else(|| panic!("tool {name} not registered"))
    }

    /// A `ServerState` bound to an endpoint that is never reached (the
    /// handler errors during validation first). Marked CDP so we don't
    /// trip the BiDi-lock path.
    fn unreached_state() -> ServerState {
        ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            // Port 0 never accepts; any attempt to open a backend would
            // fail, but these tests assert the *validation* error fires
            // first.
            endpoint: "ws://127.0.0.1:0".into(),
            source: Source::External,
        })
    }

    #[tokio::test]
    async fn navigate_missing_url_errors_before_backend() {
        let h = handler_for("browser_navigate");
        let err = h(unreached_state(), json!({}))
            .await
            .expect_err("missing url must error");
        assert!(err.to_string().contains("missing 'url'"), "got: {err:#}");
    }

    #[tokio::test]
    async fn fetch_missing_url_errors_before_backend() {
        let h = handler_for("browser_fetch");
        let err = h(unreached_state(), json!({"method": "GET"}))
            .await
            .expect_err("missing url must error");
        assert!(err.to_string().contains("missing 'url'"), "got: {err:#}");
    }

    #[tokio::test]
    async fn storage_set_missing_value_errors_before_backend() {
        let h = handler_for("browser_storage_set");
        let err = h(unreached_state(), json!({"key": "k"}))
            .await
            .expect_err("missing value must error");
        assert!(err.to_string().contains("missing 'value'"), "got: {err:#}");
    }

    #[tokio::test]
    async fn storage_get_missing_key_errors_before_backend() {
        let h = handler_for("browser_storage_get");
        let err = h(unreached_state(), json!({}))
            .await
            .expect_err("missing key must error");
        assert!(err.to_string().contains("missing 'key'"), "got: {err:#}");
    }

    /// `tab` and `target` are mutually exclusive; the reject fires in
    /// `resolve_target_for_args` before any backend connection.
    #[tokio::test]
    async fn navigate_tab_and_target_mutually_exclusive() {
        let h = handler_for("browser_navigate");
        let err = h(
            unreached_state(),
            json!({"url": "https://e.test/", "tab": "a", "target": "b"}),
        )
        .await
        .expect_err("tab+target must error");
        assert!(
            err.to_string().contains("mutually exclusive"),
            "got: {err:#}"
        );
    }
}
