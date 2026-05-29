//! `browser-control fetch` — run an HTTP request from the page's context.
//!
//! The request is executed by injecting [`crate::dom::scripts::FETCH_JS`] into
//! the active page via the engine-agnostic [`PageSession`] and parsing the
//! `{status, statusText, headers, body}` envelope it returns.
//!
//! Output mirrors `curl`:
//! - `--include` prepends `HTTP/1.1 <code> <text>\r\n` and response headers.
//! - `--output PATH` writes the body to PATH (and `chmod 0600` on Unix).
//! - Without `--output`, the body is written to stdout.
//!
//! Transport errors (script failure, attach failure) exit non-zero; HTTP
//! status is reported verbatim and does not change the exit code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::cli::route;
use crate::cli::trace::CommandTrace;
use crate::dom::scripts::FETCH_JS;
use crate::session::{evaluate_for_origin_with_recover_once, PageSession};

// The default fetch timeout (60 000 ms) is set on the CLI flag in
// `main.rs`. Bounded so a wedged renderer fails fast instead of dragging
// out the upstream protocol timeout; tunable per invocation via
// `--timeout-ms` for slow-network workloads.

#[allow(clippy::too_many_arguments)]
pub async fn run(
    browser: Option<String>,
    url: String,
    method: String,
    headers: Vec<String>,
    data: Option<String>,
    target: Option<String>,
    include: bool,
    output: Option<PathBuf>,
    timeout_ms: u64,
) -> Result<()> {
    let mut trace = CommandTrace::new("fetch");
    let result = run_inner(
        browser, url, method, headers, data, target, include, output, timeout_ms, &mut trace,
    )
    .await;
    trace.finish(result)
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    browser: Option<String>,
    url: String,
    method: String,
    headers: Vec<String>,
    data: Option<String>,
    target: Option<String>,
    include: bool,
    output: Option<PathBuf>,
    timeout_ms: u64,
    trace: &mut CommandTrace,
) -> Result<()> {
    let header_map = parse_headers(&headers)?;
    let expr = build_fetch_expr(&url, &method, &header_map, data.as_deref())?;
    let fetch_timeout = Duration::from_millis(timeout_ms);

    // Path-syntax parsing: `<browser>` or `<browser>/<tab>` in the
    // positional. `--target <regex>` and `/<tab>` are mutually exclusive.
    // The preamble (parse, mutual-exclusion, resolve, registry, BiDi lock) is
    // shared with `eval`/`storage`; see `crate::cli::route`.
    let r = route::preamble(browser, target.as_deref(), trace).await?;
    let resolved = &r.resolved;

    let result = match (r.tab_name.clone(), target.as_deref()) {
        // Path 1: <browser>/<tab> — fetch against the named tab.
        // Engine-agnostic via TabBackend + recover-once on dead tabs.
        (Some(name), None) => {
            trace.route("named-tab").tab_name(&name);
            let expr = expr.clone();
            route::run_named_tab(
                &r,
                &name,
                "named tabs (`<browser>/<name>`) require a registered browser",
                move |b, target_id| {
                    let expr = expr.clone();
                    async move { b.evaluate(&target_id, &expr, true, fetch_timeout).await }
                },
            )
            .await?
        }
        // Path 2: --target regex (legacy).
        (None, Some(regex)) => {
            trace.route("target-regex");
            let session =
                PageSession::attach(&resolved.endpoint, resolved.engine, Some(regex)).await?;
            let value = session
                .evaluate_with_timeout(&expr, true, Some(fetch_timeout))
                .await;
            session.close().await;
            value?
        }
        // Path 3: bare browser → attach_for_origin (current default).
        // Auth-inheritance: the request runs from the URL's origin, so
        // cookies/credentials propagate. Scratch routing (which would use
        // about:blank) would lose this; we deliberately keep
        // attach_for_origin here instead.
        //
        // Recover-once lives in the shared session helper so CLI fetch and
        // wait-for-cookie validation cannot drift on the origin-bound contract.
        (None, None) => {
            trace.route("attach-for-origin");
            evaluate_for_origin_with_recover_once(
                &resolved.endpoint,
                resolved.engine,
                &url,
                &expr,
                true,
                fetch_timeout,
            )
            .await?
        }
        _ => unreachable!("mutex was checked above"),
    };

    let envelope = parse_envelope(&result)?;

    let mut bytes = Vec::new();
    if include {
        bytes.extend_from_slice(format_status_and_headers(&envelope).as_bytes());
    }
    bytes.extend_from_slice(envelope.body.as_bytes());

    match output {
        Some(path) => {
            write_file(&path, &bytes)?;
            tracing::info!(
                target = "fetch",
                "wrote {} bytes to {}",
                bytes.len(),
                path.display()
            );
            eprintln!("wrote {} bytes to {}", bytes.len(), path.display());
        }
        None => {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            out.write_all(&bytes)?;
        }
    }
    Ok(())
}

