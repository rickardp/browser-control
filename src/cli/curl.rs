//! `browser-control curl` — invoke the system curl with browser credentials.
//!
//! Unlike [`crate::cli::fetch`], this request does not execute in a renderer.
//! The selected browser supplies a snapshot of its cookie jar and User-Agent;
//! the real curl process supplies transport, streaming, redirects, and every
//! curl CLI option. Browser cookies are written to a mode-0600 temporary
//! Netscape jar which is removed when the command finishes.

use std::ffi::OsString;
use std::io::Write;
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::cli::trace::CommandTrace;
use crate::session::backend::{open_backend, TabBackend};

/// Maximum raw curl stdout returned inside an MCP tool result. File output
/// selected by curl's `-o`/`--output` does not pass through this buffer and is
/// therefore unrestricted.
pub const MCP_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

/// Keep diagnostic stderr useful without allowing verbose curl traces to
/// create another unbounded MCP response.
const MCP_STDERR_LIMIT: usize = 256 * 1024;

pub(crate) struct PreparedCurl {
    cookie_jar: tempfile::NamedTempFile,
    user_agent: String,
    origin: Option<String>,
    referer: Option<String>,
}

#[derive(Debug)]
pub(crate) struct CurlOutput {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stderr_truncated: bool,
}

impl PreparedCurl {
    fn command<I, S>(&self, args: I) -> Result<tokio::process::Command>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let executable = which::which("curl").context("curl executable not found in PATH")?;
        let mut command = tokio::process::Command::new(executable);
        // Browser-derived values are defaults. User argv follows unchanged,
        // so curl's normal last-option-wins behavior can override User-Agent
        // and augment other cookie sources when explicitly requested.
        command
            .arg("--cookie")
            .arg(self.cookie_jar.path())
            .arg("--user-agent")
            .arg(&self.user_agent);
        if let Some(origin) = &self.origin {
            command.arg("--header").arg(format!("Origin: {origin}"));
        }
        if let Some(referer) = &self.referer {
            command.arg("--referer").arg(referer);
        }
        command.args(args);
        Ok(command)
    }
}

/// Snapshot browser credentials into the short-lived inputs consumed by
/// curl. `target_id` is optional because cookies are browser-wide; when set it
/// is used to read a target-specific `navigator.userAgent` override.
pub(crate) async fn prepare(backend: &TabBackend, target_id: Option<&str>) -> Result<PreparedCurl> {
    let cookies = backend.cookies().await?;
    let live_targets = backend.live_targets().await?;
    let context_target_id = target_id
        .filter(|target_id| live_targets.iter().any(|target| target.id == *target_id))
        .map(String::from)
        .or_else(|| live_targets.first().map(|target| target.id.clone()));
    let source_url = context_target_id.as_deref().and_then(|target_id| {
        live_targets
            .iter()
            .find(|target| target.id == target_id)
            .map(|target| target.url.clone())
    });
    let user_agent = backend.user_agent(context_target_id.as_deref()).await?;
    let (origin, referer) = source_url
        .as_deref()
        .map(request_context_headers)
        .unwrap_or((None, None));
    let mut cookie_jar =
        tempfile::NamedTempFile::new().context("creating temporary browser cookie jar for curl")?;
    cookie_jar
        .write_all(crate::cli::cookies::format_netscape(&cookies).as_bytes())
        .context("writing temporary browser cookie jar for curl")?;
    cookie_jar
        .flush()
        .context("flushing temporary browser cookie jar for curl")?;
    Ok(PreparedCurl {
        cookie_jar,
        user_agent,
        origin,
        referer,
    })
}

fn request_context_headers(source_url: &str) -> (Option<String>, Option<String>) {
    let Ok(mut parsed) = url::Url::parse(source_url) else {
        return (None, None);
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return (None, None);
    }
    let origin = parsed.origin().ascii_serialization();
    parsed.set_fragment(None);
    (Some(origin), Some(parsed.to_string()))
}

/// CLI entry point. Curl owns stdin/stdout/stderr, so ordinary streaming and
/// `-o` downloads behave exactly like invoking curl directly.
pub async fn run(browser: Option<String>, args: Vec<OsString>) -> Result<()> {
    if args.is_empty() {
        bail!("curl requires arguments; pass curl options and at least one URL");
    }
    let mut trace = CommandTrace::new("curl");
    let result = run_inner(browser, args, &mut trace).await;
    trace.finish(result)
}

async fn run_inner(
    browser: Option<String>,
    args: Vec<OsString>,
    trace: &mut CommandTrace,
) -> Result<()> {
    let route = crate::cli::route::preamble(browser, None, trace).await?;
    let backend = open_backend(&route.resolved.endpoint, route.resolved.engine).await?;
    let target_id = match route.tab_name.as_deref() {
        Some(tab_name) => {
            trace.route("named-tab").tab_name(tab_name);
            let browser_name = match &route.resolved.source {
                crate::cli::env_resolver::Source::Registered { name } => name.clone(),
                crate::cli::env_resolver::Source::External => {
                    bail!("named tabs (`<browser>/<name>`) require a registered browser")
                }
            };
            let row =
                crate::session::resolve_tab(&backend, &route.registry, &browser_name, tab_name)
                    .await?
                    .ok_or_else(|| crate::errors::SessionError::TabNotFound {
                        browser: browser_name,
                        name: tab_name.to_string(),
                    })?;
            trace.target_id(&row.target_id);
            Some(row.target_id)
        }
        None => {
            trace.route("browser-wide");
            None
        }
    };

    let prepared = prepare(&backend, target_id.as_deref()).await?;
    // Curl no longer needs the browser protocol connection or registry lock.
    // Release both before a potentially long download.
    drop(backend);
    drop(route);

    let mut command = prepared.command(args.iter())?;
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = command.status().await.context("running curl")?;
    if !status.success() {
        bail!(
            "curl exited with status {}",
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "terminated by signal".to_string())
        );
    }
    Ok(())
}

