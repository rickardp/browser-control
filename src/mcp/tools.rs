//! MCP tools exposed by the `browser-control mcp` server.
//!
//! The tool surface is Playwright-shaped (`browser_*` prefix) plus
//! browser-control extensions (`browser_get_html`, `browser_fetch`,
//! `browser_eval`, `browser_select_element`, `browser_cookies`, `browser_storage_*`,
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

use crate::a11y::{self, FindOptions, RefEntry, SnapshotOptions};
use crate::cli::fetch::script_fetch_timeout_ms;
use crate::cli::storage::{build_get_expr, build_set_expr, ns_global};
use crate::cli::wait_for_cookie::cookie_matches;
use crate::detect::Engine;
use crate::dom::scripts::{
    FETCH_JS, GET_CLIP_RECT_JS, GET_DOM_JS, GET_PAGE_TEXT_JS, SELECT_ELEMENT_JS,
};
use crate::errors::SessionError;
use crate::mcp::server::{RegisteredTool, ServerState, ToolHandler, ToolRegistry};
use crate::session::backend::{ImageFormat, ScreenshotOptions, TabBackend};
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

/// Native wake/probe budget used only after a Playwright sidecar CDP failure.
const SIDECAR_WAKE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Per-op timeout for `Accessibility.getFullAXTree`. Large pages serialise
/// tens of thousands of nodes; 20 s keeps that below the 30 s transport
/// timeout so a wedged renderer still surfaces as recoverable `TabHung`.
const MCP_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);

/// Register the standard tool set onto the given registry.
pub fn register_all(registry: &ToolRegistry) {
    // Renamed-from-Playwright tools.
    registry.register(make_navigate());
    registry.register(make_eval());
    registry.register(make_get_html());
    registry.register(make_get_page_text());
    registry.register(make_take_screenshot());
    registry.register(make_fetch());
    registry.register(make_curl());
    registry.register(make_select_element());
    registry.register(make_cookies());
    registry.register(make_storage_get());
    registry.register(make_storage_set());
    registry.register(make_wait_for_cookie());
    // Passive console/network capture (native CDP or BiDi events; bodies Chromium-only).
    registry.register(make_console_messages());
    registry.register(make_network_requests());
    registry.register(make_network_body());
    // Diagnostic enumeration (kept).
    registry.register(make_list_targets());
    // New tab-management tools.
    registry.register(make_tab_list());
    registry.register(make_tab_new());
    registry.register(make_tab_select());
    registry.register(make_tab_close());
    registry.register(make_tab_foreground());
    // New browser-management tools.
    registry.register(make_browser_start());
    registry.register(make_browser_select());
    registry.register(make_browser_list());
    registry.register(make_browser_show());
    // Native accessibility tools (no sidecar) on both engines.
    registry.register(make_snapshot());
    registry.register(make_find());
    // Interaction tools. A `ref` routes through native input on either
    // engine; a CSS `selector` routes through the Node sidecar and errors
    // with `EngineUnsupported` when the active browser is BiDi.
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

fn image_content(data: String, mime: &str) -> Value {
    json!({
        "content": [ { "type": "image", "data": data, "mimeType": mime } ]
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

fn timeout_ms_arg(args: &Value, key: &str, default: Duration) -> Result<Duration> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(Duration::from_millis)
            .ok_or_else(|| anyhow!("`{key}` number must be non-negative milliseconds")),
        Some(_) => Err(anyhow!(
            "`{key}` must be a non-negative number of milliseconds"
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
                // Make sure capture is live before the load starts so the
                // document request and load-time console output land in
                // the buffers. Bounded by `TOUCH_WAIT`; usually milliseconds.
                state.capture.touch_and_wait(&backend, &target_id).await;
                backend.navigate(&target_id, &url).await?;
                Ok(text_content(format!("Navigated to {url}")))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// browser_eval
// ---------------------------------------------------------------------------

fn make_eval() -> RegisteredTool {
    RegisteredTool {
        name: "browser_eval".into(),
        description: "Evaluate a JavaScript expression in the active page.".into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "expression": {
                    "type": "string",
                    "description": "JavaScript expression to evaluate."
                },
                "await_promise": {
                    "type": "boolean",
                    "default": true,
                    "description": "Treat the expression as a Promise and await it. Ignored on Firefox, which always awaits."
                },
                "timeout_ms": {
                    "type": "number",
                    "description": "Per-call timeout in milliseconds (default 10000)."
                },
                "max_age": {
                    "type": "string",
                    "description": "Reload the page first if its document is older than this duration (default 10m)."
                }
            })),
            "required": ["expression"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let expression = args
                    .get("expression")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing 'expression'"))?
                    .to_string();
                let await_promise = args
                    .get("await_promise")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let timeout = timeout_ms_arg(&args, "timeout_ms", MCP_OP_TIMEOUT)?;
                let max_age = max_age_arg(&args)?;
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                backend.ensure_fresh(&target_id, max_age).await?;
                let value = backend
                    .evaluate(&target_id, &expression, await_promise, timeout)
                    .await?;
                Ok(text_content(serde_json::to_string_pretty(&value)?))
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
// browser_get_page_text
// ---------------------------------------------------------------------------

const PAGE_TEXT_DEFAULT_MAX: usize = 20_000;

fn make_get_page_text() -> RegisteredTool {
    RegisteredTool {
        name: "browser_get_page_text".into(),
        description: "Readable text of the page (article-first: main/article content, page \
                      chrome and hidden elements stripped, headings and list items kept) as \
                      plain text. The cheapest way to read a page; use browser_snapshot when you \
                      need structure and refs, browser_get_html for markup. Works on every \
                      engine including Firefox."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "max_chars": {
                    "type": "integer",
                    "minimum": 500,
                    "description": "Truncate at a line boundary before this many characters. Default 20000."
                },
                "selector": {
                    "type": "string",
                    "description": "Optional CSS selector to extract from instead of the auto-detected main content."
                }
            })),
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let max_chars =
                    count_arg(&args, "max_chars", PAGE_TEXT_DEFAULT_MAX, 500, usize::MAX)?;
                let selector_literal = match string_arg(&args, "selector")? {
                    Some(s) => serde_json::to_string(&s)?,
                    None => "null".to_string(),
                };
                let expr = format!("({GET_PAGE_TEXT_JS})({max_chars}, {selector_literal})");
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                let value = backend
                    .evaluate(&target_id, &expr, false, MCP_OP_TIMEOUT)
                    .await?;
                let raw = value
                    .as_str()
                    .ok_or_else(|| anyhow!("page text script returned no result"))?;
                let parsed: Value = serde_json::from_str(raw)
                    .map_err(|e| anyhow!("page text script returned invalid JSON: {e}"))?;
                if let Some(err) = parsed["error"].as_str() {
                    return Err(anyhow!("{err}"));
                }
                let mut out = String::new();
                if let Some(t) = parsed["title"].as_str().filter(|t| !t.is_empty()) {
                    out.push_str(t);
                    out.push('\n');
                }
                if let Some(u) = parsed["url"].as_str() {
                    out.push_str(u);
                    out.push('\n');
                }
                out.push('\n');
                out.push_str(parsed["text"].as_str().unwrap_or_default());
                if parsed["truncated"].as_bool().unwrap_or(false) {
                    out.push_str(&format!(
                        "\n… [truncated at {} of {} chars; pass max_chars or selector to narrow]",
                        max_chars,
                        parsed["total_chars"].as_u64().unwrap_or(0)
                    ));
                }
                Ok(text_content(out))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// browser_take_screenshot
// ---------------------------------------------------------------------------

/// Parse and validate the screenshot arguments that do not need a
/// backend, so bad input fails before any I/O.
fn screenshot_opts(args: &Value) -> Result<(ScreenshotOptions, Option<std::path::PathBuf>)> {
    let full_page = bool_arg(args, "full_page", false)?;
    let format = match args.get("format") {
        None | Some(Value::Null) => ImageFormat::Png,
        Some(Value::String(s)) if s == "png" => ImageFormat::Png,
        Some(Value::String(s)) if s == "jpeg" || s == "jpg" => ImageFormat::Jpeg,
        Some(_) => return Err(anyhow!("`format` must be \"png\" or \"jpeg\"")),
    };
    let quality = match args.get("quality") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => {
            let q = n
                .as_u64()
                .filter(|q| (1..=100).contains(q))
                .ok_or_else(|| anyhow!("`quality` must be an integer from 1 to 100"))?;
            if format != ImageFormat::Jpeg {
                return Err(anyhow!("`quality` only applies to `format: \"jpeg\"`"));
            }
            Some(q as u8)
        }
        Some(_) => return Err(anyhow!("`quality` must be an integer from 1 to 100")),
    };
    let max_width = match args.get("max_width") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => Some(
            n.as_u64()
                .filter(|w| *w >= 64)
                .ok_or_else(|| anyhow!("`max_width` must be an integer of at least 64"))?
                as u32,
        ),
        Some(_) => return Err(anyhow!("`max_width` must be an integer of at least 64")),
    };
    let save_to = match string_arg(args, "save_to")? {
        None => None,
        Some(p) => {
            let path = std::path::PathBuf::from(&p);
            if !path.is_absolute() {
                return Err(anyhow!("`save_to` must be an absolute path, got `{p}`"));
            }
            match path.parent() {
                Some(dir) if dir.is_dir() => {}
                _ => return Err(anyhow!("`save_to` parent directory does not exist: `{p}`")),
            }
            Some(path)
        }
    };
    if args.get("selector").is_some_and(Value::is_string)
        && args.get("ref").is_some_and(Value::is_string)
    {
        return Err(anyhow!("`selector` and `ref` are mutually exclusive"));
    }
    Ok((
        ScreenshotOptions {
            full_page,
            clip: None,
            format,
            quality,
            max_width,
        },
        save_to,
    ))
}

fn make_take_screenshot() -> RegisteredTool {
    RegisteredTool {
        name: "browser_take_screenshot".into(),
        description: "Capture a screenshot of the page, or of one element via `selector` or \
                      `ref`. Screenshots are expensive in context: prefer browser_snapshot or \
                      browser_get_page_text for reading, and when you do need pixels use \
                      `format: \"jpeg\"` with `max_width` (e.g. 1024), or `save_to` to write the \
                      file to disk and keep it out of the conversation. Default output is an \
                      unscaled PNG image."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "full_page": { "type": "boolean", "default": false },
                "selector": { "type": "string", "description": "CSS selector to clip to; mutually exclusive with `ref`." },
                "ref": { "type": "string", "description": "Element ref from browser_snapshot/browser_find to clip to; mutually exclusive with `selector`." },
                "format": { "type": "string", "enum": ["png", "jpeg"], "description": "Default png." },
                "quality": { "type": "integer", "minimum": 1, "maximum": 100, "description": "JPEG quality (default 80). jpeg only." },
                "max_width": { "type": "integer", "minimum": 64, "description": "Downscale so the image is at most this many pixels wide. Chromium only; ignored on Firefox." },
                "save_to": { "type": "string", "description": "Absolute file path. When set, the image is written there (0600) and only the path and dimensions are returned." }
            })),
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let (mut opts, save_to) = screenshot_opts(&args)?;
                let selector = args.get("selector").and_then(|v| v.as_str());
                let r = args.get("ref").and_then(|v| v.as_str());
                if r.is_some() {
                    state.ensure_native_ready("browser_take_screenshot").await?;
                }
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                // A selector or ref clips the capture to that element's box.
                opts.clip = match (selector, r) {
                    (Some(sel), _) => {
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
                    (None, Some(r)) => {
                        let entry = resolve_ref(&state, &backend, &target_id, r).await?;
                        Some(
                            backend
                                .node_clip_rect(&target_id, entry.backend_node_id, MCP_OP_TIMEOUT)
                                .await
                                .map_err(|e| stale_on_node_gone(e, r, &target_id))?,
                        )
                    }
                    (None, None) => None,
                };
                let b64 = backend.screenshot(&target_id, &opts).await?;
                match save_to {
                    None => Ok(image_content(b64, opts.format.mime())),
                    Some(path) => {
                        use base64::Engine as _;
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(b64.as_bytes())
                            .map_err(|e| anyhow!("decoding screenshot data: {e}"))?;
                        crate::cli::output::write_private_file(&path, &bytes)?;
                        let dims = crate::cli::output::image_dimensions(&bytes)
                            .map(|(w, h)| format!("{w}x{h}, "))
                            .unwrap_or_default();
                        Ok(text_content(format!(
                            "Saved screenshot to {} ({dims}{}, {} KiB)",
                            path.display(),
                            opts.format.mime(),
                            bytes.len().div_ceil(1024)
                        )))
                    }
                }
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
            "Perform an HTTP request from the page context. Preserves cookies and remains subject to browser CORS/CSP rules. Prefer `browser_curl` for large responses or direct file downloads."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "url": { "type": "string" },
                "method": { "type": "string" },
                "headers": { "type": "object" },
                "body": { "type": "string" },
                "timeout_ms": {
                    "type": "number",
                    "description": "Per-call timeout in milliseconds for the in-page fetch. Default 60s."
                },
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
                    obj.remove("timeout_ms");
                }
                let timeout = timeout_ms_arg(&args, "timeout_ms", MCP_FETCH_TIMEOUT)?;
                if let Some(obj) = for_js.as_object_mut() {
                    obj.insert(
                        "timeoutMs".to_string(),
                        json!(script_fetch_timeout_ms(timeout)),
                    );
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
                let value = backend.evaluate(&target_id, &expr, true, timeout).await?;
                let raw = value.as_str().unwrap_or("").to_string();
                let mut parsed: Value = serde_json::from_str(&raw)
                    .map_err(|e| anyhow!("invalid fetch response JSON: {e}"))?;
                if parsed.get("ok").and_then(Value::as_bool) == Some(false) {
                    let mut msg = parsed
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("fetch failed")
                        .to_string();
                    if let Some(name) = parsed.get("errorName").and_then(Value::as_str) {
                        if !name.is_empty() {
                            msg.push_str(&format!(" ({name})"));
                        }
                    }
                    return Err(anyhow!(msg));
                }
                if let Some(obj) = parsed.as_object_mut() {
                    obj.remove("ok");
                }
                let pretty = serde_json::to_string_pretty(&parsed)?;
                Ok(text_content(pretty))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// browser_curl
// ---------------------------------------------------------------------------

fn make_curl() -> RegisteredTool {
    RegisteredTool {
        name: "browser_curl".into(),
        description: format!(
            "Run the real curl out of page context with cookies and User-Agent copied from the active browser, plus Origin and Referer derived from the selected source tab. Arguments use ordinary curl syntax and are forwarded unchanged. Omit `-o` to return up to {} MiB through MCP; use `-o <path>`/`--output <path>` for unrestricted streaming downloads. Unlike browser_fetch, curl is not subject to browser CORS/CSP and does not reproduce the browser TLS fingerprint.",
            crate::cli::curl::MCP_RESPONSE_LIMIT / (1024 * 1024)
        ),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Exact curl arguments, including options and URL(s), e.g. [\"-L\", \"--fail-with-body\", \"-o\", \"/tmp/file.zip\", \"https://example.com/file.zip\"]."
                }
            })),
            "required": ["args"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let curl_args = args
                    .get("args")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("missing or invalid 'args': expected an array of strings"))?
                    .iter()
                    .map(|arg| {
                        arg.as_str()
                            .map(String::from)
                            .ok_or_else(|| anyhow!("every curl argument must be a string"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                if curl_args.is_empty() {
                    return Err(anyhow!(
                        "'args' must contain curl options and at least one URL"
                    ));
                }

                // Cookies are browser-wide. Explicit tab/target routing
                // selects the document used for navigator.userAgent, Origin,
                // and Referer. Otherwise prefer the MCP active tab, falling
                // back to any live tab inside `prepare`.
                let (tab, target) = extract_tab_target(&args);
                let has_route = tab.is_some() || target.is_some();
                let (backend, target_id) = if has_route {
                    let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                    (backend, Some(target_id))
                } else {
                    let backend = state.ensure_backend().await?;
                    let target_id = state.active_target_id.lock().await.clone();
                    (backend, target_id)
                };
                let prepared = crate::cli::curl::prepare(&backend, target_id.as_deref()).await?;
                let output = crate::cli::curl::execute_mcp(&prepared, &curl_args).await?;
                Ok(crate::cli::curl::mcp_result(output))
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
                      (`[{target_id, url, title, active, foreground}]`). Titles are empty on \
                      Firefox; `foreground` reports foreground emulation."
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
    let foreground = state.capture.foreground_tabs();
    let arr: Vec<Value> = targets
        .into_iter()
        .map(|t| {
            json!({
                "target_id": t.id,
                "url": t.url,
                "title": t.title,
                "active": active.as_deref() == Some(t.id.as_str()),
                "foreground": foreground.contains(&t.id),
            })
        })
        .collect();
    Ok(Value::Array(arr))
}

fn make_tab_new() -> RegisteredTool {
    RegisteredTool {
        name: "browser_tab_new".into(),
        description: "Create a new tab and make it the active tab. Defaults to about:blank. \
                      Pass `name` to create or select a durable named tab addressable as \
                      `<browser>/<name>`."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Optional named-tab id (a-z, 0-9, '-', '_')." },
                "url": { "type": "string", "description": "Optional URL; defaults to about:blank." },
                "foreground": { "type": "boolean", "description": "Emulate a focused, visible foreground for this tab (see browser_tab_foreground). Chromium only." }
            },
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let foreground = match args.get("foreground") {
                    None | Some(Value::Null) => None,
                    Some(Value::Bool(b)) => Some(*b),
                    Some(_) => return Err(anyhow!("`foreground` must be a boolean")),
                };
                if foreground.is_some() {
                    state
                        .ensure_cdp_engine("browser_tab_new", FOREGROUND_HINT)
                        .await?;
                }
                if let Some(name) = args.get("name").and_then(|v| v.as_str()) {
                    let url = args.get("url").and_then(|v| v.as_str());
                    let mut opened = open_or_create_named_tab(&state, name, url).await?;
                    if let Some(fg) = foreground {
                        let backend = state.ensure_backend().await?;
                        let tid = opened["target_id"].as_str().unwrap_or_default().to_string();
                        state.capture.set_foreground(&backend, &tid, fg).await?;
                        opened["foreground"] = json!(fg);
                    }
                    return Ok(text_content(serde_json::to_string_pretty(&opened)?));
                }
                let url = args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("about:blank")
                    .to_string();
                let backend = state.ensure_backend().await?;
                let tid = backend.create_tab(&url).await?;
                *state.active_target_id.lock().await = Some(tid.clone());
                state.capture.touch_with(&backend, &tid, foreground);
                if let Some(fg) = foreground {
                    state.capture.set_foreground(&backend, &tid, fg).await?;
                }
                Ok(text_content(serde_json::to_string_pretty(&json!({
                    "target_id": tid,
                    "url": url,
                    "active": true,
                    "foreground": state.capture.foreground_tabs().contains(&tid),
                }))?))
            })
        }),
    }
}

