//! `browser-control eval` — evaluate a JS expression in a page.
//!
//! Three routing paths, picked by what the caller supplies:
//!
//! 1. **`eval <browser>/<tab>`** — named-tab path. Resolves `<tab>` in the
//!    `tabs` SQLite table via the engine-agnostic [`TabBackend`]; if
//!    missing, returns `TabNotFound` so the agent can
//!    `tab open <browser>/<tab>` first. Mutually exclusive with
//!    `--target`. Works for both CDP and BiDi browsers.
//! 2. **`eval <browser>` with no `--target`** — routes through the
//!    daemon-style scratch tab with recover-once. The default, and the
//!    architectural fix for the iLO failure mode: lock-free `eval`
//!    never silently lands on a user's admin tab. Engine-agnostic.
//! 3. **`eval <browser> --target <regex>`** — explicit selector against
//!    a user tab matching the URL regex. The original behaviour, kept
//!    for ad-hoc targeted use.
//!
//! All three paths share the BiDi single-session lock when the resolved
//! browser is on the BiDi engine.

use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::cli::env_resolver::{self, Source};
use crate::cli::mcp::{acquire_bidi_lock_if_needed, resolve_browser};
use crate::cli::trace::CommandTrace;
use crate::registry::Registry;
use crate::session::backend::open_backend;
use crate::session::{with_named_tab_recovery, with_scratch_recovery, PageSession};

pub async fn run(
    browser: Option<String>,
    expression: String,
    target: Option<String>,
    json: bool,
    await_promise: bool,
    timeout_ms: u64,
) -> Result<()> {
    let mut trace = CommandTrace::new("eval");
    match run_inner(
        browser,
        expression,
        target,
        json,
        await_promise,
        timeout_ms,
        &mut trace,
    )
    .await
    {
        Ok(()) => {
            trace.ok(());
            Ok(())
        }
        Err(e) => Err(trace.err(e)),
    }
}

async fn run_inner(
    browser: Option<String>,
    expression: String,
    target: Option<String>,
    json: bool,
    await_promise: bool,
    timeout_ms: u64,
    trace: &mut CommandTrace,
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
    trace.browser(&browser_only).engine(resolved.engine);
    let timeout = Duration::from_millis(timeout_ms);

    // Open the registry once for the function lifetime — used for the
    // BiDi single-session lock, scratch row, and named-tab resolution.
    // Re-opening would block on the per-process file lock.
    let registry = Registry::open()?;
    // Acquire the Firefox BiDi lock if applicable. RAII releases on
    // function exit; held across whatever path we take below.
    let _bidi_lock = acquire_bidi_lock_if_needed(&registry, &resolved)?;

    let value = match (tab_name, target) {
        // Path 1: <browser>/<tab> — named tab, engine-agnostic, with
        // recover-once around a tab that dies between resolve and op.
        (Some(name), None) => {
            trace.route("named-tab").tab_name(&name);
            let browser_name = match &resolved.source {
                Source::Registered { name } => name.clone(),
                _ => bail!(
                    "named tabs (`<browser>/<name>`) require a registered browser; \
                     external endpoints can't carry tab names"
                ),
            };
            let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
            let expr = expression.clone();
            with_named_tab_recovery(
                &backend,
                &registry,
                &browser_name,
                &name,
                move |b, target_id| {
                    let expr = expr.clone();
                    async move { b.evaluate(&target_id, &expr, await_promise, timeout).await }
                },
            )
            .await?
        }
        // Path 2: bare browser, no `--target` → scratch tab with
        // recover-once. Engine-agnostic via TabBackend.
        (None, None) => {
            if matches!(resolved.source, Source::External) {
                // External URL endpoints don't have a registered name to
                // key the scratch row by. Fall back to the legacy direct
                // attach so `eval ws://...` still works.
                trace.route("direct");
                let session =
                    PageSession::attach(&resolved.endpoint, resolved.engine, None).await?;
                let value = session
                    .evaluate_with_timeout(&expression, await_promise, Some(timeout))
                    .await;
                session.close().await;
                value?
            } else {
                trace.route("scratch");
                let browser_name = match &resolved.source {
                    Source::Registered { name } => name.clone(),
                    _ => unreachable!("Source::External branch handled above"),
                };
                let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
                let expr = expression.clone();
                with_scratch_recovery(&backend, &registry, &browser_name, move |b, target_id| {
                    let expr = expr.clone();
                    async move { b.evaluate(&target_id, &expr, await_promise, timeout).await }
                })
                .await?
            }
        }
        // Path 3: bare browser, --target regex → legacy selector.
        (None, Some(regex)) => {
            trace.route("target-regex");
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

fn format_output(v: &Value, json: bool) -> String {
    if json {
        serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
    } else if let Some(s) = v.as_str() {
        s.to_string()
    } else {
        serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
    }
}

/// Strip `/<tab>` suffix from a raw `<browser>[/<tab>]` positional.
fn strip_tab(raw: &str, tab: Option<&str>) -> String {
    match tab {
        Some(name) => raw
            .strip_suffix(&format!("/{name}"))
            .unwrap_or(raw)
            .to_string(),
        None => raw.to_string(),
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
