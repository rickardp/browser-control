//! MCP tools exposed by the `browser-control mcp` server.
//!
//! Ten tools wrap the underlying `session::PageSession` and related helpers:
//! `navigate`, `get_dom`, `screenshot`, `fetch`, `select_element`,
//! `list_targets`, `cookies`, `storage_get`, `storage_set`, `wait_for_cookie`.

use anyhow::{anyhow, Result};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cli::cookies::fetch_cookies;
use crate::cli::storage::{build_get_expr, build_set_expr, ns_global};
use crate::cli::wait_for_cookie::cookie_matches;
use crate::detect::Engine;
use crate::dom::scripts::{FETCH_JS, GET_DOM_JS, SELECT_ELEMENT_JS};
use crate::mcp::server::{RegisteredTool, ServerState, ToolHandler, ToolRegistry};
use crate::session::attach::PageSession;
use crate::session::targets::{list as list_targets, open_bidi};

/// Register the standard tool set onto the given registry.
pub fn register_all(registry: &ToolRegistry) {
    registry.register(make_navigate());
    registry.register(make_get_dom());
    registry.register(make_screenshot());
    registry.register(make_fetch());
    registry.register(make_select_element());
    registry.register(make_list_targets());
    registry.register(make_cookies());
    registry.register(make_storage_get());
    registry.register(make_storage_set());
    registry.register(make_wait_for_cookie());
}

// ---------------------------------------------------------------------------
// Engine dispatch helpers.
// ---------------------------------------------------------------------------

/// Open (or return the cached) page session for the configured browser.
///
/// CDP sessions are short-lived: we open and close a fresh WebSocket per
/// call. BiDi sessions reuse one persistent client, because Firefox limits
/// a browser instance to a single concurrent BiDi session.
async fn attach_active(state: &ServerState) -> Result<PageSession> {
    match state.browser.engine {
        Engine::Cdp => PageSession::attach(&state.browser.endpoint, Engine::Cdp, None).await,
        Engine::Bidi => {
            let mut guard = state.bidi.lock().await;
            let client = if let Some(c) = guard.as_ref() {
                c.clone()
            } else {
                let c = Arc::new(open_bidi(&state.browser.endpoint).await?);
                c.session_new().await?;
                *guard = Some(c.clone());
                c
            };
            PageSession::from_bidi_cache(client, None).await
        }
    }
}

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

// ---------------------------------------------------------------------------
// navigate
// ---------------------------------------------------------------------------