async fn open_or_create_named_tab(
    state: &ServerState,
    name: &str,
    url: Option<&str>,
) -> Result<Value> {
    crate::cli::env_resolver::validate_tab_name(name)?;
    let want_url = url.unwrap_or("about:blank").to_string();
    let backend = state.ensure_backend().await?;
    let browser_name = state.registered_browser_name().await?;

    let existing = {
        let bn = browser_name.clone();
        let n = name.to_string();
        crate::mcp::server::sync_registry_op(move |reg| reg.tab_get(&bn, &n)).await?
    };
    if let Some(row) = existing {
        let live = backend.live_target_ids().await?;
        if live.contains(&row.target_id) {
            if url.is_some() && row.last_url != want_url {
                backend.navigate(&row.target_id, &want_url).await?;
                let bn = browser_name.clone();
                let n = name.to_string();
                let u = want_url.clone();
                crate::mcp::server::sync_registry_op(move |reg| reg.tab_set_url(&bn, &n, &u))
                    .await?;
            } else {
                let bn = browser_name.clone();
                let n = name.to_string();
                crate::mcp::server::sync_registry_op(move |reg| reg.tab_touch(&bn, &n)).await?;
            }
            *state.active_target_id.lock().await = Some(row.target_id.clone());
            state.capture.touch(&backend, &row.target_id);
            return Ok(json!({
                "name": name,
                "target_id": row.target_id,
                "url": if url.is_some() { want_url } else { row.last_url },
                "active": true,
                "created": false,
            }));
        }

        let _ = backend.close_tab(&row.target_id).await;
        state.capture.forget(&backend, &row.target_id);
        let bn = browser_name.clone();
        let n = name.to_string();
        crate::mcp::server::sync_registry_op(move |reg| reg.tab_delete(&bn, &n)).await?;
    }

    let victim = {
        let bn = browser_name.clone();
        crate::mcp::server::sync_registry_op(
            move |reg| -> Result<Option<crate::registry::TabRow>> {
                if reg.tabs_count_daemon_created(&bn)? >= crate::session::tabs::HARD_CAP {
                    reg.tabs_lru_daemon_created(&bn)
                } else {
                    Ok(None)
                }
            },
        )
        .await?
    };
    if let Some(victim) = victim {
        let _ = backend.close_tab(&victim.target_id).await;
        state.capture.forget(&backend, &victim.target_id);
        let bn = victim.browser_name;
        let n = victim.name;
        crate::mcp::server::sync_registry_op(move |reg| reg.tab_delete(&bn, &n)).await?;
    }

    let target_id = backend.create_tab(&want_url).await?;
    let bn = browser_name;
    let n = name.to_string();
    let tid = target_id.clone();
    let u = want_url.clone();
    crate::mcp::server::sync_registry_op(move |reg| reg.tab_upsert(&bn, &n, &tid, &u, true))
        .await?;
    *state.active_target_id.lock().await = Some(target_id.clone());
    state.capture.touch(&backend, &target_id);
    Ok(json!({
        "name": name,
        "target_id": target_id,
        "url": want_url,
        "active": true,
        "created": true,
    }))
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
                "target_id": { "type": "string" },
                "foreground": { "type": "boolean", "description": "Emulate a focused, visible foreground for this tab (see browser_tab_foreground). Chromium only." }
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
                let foreground = match args.get("foreground") {
                    None | Some(Value::Null) => None,
                    Some(Value::Bool(b)) => Some(*b),
                    Some(_) => return Err(anyhow!("`foreground` must be a boolean")),
                };
                if foreground.is_some() {
                    state
                        .ensure_cdp_engine("browser_tab_select", FOREGROUND_HINT)
                        .await?;
                }
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
                state.capture.touch_with(&backend, &tid, foreground);
                if let Some(fg) = foreground {
                    state.capture.set_foreground(&backend, &tid, fg).await?;
                }
                Ok(text_content(serde_json::to_string_pretty(&json!({
                    "target_id": tid,
                    "active": true,
                    "foreground": state.capture.foreground_tabs().contains(&tid),
                }))?))
            })
        }),
    }
}

const FOREGROUND_HINT: &str = "foreground emulation uses CDP Emulation.setFocusEmulationEnabled; Firefox has no WebDriver BiDi equivalent, so switch to a Chromium browser via browser_select";

fn make_tab_foreground() -> RegisteredTool {
    RegisteredTool {
        name: "browser_tab_foreground".into(),
        description: "Make a tab behave as if it were the focused, visible foreground tab on an \
                      unlocked display, even while the browser window is minimized or the \
                      machine's display is locked: `document.visibilityState` reports \
                      `visible`, `document.hasFocus()` is true, `requestAnimationFrame` and \
                      timers run at full rate, and screenshots show live content. Use it for \
                      games, canvas apps, and anything that pauses in the background. Stays on \
                      until disabled, the tab closes, or the MCP server exits; `browser-control \
                      set foreground always` turns it on for every tab. Chromium only."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "enabled": { "type": "boolean", "description": "Default true; false turns emulation off again." }
            })),
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let enabled = bool_arg(&args, "enabled", true)?;
                state
                    .ensure_cdp_engine("browser_tab_foreground", FOREGROUND_HINT)
                    .await?;
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                state
                    .capture
                    .set_foreground(&backend, &target_id, enabled)
                    .await?;
                Ok(text_content(if enabled {
                    format!(
                        "foreground emulation on for tab {target_id}: the page reports visible and focused, and requestAnimationFrame/timers run at full rate while the window is minimized or the display is locked. Stays on until disabled, the tab closes, or the MCP server exits."
                    )
                } else {
                    format!("foreground emulation off for tab {target_id}")
                }))
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
                let closed = backend.close_tab(&tid).await;
                // Capture state and element refs die with the tab, whether
                // or not the close RPC succeeded (the target is gone either
                // way).
                state.capture.forget(&backend, &tid);
                state.refs.lock().await.remove(&tid);
                closed?;
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

fn make_browser_start() -> RegisteredTool {
    RegisteredTool {
        name: "browser_start".into(),
        description: "Start or reuse a browser, then make it the active MCP browser. \
                      Use this to recover after the active browser exits."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "browser": { "type": "string", "description": "Optional browser kind (chrome, edge, chromium, brave, firefox). Defaults to an already-running installed browser if any, otherwise the first installed Chromium-family browser." },
                "headless": { "type": "boolean", "default": false },
                "wait_timeout_seconds": { "type": "integer", "default": 30 }
            },
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let browser = args
                    .get("browser")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let headless = args
                    .get("headless")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let wait_timeout = args
                    .get("wait_timeout_seconds")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30);
                let started =
                    crate::cli::start::ensure_started(browser, headless, false, wait_timeout)
                        .await?;
                let resolved = crate::cli::env_resolver::ResolvedBrowser {
                    endpoint: started.endpoint.clone(),
                    engine: started.engine,
                    source: crate::cli::env_resolver::Source::Registered {
                        name: started.name.clone(),
                    },
                };
                state.switch_browser(resolved).await?;
                let tabs = tab_list_value(&state).await?;
                Ok(text_content(serde_json::to_string_pretty(&json!({
                    "name": started.name,
                    "kind": started.kind.as_str(),
                    "engine": match started.engine {
                        crate::detect::Engine::Cdp => "cdp",
                        crate::detect::Engine::Bidi => "bidi",
                    },
                    "endpoint": started.endpoint,
                    "reused": started.reused,
                    "selected": true,
                    "tabs": tabs,
                }))?))
            })
        }),
    }
}