/// Execute curl for MCP. Stdout is read incrementally and the child is killed
/// as soon as the response would exceed `MCP_RESPONSE_LIMIT`; stderr is always
/// drained concurrently and retained only up to `MCP_STDERR_LIMIT`.
pub(crate) async fn execute_mcp(prepared: &PreparedCurl, args: &[String]) -> Result<CurlOutput> {
    if args.is_empty() {
        bail!("`args` must contain curl options and at least one URL");
    }
    let mut command = prepared.command(args)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("running curl")?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to capture curl stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to capture curl stderr"))?;
    let stderr_task = tokio::spawn(read_bounded_and_drain(stderr, MCP_STDERR_LIMIT));

    let mut body = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let n = stdout
            .read(&mut chunk)
            .await
            .context("reading curl stdout")?;
        if n == 0 {
            break;
        }
        if body.len().saturating_add(n) > MCP_RESPONSE_LIMIT {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stderr_task.await;
            bail!(
                "curl response exceeded the 8 MiB MCP limit; retry with `-o <path>` or `--output <path>` to stream it directly to a file"
            );
        }
        body.extend_from_slice(&chunk[..n]);
    }

    let status = child.wait().await.context("waiting for curl")?;
    let (stderr, stderr_truncated) = stderr_task.await.context("joining curl stderr reader")??;
    Ok(CurlOutput {
        exit_code: status.code(),
        stdout: body,
        stderr,
        stderr_truncated,
    })
}

async fn read_bounded_and_drain<R>(mut reader: R, limit: usize) -> Result<(Vec<u8>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let remaining = limit.saturating_sub(kept.len());
        let take = remaining.min(n);
        kept.extend_from_slice(&chunk[..take]);
        truncated |= take < n;
    }
    Ok((kept, truncated))
}

/// Convert a completed curl invocation to an MCP tool result. Text is emitted
/// directly; arbitrary bytes use the protocol's embedded-resource blob form.
pub(crate) fn mcp_result(output: CurlOutput) -> Value {
    let success = output.exit_code == Some(0);
    let mut content = Vec::new();
    if !output.stdout.is_empty() {
        match std::str::from_utf8(&output.stdout) {
            Ok(text) => content.push(json!({ "type": "text", "text": text })),
            Err(_) => content.push(json!({
                "type": "resource",
                "resource": {
                    "uri": "browser-control://curl/response",
                    "mimeType": "application/octet-stream",
                    "blob": base64::engine::general_purpose::STANDARD.encode(&output.stdout),
                }
            })),
        }
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    content.push(json!({
        "type": "text",
        "text": serde_json::to_string_pretty(&json!({
            "exit_code": output.exit_code,
            "stdout_bytes": output.stdout.len(),
            "stderr": stderr,
            "stderr_truncated": output.stderr_truncated,
        })).expect("serializing curl metadata cannot fail")
    }));
    json!({
        "content": content,
        "isError": !success,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_result_returns_utf8_as_text() {
        let result = mcp_result(CurlOutput {
            exit_code: Some(0),
            stdout: b"hello".to_vec(),
            stderr: Vec::new(),
            stderr_truncated: false,
        });
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], "hello");
    }

    #[test]
    fn mcp_result_returns_binary_as_embedded_resource() {
        let result = mcp_result(CurlOutput {
            exit_code: Some(0),
            stdout: vec![0, 159, 146, 150],
            stderr: Vec::new(),
            stderr_truncated: false,
        });
        assert_eq!(result["content"][0]["type"], "resource");
        assert_eq!(result["content"][0]["resource"]["blob"], "AJ+Slg==");
    }

    #[test]
    fn mcp_result_marks_nonzero_curl_exit_as_tool_error() {
        let result = mcp_result(CurlOutput {
            exit_code: Some(22),
            stdout: b"not found".to_vec(),
            stderr: b"curl: (22) HTTP response code said error".to_vec(),
            stderr_truncated: false,
        });
        assert_eq!(result["isError"], true);
        assert!(result["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("\"exit_code\": 22"));
    }

    #[test]
    fn request_context_headers_use_tab_origin_and_fragmentless_url() {
        let (origin, referer) =
            request_context_headers("https://app.example.com:8443/work?q=1#section");
        assert_eq!(origin.as_deref(), Some("https://app.example.com:8443"));
        assert_eq!(
            referer.as_deref(),
            Some("https://app.example.com:8443/work?q=1")
        );
    }

    #[test]
    fn request_context_headers_ignore_non_http_tabs() {
        assert_eq!(request_context_headers("about:blank"), (None, None));
    }
}
