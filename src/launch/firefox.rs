//! Firefox launcher (WebDriver BiDi via `--remote-debugging-port`, Firefox 129+).

use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::detect::{Engine, Installed};

use super::{allocate_free_port, LaunchOpts, LaunchedHandle};

pub async fn launch(installed: &Installed, opts: LaunchOpts) -> Result<LaunchedHandle> {
    let port = allocate_free_port().context("allocating BiDi port")?;

    if !opts.profile_dir.exists() {
        std::fs::create_dir_all(&opts.profile_dir)
            .with_context(|| format!("creating profile dir {}", opts.profile_dir.display()))?;
    }

    let mut cmd = Command::new(&installed.executable);
    cmd.arg("-profile").arg(&opts.profile_dir).arg("-no-remote");
    if opts.headless {
        cmd.arg("-headless");
    }
    cmd.arg("--remote-debugging-port")
        .arg(port.to_string())
        .arg("about:blank");

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", installed.executable.display()))?;
    let pid = child.id().context("child has no pid")?;

    let endpoint = wait_for_firefox_endpoint(port, &mut child).await?;

    Ok(LaunchedHandle {
        pid,
        port,
        endpoint,
        engine: Engine::Bidi,
        profile_dir: opts.profile_dir,
        child: Some(child),
    })
}

/// Wait for Firefox to print "WebDriver BiDi listening on ws://..." on stderr.
///
/// Firefox does not expose a `/json/version` HTTP endpoint, and its BiDi WS
/// path is `/session`. We could synthesize `ws://127.0.0.1:<port>/session`
/// directly, but reading the stderr line gives us the exact URL Firefox
/// chose (and confirms readiness).
async fn wait_for_firefox_endpoint(port: u16, child: &mut tokio::process::Child) -> Result<String> {
    let stderr = child
        .stderr
        .take()
        .context("firefox child has no captured stderr")?;
    let mut reader = BufReader::new(stderr).lines();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let _ = child.start_kill();
            bail!("timed out waiting for Firefox WebDriver BiDi endpoint on port {port}");
        }

        tokio::select! {
            line = tokio::time::timeout(remaining, reader.next_line()) => {
                let line = line.map_err(|_| anyhow::anyhow!("timeout reading firefox stderr"))?;
                match line {
                    Ok(Some(l)) => {
                        if let Some(url) = parse_bidi_url(&l) {
                            return Ok(url);
                        }
                    }
                    Ok(None) => {
                        bail!("firefox stderr closed before BiDi endpoint was advertised");
                    }
                    Err(e) => bail!("reading firefox stderr: {e}"),
                }
            }
            status = child.wait() => {
                let status = status.context("waiting on firefox child")?;
                bail!("firefox exited before BiDi endpoint was advertised (status: {status})");
            }
        }
    }
}

fn parse_bidi_url(line: &str) -> Option<String> {
    // Match: "WebDriver BiDi listening on ws://HOST:PORT" (path may be absent;
    // Firefox typically logs without a trailing path. The actual WS endpoint
    // for the browser is `<base>/session`).
    let needle = "WebDriver BiDi listening on ";
    let idx = line.find(needle)?;
    let rest = &line[idx + needle.len()..];
    let url = rest.split_whitespace().next()?.trim();
    if !url.starts_with("ws://") && !url.starts_with("wss://") {
        return None;
    }
    let trimmed = url.trim_end_matches('/');
    Some(format!("{trimmed}/session"))
}

#[cfg(test)]
mod tests {
    use super::parse_bidi_url;

    #[test]
    fn parses_bidi_listening_line() {
        let l = "WebDriver BiDi listening on ws://127.0.0.1:9876";
        assert_eq!(
            parse_bidi_url(l).as_deref(),
            Some("ws://127.0.0.1:9876/session")
        );
    }

    #[test]
    fn ignores_unrelated_lines() {
        assert!(parse_bidi_url("*** You are running in headless mode.").is_none());
        assert!(parse_bidi_url("[GFX1-]: noise").is_none());
    }

    #[test]
    fn handles_trailing_slash() {
        let l = "WebDriver BiDi listening on ws://127.0.0.1:1234/";
        assert_eq!(
            parse_bidi_url(l).as_deref(),
            Some("ws://127.0.0.1:1234/session")
        );
    }
}
