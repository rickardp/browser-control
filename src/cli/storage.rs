//! `browser-control storage` — local/sessionStorage get/set/list.
//!
//! Routing mirrors `eval`/`fetch`. The `--browser` flag accepts the
//! unified `<browser>[/<tab>]` syntax; three paths fall out of `(tab, --target)`:
//!
//! 1. `<browser>/<tab>` (no `--target`) — named-tab path via
//!    [`with_named_tab_recovery`]. Engine-agnostic, recover-once on dead tabs.
//! 2. `<browser>` (no `--target`) — scratch tab via [`with_scratch_recovery`].
//!    Storage `Set` against a scratch (`about:blank`) writes to that origin —
//!    symmetric with `eval`, where the same caveat applies.
//! 3. `<browser> --target <regex>` — legacy `PageSession::attach`.
//!
//! `<browser>/<tab>` and `--target` are mutually exclusive.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use serde_json::Value;

use crate::cli::env_resolver::{self, Source};
use crate::cli::mcp::{acquire_bidi_lock_if_needed, resolve_browser};
use crate::cli::trace::CommandTrace;
use crate::registry::Registry;
use crate::session::backend::open_backend;
use crate::session::{with_named_tab_recovery, with_scratch_recovery, PageSession};

/// Per-call timeout for storage probes. The expressions injected here are
/// trivial (`localStorage.getItem`, `JSON.stringify(Object.entries(...))`,
/// etc.); 10 s is generous and bounds wedged renderers.
const STORAGE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Subcommand, Debug)]
pub enum StorageCmd {
    /// Read a single storage entry. With --key-regex, returns the first match.
    Get {
        #[arg(long, short = 'b', env = "BROWSER_CONTROL")]
        browser: Option<String>,
        key: Option<String>,
        #[arg(long)]
        key_regex: Option<String>,
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value = "local")]
        namespace: String,
        #[arg(long)]
        json: bool,
    },
    /// Write a single storage entry.
    Set {
        #[arg(long, short = 'b', env = "BROWSER_CONTROL")]
        browser: Option<String>,
        key: String,
        value: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value = "local")]
        namespace: String,
    },
    /// List all storage entries (optionally filtered by key regex).
    List {
        #[arg(long, short = 'b', env = "BROWSER_CONTROL")]
        browser: Option<String>,
        #[arg(long)]
        key_regex: Option<String>,
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value = "local")]
        namespace: String,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(cmd: StorageCmd) -> Result<()> {
    match cmd {
        StorageCmd::Get {
            browser,
            key,
            key_regex,
            target,
            namespace,
            json,
        } => {
            let mut trace = CommandTrace::new("storage-get");
            match run_get(browser, key, key_regex, target, namespace, json, &mut trace).await {
                Ok(()) => {
                    trace.ok(());
                    Ok(())
                }
                Err(e) => Err(trace.err(e)),
            }
        }
        StorageCmd::Set {
            browser,
            key,
            value,
            target,
            namespace,
        } => {
            let mut trace = CommandTrace::new("storage-set");
            match run_set(browser, key, value, target, namespace, &mut trace).await {
                Ok(()) => {
                    trace.ok(());
                    Ok(())
                }
                Err(e) => Err(trace.err(e)),
            }
        }
        StorageCmd::List {
            browser,
            key_regex,
            target,
            namespace,
            json,
        } => {
            let mut trace = CommandTrace::new("storage-list");
            match run_list(browser, key_regex, target, namespace, json, &mut trace).await {
                Ok(()) => {
                    trace.ok(());
                    Ok(())
                }
                Err(e) => Err(trace.err(e)),
            }
        }
    }
}

