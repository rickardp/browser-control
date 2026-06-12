//! Firefox launcher (WebDriver BiDi via `--remote-debugging-port`, Firefox 129+).

use std::fs::File;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use crate::detect::{Engine, Installed};

use super::{
    allocate_free_port, background_after_launch, configure_session_detachment, LaunchOpts,
    LaunchedHandle,
};

pub async fn launch(installed: &Installed, opts: LaunchOpts) -> Result<LaunchedHandle> {
    let port = allocate_free_port().context("allocating BiDi port")?;

    if !opts.profile_dir.exists() {
        std::fs::create_dir_all(&opts.profile_dir)
            .with_context(|| format!("creating profile dir {}", opts.profile_dir.display()))?;
    }

    let log_path = opts.profile_dir.join("browser.log");
    let log_file =
        File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    let log_clone = log_file
        .try_clone()
        .context("cloning log file handle for stderr")?;

    let mut cmd = Command::new(&installed.executable);
    cmd.arg("-profile").arg(&opts.profile_dir).arg("-no-remote");
    if opts.headless {
        cmd.arg("-headless");
    }
    cmd.arg("--remote-debugging-port")
        .arg(port.to_string())
        .arg("about:blank");

    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_clone))
        .kill_on_drop(false);
    configure_session_detachment(&mut cmd);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", installed.executable.display()))?;
    let pid = child.id().context("child has no pid")?;

    let endpoint = wait_for_firefox_endpoint(port, &mut child, &log_path).await?;
    if !opts.headless {
        background_after_launch(pid);
    }

    Ok(LaunchedHandle {
        pid,
        port,
        endpoint,
        engine: Engine::Bidi,
        profile_dir: opts.profile_dir,
        child: Some(child),
    })
}

/// Wait for Firefox to print "WebDriver BiDi listening on ws://..." to its
/// log file (we redirect stderr there because Firefox does not expose a
/// `/json/version` HTTP endpoint).
async fn wait_for_firefox_endpoint(
    port: u16,
    child: &mut tokio::process::Child,
    log_path: &std::path::Path,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut seen = 0usize;

    loop {
        if let Some(status) = child.try_wait().context("polling child status")? {
            let log = std::fs::read_to_string(log_path).unwrap_or_default();
            bail!(
                "firefox exited before BiDi endpoint was advertised (status: {status}); \
                 log ({}):\n{}",
                log_path.display(),
                log
            );
        }

        if let Ok(s) = std::fs::read_to_string(log_path) {
            if s.len() > seen {
                for line in s[seen..].lines() {
                    if let Some(url) = parse_bidi_url(line) {
                        return Ok(url);
                    }
                }
                seen = s.len();
            }
        }

        if tokio::time::Instant::now() >= deadline {
            let _ = child.start_kill();
            bail!(
                "timed out waiting for Firefox WebDriver BiDi endpoint on port {port}; \
                 see log at {}",
                log_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
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