fn make_browser_select() -> RegisteredTool {
    RegisteredTool {
        name: "browser_select".into(),
        description: "Switch the active browser by registered name, kind, URL, or CLI target \
                      syntax such as `chrome` or `brave/cart`. A kind selector starts or reuses \
                      that browser when none is live. The switch is committed before \
                      Firefox BiDi lock preparation; if preparation fails, the new browser remains \
                      active and the caller decides whether to retry, switch elsewhere, or switch back."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Browser selector, optionally `<browser>/<tab>`." }
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
                let target = crate::cli::env_resolver::parse_target(&name)?;
                let resolved =
                    crate::mcp::server::resolve_browser_send(target.browser.clone()).await?;
                let resolved_clone = resolved.clone();
                state.switch_browser(resolved).await?;
                let selected_tab = if let Some(tab) = target.tab.as_deref() {
                    Some(open_or_create_named_tab(&state, tab, None).await?)
                } else {
                    None
                };
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
                    "selected_tab": selected_tab,
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
                // External endpoints have no registered executable to
                // activate. Avoid opening the global registry in that case;
                // besides being unnecessary I/O, it could race a concurrent
                // browser switch or test-time data-directory override.
                let os_activated = match source {
                    crate::cli::env_resolver::Source::External => false,
                    source @ crate::cli::env_resolver::Source::Registered { .. } => {
                        tokio::task::spawn_blocking(move || -> Result<bool> {
                            let registry = crate::registry::Registry::open()?;
                            crate::cli::show::activate_resolved_app(&registry, &source)
                        })
                        .await??
                    }
                };
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

/// Forward a sidecar call. Resolves the target natively first, ensures the
/// sidecar is up, then sends the RPC with `target_id` merged into the params.
/// If Playwright fails at the CDP attachment/connection layer, wake and probe
/// the tab through browser-control's native backend before returning a typed
/// sidecar-specific error. This prevents agents from misreading a sidecar CDP
/// timeout as evidence that the page itself is hung.
async fn forward_to_sidecar(
    state: &ServerState,
    tool_name: &str,
    args: &Value,
    sidecar_method: &str,
    mut params: serde_json::Map<String, Value>,
) -> Result<Value> {
    // Preflight: check engine support before resolving the target, but do not
    // spawn the sidecar yet. If Playwright attach fails, we still need a native
    // backend + target id for the wake/probe diagnostic.
    state.ensure_sidecar_supported(tool_name).await?;
    let (backend, target_id) = state.resolve_target_for_args(args).await?;
    params.insert("target_id".into(), Value::String(target_id));
    let sc = match state.ensure_sidecar(tool_name).await {
        Ok(sc) => sc,
        Err(e) if looks_like_sidecar_cdp_attach_failure(&e) => {
            return sidecar_cdp_failure_after_probe(
                state,
                &backend,
                tool_name,
                sidecar_method,
                params
                    .get("target_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
                e,
            )
            .await;
        }
        Err(e) => return Err(e),
    };
    match sc.call(sidecar_method, Value::Object(params.clone())).await {
        Ok(v) => Ok(v),
        Err(e) if looks_like_sidecar_cdp_attach_failure(&e) => {
            sidecar_cdp_failure_after_probe(
                state,
                &backend,
                tool_name,
                sidecar_method,
                params
                    .get("target_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
                e,
            )
            .await
        }
        Err(e) => Err(e),
    }
}

async fn sidecar_cdp_failure_after_probe(
    state: &ServerState,
    backend: &TabBackend,
    tool_name: &str,
    sidecar_method: &str,
    target_id: &str,
    err: anyhow::Error,
) -> Result<Value> {
    state.reset_sidecar().await;
    let url = wake_and_probe_target(backend, target_id).await?;
    Err(SessionError::SidecarConnectionFailed {
        tool: tool_name.to_string(),
        method: sidecar_method.to_string(),
        target_id: target_id.to_string(),
        url,
        details: format!("{err:#}"),
        hint: "retry the Playwright-sidecar tool or inspect with browser_get_html / browser_take_screenshot",
    }
    .into())
}

async fn wake_and_probe_target(backend: &TabBackend, target_id: &str) -> Result<Option<String>> {
    match tokio::time::timeout(SIDECAR_WAKE_PROBE_TIMEOUT, backend.show_tab(target_id)).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(SessionError::TabHung {
                target_id: Some(target_id.to_string()),
                url: None,
                timeout_ms: SIDECAR_WAKE_PROBE_TIMEOUT.as_millis() as u64,
                hint: "sidecar-wake-timeout",
            }
            .into());
        }
    }

    match tokio::time::timeout(
        SIDECAR_WAKE_PROBE_TIMEOUT,
        backend.evaluate(target_id, "1", false, SIDECAR_WAKE_PROBE_TIMEOUT),
    )
    .await
    {
        Ok(r) => {
            let _ = r?;
        }
        Err(_) => {
            return Err(SessionError::TabHung {
                target_id: Some(target_id.to_string()),
                url: None,
                timeout_ms: SIDECAR_WAKE_PROBE_TIMEOUT.as_millis() as u64,
                hint: "sidecar-probe-timeout",
            }
            .into());
        }
    }

    match tokio::time::timeout(SIDECAR_WAKE_PROBE_TIMEOUT, backend.live_targets()).await {
        Ok(Ok(targets)) => Ok(targets
            .into_iter()
            .find(|t| t.id == target_id)
            .map(|t| t.url)),
        _ => Ok(None),
    }
}

fn looks_like_sidecar_cdp_attach_failure(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("<ws connecting>")
        || msg.contains("connectovercdp")
        || msg.contains("websocket")
        || msg.contains("browser has been closed")
        || msg.contains("browser closed")
        || msg.contains("browser disconnected")
        || msg.contains("target closed")
        || msg.contains("cdp session closed")
        || msg.contains("econnrefused")
        || msg.contains("econnreset")
        || msg.contains("socket hang up")
        || msg.contains("sidecar stdout closed")
        || msg.contains("sidecar writer closed")
        || msg.contains("sidecar response channel dropped")
}

// ---------------------------------------------------------------------------
// Console / network capture tools.
// ---------------------------------------------------------------------------

fn regex_arg(args: &Value, key: &str) -> Result<Option<Regex>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) => Regex::new(s)
            .map(Some)
            .map_err(|e| anyhow!("invalid `{key}` regex: {e}")),
        Some(_) => Err(anyhow!("`{key}` must be a string")),
    }
}

fn bool_arg(args: &Value, key: &str, default: bool) -> Result<bool> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(anyhow!("`{key}` must be a boolean")),
    }
}

fn count_arg(args: &Value, key: &str, default: usize, min: usize, max: usize) -> Result<usize> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => {
            let n = n
                .as_u64()
                .ok_or_else(|| anyhow!("`{key}` must be a non-negative integer"))?
                as usize;
            if n < min {
                return Err(anyhow!("`{key}` must be at least {min}"));
            }
            Ok(n.min(max))
        }
        Some(_) => Err(anyhow!("`{key}` must be a non-negative integer")),
    }
}

fn string_arg(args: &Value, key: &str) -> Result<Option<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(anyhow!("`{key}` must be a string")),
    }
}

/// `format: "text" | "json"` (default text).
fn wants_json(args: &Value) -> Result<bool> {
    match args.get("format") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::String(s)) if s == "text" => Ok(false),
        Some(Value::String(s)) if s == "json" => Ok(true),
        Some(_) => Err(anyhow!("`format` must be \"text\" or \"json\"")),
    }
}

fn capture_common_schema() -> Value {
    json!({
        "limit": {
            "type": "integer",
            "minimum": 0,
            "description": "Return the most recent N matching entries. Default 100. `limit: 0` with `clear: true` just clears."
        },
        "clear": {
            "type": "boolean",
            "description": "Clear the tab's buffer after reading. Use before an action to isolate its effects."
        },
        "format": {
            "type": "string",
            "enum": ["text", "json"],
            "description": "Output format. Default text (one line per entry)."
        }
    })
}

fn make_console_messages() -> RegisteredTool {
    use crate::session::capture::{format_console_text, ConsoleQuery, CONSOLE_CAP};
    let mut props = capture_common_schema();
    props["pattern"] = json!({
        "type": "string",
        "description": "Unanchored regex applied to the rendered line (level, source URL, message, page URL). Always pass one on busy pages."
    });
    props["only_errors"] = json!({
        "type": "boolean",
        "description": "Only error-level entries (console.error, uncaught exceptions, failed resources). Default false."
    });
    RegisteredTool {
        name: "browser_console_messages".into(),
        description: format!(
            "Read console messages (console.*, uncaught exceptions, browser log entries such as \
             failed resource loads and CSP violations) captured for a tab. Capture starts when \
             the MCP server first touches a tab (browser_navigate, browser_tab_select, …) and \
             keeps the last {CONSOLE_CAP} entries across navigations until `clear`. Pass \
             `pattern` or `only_errors` to keep output small. Native protocol events on \
             Chromium (CDP) and Firefox (BiDi); no Node."
        ),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(props),
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let q = ConsoleQuery {
                    pattern: regex_arg(&args, "pattern")?,
                    only_errors: bool_arg(&args, "only_errors", false)?,
                    limit: count_arg(&args, "limit", 100, 0, CONSOLE_CAP)?,
                    clear: bool_arg(&args, "clear", false)?,
                };
                let json_out = wants_json(&args)?;
                let (_backend, target_id) = state.resolve_target_for_args(&args).await?;
                let report = state.capture.read_console(&target_id, &q).await?;
                if json_out {
                    Ok(text_content(serde_json::to_string_pretty(&report)?))
                } else {
                    Ok(text_content(format_console_text(&report)))
                }
            })
        }),
    }
}

fn make_network_requests() -> RegisteredTool {
    use crate::session::capture::{format_network_text, NetworkQuery, StatusFilter, NETWORK_CAP};
    let mut props = capture_common_schema();
    props["url_pattern"] = json!({
        "type": "string",
        "description": "Unanchored regex applied to the request URL."
    });
    props["method"] = json!({
        "type": "string",
        "description": "Exact HTTP method (case-insensitive)."
    });
    props["status"] = json!({
        "type": "string",
        "description": "Exact code (\"404\"), class (\"2xx\"…\"5xx\"), \"failed\", or \"pending\"."
    });
    props["resource_type"] = json!({
        "type": "string",
        "description": "Resource type: Document, XHR, Fetch, Script, Stylesheet, Image, Font, WebSocket, … On Firefox the type is derived from the request destination or MIME type and may be absent."
    });
    RegisteredTool {
        name: "browser_network_requests".into(),
        description: format!(
            "List network requests captured for a tab: method, URL, status, MIME type, size, \
             duration, failure reason, and the request id to pass to browser_network_body. \
             Capture starts when the MCP server first touches a tab and keeps the last \
             {NETWORK_CAP} requests across navigations until `clear`. Native protocol events \
             on Chromium (CDP) and Firefox (BiDi); no Node."
        ),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(props),
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let q = NetworkQuery {
                    url_pattern: regex_arg(&args, "url_pattern")?,
                    method: string_arg(&args, "method")?,
                    status: string_arg(&args, "status")?
                        .map(|s| StatusFilter::parse(&s))
                        .transpose()?,
                    resource_type: string_arg(&args, "resource_type")?,
                    limit: count_arg(&args, "limit", 100, 0, NETWORK_CAP)?,
                    clear: bool_arg(&args, "clear", false)?,
                };
                let json_out = wants_json(&args)?;
                let (_backend, target_id) = state.resolve_target_for_args(&args).await?;
                let report = state.capture.read_network(&target_id, &q).await?;
                if json_out {
                    Ok(text_content(serde_json::to_string_pretty(&report)?))
                } else {
                    Ok(text_content(format_network_text(&report)))
                }
            })
        }),
    }
}