/// Parsed `{status, statusText, headers, body}` envelope from `FETCH_JS`.
#[derive(Debug, Clone, PartialEq)]
struct FetchEnvelope {
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// Wire shape of the `FETCH_JS` envelope. Deserialized directly; only `status`
/// is required. `headers` is a `BTreeMap` so iteration is key-sorted, matching
/// the previous `serde_json::Map` iteration order.
#[derive(Deserialize)]
struct RawFetchEnvelope {
    status: u16,
    #[serde(default, rename = "statusText")]
    status_text: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: String,
}

fn parse_headers(headers: &[String]) -> Result<Map<String, Value>> {
    let mut map = Map::new();
    for raw in headers {
        let (k, v) = raw
            .split_once(':')
            .ok_or_else(|| anyhow!("malformed header `{raw}`: expected `Key: Value`"))?;
        let key = k.trim();
        if key.is_empty() {
            bail!("malformed header `{raw}`: empty key");
        }
        // Per RFC 7230, header names are tokens; reject whitespace/control in name.
        if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            bail!("malformed header `{raw}`: invalid character in name");
        }
        let value = v.trim();
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
    Ok(map)
}

/// Build the JS expression that invokes `FETCH_JS` with a JSON-encoded arg
/// string. All user-controlled fields are JSON-encoded twice (once inside the
/// args object, once when we embed the args string as a JS string literal)
/// so the page can't be tricked into evaluating arbitrary expressions.
fn build_fetch_expr(
    url: &str,
    method: &str,
    headers: &Map<String, Value>,
    body: Option<&str>,
) -> Result<String> {
    let args = json!({
        "url": url,
        "method": method,
        "headers": Value::Object(headers.clone()),
        "body": body,
    });
    let args_json = serde_json::to_string(&args)?;
    let args_literal = serde_json::to_string(&args_json)?;
    Ok(format!("({FETCH_JS})({args_literal})"))
}

/// `FETCH_JS` returns `JSON.stringify({...})`, so the evaluator hands us a
/// JSON value of *type string*. Decode the inner JSON.
fn parse_envelope(v: &Value) -> Result<FetchEnvelope> {
    let s = v
        .as_str()
        .ok_or_else(|| anyhow!("fetch script returned non-string value: {v}"))?;
    let raw: RawFetchEnvelope = serde_json::from_str(s)
        .with_context(|| format!("fetch script returned invalid JSON: {s}"))?;
    Ok(FetchEnvelope {
        status: raw.status,
        status_text: raw.status_text,
        headers: raw.headers.into_iter().collect(),
        body: raw.body,
    })
}

fn format_status_and_headers(env: &FetchEnvelope) -> String {
    let mut s = format!("HTTP/1.1 {} {}\r\n", env.status, env.status_text);
    for (k, v) in &env.headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str("\r\n");
    s
}

fn write_file(path: &Path, body: &[u8]) -> Result<()> {
    std::fs::write(path, body).with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod 600 {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_headers_basic() {
        let m = parse_headers(&[
            "Accept: application/json".to_string(),
            "X-Token: abc".to_string(),
        ])
        .unwrap();
        assert_eq!(m.get("Accept").unwrap(), &json!("application/json"));
        assert_eq!(m.get("X-Token").unwrap(), &json!("abc"));
    }

    #[test]
    fn parse_headers_trims_extra_spaces() {
        let m = parse_headers(&["  Accept   :   text/plain  ".to_string()]).unwrap();
        assert_eq!(m.get("Accept").unwrap(), &json!("text/plain"));
    }

    #[test]
    fn parse_headers_value_with_colon_kept_intact() {
        // Only the first `:` separates key/value; the value may contain colons.
        let m = parse_headers(&["Authorization: Bearer a:b:c".to_string()]).unwrap();
        assert_eq!(m.get("Authorization").unwrap(), &json!("Bearer a:b:c"));
    }

    #[test]
    fn parse_headers_rejects_missing_colon() {
        let err = parse_headers(&["NoColonHere".to_string()]).unwrap_err();
        assert!(err.to_string().contains("malformed header"));
    }

    #[test]
    fn parse_headers_rejects_empty_key() {
        let err = parse_headers(&[": value".to_string()]).unwrap_err();
        assert!(err.to_string().contains("empty key"));
    }

    #[test]
    fn parse_headers_rejects_whitespace_in_name() {
        let err = parse_headers(&["bad name: v".to_string()]).unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    fn build_expr_json_escapes_url_and_body() {
        let mut h = Map::new();
        h.insert("X".to_string(), json!("y"));
        // Body and URL contain quotes / backslashes / newlines that would
        // break naive string interpolation.
        let url = "https://x.test/?q=\"hi\"";
        let body = "line1\n\"line2\"\\end";
        let expr = build_fetch_expr(url, "POST", &h, Some(body)).unwrap();
        // The expression must wrap a single JSON-encoded string argument.
        let prefix = format!("({FETCH_JS})(");
        let inner = expr
            .strip_prefix(&prefix)
            .unwrap()
            .strip_suffix(')')
            .unwrap();
        // No raw user-controlled quote or newline can appear unescaped at the
        // top level — the literal is JSON, so quotes inside are `\"` and the
        // string contains no real newline byte.
        assert!(!inner.contains('\n'));
        // Decode the literal back twice and confirm round-trip equality.
        let args_str: String = serde_json::from_str(inner).unwrap();
        let args: Value = serde_json::from_str(&args_str).unwrap();
        assert_eq!(args["url"], url);
        assert_eq!(args["body"], body);
        assert_eq!(args["method"], "POST");
    }

    #[test]
    fn build_expr_method_and_headers_round_trip() {
        let mut h = Map::new();
        h.insert("Accept".to_string(), json!("*/*"));
        let expr = build_fetch_expr("https://x.test/", "GET", &h, None).unwrap();
        // Extract the JSON-string literal argument and decode twice.
        let prefix = format!("({FETCH_JS})(");
        let inner = expr
            .strip_prefix(&prefix)
            .unwrap()
            .strip_suffix(')')
            .unwrap();
        let args_str: String = serde_json::from_str(inner).unwrap();
        let args: Value = serde_json::from_str(&args_str).unwrap();
        assert_eq!(args["url"], "https://x.test/");
        assert_eq!(args["method"], "GET");
        assert_eq!(args["headers"]["Accept"], "*/*");
        assert!(args["body"].is_null());
    }

    #[test]
    fn parse_envelope_decodes_inner_json() {
        let inner = json!({
            "status": 200,
            "statusText": "OK",
            "headers": {"content-type": "text/plain"},
            "body": "hello"
        });
        let v = Value::String(inner.to_string());
        let env = parse_envelope(&v).unwrap();
        assert_eq!(env.status, 200);
        assert_eq!(env.status_text, "OK");
        assert_eq!(env.body, "hello");
        assert_eq!(
            env.headers,
            vec![("content-type".to_string(), "text/plain".to_string())]
        );
    }

    #[test]
    fn parse_envelope_rejects_non_string() {
        let v = json!({"status": 200});
        assert!(parse_envelope(&v).is_err());
    }

    #[test]
    fn format_include_emits_status_and_headers() {
        let env = FetchEnvelope {
            status: 404,
            status_text: "Not Found".to_string(),
            headers: vec![
                ("content-type".to_string(), "text/plain".to_string()),
                ("x-trace".to_string(), "abc".to_string()),
            ],
            body: "missing".to_string(),
        };
        let s = format_status_and_headers(&env);
        assert_eq!(
            s,
            "HTTP/1.1 404 Not Found\r\n\
             content-type: text/plain\r\n\
             x-trace: abc\r\n\
             \r\n"
        );
    }

    #[test]
    fn write_file_chmods_0600_on_unix() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("fetch-test-scratch");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("out-{}.bin", std::process::id()));
        write_file(&p, b"hello").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }
}
