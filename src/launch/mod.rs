//! Browser launcher: spawns a browser process and waits for its remote
//! debugging endpoint to come up.

use std::path::PathBuf;

use anyhow::Result;

use crate::detect::{Engine, Installed};

pub mod chromium;
pub mod firefox;

#[derive(Debug, Clone)]
pub struct LaunchOpts {
    pub headless: bool,
    pub profile_dir: PathBuf,
}

#[derive(Debug)]
pub struct LaunchedHandle {
    pub pid: u32,
    pub port: u16,
    /// CDP browser-level WS endpoint for Chromium, BiDi WS endpoint for Firefox.
    pub endpoint: String,
    pub engine: Engine,
    pub profile_dir: PathBuf,
    /// Held so tests can kill the child; in production the handle is `forget()`ed
    /// so the child is dropped without being killed (Command is configured with
    /// `kill_on_drop(false)`).
    pub(crate) child: Option<tokio::process::Child>,
}

impl LaunchedHandle {
    /// Release the underlying Child so dropping this handle won't try to manage it.
    /// Returns the pid. Production callers (e.g. `cli-start`) use this to fully
    /// detach the browser process.
    pub fn forget(mut self) -> u32 {
        let pid = self.pid;
        if let Some(child) = self.child.take() {
            // Forget the Child without killing. `kill_on_drop(false)` was set,
            // so dropping it does not kill the process.
            drop(child);
        }
        pid
    }

    /// Kill the spawned process. Mainly for tests.
    pub async fn kill(mut self) -> Result<()> {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill().await;
        }
        Ok(())
    }
}

/// Allocate a free TCP port by binding to 0 then dropping the listener.
pub fn allocate_free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// Top-level entry point: dispatches to chromium or firefox based on kind.
pub async fn launch(installed: &Installed, opts: LaunchOpts) -> Result<LaunchedHandle> {
    if installed.kind.is_chromium() {
        chromium::launch(installed, opts).await
    } else {
        firefox::launch(installed, opts).await
    }
}

/// Poll `http://127.0.0.1:<port>/json/version` at 50ms cadence for up to 15s.
/// Returns the `webSocketDebuggerUrl` once available.
///
/// On each iteration, also checks whether the child process has already exited;
/// if so, returns an error including any captured stderr/stdout.
pub(crate) async fn wait_for_endpoint(
    port: u16,
    child: &mut tokio::process::Child,
) -> Result<String> {
    use anyhow::{bail, Context};
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .context("building reqwest client")?;
    let url = format!("http://127.0.0.1:{port}/json/version");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().context("polling child status")? {
            let mut stderr_buf = String::new();
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_string(&mut stderr_buf).await;
            }
            let mut stdout_buf = String::new();
            if let Some(mut s) = child.stdout.take() {
                let _ = s.read_to_string(&mut stdout_buf).await;
            }
            bail!(
                "browser process exited before endpoint came up (status: {status}); \
                 stderr: {stderr_buf}; stdout: {stdout_buf}"
            );
        }

        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    if let Some(ws) = json.get("webSocketDebuggerUrl").and_then(|v| v.as_str()) {
                        return Ok(ws.to_string());
                    }
                }
            }
        }

        if std::time::Instant::now() >= deadline {
            // Best-effort kill; bubble up timeout.
            let _ = child.start_kill();
            bail!("timed out waiting for browser endpoint on port {port}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{Engine, Installed, Kind};
    use tempfile::TempDir;

    fn build_fake_browser() -> std::path::PathBuf {
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--example", "fake_browser", "--quiet"])
            .status()
            .expect("invoke cargo build");
        assert!(status.success(), "failed to build fake_browser example");
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("target");
        p.push("debug");
        p.push("examples");
        #[cfg(windows)]
        p.push("fake_browser.exe");
        #[cfg(not(windows))]
        p.push("fake_browser");
        assert!(
            p.exists(),
            "fake_browser binary not found at {}",
            p.display()
        );
        p
    }

    #[tokio::test]
    async fn allocate_free_port_returns_nonzero() {
        let p = allocate_free_port().unwrap();
        assert!(p > 0);
    }

    #[tokio::test]
    async fn chromium_launch_against_fake() {
        let exe = build_fake_browser();
        let tmp = TempDir::new().unwrap();
        let installed = Installed {
            kind: Kind::Chrome,
            executable: exe,
            version: "fake".into(),
            engine: Engine::Cdp,
        };
        let opts = LaunchOpts {
            headless: true,
            profile_dir: tmp.path().join("profile"),
        };
        let h = launch(&installed, opts).await.expect("launch chromium");
        assert!(h.endpoint.starts_with("ws://"), "endpoint: {}", h.endpoint);
        assert!(h.port > 0);
        assert_eq!(h.engine, Engine::Cdp);
        h.kill().await.unwrap();
    }

    #[tokio::test]
    async fn firefox_launch_against_fake() {
        let exe = build_fake_browser();
        let tmp = TempDir::new().unwrap();
        let installed = Installed {
            kind: Kind::Firefox,
            executable: exe,
            version: "fake".into(),
            engine: Engine::Bidi,
        };
        let opts = LaunchOpts {
            headless: true,
            profile_dir: tmp.path().join("profile"),
        };
        let h = launch(&installed, opts).await.expect("launch firefox");
        assert!(h.endpoint.starts_with("ws://"), "endpoint: {}", h.endpoint);
        assert!(h.port > 0);
        assert_eq!(h.engine, Engine::Bidi);
        h.kill().await.unwrap();
    }

    #[tokio::test]
    async fn launch_fails_when_process_exits_immediately() {
        // Use `/usr/bin/true`-style: a binary that exits immediately.
        // We use the system `true` on unix; on windows skip.
        #[cfg(unix)]
        {
            let tmp = TempDir::new().unwrap();
            let installed = Installed {
                kind: Kind::Chrome,
                executable: std::path::PathBuf::from("/usr/bin/true"),
                version: "fake".into(),
                engine: Engine::Cdp,
            };
            let opts = LaunchOpts {
                headless: true,
                profile_dir: tmp.path().join("profile"),
            };
            let err = launch(&installed, opts).await.unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("exited") || msg.contains("timed out"),
                "unexpected error: {msg}"
            );
        }
    }
}