fn make_network_body() -> RegisteredTool {
    use crate::session::capture::{BODY_DEFAULT_MAX, BODY_HARD_MAX};
    RegisteredTool {
        name: "browser_network_body".into(),
        description: "Fetch the response body of a captured request by the request id printed by \
                      browser_network_requests. Text bodies come back as text, binary as an \
                      embedded base64 resource, followed by a JSON metadata block. Default cap \
                      256 KiB, hard max 8 MiB. Bodies are evicted by the browser after \
                      navigation, so fetch promptly. Chromium-only: Firefox does not expose \
                      captured bodies; use browser_fetch there."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "request_id": {
                    "type": "string",
                    "description": "Request id from browser_network_requests (e.g. \"1234.56\")."
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": BODY_HARD_MAX,
                    "description": "Truncate the body after this many bytes. Default 262144."
                }
            })),
            "required": ["request_id"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let request_id = string_arg(&args, "request_id")?
                    .ok_or_else(|| anyhow!("missing 'request_id'"))?;
                let max_bytes = count_arg(&args, "max_bytes", BODY_DEFAULT_MAX, 1, BODY_HARD_MAX)?;
                state
                    .ensure_body_capture_supported("browser_network_body")
                    .await?;
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                let body = state
                    .capture
                    .response_body(&backend, &target_id, &request_id, max_bytes, MCP_OP_TIMEOUT)
                    .await?;
                let mut content = Vec::new();
                match std::str::from_utf8(&body.bytes) {
                    Ok(text) => content.push(json!({ "type": "text", "text": text })),
                    Err(_) => {
                        use base64::Engine as _;
                        content.push(json!({
                            "type": "resource",
                            "resource": {
                                "uri": format!("browser-control://network/{}", body.request_id),
                                "mimeType": body.mime_type.clone().unwrap_or_else(|| "application/octet-stream".into()),
                                "blob": base64::engine::general_purpose::STANDARD.encode(&body.bytes),
                            }
                        }))
                    }
                }
                content.push(json!({
                    "type": "text",
                    "text": serde_json::to_string_pretty(&json!({
                        "request_id": body.request_id,
                        "url": body.url,
                        "status": body.status,
                        "mime_type": body.mime_type,
                        "bytes": body.bytes.len(),
                        "total_bytes": body.total_bytes,
                        "truncated": body.truncated,
                    }))?
                }));
                Ok(json!({ "content": content }))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Native accessibility snapshot, find, and ref resolution.
// ---------------------------------------------------------------------------

/// Parse the rendering options shared by `browser_snapshot`. Pure
/// validation so bad args fail before any backend I/O.
fn snapshot_opts(args: &Value) -> Result<SnapshotOptions> {
    let interactive_only = match args.get("interactive_only") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(anyhow!("`interactive_only` must be a boolean")),
    };
    let max_chars = match args.get("max_chars") {
        None | Some(Value::Null) => a11y::DEFAULT_MAX_CHARS,
        Some(Value::Number(n)) => {
            let n = n
                .as_u64()
                .ok_or_else(|| anyhow!("`max_chars` must be a positive integer"))?;
            if n < 1000 {
                return Err(anyhow!("`max_chars` must be at least 1000"));
            }
            n as usize
        }
        Some(_) => return Err(anyhow!("`max_chars` must be a positive integer")),
    };
    let depth = match args.get("depth") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => Some(
            n.as_u64()
                .ok_or_else(|| anyhow!("`depth` must be a non-negative integer"))?
                as usize,
        ),
        Some(_) => return Err(anyhow!("`depth` must be a non-negative integer")),
    };
    if let Some(v) = args.get("ref") {
        if !v.is_null() && !v.is_string() {
            return Err(anyhow!("`ref` must be a string such as \"e12\""));
        }
    }
    Ok(SnapshotOptions {
        interactive_only,
        max_chars,
        root_backend_id: None,
        depth,
    })
}

/// Fetch and parse the accessibility tree for `target_id`.
async fn fetch_ax_tree(backend: &TabBackend, target_id: &str) -> Result<a11y::AxTree> {
    let raw = backend
        .accessibility_tree(target_id, None, MCP_SNAPSHOT_TIMEOUT)
        .await?;
    a11y::parse_full_ax_tree(&raw)
}

/// Run `f` against the ref table for `target_id`, replacing the table
/// when the tree belongs to a different document than the stored refs.
async fn with_ref_table<T>(
    state: &ServerState,
    target_id: &str,
    tree: &a11y::AxTree,
    f: impl FnOnce(&mut a11y::RefTable) -> Result<T>,
) -> Result<T> {
    let token = a11y::document_token(tree).unwrap_or(0);
    let mut refs = state.refs.lock().await;
    let table = refs
        .entry(target_id.to_string())
        .or_insert_with(|| a11y::RefTable::new(token));
    if table.doc_token != token {
        *table = a11y::RefTable::new(token);
    }
    f(table)
}

/// Resolve an agent-facing ref to its element, verifying the tab is still
/// on the document the ref was taken from. A mismatch drops the table and
/// reports `StaleRef` so the agent re-snapshots instead of hitting a
/// recycled node id.
async fn resolve_ref(
    state: &ServerState,
    backend: &TabBackend,
    target_id: &str,
    r: &str,
) -> Result<RefEntry> {
    let unknown = || SessionError::RefUnknown {
        element: r.to_string(),
        target_id: target_id.to_string(),
    };
    let (entry, doc_token) = {
        let refs = state.refs.lock().await;
        let table = refs.get(target_id).ok_or_else(unknown)?;
        let entry = table.lookup(r).ok_or_else(unknown)?.clone();
        (entry, table.doc_token)
    };
    let current = backend.document_token(target_id, MCP_OP_TIMEOUT).await?;
    if current != doc_token {
        state.refs.lock().await.remove(target_id);
        return Err(SessionError::StaleRef {
            element: r.to_string(),
            target_id: target_id.to_string(),
            reason: "document changed",
        }
        .into());
    }
    Ok(entry)
}

/// Translate the input layer's `NodeGone` into the agent-facing
/// `StaleRef` for `r`.
fn stale_on_node_gone(err: anyhow::Error, r: &str, target_id: &str) -> anyhow::Error {
    if matches!(
        err.downcast_ref::<SessionError>(),
        Some(SessionError::NodeGone { .. })
    ) {
        return SessionError::StaleRef {
            element: r.to_string(),
            target_id: target_id.to_string(),
            reason: "node no longer exists",
        }
        .into();
    }
    err
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

fn describe_ref(entry: &RefEntry) -> String {
    if entry.name.is_empty() {
        entry.role.clone()
    } else {
        format!("{} {}", entry.role, quote(&entry.name))
    }
}

/// `# <title> (<url>)` header for snapshot output, from the live target
/// list (already fetched by tab routing, so effectively free).
async fn page_header(backend: &TabBackend, target_id: &str, title_hint: &str) -> String {
    match backend.live_targets().await {
        Ok(targets) => targets
            .iter()
            .find(|t| t.id == target_id)
            .map(|t| {
                // BiDi's `getTree` carries no titles; the walker reports
                // `document.title` on its root node instead.
                let title = if t.title.is_empty() {
                    title_hint
                } else {
                    &t.title
                };
                if title.is_empty() {
                    format!("# {}\n", t.url)
                } else {
                    format!("# {} ({})\n", title, t.url)
                }
            })
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

fn make_snapshot() -> RegisteredTool {
    RegisteredTool {
        name: "browser_snapshot".into(),
        description: "Accessibility snapshot of the page with stable element refs (`[ref=eN]`) \
                      usable by browser_click / browser_type / browser_hover / browser_drag / \
                      browser_take_screenshot. Prefer this over screenshots for reading page \
                      structure. `interactive_only` keeps only actionable elements and their \
                      ancestors (good for forms); `ref` renders one subtree; `depth` limits \
                      nesting; `max_chars` caps output (default 50000, cut at a line boundary \
                      with a note). Refs stay valid until the page navigates. Native on \
                      Chromium (accessibility tree) and Firefox (injected DOM walker: names and \
                      roles are approximate, closed shadow roots are not visible); iframe \
                      contents are not included."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "interactive_only": {
                    "type": "boolean",
                    "description": "Only interactive elements (buttons, links, inputs, …) and their ancestors. Default false."
                },
                "max_chars": {
                    "type": "integer",
                    "minimum": 1000,
                    "description": "Truncate output at a line boundary before this many characters. Default 50000."
                },
                "ref": {
                    "type": "string",
                    "description": "Render only the subtree rooted at this ref from a previous snapshot or find."
                },
                "depth": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Levels below the root to include; deeper content collapses to `… (N more)`."
                }
            })),
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let mut opts = snapshot_opts(&args)?;
                state.ensure_native_ready("browser_snapshot").await?;
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                if let Some(r) = args.get("ref").and_then(Value::as_str) {
                    let entry = resolve_ref(&state, &backend, &target_id, r).await?;
                    opts.root_backend_id = Some(entry.backend_node_id);
                }
                let tree = fetch_ax_tree(&backend, &target_id).await?;
                let root_title = tree
                    .nodes
                    .get(&tree.root)
                    .map(|n| n.name.clone())
                    .unwrap_or_default();
                let header = page_header(&backend, &target_id, &root_title).await;
                let snap = with_ref_table(&state, &target_id, &tree, |table| {
                    a11y::render_snapshot(&tree, table, &opts)
                })
                .await?;
                Ok(text_content(format!("{header}{}", snap.text)))
            })
        }),
    }
}

fn make_find() -> RegisteredTool {
    RegisteredTool {
        name: "browser_find".into(),
        description:
            "Find elements by a short description (\"search box\", \"add to cart button\", \
                      \"Sign in\") and return their refs for browser_click / browser_type / etc. \
                      Plain text matching against accessible name, value, description, and role; \
                      no model call. Cheaper than a full browser_snapshot when you know what you \
                      are looking for. Returns up to 20 matches, best first. Native on \
                      Chromium and Firefox."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": tab_args_properties(json!({
                "query": {
                    "type": "string",
                    "description": "Words describing the element: visible text, label, placeholder, or role."
                },
                "interactive_only": {
                    "type": "boolean",
                    "description": "Only interactive elements. Default true; set false to find headings, images, text regions."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "description": "Maximum matches to return. Default 20."
                }
            })),
            "required": ["query"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|q| !q.is_empty())
                    .ok_or_else(|| anyhow!("missing 'query'"))?
                    .to_string();
                let interactive_only = match args.get("interactive_only") {
                    None | Some(Value::Null) => true,
                    Some(Value::Bool(b)) => *b,
                    Some(_) => return Err(anyhow!("`interactive_only` must be a boolean")),
                };
                let limit = match args.get("limit") {
                    None | Some(Value::Null) => a11y::DEFAULT_FIND_LIMIT,
                    Some(Value::Number(n)) => {
                        n.as_u64()
                            .filter(|n| *n >= 1)
                            .ok_or_else(|| anyhow!("`limit` must be a positive integer"))?
                            .min(a11y::DEFAULT_FIND_LIMIT as u64) as usize
                    }
                    Some(_) => return Err(anyhow!("`limit` must be a positive integer")),
                };
                state.ensure_native_ready("browser_find").await?;
                let (backend, target_id) = state.resolve_target_for_args(&args).await?;
                let tree = fetch_ax_tree(&backend, &target_id).await?;
                let opts = FindOptions {
                    interactive_only,
                    limit,
                };
                let hits = with_ref_table(&state, &target_id, &tree, |table| {
                    Ok(a11y::find(&tree, table, &query, &opts))
                })
                .await?;
                if hits.is_empty() {
                    return Ok(text_content(format!(
                        "no matches for {}; try fewer words, interactive_only: false, or browser_snapshot",
                        quote(&query)
                    )));
                }
                let mut out = format!(
                    "{} match{} for {}:\n",
                    hits.len(),
                    if hits.len() == 1 { "" } else { "es" },
                    quote(&query)
                );
                for m in &hits {
                    out.push_str(&m.r#ref);
                    out.push(' ');
                    out.push_str(&m.role);
                    if !m.name.is_empty() {
                        out.push(' ');
                        out.push_str(&quote(&m.name));
                    }
                    if let Some(v) = &m.value {
                        out.push_str(&format!(" [value={}]", quote(v)));
                    }
                    if let Some(ctx) = &m.context {
                        out.push_str(&format!(" — in {ctx}"));
                    }
                    out.push('\n');
                }
                Ok(text_content(out))
            })
        }),
    }
}

/// Which native CDP action a `ref` routes to.
#[derive(Clone, Copy, Debug)]
enum NativeAction {
    Click,
    Type,
    Hover,
    Drag,
}

/// How an interaction tool call is routed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Native,
    Sidecar,
}

/// Decide the route from the (selector, ref) argument pairs. Validation
/// only — fires before any backend access. Every pair must carry exactly
/// one side, and all pairs must agree.
fn route_mode(args: &Value, ref_pairs: &[(&str, &str)]) -> Result<Route> {
    if ref_pairs.is_empty() {
        return Ok(Route::Sidecar);
    }
    let mut native = 0;
    let mut sidecar = 0;
    for (sel, r) in ref_pairs {
        let has_sel = args.get(*sel).is_some_and(Value::is_string);
        let has_ref = args.get(*r).is_some_and(Value::is_string);
        match (has_sel, has_ref) {
            (true, true) => {
                return Err(anyhow!("`{sel}` and `{r}` are mutually exclusive; pass one of them"))
            }
            (false, false) => {
                return Err(anyhow!(
                    "exactly one of `{sel}` (CSS selector) or `{r}` (ref from browser_snapshot/browser_find) is required"
                ))
            }
            (true, false) => sidecar += 1,
            (false, true) => native += 1,
        }
    }
    if native > 0 && sidecar > 0 {
        return Err(anyhow!(
            "use refs for every element or selectors for every element, not a mix"
        ));
    }
    Ok(if native > 0 {
        Route::Native
    } else {
        Route::Sidecar
    })
}

