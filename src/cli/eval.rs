//! `browser-control eval` — evaluate a JS expression in a page.
//!
//! Three routing paths, picked by what the caller supplies:
//!
//! 1. **`eval <browser>/<tab>`** — named-tab path. Resolves `<tab>` in the
//!    `tabs` SQLite table; if missing, returns `TabNotFound` so the agent
//!    can `tab open <browser>/<tab>` first. Mutually exclusive with
//!    `--target`.
//! 2. **`eval <browser>` with no `--target`** — routes through the
//!    daemon-style scratch tab with recover-once. The default, and the
//!    architectural fix for the iLO failure mode: lock-free `eval`
//!    never silently lands on a user's admin tab.
//! 3. **`eval <browser> --target <regex>`** — explicit selector against
//!    a user tab matching the URL regex. The original behaviour, kept
//!    for ad-hoc targeted use.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::cdp::CdpClient;
use crate::cli::env_resolver::{self, Source};
use crate::cli::mcp::{acquire_bidi_lock_if_needed, resolve_browser};
use crate::detect::Engine;
use crate::errors::SessionError;
use crate::registry::Registry;
use crate::session::{tabs as session_tabs, with_scratch_recovery, PageSession};

pub async fn run(
    browser: Option<String>,
    expression: String,
    target: Option<String>,
    json: bool,
    await_promise: bool,
    timeout_ms: u64,
) -> Result<()> {
    let raw = browser.unwrap_or_default();
    let parsed = if raw.is_empty() {
        None
    } else {
        Some(env_resolver::parse_target(&raw)?)
    };
    let tab_name = parsed.as_ref().and_then(|p| p.tab.clone());
    if tab_name.is_some() && target.is_some() {
        bail!("specify the tab via either `<browser>/<name>` or `--target <regex>`, not both");
    }
    let browser_only = parsed
        .as_ref()
        .map(|p| strip_tab(&raw, p.tab.as_deref()))
        .unwrap_or_default();
    let resolved = resolve_browser(if browser_only.is_empty() {
        None
    } else {
        Some(browser_only.clone())
    })
    .await?;
    let timeout = Duration::from_millis(timeout_ms);

    // Open the registry once for the function lifetime — used for the
    // BiDi single-session lock, scratch row, and named-tab resolution.
    // Re-opening would block on the per-process file lock.
    let registry = Registry::open()?;
    // Acquire the Firefox BiDi lock if applicable. RAII releases on
    // function exit; held across whatever path we take below.
    let _bidi_lock = acquire_bidi_lock_if_needed(&registry, &resolved)?;

    let value = match (tab_name, target) {
        // Path 1: <browser>/<tab> — named tab via SQLite resolver.
        (Some(name), None) => {
            let browser_name = match &resolved.source {
                Source::Registered { name } => name.clone(),
                _ => bail!(
                    "named tabs (`<browser>/<name>`) require a registered browser; \
                     external endpoints can't carry tab names"
                ),
            };
            // Named-tab routing is CDP-only in this PR. BiDi callers that
            // want a named tab should use the legacy `--target` regex for
            // now; a BiDi-flavoured equivalent (resolving via
            // `browsingContext`) is a future addition.
            if resolved.engine == Engine::Bidi {
                bail!(
                    "named tabs are not yet supported for BiDi (Firefox); \
                     use `--target <url-regex>` to select a context"
                );
            }
            let client = Arc::new(open_cdp(&resolved.endpoint).await?);
            let row = session_tabs::resolve_tab(&client, &registry, &browser_name, &name)
                .await?
                .ok_or_else(|| {
                    anyhow!(
                        "TabNotFound: no live tab `{name}` for `{browser_name}` \
                         — run `browser-control tab open {browser_name}/{name}` first"
                    )
                })?;
            let value =
                eval_in_target(&client, &row.target_id, &expression, await_promise, timeout).await;
            drop(client);
            value?
        }
        // Path 2: bare browser, no `--target` → scratch (CDP) or legacy
        // (BiDi / external endpoints).
        (None, None) => {
            // BiDi or external: scratch tab plumbing is CDP-only. Fall
            // back to the existing direct attach.
            if resolved.engine == Engine::Bidi || matches!(resolved.source, Source::External) {
                let session =
                    PageSession::attach(&resolved.endpoint, resolved.engine, None).await?;
                let value = session
                    .evaluate_with_timeout(&expression, await_promise, Some(timeout))
                    .await;
                session.close().await;
                value?
            } else {
                // CDP registered: scratch tab with recover-once.
                let browser_name = match &resolved.source {
                    Source::Registered { name } => name.clone(),
                    _ => unreachable!("Source::External branch handled above"),
                };
                let client = Arc::new(open_cdp(&resolved.endpoint).await?);
                let expr = expression.clone();
                let value = with_scratch_recovery(
                    client.clone(),
                    &registry,
                    &browser_name,
                    move |c, session_id, _target_id| {
                        let expr = expr.clone();
                        async move {
                            let inner = c.send_with_session(
                                "Runtime.evaluate",
                                json!({
                                    "expression": expr,
                                    "returnByValue": true,
                                    "awaitPromise": await_promise,
                                }),
                                Some(&session_id),
                            );
                            match tokio::time::timeout(timeout, inner).await {
                                Ok(Ok(v)) => Ok(v["result"]["value"].clone()),
                                Ok(Err(e)) => Err(e),
                                Err(_) => Err(SessionError::TabHung {
                                    target_id: None,
                                    url: None,
                                    timeout_ms: timeout.as_millis() as u64,
                                    hint: "op-timeout",
                                }
                                .into()),
                            }
                        }
                    },
                )
                .await;
                drop(client);
                value?
            }
        }
        // Path 3: bare browser, --target regex → legacy selector.
        (None, Some(regex)) => {
            let session =
                PageSession::attach(&resolved.endpoint, resolved.engine, Some(&regex)).await?;
            let value = session
                .evaluate_with_timeout(&expression, await_promise, Some(timeout))
                .await;
            session.close().await;
            value?
        }
        _ => unreachable!("mutex was checked above"),
    };

    println!("{}", format_output(&value, json));
    Ok(())
}

