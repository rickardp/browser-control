//! Stdio passthrough for the official `@playwright/mcp` server.
//!
//! Spawns the Playwright MCP as a child process, injects `--cdp-endpoint <url>`
//! resolved from our browser registry, and forwards stdin/stdout/stderr
//! bidirectionally.

use anyhow::{anyhow, Context, Result};
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Command;

use crate::cli::env_resolver::ResolvedBrowser;

/// Spawn the official Playwright MCP server and forward stdio bidirectionally.
///
/// Resolves the CDP endpoint URL from `browser.endpoint`. Tries `npx -y @playwright/mcp@latest`,
/// falling back to `bunx` if `npx` is not on PATH. Injects `--cdp-endpoint <url>` as a CLI arg.
///
/// Forwards our process's stdin → child stdin, child stdout → our stdout, child stderr → our stderr.
/// Returns the child's exit code.
pub async fn run(browser: &ResolvedBrowser) -> Result<i32> {
    run_with_streams(
        browser,
        tokio::io::stdin(),
        tokio::io::stdout(),
        tokio::io::stderr(),
        None,
    )
    .await
}

/// Same as `run`, but tests can pass custom streams and override the launcher command.
pub async fn run_with_streams<I, O, E>(
    browser: &ResolvedBrowser,
    mut stdin: I,
    mut stdout: O,
    mut stderr: E,
    override_cmd: Option<(String, Vec<String>)>,
) -> Result<i32>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
    E: AsyncWrite + Unpin + Send + 'static,
{
    let (program, args) = match override_cmd {
        Some(c) => c,
        None => choose_launcher(browser)?,
    };

    let mut child = Command::new(&program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to spawn `{program}`. Install Node.js (npm) or Bun."))?;

    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("no child stdin"))?;
    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no child stdout"))?;
    let mut child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("no child stderr"))?;

    let in_to_child = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stdin, &mut child_stdin).await;
    });
    let out_to_us = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut child_stdout, &mut stdout).await;
    });
    let err_to_us = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut child_stderr, &mut stderr).await;
    });

    let status = child.wait().await?;
    in_to_child.abort();
    out_to_us.await.ok();
    err_to_us.await.ok();

    Ok(status.code().unwrap_or(0))
}

fn choose_launcher(browser: &ResolvedBrowser) -> Result<(String, Vec<String>)> {
    let endpoint = browser.endpoint.clone();
    let args = vec![
        "-y".into(),
        "@playwright/mcp@latest".into(),
        "--cdp-endpoint".into(),
        endpoint,
    ];
    if which::which("npx").is_ok() {
        Ok(("npx".into(), args))
    } else if which::which("bunx").is_ok() {
        Ok(("bunx".into(), args))
    } else {
        Err(anyhow!("neither `npx` nor `bunx` is on PATH. Install Node.js (https://nodejs.org/) or Bun (https://bun.sh/)."))
    }
}
