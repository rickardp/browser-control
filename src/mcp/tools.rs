//! MCP tools exposed by the `browser-control mcp` server.
//!
//! Five tools wrap the underlying CDP / BiDi clients:
//! `navigate`, `get_dom`, `screenshot`, `fetch`, `select_element`.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::detect::Engine;
use crate::dom::scripts::{FETCH_JS, GET_DOM_JS, SELECT_ELEMENT_JS};
use crate::mcp::server::{RegisteredTool, ServerState, ToolHandler, ToolRegistry};

/// Register the standard tool set onto the given registry.
pub fn register_all(registry: &ToolRegistry) {
    registry.register(make_navigate());
    registry.register(make_get_dom());
    registry.register(make_screenshot());
    registry.register(make_fetch());
    registry.register(make_select_element());
}

// ---------------------------------------------------------------------------
// Engine dispatch helpers.
// ---------------------------------------------------------------------------

async fn open_cdp(endpoint: &str) -> Result<crate::cdp::CdpClient> {
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        crate::cdp::CdpClient::connect(endpoint).await
    } else {
        crate::cdp::CdpClient::connect_http(endpoint).await
    }
}

async fn open_bidi(endpoint: &str) -> Result<crate::bidi::BidiClient> {
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        crate::bidi::BidiClient::connect(endpoint).await
    } else {
        let client = reqwest::Client::new();
        let v: Value = client
            .get(format!("{}/json/version", endpoint.trim_end_matches('/')))
            .send()
            .await?
            .json()
            .await?;
        let ws = v
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("no webSocketDebuggerUrl"))?
            .to_string();
        crate::bidi::BidiClient::connect(&ws).await
    }
}

/// Attach to the first `page` target via CDP and return `(client, session_id)`.
async fn cdp_attach_first_page(endpoint: &str) -> Result<(crate::cdp::CdpClient, String)> {
    let client = open_cdp(endpoint).await?;
    let targets = client.list_targets().await?;
    let target_id = targets
        .iter()
        .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
        .and_then(|t| t.get("targetId").and_then(|v| v.as_str()))
        .ok_or_else(|| anyhow!("no page target found"))?
        .to_string();
    let session_id = client.attach_to_target(&target_id).await?;
    Ok((client, session_id))
}

/// Open or return the cached BiDi session and top-level browsing context.
///
/// Firefox limits a browser instance to one active BiDi session at a time,
/// so we open the WebSocket once and reuse it for every tool call.
async fn bidi_top_context(
    state: &ServerState,
) -> Result<(std::sync::Arc<crate::bidi::BidiClient>, String)> {
    let mut guard = state.bidi.lock().await;
    if let Some((c, ctx)) = guard.as_ref() {
        return Ok((c.clone(), ctx.clone()));
    }
    let client = open_bidi(&state.browser.endpoint).await?;
    client.session_new().await?;
    let tree = client.send("browsingContext.getTree", json!({})).await?;
    let ctx = tree["contexts"][0]["context"]
        .as_str()
        .ok_or_else(|| anyhow!("no top-level browsing context"))?
        .to_string();
    let arc = std::sync::Arc::new(client);
    *guard = Some((arc.clone(), ctx.clone()));
    Ok((arc, ctx))
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
                match state.browser.engine {
                    Engine::Cdp => {
                        let (client, session_id) =
                            cdp_attach_first_page(&state.browser.endpoint).await?;
                        client
                            .send_with_session(
                                "Page.navigate",
                                json!({ "url": url }),
                                Some(&session_id),
                            )
                            .await?;
                        client.close().await;
                    }
                    Engine::Bidi => {
                        let (client, ctx) = bidi_top_context(&state).await?;
                        client.browsing_context_navigate(&ctx, &url).await?;
                    }
                }
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
                let html = match state.browser.engine {
                    Engine::Cdp => {
                        let (client, session_id) =
                            cdp_attach_first_page(&state.browser.endpoint).await?;
                        let v = client
                            .send_with_session(
                                "Runtime.evaluate",
                                json!({
                                    "expression": expr,
                                    "returnByValue": true,
                                    "awaitPromise": false,
                                }),
                                Some(&session_id),
                            )
                            .await?;
                        client.close().await;
                        v["result"]["value"].as_str().unwrap_or("").to_string()
                    }
                    Engine::Bidi => {
                        let (client, ctx) = bidi_top_context(&state).await?;
                        let v = client.script_evaluate(&ctx, &expr).await?;
                        v["result"]["value"].as_str().unwrap_or("").to_string()
                    }
                };
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
                let b64 = match state.browser.engine {
                    Engine::Cdp => {
                        let (client, session_id) =
                            cdp_attach_first_page(&state.browser.endpoint).await?;
                        let v = client
                            .send_with_session(
                                "Page.captureScreenshot",
                                json!({
                                    "format": "png",
                                    "captureBeyondViewport": full_page,
                                }),
                                Some(&session_id),
                            )
                            .await?;
                        client.close().await;
                        v["data"]
                            .as_str()
                            .ok_or_else(|| anyhow!("no screenshot data"))?
                            .to_string()
                    }
                    Engine::Bidi => {
                        let (client, ctx) = bidi_top_context(&state).await?;
                        let data = client.browsing_context_capture_screenshot(&ctx).await?;
                        data
                    }
                };
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
                let raw = match state.browser.engine {
                    Engine::Cdp => {
                        let (client, session_id) =
                            cdp_attach_first_page(&state.browser.endpoint).await?;
                        let v = client
                            .send_with_session(
                                "Runtime.evaluate",
                                json!({
                                    "expression": expr,
                                    "returnByValue": true,
                                    "awaitPromise": true,
                                }),
                                Some(&session_id),
                            )
                            .await?;
                        client.close().await;
                        v["result"]["value"].as_str().unwrap_or("").to_string()
                    }
                    Engine::Bidi => {
                        let (client, ctx) = bidi_top_context(&state).await?;
                        let v = client.script_evaluate(&ctx, &expr).await?;
                        v["result"]["value"].as_str().unwrap_or("").to_string()
                    }
                };
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
                let selector = match state.browser.engine {
                    Engine::Cdp => {
                        let (client, session_id) =
                            cdp_attach_first_page(&state.browser.endpoint).await?;
                        let v = client
                            .send_with_session(
                                "Runtime.evaluate",
                                json!({
                                    "expression": expr,
                                    "returnByValue": true,
                                    "awaitPromise": true,
                                }),
                                Some(&session_id),
                            )
                            .await?;
                        client.close().await;
                        v["result"]["value"].as_str().unwrap_or("").to_string()
                    }
                    Engine::Bidi => {
                        let (client, ctx) = bidi_top_context(&state).await?;
                        let v = client.script_evaluate(&ctx, &expr).await?;
                        v["result"]["value"].as_str().unwrap_or("").to_string()
                    }
                };
                Ok(text_content(selector))
            })
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_all_adds_five_tools() {
        let registry = ToolRegistry::new();
        register_all(&registry);
        let list = registry.list();
        assert_eq!(list.len(), 5);
        let names: Vec<&str> = list.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in &[
            "navigate",
            "get_dom",
            "screenshot",
            "fetch",
            "select_element",
        ] {
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
}
