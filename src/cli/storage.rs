//! `browser-control storage` — local/sessionStorage get/set/list.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use serde_json::Value;

use crate::cli::mcp::{acquire_bidi_lock_if_needed, resolve_browser};
use crate::registry::Registry;
use crate::session::PageSession;

/// Per-call timeout for storage probes. The expressions injected here are
/// trivial (`localStorage.getItem`, `JSON.stringify(Object.entries(...))`,
/// etc.); 10 s is generous and bounds wedged renderers.
const STORAGE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Subcommand, Debug)]
pub enum StorageCmd {
    /// Read a single storage entry. With --key-regex, returns the first match.
    Get {
        #[arg(env = "BROWSER_CONTROL")]
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
        #[arg(env = "BROWSER_CONTROL")]
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
        #[arg(env = "BROWSER_CONTROL")]
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
    let (sub, result) = match cmd {
        StorageCmd::Get {
            browser,
            key,
            key_regex,
            target,
            namespace,
            json,
        } => (
            "storage-get",
            run_get(browser, key, key_regex, target, namespace, json).await,
        ),
        StorageCmd::Set {
            browser,
            key,
            value,
            target,
            namespace,
        } => (
            "storage-set",
            run_set(browser, key, value, target, namespace).await,
        ),
        StorageCmd::List {
            browser,
            key_regex,
            target,
            namespace,
            json,
        } => (
            "storage-list",
            run_list(browser, key_regex, target, namespace, json).await,
        ),
    };
    let mut trace = crate::cli::trace::CommandTrace::new(sub);
    trace.route("registry");
    match result {
        Ok(()) => {
            trace.ok(());
            Ok(())
        }
        Err(e) => Err(trace.err(e)),
    }
}

async fn evaluate_in_target(
    browser: Option<String>,
    target: Option<String>,
    expr: &str,
) -> Result<Value> {
    let resolved = resolve_browser(browser).await?;
    // Acquire the Firefox BiDi single-session lock if this is a BiDi
    // browser; held for the storage op. No-op on CDP.
    let _bidi_lock = {
        let registry = Registry::open()?;
        acquire_bidi_lock_if_needed(&registry, &resolved)?
    };
    let session =
        PageSession::attach(&resolved.endpoint, resolved.engine, target.as_deref()).await?;
    let value = session
        .evaluate_with_timeout(expr, true, Some(STORAGE_TIMEOUT))
        .await;
    session.close().await;
    value
}

async fn run_get(
    browser: Option<String>,
    key: Option<String>,
    key_regex: Option<String>,
    target: Option<String>,
    namespace: String,
    json: bool,
) -> Result<()> {
    let ns = ns_global(&namespace)?;
    match (key.as_deref(), key_regex.as_deref()) {
        (Some(_), Some(_)) => bail!("specify either KEY or --key-regex, not both"),
        (None, None) => bail!("specify a KEY or --key-regex"),
        (Some(k), None) => {
            let expr = build_get_expr(ns, k);
            let value = evaluate_in_target(browser, target, &expr).await?;
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
            let value = evaluate_in_target(browser, target, &expr).await?;
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
) -> Result<()> {
    let ns = ns_global(&namespace)?;
    let expr = build_set_expr(ns, &key, &value);
    evaluate_in_target(browser, target, &expr).await?;
    Ok(())
}

async fn run_list(
    browser: Option<String>,
    key_regex: Option<String>,
    target: Option<String>,
    namespace: String,
    json: bool,
) -> Result<()> {
    let ns = ns_global(&namespace)?;
    let expr = build_list_expr(ns, key_regex.as_deref());
    let value = evaluate_in_target(browser, target, &expr).await?;
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
}