/// Run `Runtime.evaluate` against a known `target_id` (after attaching a
/// transient session). Used by the `<browser>/<tab>` path.
async fn eval_in_target(
    client: &CdpClient,
    target_id: &str,
    expression: &str,
    await_promise: bool,
    timeout: Duration,
) -> Result<Value> {
    let attach = client
        .send(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await?;
    let session_id = attach
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Target.attachToTarget returned no sessionId"))?
        .to_string();
    let inner = client.send_with_session(
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
    let _ = client
        .send(
            "Target.detachFromTarget",
            json!({ "sessionId": session_id }),
        )
        .await;
    value
}

async fn open_cdp(endpoint: &str) -> Result<CdpClient> {
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        CdpClient::connect(endpoint).await
    } else {
        CdpClient::connect_http(endpoint).await
    }
}

/// Strip `/<tab>` suffix from a raw `<browser>[/<tab>]` positional.
fn strip_tab(raw: &str, tab: Option<&str>) -> String {
    match tab {
        Some(name) => {
            // The tab is everything after the first `/`. Slice off
            // `/<name>` to recover the original browser part. We don't
            // re-parse to avoid the cost.
            let suffix = format!("/{name}");
            raw.strip_suffix(&suffix).unwrap_or(raw).to_string()
        }
        None => raw.to_string(),
    }
}

fn format_output(v: &Value, json: bool) -> String {
    if json {
        serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
    } else if let Some(s) = v.as_str() {
        s.to_string()
    } else {
        serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn eval_returns_string_unquoted_in_text_mode() {
        let v = json!("hello");
        assert_eq!(format_output(&v, false), "hello");
    }

    #[test]
    fn eval_returns_number_as_json() {
        let v = json!(42);
        assert_eq!(format_output(&v, false), "42");
    }

    #[test]
    fn eval_returns_json_envelope_when_json_flag() {
        let v = json!({"a": 1});
        let out = format_output(&v, true);
        let parsed: Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed, v);
        assert!(out.contains("\"a\""));
    }

    #[test]
    fn eval_null_text_mode() {
        let v = json!(null);
        assert_eq!(format_output(&v, false), "null");
    }

    #[test]
    fn eval_bool_text_mode() {
        assert_eq!(format_output(&json!(true), false), "true");
    }

    // Mock CDP round-trip test mirroring src/session/attach.rs tests.
    use crate::detect::Engine;
    use crate::session::PageSession;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    async fn spawn_cdp_mock(targets: Vec<Value>, eval_value: Value) -> String {
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
                    "Runtime.evaluate" => json!({"result": {"value": eval_value.clone()}}),
                    _ => json!({}),
                };
                let resp = json!({"id": id, "result": result});
                ws.send(Message::Text(resp.to_string())).await.unwrap();
            }
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn eval_mock_returns_string() {
        let url = spawn_cdp_mock(
            vec![json!({"targetId":"a","type":"page","url":"https://example.com/"})],
            json!("hello"),
        )
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, None).await.unwrap();
        let v = s.evaluate("'hello'", false).await.unwrap();
        s.close().await;
        assert_eq!(format_output(&v, false), "hello");
    }

    #[tokio::test]
    async fn eval_mock_returns_object_json() {
        let url = spawn_cdp_mock(
            vec![json!({"targetId":"a","type":"page","url":"https://example.com/"})],
            json!({"a": 1}),
        )
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, None).await.unwrap();
        let v = s.evaluate("({a:1})", false).await.unwrap();
        s.close().await;
        let out = format_output(&v, true);
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, json!({"a": 1}));
    }
}