fn make_navigate() -> RegisteredTool {
    RegisteredTool {
        name: "navigate".into(),
        description: "Navigate the active page to a URL.".into(),
        input_schema: json!({
            "type": "object",
            "properties": { "url": { "type": "string" } },
            "required": ["url"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let url = args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing 'url'"))?
                    .to_string();
                let session = attach_active(&state).await?;
                session.navigate(&url).await?;
                session.close().await;
                Ok(text_content(format!("Navigated to {url}")))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// get_dom
// ---------------------------------------------------------------------------

fn make_get_dom() -> RegisteredTool {
    RegisteredTool {
        name: "get_dom".into(),
        description: "Get the rendered DOM as HTML, with shadow roots serialized when supported."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "selector": {
                    "type": "string",
                    "description": "Optional CSS selector; defaults to the document element."
                }
            },
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let selector_arg = args.get("selector").and_then(|v| v.as_str());
                let selector_literal = match selector_arg {
                    Some(s) => serde_json::to_string(s)?,
                    None => "null".to_string(),
                };
                let expr = format!("({GET_DOM_JS})({selector_literal})");
                let session = attach_active(&state).await?;
                let value = session.evaluate(&expr, false).await?;
                session.close().await;
                let html = value.as_str().unwrap_or("").to_string();
                Ok(text_content(html))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// screenshot
// ---------------------------------------------------------------------------

fn make_screenshot() -> RegisteredTool {
    RegisteredTool {
        name: "screenshot".into(),
        description: "Capture a PNG screenshot of the active page.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "full_page": { "type": "boolean", "default": false },
                "selector": { "type": "string" }
            },
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                let full_page = args
                    .get("full_page")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let session = attach_active(&state).await?;
                let b64 = session.screenshot(full_page).await?;
                session.close().await;
                Ok(image_content(b64))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// fetch
// ---------------------------------------------------------------------------

fn make_fetch() -> RegisteredTool {
    RegisteredTool {
        name: "fetch".into(),
        description:
            "Perform an HTTP request from the page context (preserves cookies, bypasses CORS)."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "method": { "type": "string" },
                "headers": { "type": "object" },
                "body": { "type": "string" }
            },
            "required": ["url"],
        }),
        handler: handler(|state, args| {
            Box::pin(async move {
                if args.get("url").and_then(|v| v.as_str()).is_none() {
                    return Err(anyhow!("missing 'url'"));
                }
                let args_json = serde_json::to_string(&args)?;
                let args_literal = serde_json::to_string(&args_json)?;
                let expr = format!("({FETCH_JS})({args_literal})");
                let session = attach_active(&state).await?;
                let value = session.evaluate(&expr, true).await?;
                session.close().await;
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
// select_element
// ---------------------------------------------------------------------------

fn make_select_element() -> RegisteredTool {
    RegisteredTool {
        name: "select_element".into(),
        description:
            "Show an interactive overlay; resolve with the CSS selector for the clicked element."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {},
        }),
        handler: handler(|state, _args| {
            Box::pin(async move {
                let expr = SELECT_ELEMENT_JS.to_string();
                let session = attach_active(&state).await?;
                let value = session.evaluate(&expr, true).await?;
                session.close().await;
                let selector = value.as_str().unwrap_or("").to_string();
                Ok(text_content(selector))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// list_targets
// ---------------------------------------------------------------------------

fn make_list_targets() -> RegisteredTool {
    RegisteredTool {
        name: "list_targets".into(),
        description: "List open page targets, optionally filtered by an unanchored URL regex."
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
                let filter = args
                    .get("filter")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let targets = list_targets(
                    &state.browser.endpoint,
                    state.browser.engine,
                    filter.as_deref(),
                )
                .await?;
                Ok(text_content(serde_json::to_string_pretty(&targets)?))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// cookies
// ---------------------------------------------------------------------------

fn make_cookies() -> RegisteredTool {
    RegisteredTool {
        name: "cookies".into(),
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
                let all = fetch_cookies(&state.browser).await?;
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
// storage_get / storage_set
// ---------------------------------------------------------------------------

fn make_storage_get() -> RegisteredTool {
    RegisteredTool {
        name: "storage_get".into(),
        description: "Read a value from localStorage or sessionStorage on the active page.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "key": { "type": "string" },
                "namespace": {
                    "type": "string",
                    "enum": ["local", "session"],
                    "default": "local"
                }
            },
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
                let session = attach_active(&state).await?;
                let value = session.evaluate(&expr, true).await?;
                session.close().await;
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
        name: "storage_set".into(),
        description: "Write a value to localStorage or sessionStorage on the active page.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "key": { "type": "string" },
                "value": { "type": "string" },
                "namespace": {
                    "type": "string",
                    "enum": ["local", "session"],
                    "default": "local"
                }
            },
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
                let session = attach_active(&state).await?;
                let _ = session.evaluate(&expr, true).await?;
                session.close().await;
                Ok(text_content("ok"))
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// wait_for_cookie
// ---------------------------------------------------------------------------

fn make_wait_for_cookie() -> RegisteredTool {
    RegisteredTool {
        name: "wait_for_cookie".into(),
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
                loop {
                    let cookies = fetch_cookies(&state.browser).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED_TOOLS: &[&str] = &[
        "navigate",
        "get_dom",
        "screenshot",
        "fetch",
        "select_element",
        "list_targets",
        "cookies",
        "storage_get",
        "storage_set",
        "wait_for_cookie",
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

    #[test]
    fn register_all_adds_ten_tools() {
        let registry = ToolRegistry::new();
        register_all(&registry);
        let list = registry.list();
        assert_eq!(list.len(), 10);
        let names: Vec<&str> = list.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in EXPECTED_TOOLS {
            assert!(
                names.contains(expected),
                "missing tool {expected} in {names:?}"
            );
        }
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
    fn cookies_schema_has_optional_filters() {
        let schema = schema_for("cookies");
        assert_eq!(schema["properties"]["domain"]["type"], "string");
        assert_eq!(schema["properties"]["name"]["type"], "string");
        assert!(
            schema.get("required").is_none() || schema["required"].as_array().unwrap().is_empty()
        );
    }

    #[test]
    fn storage_get_requires_key() {
        let schema = schema_for("storage_get");
        let required = schema["required"].as_array().expect("required array");
        assert!(required.iter().any(|v| v == "key"));
        assert_eq!(schema["properties"]["key"]["type"], "string");
        assert_eq!(schema["properties"]["namespace"]["type"], "string");
    }

    #[test]
    fn storage_set_requires_key_and_value() {
        let schema = schema_for("storage_set");
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
    fn wait_for_cookie_requires_domain_and_name() {
        let schema = schema_for("wait_for_cookie");
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
}