/// Execute an interaction tool through native CDP input.
async fn run_native(
    state: &ServerState,
    tool_name: &str,
    action: NativeAction,
    args: &Value,
) -> Result<Value> {
    state.ensure_native_ready(tool_name).await?;
    let (backend, target_id) = state.resolve_target_for_args(args).await?;
    let timeout = timeout_ms_arg(args, "timeout_ms", MCP_OP_TIMEOUT)?;
    let ref_arg = |key: &str| -> Result<String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| anyhow!("missing '{key}'"))
    };
    match action {
        NativeAction::Click => {
            let r = ref_arg("ref")?;
            let entry = resolve_ref(state, &backend, &target_id, &r).await?;
            backend
                .click_node(&target_id, entry.backend_node_id, timeout)
                .await
                .map_err(|e| stale_on_node_gone(e, &r, &target_id))?;
            Ok(text_content(format!(
                "clicked {r} ({})",
                describe_ref(&entry)
            )))
        }
        NativeAction::Hover => {
            let r = ref_arg("ref")?;
            let entry = resolve_ref(state, &backend, &target_id, &r).await?;
            backend
                .hover_node(&target_id, entry.backend_node_id, timeout)
                .await
                .map_err(|e| stale_on_node_gone(e, &r, &target_id))?;
            Ok(text_content(format!(
                "hovered {r} ({})",
                describe_ref(&entry)
            )))
        }
        NativeAction::Type => {
            let r = ref_arg("ref")?;
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing 'text'"))?
                .to_string();
            let press_sequentially = args
                .get("press_sequentially")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
            let entry = resolve_ref(state, &backend, &target_id, &r).await?;
            backend
                .type_into_node(
                    &target_id,
                    entry.backend_node_id,
                    &text,
                    press_sequentially,
                    submit,
                    timeout,
                )
                .await
                .map_err(|e| stale_on_node_gone(e, &r, &target_id))?;
            Ok(text_content(format!(
                "typed into {r} ({}){}",
                describe_ref(&entry),
                if submit { " and pressed Enter" } else { "" }
            )))
        }
        NativeAction::Drag => {
            let a = ref_arg("source_ref")?;
            let b = ref_arg("target_ref")?;
            let from = resolve_ref(state, &backend, &target_id, &a).await?;
            let to = resolve_ref(state, &backend, &target_id, &b).await?;
            backend
                .drag_nodes(
                    &target_id,
                    from.backend_node_id,
                    to.backend_node_id,
                    timeout,
                )
                .await
                .map_err(|e| stale_on_node_gone(e, &a, &target_id))?;
            Ok(text_content(format!(
                "dragged {a} ({}) to {b} ({})",
                describe_ref(&from),
                describe_ref(&to)
            )))
        }
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

/// Declarative spec for an interaction tool. Both the input schema and
/// the param-forwarding are derived from the single `params` slice.
///
/// Tools with `ref_pairs` accept either a CSS selector (forwarded to the
/// Playwright sidecar) or an element ref (handled natively over CDP by
/// `run_native`). `route_mode` validates the pairing before any I/O.
struct SidecarTool {
    name: &'static str,
    description: &'static str,
    /// The sidecar RPC method (e.g. `"click"`).
    method: &'static str,
    params: Vec<SidecarParam>,
    /// Fixed success message returned as text content.
    success: &'static str,
    /// Native action taken when the call carries refs instead of selectors.
    native: Option<NativeAction>,
    /// `(selector_param, ref_param)` pairs; empty for sidecar-only tools.
    ref_pairs: &'static [(&'static str, &'static str)],
}

const REF_PARAM_DESC: &str = "Element ref from browser_snapshot or browser_find (e.g. \"e12\"); \
                              handled natively on Chromium and Firefox, no Node needed. Mutually exclusive with \
                              the CSS selector; exactly one is required.";

impl SidecarTool {
    fn build(self) -> RegisteredTool {
        let SidecarTool {
            name,
            description,
            method,
            params,
            success,
            native,
            ref_pairs,
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
        // to drift out of sync. Ref params never reach the sidecar.
        let param_names: Vec<&'static str> = params
            .iter()
            .map(|p| p.name)
            .filter(|n| !ref_pairs.iter().any(|(_, r)| r == n))
            .collect();
        RegisteredTool {
            name: name.into(),
            description: description.into(),
            input_schema,
            handler: handler(move |state, args| {
                let param_names = param_names.clone();
                Box::pin(async move {
                    match (route_mode(&args, ref_pairs)?, native) {
                        (Route::Native, Some(action)) => {
                            run_native(&state, name, action, &args).await
                        }
                        (Route::Native, None) => Err(anyhow!("{name} has no native path")),
                        (Route::Sidecar, _) => {
                            let mut params = serde_json::Map::new();
                            for key in &param_names {
                                copy_arg(&args, key, &mut params);
                            }
                            forward_to_sidecar(&state, name, &args, method, params).await?;
                            Ok(text_content(success))
                        }
                    }
                })
            }),
        }
    }
}

fn make_click() -> RegisteredTool {
    SidecarTool {
        name: "browser_click",
        description: "Click an element by `ref` (from browser_snapshot/browser_find; native, \
                      Chromium and Firefox) or by CSS `selector` (Playwright sidecar, Chromium).",
        method: "click",
        params: vec![
            SidecarParam {
                name: "selector",
                schema: json!({"type": "string", "description": "CSS selector; mutually exclusive with `ref`."}),
                required: false,
            },
            SidecarParam {
                name: "ref",
                schema: json!({"type": "string", "description": REF_PARAM_DESC}),
                required: false,
            },
            SidecarParam {
                name: "timeout_ms",
                schema: json!({"type": "integer"}),
                required: false,
            },
        ],
        success: "clicked",
        native: Some(NativeAction::Click),
        ref_pairs: &[("selector", "ref")],
    }
    .build()
}

fn make_type() -> RegisteredTool {
    SidecarTool {
        name: "browser_type",
        description: "Replace the content of an input with `text`, addressed by `ref` (native, \
                      Chromium and Firefox) or CSS `selector` (Playwright sidecar, Chromium). \
                      `press_sequentially=true` sends one character at a time; `submit=true` \
                      presses Enter afterwards.",
        method: "type",
        params: vec![
            SidecarParam {
                name: "selector",
                schema: json!({"type": "string", "description": "CSS selector; mutually exclusive with `ref`."}),
                required: false,
            },
            SidecarParam {
                name: "ref",
                schema: json!({"type": "string", "description": REF_PARAM_DESC}),
                required: false,
            },
            SidecarParam {
                name: "text",
                schema: json!({"type": "string"}),
                required: true,
            },
            SidecarParam {
                name: "press_sequentially",
                schema: json!({"type": "boolean", "description": "Send the text one character at a time. On Firefox this dispatches real key events; on Chromium it inserts one character per event without keydown/keyup."}),
                required: false,
            },
            SidecarParam {
                name: "submit",
                schema: json!({"type": "boolean", "description": "Press Enter after typing."}),
                required: false,
            },
            SidecarParam {
                name: "timeout_ms",
                schema: json!({"type": "integer"}),
                required: false,
            },
        ],
        success: "typed",
        native: Some(NativeAction::Type),
        ref_pairs: &[("selector", "ref")],
    }
    .build()
}

fn make_hover() -> RegisteredTool {
    SidecarTool {
        name: "browser_hover",
        description: "Hover an element by `ref` (native, Chromium and Firefox) or CSS `selector` \
                      (Playwright sidecar, Chromium).",
        method: "hover",
        params: vec![
            SidecarParam {
                name: "selector",
                schema: json!({"type": "string", "description": "CSS selector; mutually exclusive with `ref`."}),
                required: false,
            },
            SidecarParam {
                name: "ref",
                schema: json!({"type": "string", "description": REF_PARAM_DESC}),
                required: false,
            },
            SidecarParam {
                name: "timeout_ms",
                schema: json!({"type": "integer"}),
                required: false,
            },
        ],
        success: "hovered",
        native: Some(NativeAction::Hover),
        ref_pairs: &[("selector", "ref")],
    }
    .build()
}

fn make_drag() -> RegisteredTool {
    SidecarTool {
        name: "browser_drag",
        description: "Drag one element onto another, by refs (`source_ref`/`target_ref`, native \
                      pointer events on Chromium and Firefox) or CSS selectors \
                      (`source_selector`/`target_selector`, Playwright sidecar, Chromium).",
        method: "drag",
        params: vec![
            SidecarParam {
                name: "source_selector",
                schema: json!({"type": "string", "description": "CSS selector; mutually exclusive with `source_ref`."}),
                required: false,
            },
            SidecarParam {
                name: "target_selector",
                schema: json!({"type": "string", "description": "CSS selector; mutually exclusive with `target_ref`."}),
                required: false,
            },
            SidecarParam {
                name: "source_ref",
                schema: json!({"type": "string", "description": REF_PARAM_DESC}),
                required: false,
            },
            SidecarParam {
                name: "target_ref",
                schema: json!({"type": "string", "description": REF_PARAM_DESC}),
                required: false,
            },
        ],
        success: "dragged",
        native: Some(NativeAction::Drag),
        ref_pairs: &[
            ("source_selector", "source_ref"),
            ("target_selector", "target_ref"),
        ],
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
        native: None,
        ref_pairs: &[],
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
        native: None,
        ref_pairs: &[],
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
        "browser_eval",
        "browser_get_html",
        "browser_get_page_text",
        "browser_take_screenshot",
        "browser_fetch",
        "browser_curl",
        "browser_select_element",
        "browser_cookies",
        "browser_storage_get",
        "browser_storage_set",
        "browser_wait_for_cookie",
        "browser_console_messages",
        "browser_network_requests",
        "browser_network_body",
        "list_targets",
        "browser_tab_list",
        "browser_tab_new",
        "browser_tab_select",
        "browser_tab_close",
        "browser_tab_foreground",
        "browser_start",
        "browser_select",
        "browser_list",
        "browser_show",
        "browser_snapshot",
        "browser_find",
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
        spawn_screenshot_mock_with_data(selector_rect, "PNGDATA".into()).await
    }

    /// Minimal PNG header (signature + IHDR) for a 1280x720 image; enough
    /// for `image_dimensions`, not a decodable file.
    fn fake_png_1280x720() -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1280u32.to_be_bytes());
        png.extend_from_slice(&720u32.to_be_bytes());
        png
    }

    /// `selector_rect` is returned by every `Runtime.evaluate` after the
    /// first (the routing probe), so it doubles as the `devicePixelRatio`
    /// answer for `max_width` tests. `data` is the capture payload.
    async fn spawn_screenshot_mock_with_data(selector_rect: Value, data: String) -> ScreenshotMock {
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
                            json!({"data": data})
                        }
                        "Page.getLayoutMetrics" => json!({
                            "cssLayoutViewport": {"pageX": 0, "pageY": 100, "clientWidth": 1000, "clientHeight": 500},
                            "cssContentSize": {"width": 1000, "height": 3000},
                        }),
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
    fn browser_eval_requires_expression_and_supports_routing() {
        let schema = schema_for("browser_eval");
        let required = schema["required"].as_array().expect("required array");
        assert!(required.iter().any(|v| v == "expression"));
        assert_eq!(schema["properties"]["expression"]["type"], "string");
        assert_eq!(schema["properties"]["await_promise"]["type"], "boolean");
        assert_eq!(schema["properties"]["timeout_ms"]["type"], "number");
        assert_eq!(schema["properties"]["tab"]["type"], "string");
        assert_eq!(schema["properties"]["target"]["type"], "string");
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

    /// `selector` is no longer statically required on the interaction
    /// tools (a `ref` is the alternative; `route_mode` enforces exactly
    /// one at call time). `text` stays required on `browser_type`;
    /// `browser_snapshot` / `browser_pdf_save` have no required args.
    #[test]
    fn sidecar_tools_required_args() {
        let click = schema_for("browser_click");
        assert!(
            click.get("required").is_none() || click["required"].as_array().unwrap().is_empty()
        );
        assert_eq!(click["properties"]["selector"]["type"], "string");
        assert_eq!(click["properties"]["ref"]["type"], "string");

        let t = schema_for("browser_type");
        let req: Vec<&str> = t["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(req, vec!["text"]);
        assert_eq!(t["properties"]["ref"]["type"], "string");
        assert_eq!(t["properties"]["submit"]["type"], "boolean");

        let hover = schema_for("browser_hover");
        assert_eq!(hover["properties"]["ref"]["type"], "string");
        let drag = schema_for("browser_drag");
        assert_eq!(drag["properties"]["source_ref"]["type"], "string");
        assert_eq!(drag["properties"]["target_ref"]["type"], "string");
        assert!(drag.get("required").is_none());

        // No required args on these.
        let snap = schema_for("browser_snapshot");
        assert!(snap.get("required").is_none() || snap["required"].as_array().unwrap().is_empty());
        for key in ["interactive_only", "max_chars", "ref", "depth"] {
            assert!(
                snap["properties"][key].is_object(),
                "snapshot missing {key}"
            );
        }
        let pdf = schema_for("browser_pdf_save");
        assert!(pdf.get("required").is_none() || pdf["required"].as_array().unwrap().is_empty());

        let find = schema_for("browser_find");
        assert_eq!(find["required"], json!(["query"]));
        assert_eq!(find["properties"]["tab"]["type"], "string");
    }

    #[test]
    fn route_mode_validates_selector_ref_pairs() {
        let pairs = &[("selector", "ref")];
        assert_eq!(
            route_mode(&json!({"ref": "e1"}), pairs).unwrap(),
            Route::Native
        );
        assert_eq!(
            route_mode(&json!({"selector": "#x"}), pairs).unwrap(),
            Route::Sidecar
        );
        let err = route_mode(&json!({}), pairs).unwrap_err().to_string();
        assert!(err.contains("exactly one of `selector`"), "{err}");
        let err = route_mode(&json!({"selector": "#x", "ref": "e1"}), pairs)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mutually exclusive"), "{err}");

        let drag = &[
            ("source_selector", "source_ref"),
            ("target_selector", "target_ref"),
        ];
        let err = route_mode(&json!({"source_ref": "e1", "target_selector": "#y"}), drag)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a mix"), "{err}");
        assert_eq!(
            route_mode(&json!({"source_ref": "e1", "target_ref": "e2"}), drag).unwrap(),
            Route::Native
        );
        // Sidecar-only tools never route natively.
        assert_eq!(route_mode(&json!({}), &[]).unwrap(), Route::Sidecar);
    }

    #[tokio::test]
    async fn click_without_selector_or_ref_errors_before_backend() {
        let h = handler_for("browser_click");
        let err = h(unreached_state(), json!({}))
            .await
            .expect_err("must error");
        assert!(err.to_string().contains("exactly one of"), "got: {err:#}");
        let h = handler_for("browser_drag");
        let err = h(
            unreached_state(),
            json!({"source_ref": "e1", "target_selector": "#a"}),
        )
        .await
        .expect_err("must error");
        assert!(err.to_string().contains("not a mix"), "got: {err:#}");
    }

    #[tokio::test]
    async fn snapshot_rejects_bad_options_before_backend() {
        let h = handler_for("browser_snapshot");
        let err = h(unreached_state(), json!({"max_chars": 10}))
            .await
            .expect_err("must error");
        assert!(err.to_string().contains("at least 1000"), "got: {err:#}");
        let h = handler_for("browser_find");
        let err = h(unreached_state(), json!({"query": "  "}))
            .await
            .expect_err("must error");
        assert!(err.to_string().contains("missing 'query'"), "got: {err:#}");
    }

    /// BiDi-framed mock for the native tools on Firefox: session handshake,
    /// one context, marker-dispatched `script.callFunction`, a mutable
    /// document token, and recorded `input.performActions`.
    struct BidiA11yMock {
        endpoint: String,
        doc_token: Arc<std::sync::atomic::AtomicU64>,
        requests: Arc<Mutex<Vec<Value>>>,
    }

    async fn spawn_bidi_a11y_mock() -> BidiA11yMock {
        use std::sync::atomic::{AtomicU64, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let doc_token = Arc::new(AtomicU64::new(4294967296));
        let requests = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn({
            let doc_token = doc_token.clone();
            let requests = requests.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let req: Value = serde_json::from_str(&t).unwrap();
                    requests.lock().await.push(req.clone());
                    let id = req["id"].as_u64().unwrap();
                    let method = req["method"].as_str().unwrap_or("");
                    let decl = req["params"]["functionDeclaration"].as_str().unwrap_or("");
                    let expr = req["params"]["expression"].as_str().unwrap_or("");
                    let string = |s: String| json!({"type": "success", "result": {"type": "string", "value": s}, "realm": "R1"});
                    let result = match method {
                        "session.new" => json!({"sessionId": "S1", "capabilities": {}}),
                        "browsingContext.getTree" => json!({"contexts": [
                            {"context": "CTX1", "url": "https://example.com/", "children": []}
                        ]}),
                        "browsingContext.captureScreenshot" => json!({"data": "PNGDATA"}),
                        "script.callFunction" if decl.contains("bc:snapshot") => string(
                            json!({"nodes": [
                                {"nodeId": "root", "backendDOMNodeId": 4294967296u64,
                                 "role": {"value": "RootWebArea"}, "name": {"value": "Example"}, "childIds": ["n1", "n2"]},
                                {"nodeId": "n1", "parentId": "root", "backendDOMNodeId": 1,
                                 "role": {"value": "button"}, "name": {"value": "Submit"}, "childIds": [],
                                 "properties": [{"name": "focusable", "value": {"value": true}}]},
                                {"nodeId": "n2", "parentId": "root", "backendDOMNodeId": 2,
                                 "role": {"value": "link"}, "name": {"value": "Docs"}, "childIds": [],
                                 "properties": [{"name": "focusable", "value": {"value": true}}]}
                            ], "truncated": false})
                            .to_string(),
                        ),
                        "script.callFunction" if decl.contains("bc:center") => {
                            string("{\"x\":30,\"y\":20}".into())
                        }
                        "script.callFunction" if decl.contains("bc:clip") => {
                            string("{\"x\":10,\"y\":30,\"width\":100,\"height\":20}".into())
                        }
                        "script.callFunction" if decl.contains("bc:type") => {
                            string("{\"kind\":\"field\",\"method\":\"execCommand\"}".into())
                        }
                        "script.evaluate" if expr.contains("__bcDocToken") => {
                            string(doc_token.load(Ordering::SeqCst).to_string())
                        }
                        "script.evaluate" => {
                            json!({"type": "success", "result": {"type": "number", "value": 1}, "realm": "R1"})
                        }
                        _ => json!({}),
                    };
                    ws.send(Message::Text(
                        json!({"type": "success", "id": id, "result": result}).to_string(),
                    ))
                    .await
                    .unwrap();
                }
            }
        });
        BidiA11yMock {
            endpoint: format!("ws://{addr}"),
            doc_token,
            requests,
        }
    }

    fn bidi_state(endpoint: &str) -> ServerState {
        ServerState::new(ResolvedBrowser {
            engine: Engine::Bidi,
            endpoint: endpoint.to_string(),
            source: Source::External,
        })
    }

    #[tokio::test]
    async fn bidi_snapshot_then_click_by_ref_performs_actions() {
        use std::sync::atomic::Ordering;
        let mock = spawn_bidi_a11y_mock().await;
        let state = bidi_state(&mock.endpoint);
        let route = json!({"target": "example\\.com"});

        let snap = handler_for("browser_snapshot")(state.clone(), route.clone())
            .await
            .unwrap();
        assert_eq!(
            snap["content"][0]["text"],
            "# Example (https://example.com/)\n- button \"Submit\" [ref=e1]\n- link \"Docs\" [ref=e2]\n"
        );

        let mut args = route.clone();
        args["ref"] = json!("e1");
        let out = handler_for("browser_click")(state.clone(), args.clone())
            .await
            .unwrap();
        assert_eq!(out["content"][0]["text"], "clicked e1 (button \"Submit\")");
        {
            let reqs = mock.requests.lock().await;
            let perform = reqs
                .iter()
                .find(|r| r["method"] == "input.performActions")
                .expect("performActions");
            assert_eq!(perform["params"]["context"], "CTX1");
            let acts = &perform["params"]["actions"][0]["actions"];
            assert_eq!(acts[0]["type"], "pointerMove");
            assert_eq!(acts[0]["x"], 30);
            assert_eq!(acts[1]["type"], "pointerDown");
            assert_eq!(acts[2]["type"], "pointerUp");
            assert!(reqs.iter().any(|r| r["method"] == "script.callFunction"
                && r["params"]["arguments"][0]["value"] == 1));
        }
        assert!(state.sidecar.lock().await.is_none());

        let mut find_args = route.clone();
        find_args["query"] = json!("docs");
        let out = handler_for("browser_find")(state.clone(), find_args)
            .await
            .unwrap();
        assert_eq!(
            out["content"][0]["text"],
            "1 match for \"docs\":\ne2 link \"Docs\"\n"
        );

        mock.doc_token.store(4294967297, Ordering::SeqCst);
        let err = handler_for("browser_click")(state.clone(), args.clone())
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<SessionError>(),
            Some(SessionError::StaleRef {
                reason: "document changed",
                ..
            })
        ));
        let err = handler_for("browser_click")(state.clone(), args)
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<SessionError>(),
            Some(SessionError::RefUnknown { .. })
        ));
    }

    #[tokio::test]
    async fn bidi_type_by_ref_fills_and_submits() {
        let mock = spawn_bidi_a11y_mock().await;
        let state = bidi_state(&mock.endpoint);
        let route = json!({"target": "example\\.com"});
        handler_for("browser_snapshot")(state.clone(), route.clone())
            .await
            .unwrap();
        let mut args = route.clone();
        args["ref"] = json!("e1");
        args["text"] = json!("hello");
        args["submit"] = json!(true);
        let out = handler_for("browser_type")(state.clone(), args)
            .await
            .unwrap();
        assert_eq!(
            out["content"][0]["text"],
            "typed into e1 (button \"Submit\") and pressed Enter"
        );
        let reqs = mock.requests.lock().await;
        let typed = reqs
            .iter()
            .find(|r| {
                r["params"]["functionDeclaration"]
                    .as_str()
                    .is_some_and(|d| d.contains("bc:type"))
            })
            .expect("type helper");
        assert_eq!(typed["params"]["arguments"][1]["value"], "hello");
        assert_eq!(typed["params"]["arguments"][2]["value"], "fill");
        let keys = reqs
            .iter()
            .find(|r| r["method"] == "input.performActions")
            .expect("enter");
        assert_eq!(keys["params"]["actions"][0]["type"], "key");
        assert_eq!(
            keys["params"]["actions"][0]["actions"][0]["value"],
            "\u{e007}"
        );
    }

    #[tokio::test]
    async fn bidi_screenshot_by_ref_and_full_page_clip_to_document() {
        let mock = spawn_bidi_a11y_mock().await;
        let state = bidi_state(&mock.endpoint);
        let route = json!({"target": "example\\.com"});
        handler_for("browser_snapshot")(state.clone(), route.clone())
            .await
            .unwrap();
        let mut args = route.clone();
        args["ref"] = json!("e2");
        let out = handler_for("browser_take_screenshot")(state.clone(), args)
            .await
            .unwrap();
        assert_eq!(out["content"][0]["type"], "image");
        let reqs = mock.requests.lock().await;
        let cap = reqs
            .iter()
            .find(|r| r["method"] == "browsingContext.captureScreenshot")
            .expect("capture");
        assert_eq!(cap["params"]["origin"], "document");
        assert_eq!(cap["params"]["clip"]["type"], "box");
        assert_eq!(cap["params"]["clip"]["x"], 10);
        assert_eq!(cap["params"]["clip"]["width"], 100);
    }

    /// CDP mock serving an accessibility tree, document identity, and
    /// element geometry; records every request.
    struct A11yMock {
        endpoint: String,
        doc_token: Arc<std::sync::atomic::AtomicU64>,
        requests: Arc<Mutex<Vec<Value>>>,
    }

    async fn spawn_a11y_mock() -> A11yMock {
        use std::sync::atomic::{AtomicU64, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let doc_token = Arc::new(AtomicU64::new(101));
        let requests = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn({
            let doc_token = doc_token.clone();
            let requests = requests.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let mut next_session = 0u32;
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let req: Value = serde_json::from_str(&t).unwrap();
                    requests.lock().await.push(req.clone());
                    let id = req["id"].as_u64().unwrap();
                    let method = req["method"].as_str().unwrap_or("");
                    let result = match method {
                        "Target.getTargets" => json!({"targetInfos": [{
                            "targetId": "T1", "type": "page",
                            "url": "https://example.com/", "title": "Example",
                        }]}),
                        "Target.attachToTarget" => {
                            next_session += 1;
                            json!({"sessionId": format!("S{next_session}")})
                        }
                        "Runtime.evaluate" => json!({"result": {"value": 1}}),
                        "Accessibility.getFullAXTree" => json!({"nodes": [
                            {"nodeId": "1", "backendDOMNodeId": 101,
                             "role": {"value": "RootWebArea"}, "name": {"value": "Example"},
                             "childIds": ["2", "3"]},
                            {"nodeId": "2", "parentId": "1", "backendDOMNodeId": 106,
                             "role": {"value": "button"}, "name": {"value": "Submit"},
                             "properties": [{"name": "focusable", "value": {"value": true}}],
                             "childIds": []},
                            {"nodeId": "3", "parentId": "1", "backendDOMNodeId": 108,
                             "role": {"value": "link"}, "name": {"value": "Docs"},
                             "properties": [{"name": "focusable", "value": {"value": true}}],
                             "childIds": []},
                        ]}),
                        "DOM.getDocument" => {
                            json!({"root": {"backendNodeId": doc_token.load(Ordering::SeqCst)}})
                        }
                        "DOM.getContentQuads" => {
                            json!({"quads": [[10, 10, 50, 10, 50, 30, 10, 30]]})
                        }
                        "DOM.resolveNode" => json!({"object": {"objectId": "obj-1"}}),
                        "DOM.getBoxModel" => {
                            json!({"model": {"border": [10, 30, 110, 30, 110, 50, 10, 50]}})
                        }
                        "Page.captureScreenshot" => json!({"data": "PNGDATA"}),
                        "Page.getLayoutMetrics" => json!({"cssLayoutViewport": {
                            "pageX": 0, "pageY": 0, "clientWidth": 800, "clientHeight": 600
                        }}),
                        _ => json!({}),
                    };
                    let resp = json!({"id": id, "result": result});
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                }
            }
        });
        A11yMock {
            endpoint: format!("ws://{addr}"),
            doc_token,
            requests,
        }
    }

    #[tokio::test]
    async fn snapshot_then_click_by_ref_dispatches_native_input() {
        use std::sync::atomic::Ordering;
        let mock = spawn_a11y_mock().await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: mock.endpoint.clone(),
            source: Source::External,
        });
        let route = json!({"target": "example\\.com"});

        let snap = handler_for("browser_snapshot")(state.clone(), route.clone())
            .await
            .unwrap();
        let text = snap["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            text,
            "# Example (https://example.com/)\n- button \"Submit\" [ref=e1]\n- link \"Docs\" [ref=e2]\n"
        );

        // Refs are stable across a second snapshot of the same document.
        let again = handler_for("browser_snapshot")(state.clone(), route.clone())
            .await
            .unwrap();
        assert_eq!(again["content"][0]["text"], snap["content"][0]["text"]);

        let mut args = route.clone();
        args["ref"] = json!("e1");
        let out = handler_for("browser_click")(state.clone(), args.clone())
            .await
            .unwrap();
        assert_eq!(out["content"][0]["text"], "clicked e1 (button \"Submit\")");
        {
            let reqs = mock.requests.lock().await;
            let mouse: Vec<&Value> = reqs
                .iter()
                .filter(|r| r["method"] == "Input.dispatchMouseEvent")
                .collect();
            assert_eq!(mouse.len(), 3);
            assert_eq!(mouse[1]["params"]["type"], "mousePressed");
            assert_eq!(mouse[1]["params"]["x"], 30.0);
            assert_eq!(mouse[1]["params"]["y"], 20.0);
            assert!(reqs
                .iter()
                .any(|r| r["method"] == "DOM.scrollIntoViewIfNeeded"
                    && r["params"]["backendNodeId"] == 106));
        }

        // find hands out the same refs.
        let mut find_args = route.clone();
        find_args["query"] = json!("docs");
        let out = handler_for("browser_find")(state.clone(), find_args)
            .await
            .unwrap();
        assert_eq!(
            out["content"][0]["text"],
            "1 match for \"docs\":\ne2 link \"Docs\"\n"
        );

        // Unknown ref.
        let mut bad = route.clone();
        bad["ref"] = json!("e9");
        let err = handler_for("browser_click")(state.clone(), bad)
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<SessionError>(),
            Some(SessionError::RefUnknown { .. })
        ));

        // The page navigates: document token changes, refs become stale.
        mock.doc_token.store(202, Ordering::SeqCst);
        let err = handler_for("browser_click")(state.clone(), args.clone())
            .await
            .unwrap_err();
        match err.downcast_ref::<SessionError>() {
            Some(SessionError::StaleRef { reason, .. }) => assert_eq!(*reason, "document changed"),
            other => panic!("expected StaleRef, got {other:?}"),
        }
        assert!(err.to_string().contains("browser_snapshot"));
        // The stale table was dropped, so the same ref is now unknown.
        let err = handler_for("browser_click")(state.clone(), args)
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<SessionError>(),
            Some(SessionError::RefUnknown { .. })
        ));
    }

    #[tokio::test]
    async fn type_by_ref_inserts_text_and_submits() {
        let mock = spawn_a11y_mock().await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: mock.endpoint.clone(),
            source: Source::External,
        });
        let route = json!({"target": "example\\.com"});
        handler_for("browser_snapshot")(state.clone(), route.clone())
            .await
            .unwrap();
        let mut args = route.clone();
        args["ref"] = json!("e1");
        args["text"] = json!("hello");
        args["submit"] = json!(true);
        let out = handler_for("browser_type")(state.clone(), args)
            .await
            .unwrap();
        assert_eq!(
            out["content"][0]["text"],
            "typed into e1 (button \"Submit\") and pressed Enter"
        );
        let reqs = mock.requests.lock().await;
        let insert = reqs
            .iter()
            .find(|r| r["method"] == "Input.insertText")
            .expect("insertText");
        assert_eq!(insert["params"]["text"], "hello");
        assert!(reqs
            .iter()
            .any(|r| r["method"] == "Input.dispatchKeyEvent" && r["params"]["key"] == "Enter"));
        // Nothing was forwarded to a sidecar: no Node process, no `connect`.
        assert!(state.sidecar.lock().await.is_none());
    }

    /// CDP mock for the capture tools: hands out sessions, and after
    /// `Network.enable` on a session pushes one console error and one
    /// finished request on that session.
    async fn spawn_capture_mock() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut next_session = 0u32;
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let req: Value = serde_json::from_str(&t).unwrap();
                let id = req["id"].as_u64().unwrap();
                let method = req["method"].as_str().unwrap_or("").to_string();
                let sid = req["sessionId"].as_str().unwrap_or("").to_string();
                let result = match method.as_str() {
                    "Target.getTargets" => json!({"targetInfos": [{
                        "targetId": "T1", "type": "page",
                        "url": "https://app.test/", "title": "App",
                    }]}),
                    "Target.attachToTarget" => {
                        next_session += 1;
                        json!({"sessionId": format!("S{next_session}")})
                    }
                    "Runtime.evaluate" => json!({"result": {"value": 1}}),
                    "Page.getNavigationHistory" => json!({
                        "currentIndex": 0, "entries": [{"url": "https://app.test/"}]
                    }),
                    "Network.getResponseBody" => {
                        json!({"body": "{\"ok\":true}", "base64Encoded": false})
                    }
                    _ => json!({}),
                };
                ws.send(Message::Text(
                    json!({"id": id, "result": result}).to_string(),
                ))
                .await
                .unwrap();
                if method == "Network.enable" {
                    for ev in [
                        json!({"method": "Runtime.consoleAPICalled", "sessionId": sid,
                               "params": {"type": "error", "timestamp": 1756816496120.0,
                                          "args": [{"type": "string", "value": "boom"}],
                                          "stackTrace": {"callFrames": [{"url": "https://app.test/a.js", "lineNumber": 3, "columnNumber": 0}]}}}),
                        json!({"method": "Network.requestWillBeSent", "sessionId": sid,
                               "params": {"requestId": "9.1", "timestamp": 10.0, "wallTime": 1756816496.0, "type": "XHR",
                                          "request": {"method": "GET", "url": "https://app.test/api/me"}}}),
                        json!({"method": "Network.responseReceived", "sessionId": sid,
                               "params": {"requestId": "9.1", "type": "XHR",
                                          "response": {"status": 401, "mimeType": "application/json"}}}),
                        json!({"method": "Network.loadingFinished", "sessionId": sid,
                               "params": {"requestId": "9.1", "timestamp": 10.084, "encodedDataLength": 11}}),
                    ] {
                        ws.send(Message::Text(ev.to_string())).await.unwrap();
                    }
                }
            }
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn capture_tools_read_buffered_console_and_network_after_navigate() {
        let endpoint = spawn_capture_mock().await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint,
            source: Source::External,
        });
        let route = json!({"target": "app\\.test"});
        let mut nav = route.clone();
        nav["url"] = json!("https://app.test/x");
        handler_for("browser_navigate")(state.clone(), nav)
            .await
            .unwrap();

        // Events are pushed by the mock right after Network.enable; give the
        // router a moment (test-side bounded polling only).
        let mut text = String::new();
        for _ in 0..100 {
            let out = handler_for("browser_console_messages")(state.clone(), route.clone())
                .await
                .unwrap();
            text = out["content"][0]["text"].as_str().unwrap().to_string();
            if text.contains("boom") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            text.starts_with("console tab=T1 page=https://app.test/  showing 1 of 1 matched (1 buffered, 0 evicted, 0 events lost)\n-- page: https://app.test/ --\n[error] 2025-09-02T12:34:56.120Z https://app.test/a.js:4:1  boom"),
            "{text}"
        );

        // Pattern filtering and clear.
        let mut q = route.clone();
        q["pattern"] = json!("nomatch");
        let out = handler_for("browser_console_messages")(state.clone(), q)
            .await
            .unwrap();
        assert!(out["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("showing 0 of 0 matched (1 buffered"));
        let mut q = route.clone();
        q["pattern"] = json!("[");
        let err = handler_for("browser_console_messages")(state.clone(), q)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid `pattern` regex"));
        let mut q = route.clone();
        q["limit"] = json!(0);
        q["clear"] = json!(true);
        handler_for("browser_console_messages")(state.clone(), q)
            .await
            .unwrap();
        let out = handler_for("browser_console_messages")(state.clone(), route.clone())
            .await
            .unwrap();
        assert!(out["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("(0 buffered"));

        // Network listing with filters.
        let mut q = route.clone();
        q["status"] = json!("4xx");
        q["url_pattern"] = json!("/api/");
        let out = handler_for("browser_network_requests")(state.clone(), q)
            .await
            .unwrap();
        let text = out["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains(
                "9.1  GET    https://app.test/api/me  → 401 application/json 11B 84ms [XHR]"
            ),
            "{text}"
        );
        let mut q = route.clone();
        q["status"] = json!("2xx");
        let out = handler_for("browser_network_requests")(state.clone(), q)
            .await
            .unwrap();
        assert!(out["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("showing 0 of 0 matched (1 buffered"));
        let mut q = route.clone();
        q["format"] = json!("json");
        let out = handler_for("browser_network_requests")(state.clone(), q)
            .await
            .unwrap();
        let parsed: Value =
            serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["entries"][0]["request_id"], "9.1");
        assert_eq!(parsed["entries"][0]["state"], "finished");

        // Body fetch.
        let mut q = route.clone();
        q["request_id"] = json!("9.1");
        let out = handler_for("browser_network_body")(state.clone(), q)
            .await
            .unwrap();
        assert_eq!(out["content"][0]["text"], "{\"ok\":true}");
        let meta: Value =
            serde_json::from_str(out["content"][1]["text"].as_str().unwrap()).unwrap();
        assert_eq!(meta["status"], 401);
        assert_eq!(meta["truncated"], false);

        // Closing the tab forgets its capture. (The mock keeps listing T1,
        // so a later tool call would re-touch it; assert on the hub directly.)
        assert_eq!(state.capture.captured_tabs(), vec!["T1".to_string()]);
        handler_for("browser_tab_close")(state.clone(), json!({"target_id": "T1"}))
            .await
            .unwrap();
        assert!(state.capture.captured_tabs().is_empty());
    }

    #[tokio::test]
    async fn capture_tools_validate_before_backend_and_gate_on_engine() {
        let h = handler_for("browser_network_requests");
        let err = h(unreached_state(), json!({"status": "lots"}))
            .await
            .expect_err("must error");
        assert!(err.to_string().contains("`status` must be"), "{err:#}");
        let h = handler_for("browser_network_body");
        let err = h(unreached_state(), json!({}))
            .await
            .expect_err("must error");
        assert!(err.to_string().contains("missing 'request_id'"), "{err:#}");

        // Only bodies are gated on the engine; the listing tools reach the
        // backend on Firefox (and fail here only because nothing listens).
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Bidi,
            endpoint: "ws://127.0.0.1:0".into(),
            source: Source::External,
        });
        let err = handler_for("browser_network_body")(state.clone(), json!({"request_id": "1"}))
            .await
            .expect_err("BiDi must error");
        match err.downcast_ref::<SessionError>() {
            Some(SessionError::EngineUnsupported { tool, hint, .. }) => {
                assert_eq!(tool, "browser_network_body");
                assert!(hint.contains("browser_fetch") && hint.contains("browser_select"));
            }
            other => panic!("expected EngineUnsupported, got {other:?}"),
        }
        for tool in ["browser_console_messages", "browser_network_requests"] {
            let err = handler_for(tool)(state.clone(), json!({}))
                .await
                .expect_err("unreachable endpoint must error");
            assert!(
                !matches!(
                    err.downcast_ref::<SessionError>(),
                    Some(SessionError::EngineUnsupported { .. })
                ),
                "{tool} must not be engine-gated on BiDi: {err:#}"
            );
        }
    }

    /// BiDi-framed CDP-free mock for the capture tools on Firefox: answers
    /// the session handshake, tree, navigate and subscribe, then pushes one
    /// console error and one finished request on context `C1`.
    async fn spawn_bidi_capture_mock() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let req: Value = serde_json::from_str(&t).unwrap();
                let id = req["id"].as_u64().unwrap();
                let method = req["method"].as_str().unwrap_or("").to_string();
                let result = match method.as_str() {
                    "session.new" => json!({"sessionId": "S1", "capabilities": {}}),
                    "browsingContext.getTree" => json!({"contexts": [
                        {"context": "C1", "url": "https://app.test/", "children": []}
                    ]}),
                    "browsingContext.navigate" => {
                        json!({"navigation": "N1", "url": "https://app.test/x"})
                    }
                    "script.evaluate" => {
                        json!({"type": "success", "result": {"type": "number", "value": 1}})
                    }
                    _ => json!({}),
                };
                ws.send(Message::Text(
                    json!({"type": "success", "id": id, "result": result}).to_string(),
                ))
                .await
                .unwrap();
                let first_event = req["params"]["events"][0].as_str().unwrap_or("");
                if method == "session.subscribe" && first_event.starts_with("network.") {
                    for ev in [
                        json!({"type": "event", "method": "log.entryAdded", "params": {
                            "type": "console", "level": "error", "method": "error", "text": "boom",
                            "timestamp": 1756816496120.0, "source": {"context": "C1"},
                            "stackTrace": {"callFrames": [{"url": "https://app.test/a.js", "lineNumber": 3, "columnNumber": 0}]}}}),
                        json!({"type": "event", "method": "network.beforeRequestSent", "params": {
                            "context": "C1", "navigation": null, "redirectCount": 0, "timestamp": 1756816496000.0,
                            "initiator": {"type": "other"},
                            "request": {"request": "9.1", "url": "https://app.test/api/me", "method": "GET", "bodySize": 0, "initiatorType": "xmlhttprequest"}}}),
                        json!({"type": "event", "method": "network.responseCompleted", "params": {
                            "context": "C1", "timestamp": 1756816496084.0, "redirectCount": 0,
                            "request": {"request": "9.1"},
                            "response": {"status": 401, "mimeType": "application/json", "bytesReceived": 11}}}),
                    ] {
                        ws.send(Message::Text(ev.to_string())).await.unwrap();
                    }
                }
            }
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn capture_tools_work_on_bidi_and_body_is_cdp_only() {
        let endpoint = spawn_bidi_capture_mock().await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Bidi,
            endpoint,
            source: Source::External,
        });
        let route = json!({"target": "app\\.test"});
        let mut nav = route.clone();
        nav["url"] = json!("https://app.test/x");
        handler_for("browser_navigate")(state.clone(), nav)
            .await
            .unwrap();
        let mut text = String::new();
        for _ in 0..100 {
            let out = handler_for("browser_console_messages")(state.clone(), route.clone())
                .await
                .unwrap();
            text = out["content"][0]["text"].as_str().unwrap().to_string();
            if text.contains("boom") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            text.starts_with("console tab=C1 page=https://app.test/  showing 1 of 1 matched (1 buffered, 0 evicted, 0 events lost)\n-- page: https://app.test/ --\n[error] 2025-09-02T12:34:56.120Z https://app.test/a.js:4:1  boom"),
            "{text}"
        );
        let mut q = route.clone();
        q["status"] = json!("4xx");
        let out = handler_for("browser_network_requests")(state.clone(), q)
            .await
            .unwrap();
        let net = out["content"][0]["text"].as_str().unwrap();
        assert!(
            net.contains(
                "9.1  GET    https://app.test/api/me  → 401 application/json 11B 84ms [XHR]"
            ),
            "{net}"
        );
        let mut q = route.clone();
        q["request_id"] = json!("9.1");
        let err = handler_for("browser_network_body")(state.clone(), q)
            .await
            .unwrap_err();
        match err.downcast_ref::<SessionError>() {
            Some(SessionError::EngineUnsupported { hint, .. }) => {
                assert!(hint.contains("browser_fetch"))
            }
            other => panic!("expected EngineUnsupported, got {other:?}"),
        }
        assert_eq!(state.capture.captured_tabs(), vec!["C1".to_string()]);
        handler_for("browser_tab_close")(state.clone(), json!({"target_id": "C1"}))
            .await
            .unwrap();
        assert!(state.capture.captured_tabs().is_empty());
    }

    #[tokio::test]
    async fn tab_foreground_toggles_emulation_and_shows_in_tab_list() {
        let mock = spawn_a11y_mock().await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: mock.endpoint.clone(),
            source: Source::External,
        });
        let route = json!({"target": "example\\.com"});
        let out = handler_for("browser_tab_foreground")(state.clone(), route.clone())
            .await
            .unwrap();
        assert!(out["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("foreground emulation on for tab T1"));
        {
            let reqs = mock.requests.lock().await;
            let focus = reqs
                .iter()
                .find(|r| r["method"] == "Emulation.setFocusEmulationEnabled")
                .expect("focus emulation");
            assert_eq!(focus["params"]["enabled"], true);
            assert!(focus["sessionId"].is_string());
            assert!(reqs
                .iter()
                .any(|r| r["method"] == "Emulation.setIdleOverride"
                    && r["params"]["isScreenUnlocked"] == true));
        }
        let list = handler_for("browser_tab_list")(state.clone(), json!({}))
            .await
            .unwrap();
        let rows: Value =
            serde_json::from_str(list["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(rows[0]["foreground"], true);

        let mut off = route.clone();
        off["enabled"] = json!(false);
        handler_for("browser_tab_foreground")(state.clone(), off)
            .await
            .unwrap();
        let reqs = mock.requests.lock().await;
        assert!(reqs
            .iter()
            .any(|r| r["method"] == "Emulation.setFocusEmulationEnabled"
                && r["params"]["enabled"] == false));
        assert!(state.capture.foreground_tabs().is_empty());
    }

    #[tokio::test]
    async fn tab_foreground_is_chromium_only() {
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Bidi,
            endpoint: "ws://127.0.0.1:0".into(),
            source: Source::External,
        });
        for (tool, args) in [
            ("browser_tab_foreground", json!({})),
            ("browser_tab_new", json!({"foreground": true})),
            (
                "browser_tab_select",
                json!({"target_id": "C1", "foreground": true}),
            ),
        ] {
            let err = handler_for(tool)(state.clone(), args)
                .await
                .expect_err("BiDi must error");
            match err.downcast_ref::<SessionError>() {
                Some(SessionError::EngineUnsupported { hint, .. }) => {
                    assert!(hint.contains("setFocusEmulationEnabled"))
                }
                other => panic!("{tool}: expected EngineUnsupported, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn tab_close_drops_refs() {
        let mock = spawn_a11y_mock().await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: mock.endpoint.clone(),
            source: Source::External,
        });
        handler_for("browser_snapshot")(state.clone(), json!({"target": "example\\.com"}))
            .await
            .unwrap();
        assert!(state.refs.lock().await.contains_key("T1"));
        handler_for("browser_tab_close")(state.clone(), json!({"target_id": "T1"}))
            .await
            .unwrap();
        assert!(!state.refs.lock().await.contains_key("T1"));
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

    #[test]
    fn sidecar_cdp_attach_failure_classifier_matches_connect_layer_errors() {
        let err = anyhow::anyhow!(
            "browserType.connectOverCDP: Timeout 5000ms exceeded while <ws connecting> to ws://127.0.0.1:64767/devtools/browser/x"
        );
        assert!(looks_like_sidecar_cdp_attach_failure(&err));

        let err = anyhow::anyhow!("page.waitForLoadState: Timeout 30000ms exceeded");
        assert!(
            !looks_like_sidecar_cdp_attach_failure(&err),
            "normal page wait timeouts must not be reclassified as sidecar attach failures"
        );
    }

    #[test]
    fn sidecar_connection_failed_message_discourages_page_hang_inference() {
        let err = SessionError::SidecarConnectionFailed {
            tool: "browser_snapshot".into(),
            method: "snapshot".into(),
            target_id: "T1".into(),
            url: Some("http://localhost:5173/404".into()),
            details: "browserType.connectOverCDP: Timeout 5000ms exceeded".into(),
            hint: "retry the Playwright-sidecar tool or inspect with browser_get_html / browser_take_screenshot",
        };
        let msg = err.to_string();
        assert!(msg.contains("Playwright sidecar connection failed"));
        assert!(msg.contains("not evidence that the page is hung"));
        assert!(msg.contains("browser_get_html"));
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
    async fn screenshot_jpeg_quality_and_max_width_scale_clip() {
        // Every post-probe evaluate returns 2.0 → devicePixelRatio = 2.
        let mock = spawn_screenshot_mock(json!(2.0)).await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: mock.endpoint,
            source: Source::External,
        });
        let out = handler_for("browser_take_screenshot")(
            state.clone(),
            json!({
                "target": "example\\.com",
                "format": "jpeg",
                "quality": 60,
                "max_width": 500,
            }),
        )
        .await
        .unwrap();
        assert_eq!(out["content"][0]["mimeType"], "image/jpeg");
        let captures = mock.capture_params.lock().await;
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0]["format"], "jpeg");
        assert_eq!(captures[0]["quality"], 60);
        // Viewport is 1000 CSS px wide at DPR 2 → 2000 device px; 500 → 0.25.
        assert_eq!(captures[0]["captureBeyondViewport"], true);
        assert_eq!(captures[0]["clip"]["x"], json!(0.0));
        assert_eq!(captures[0]["clip"]["y"], json!(100.0));
        assert_eq!(captures[0]["clip"]["width"], json!(1000.0));
        assert_eq!(captures[0]["clip"]["height"], json!(500.0));
        assert_eq!(captures[0]["clip"]["scale"], json!(0.25));
    }

    #[tokio::test]
    async fn screenshot_max_width_larger_than_page_keeps_default_params() {
        let mock = spawn_screenshot_mock(json!(1.0)).await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: mock.endpoint,
            source: Source::External,
        });
        handler_for("browser_take_screenshot")(
            state,
            json!({"target": "example\\.com", "max_width": 4000, "full_page": true}),
        )
        .await
        .unwrap();
        let captures = mock.capture_params.lock().await;
        assert_eq!(captures[0]["format"], "png");
        assert!(captures[0].get("clip").is_none());
        assert!(captures[0].get("quality").is_none());
        assert_eq!(captures[0]["captureBeyondViewport"], true);
    }

    #[tokio::test]
    async fn screenshot_save_to_writes_private_file_and_reports_dimensions() {
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD.encode(fake_png_1280x720());
        let mock = spawn_screenshot_mock_with_data(Value::Null, data).await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: mock.endpoint,
            source: Source::External,
        });
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("shot.png");
        let out = handler_for("browser_take_screenshot")(
            state,
            json!({"target": "example\\.com", "save_to": path.to_str().unwrap()}),
        )
        .await
        .unwrap();
        assert_eq!(out["content"][0]["type"], "text");
        let text = out["content"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with(&format!(
                "Saved screenshot to {} (1280x720, image/png, 1 KiB)",
                path.display()
            )),
            "{text}"
        );
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, fake_png_1280x720());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn screenshot_option_validation_fires_before_backend() {
        let h = handler_for("browser_take_screenshot");
        for (args, needle) in [
            (json!({"quality": 50}), "only applies to"),
            (json!({"format": "gif"}), "`format` must be"),
            (json!({"max_width": 10}), "at least 64"),
            (json!({"save_to": "relative.png"}), "absolute path"),
            (
                json!({"save_to": "/definitely/missing/dir/x.png"}),
                "parent directory",
            ),
            (json!({"selector": "#a", "ref": "e1"}), "mutually exclusive"),
        ] {
            let err = h(unreached_state(), args.clone())
                .await
                .expect_err("must error");
            assert!(err.to_string().contains(needle), "{args}: {err:#}");
        }
    }

    #[tokio::test]
    async fn screenshot_by_ref_clips_to_node_box() {
        let mock = spawn_a11y_mock().await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: mock.endpoint.clone(),
            source: Source::External,
        });
        let route = json!({"target": "example\\.com"});
        handler_for("browser_snapshot")(state.clone(), route.clone())
            .await
            .unwrap();
        let mut args = route.clone();
        args["ref"] = json!("e1");
        let out = handler_for("browser_take_screenshot")(state.clone(), args)
            .await
            .unwrap();
        assert_eq!(out["content"][0]["type"], "image");
        let reqs = mock.requests.lock().await;
        let cap = reqs
            .iter()
            .find(|r| r["method"] == "Page.captureScreenshot")
            .expect("capture");
        assert_eq!(cap["params"]["clip"]["x"], json!(10.0));
        assert_eq!(cap["params"]["clip"]["y"], json!(30.0));
        assert_eq!(cap["params"]["clip"]["width"], json!(100.0));
        assert_eq!(cap["params"]["clip"]["height"], json!(20.0));
        assert!(reqs
            .iter()
            .any(|r| r["method"] == "DOM.getBoxModel" && r["params"]["backendNodeId"] == 106));
    }

    #[tokio::test]
    async fn get_page_text_formats_result_and_truncation() {
        let payload = json!({
            "title": "Docs",
            "url": "https://example.com/docs",
            "source": "main",
            "text": "# Welcome\nHello world",
            "truncated": true,
            "total_chars": 12345,
        })
        .to_string();
        let mock = spawn_screenshot_mock(Value::String(payload)).await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: mock.endpoint,
            source: Source::External,
        });
        let out = handler_for("browser_get_page_text")(
            state,
            json!({"target": "example\\.com", "max_chars": 1000}),
        )
        .await
        .unwrap();
        assert_eq!(
            out["content"][0]["text"],
            "Docs\nhttps://example.com/docs\n\n# Welcome\nHello world\n… [truncated at 1000 of 12345 chars; pass max_chars or selector to narrow]"
        );

        let mock = spawn_screenshot_mock(Value::String(
            json!({"error": "selector matched no element: #x"}).to_string(),
        ))
        .await;
        let state = ServerState::new(ResolvedBrowser {
            engine: Engine::Cdp,
            endpoint: mock.endpoint,
            source: Source::External,
        });
        let err = handler_for("browser_get_page_text")(
            state,
            json!({"target": "example\\.com", "selector": "#x"}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("selector matched no element"));

        let err = handler_for("browser_get_page_text")(unreached_state(), json!({"max_chars": 10}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("at least 500"));
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
    async fn curl_missing_args_errors_before_backend() {
        let h = handler_for("browser_curl");
        let err = h(unreached_state(), json!({}))
            .await
            .expect_err("missing args must error");
        assert!(err.to_string().contains("'args'"), "got: {err:#}");
    }

    #[tokio::test]
    async fn curl_rejects_non_string_args_before_backend() {
        let h = handler_for("browser_curl");
        let err = h(unreached_state(), json!({"args": ["-L", 7]}))
            .await
            .expect_err("non-string args must error");
        assert!(
            err.to_string()
                .contains("every curl argument must be a string"),
            "got: {err:#}"
        );
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