/// Run a JS expression against the resolved browser using the same
/// three-path routing (`named-tab` / `scratch` / `target-regex` / `direct`)
/// that `eval` uses. Returns the evaluated value.
///
/// Routing decision rules:
/// - `<browser>/<tab>` + no `--target` → named-tab path.
/// - bare `<browser>` + no `--target` → scratch path (or `direct` for
///   external URL endpoints — there's no registry row to key a scratch by).
/// - `<browser>` + `--target <regex>` → legacy attach.
/// - Both `<browser>/<tab>` and `--target` → error.
async fn evaluate_routed(
    browser: Option<String>,
    target: Option<String>,
    expr: &str,
    trace: &mut CommandTrace,
) -> Result<Value> {
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

    let registry = Registry::open()?;
    let _bidi_lock = acquire_bidi_lock_if_needed(&registry, &resolved)?;

    match (tab_name, target) {
        // Path 1: <browser>/<tab> — named tab with recover-once.
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
            let expr = expr.to_string();
            with_named_tab_recovery(
                &backend,
                &registry,
                &browser_name,
                &name,
                move |b, target_id| {
                    let expr = expr.clone();
                    async move { b.evaluate(&target_id, &expr, true, STORAGE_TIMEOUT).await }
                },
            )
            .await
        }
        // Path 2: bare browser, no `--target` → scratch tab with recover-once.
        (None, None) => {
            if matches!(resolved.source, Source::External) {
                // External URL endpoint — no registry row to key a scratch
                // by. Fall back to the legacy direct attach so external
                // endpoints still work.
                trace.route("direct");
                let session =
                    PageSession::attach(&resolved.endpoint, resolved.engine, None).await?;
                let value = session
                    .evaluate_with_timeout(expr, true, Some(STORAGE_TIMEOUT))
                    .await;
                session.close().await;
                value
            } else {
                trace.route("scratch");
                let browser_name = match &resolved.source {
                    Source::Registered { name } => name.clone(),
                    _ => unreachable!("Source::External branch handled above"),
                };
                let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
                let expr = expr.to_string();
                with_scratch_recovery(&backend, &registry, &browser_name, move |b, target_id| {
                    let expr = expr.clone();
                    async move { b.evaluate(&target_id, &expr, true, STORAGE_TIMEOUT).await }
                })
                .await
            }
        }
        // Path 3: bare browser, --target regex → legacy selector.
        (None, Some(regex)) => {
            trace.route("target-regex");
            let session =
                PageSession::attach(&resolved.endpoint, resolved.engine, Some(&regex)).await?;
            let value = session
                .evaluate_with_timeout(expr, true, Some(STORAGE_TIMEOUT))
                .await;
            session.close().await;
            value
        }
        _ => unreachable!("mutex was checked above"),
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

async fn run_get(
    browser: Option<String>,
    key: Option<String>,
    key_regex: Option<String>,
    target: Option<String>,
    namespace: String,
    json: bool,
    trace: &mut CommandTrace,
) -> Result<()> {
    let ns = ns_global(&namespace)?;
    match (key.as_deref(), key_regex.as_deref()) {
        (Some(_), Some(_)) => bail!("specify either KEY or --key-regex, not both"),
        (None, None) => bail!("specify a KEY or --key-regex"),
        (Some(k), None) => {
            let expr = build_get_expr(ns, k);
            let value = evaluate_routed(browser, target, &expr, trace).await?;
            if value.is_null() {
                bail!("key not found: {k}");
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else if let Some(s) = value.as_str() {
                println!("{s}");
            } else {
                println!("{value}");
            }
            Ok(())
        }
        (None, Some(pat)) => {
            let expr = build_get_by_regex_expr(ns, pat);
            let value = evaluate_routed(browser, target, &expr, trace).await?;
            if value.is_null() {
                bail!("no key matches regex");
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                let v = value.get("value").unwrap_or(&Value::Null);
                if let Some(s) = v.as_str() {
                    println!("{s}");
                } else {
                    println!("{v}");
                }
            }
            Ok(())
        }
    }
}

async fn run_set(
    browser: Option<String>,
    key: String,
    value: String,
    target: Option<String>,
    namespace: String,
    trace: &mut CommandTrace,
) -> Result<()> {
    let ns = ns_global(&namespace)?;
    let expr = build_set_expr(ns, &key, &value);
    evaluate_routed(browser, target, &expr, trace).await?;
    Ok(())
}

async fn run_list(
    browser: Option<String>,
    key_regex: Option<String>,
    target: Option<String>,
    namespace: String,
    json: bool,
    trace: &mut CommandTrace,
) -> Result<()> {
    let ns = ns_global(&namespace)?;
    let expr = build_list_expr(ns, key_regex.as_deref());
    let value = evaluate_routed(browser, target, &expr, trace).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let arr = value.as_array().ok_or_else(|| anyhow!("expected array"))?;
    for entry in arr {
        let k = entry.get("key").and_then(|v| v.as_str()).unwrap_or("");
        let v_val = entry.get("value").unwrap_or(&Value::Null);
        let v_str = match v_val {
            Value::String(s) => {
                if s.contains('\t') || s.contains('\n') || s.contains('\r') {
                    serde_json::to_string(s)?
                } else {
                    s.clone()
                }
            }
            other => serde_json::to_string(other)?,
        };
        println!("{k}\t{v_str}");
    }
    Ok(())
}

pub(crate) fn ns_global(namespace: &str) -> Result<&'static str> {
    match namespace {
        "local" => Ok("localStorage"),
        "session" => Ok("sessionStorage"),
        other => bail!("invalid namespace `{other}`: expected `local` or `session`"),
    }
}

pub(crate) fn build_get_expr(namespace_js: &str, key: &str) -> String {
    let key_lit = serde_json::to_string(key).expect("string serialization is infallible");
    format!("JSON.stringify({namespace_js}.getItem({key_lit}))")
}

fn build_get_by_regex_expr(namespace_js: &str, pattern: &str) -> String {
    let pat_lit = serde_json::to_string(pattern).expect("string serialization is infallible");
    format!(
        "(() => {{ \
const re = new RegExp({pat_lit}); \
const k = Object.keys({namespace_js}).find(k => re.test(k)); \
return k ? {{key: k, value: {namespace_js}.getItem(k)}} : null; \
}})()"
    )
}

pub(crate) fn build_set_expr(namespace_js: &str, key: &str, value: &str) -> String {
    let key_lit = serde_json::to_string(key).expect("string serialization is infallible");
    let val_lit = serde_json::to_string(value).expect("string serialization is infallible");
    format!("{namespace_js}.setItem({key_lit}, {val_lit})")
}

fn build_list_expr(namespace_js: &str, pattern: Option<&str>) -> String {
    let re_expr = match pattern {
        Some(p) => {
            let pat_lit = serde_json::to_string(p).expect("string serialization is infallible");
            format!("new RegExp({pat_lit})")
        }
        None => "null".to_string(),
    };
    format!(
        "(() => {{ \
const ns = {namespace_js}; \
const re = {re_expr}; \
const out = []; \
for (let i = 0; i < ns.length; i++) {{ \
const k = ns.key(i); \
if (!re || re.test(k)) out.push({{key: k, value: ns.getItem(k)}}); \
}} \
return out; \
}})()"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ns_global_maps_known() {
        assert_eq!(ns_global("local").unwrap(), "localStorage");
        assert_eq!(ns_global("session").unwrap(), "sessionStorage");
    }

    #[test]
    fn ns_global_rejects_unknown() {
        let err = ns_global("cookies").unwrap_err().to_string();
        assert!(err.contains("invalid namespace"), "got: {err}");
        assert!(err.contains("cookies"));
    }

    #[test]
    fn build_get_expr_escapes_single_quote() {
        let expr = build_get_expr("localStorage", "it's");
        assert_eq!(expr, "JSON.stringify(localStorage.getItem(\"it's\"))");
    }

    #[test]
    fn build_get_expr_escapes_quote_and_backslash() {
        let expr = build_get_expr("sessionStorage", "a\"b\\c");
        assert_eq!(
            expr,
            "JSON.stringify(sessionStorage.getItem(\"a\\\"b\\\\c\"))"
        );
    }

    #[test]
    fn build_set_expr_escapes_both() {
        let expr = build_set_expr("localStorage", "k\"1", "v\\n");
        assert_eq!(expr, "localStorage.setItem(\"k\\\"1\", \"v\\\\n\")");
    }

    #[test]
    fn build_get_by_regex_expr_escapes_quotes() {
        let expr = build_get_by_regex_expr("localStorage", "^foo\".*$");
        assert!(
            expr.contains("new RegExp(\"^foo\\\".*$\")"),
            "expr was: {expr}"
        );
        assert!(expr.contains("Object.keys(localStorage)"));
    }

    #[test]
    fn build_list_expr_none_uses_null_regex() {
        let expr = build_list_expr("localStorage", None);
        assert!(expr.contains("const re = null;"), "expr: {expr}");
        assert!(expr.contains("const ns = localStorage;"));
    }

    #[test]
    fn build_list_expr_some_escapes_pattern() {
        let expr = build_list_expr("sessionStorage", Some("a\"b"));
        assert!(expr.contains("new RegExp(\"a\\\"b\")"), "expr: {expr}");
        assert!(expr.contains("const ns = sessionStorage;"));
    }

    #[test]
    fn strip_tab_removes_suffix_when_present() {
        assert_eq!(strip_tab("brave/cart", Some("cart")), "brave");
        assert_eq!(strip_tab("brave", None), "brave");
        // If `tab` is `Some` but does not in fact match the suffix, the
        // original raw is returned unchanged (defensive — shouldn't happen
        // in practice because the caller derives `tab` from the same raw).
        assert_eq!(strip_tab("brave/cart", Some("other")), "brave/cart");
    }

    #[tokio::test]
    async fn evaluate_routed_rejects_tab_and_target_together() {
        let mut trace = CommandTrace::new("storage-get");
        let err = evaluate_routed(
            Some("brave/cart".to_string()),
            Some(".*".to_string()),
            "1",
            &mut trace,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("either"),
            "unexpected error: {err}"
        );
    }
}
