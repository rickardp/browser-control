//! Playwright sidecar — spawn and talk to a Node script that wraps
//! `playwright-core`.
//!
//! This module owns the lifecycle of the Node child process and the
//! NDJSON-over-stdio JSON-RPC channel to it. Tools in the MCP layer call
//! [`Sidecar::call`] with `(method, params)` and get back the parsed
//! result (or an error).
//!
//! Lifecycle (per `Sidecar` instance):
//!
//! 1. [`Sidecar::start`] — picks a launcher (`bun` preferred, then
//!    `node`), prepares the cache directory containing the bundled
//!    `sidecar.mjs` + `package.json`, runs `bun install` /
//!    `npm install` if the deps aren't already there, then spawns the
//!    child with stdin/stdout piped.
//! 2. [`Sidecar::connect`] — sends a `connect` RPC carrying the CDP
//!    endpoint URL so the sidecar holds a Playwright `Browser` for
//!    the duration.
//! 3. [`Sidecar::call`] — send a request, receive the response. Many
//!    requests can be in flight (each carries a unique id); responses
//!    are routed by id.
//! 4. Drop — closes stdin, the child exits.
//!
//! Errors that originate inside the sidecar arrive as JSON-RPC error
//! responses; we surface them as `anyhow` errors. Connection-level
//! failures (sidecar process gone, stdio closed) surface as `SidecarGone`
//! so the caller can decide whether to restart.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex};

pub mod assets;
#[cfg(test)]
mod tests;

/// Default `playwright-core` version pinned in `assets/playwright-sidecar/package.json`.
/// Overridable via `--playwright-version` (CLI) or [`SidecarConfig::version`].
pub const DEFAULT_PLAYWRIGHT_VERSION: &str = "1.49.1";

/// User-facing configuration for spawning a sidecar.
#[derive(Debug, Clone, Default)]
pub struct SidecarConfig {
    /// `playwright-core` version string. `None` uses [`DEFAULT_PLAYWRIGHT_VERSION`].
    pub version: Option<String>,
}

impl SidecarConfig {
    fn resolved_version(&self) -> &str {
        self.version
            .as_deref()
            .unwrap_or(DEFAULT_PLAYWRIGHT_VERSION)
    }
}

/// Which runtime + package manager we use to launch the sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launcher {
    /// Bun: faster startup, single binary. Uses `bun install` + `bun run`.
    Bun,
    /// Node + npm: more universally installed. Uses `npm install --silent` + `node`.
    Node,
}

impl Launcher {
    /// Detect what's available. Prefers `bun`, falls back to `node` + `npm`.
    pub fn detect() -> Result<Launcher> {
        if which::which("bun").is_ok() {
            return Ok(Launcher::Bun);
        }
        if which::which("node").is_ok() && which::which("npm").is_ok() {
            return Ok(Launcher::Node);
        }
        Err(anyhow!(
            "neither `bun` nor `node`+`npm` is on PATH. \
             Install Bun (https://bun.sh/) or Node.js (https://nodejs.org/)."
        ))
    }
}

/// Pending-request channel map: id → response sender. Held inside the
/// reader task; cleared on shutdown.
type PendingMap = HashMap<u64, oneshot::Sender<Result<Value>>>;

/// Live sidecar handle. Cloneable: behaviour is shared through `Arc`s
/// inside.
#[derive(Clone)]
pub struct Sidecar {
    next_id: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingMap>>,
    write_tx: tokio::sync::mpsc::UnboundedSender<String>,
    /// Held to keep the child alive; aborted on drop of the last clone.
    _inner: Arc<SidecarInner>,
}

/// Inner state with non-clonable handles, kept behind `Arc` so the public
/// `Sidecar` can be `Clone`.
struct SidecarInner {
    #[allow(dead_code)] // kept alive; killed via Drop
    child: Mutex<Option<Child>>,
    reader_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    writer_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Truncate a stdout line to a bounded prefix for logging, so a large payload
/// never floods the diagnostics.
fn truncate_line(line: &str) -> std::borrow::Cow<'_, str> {
    const MAX: usize = 200;
    if line.len() <= MAX {
        std::borrow::Cow::Borrowed(line)
    } else {
        let end = line
            .char_indices()
            .take_while(|(i, _)| *i < MAX)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        std::borrow::Cow::Owned(format!("{}… ({} bytes total)", &line[..end], line.len()))
    }
}

impl Drop for SidecarInner {
    fn drop(&mut self) {
        // Best-effort: kill the child and abort the IO tasks. Reader/writer
        // will short-circuit when the pipes close.
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(mut c) = guard.take() {
                let _ = c.start_kill();
            }
        }
        if let Ok(mut guard) = self.reader_handle.try_lock() {
            if let Some(h) = guard.take() {
                h.abort();
            }
        }
        if let Ok(mut guard) = self.writer_handle.try_lock() {
            if let Some(h) = guard.take() {
                h.abort();
            }
        }
    }
}

impl Sidecar {
    /// Start a sidecar process. Prepares the cache directory (one-time
    /// install) and spawns the child. The returned handle is not yet
    /// connected to a browser — call [`Sidecar::connect`] next.
    pub async fn start(config: SidecarConfig) -> Result<Self> {
        let launcher = Launcher::detect()?;
        let cache_dir = assets::ensure_sidecar_dir(config.resolved_version())
            .await
            .context("preparing sidecar cache directory")?;
        Self::install_deps(launcher, &cache_dir).await?;
        Self::spawn(launcher, &cache_dir).await
    }

    /// Run `bun install` / `npm install` in `cache_dir` if the
    /// dependencies aren't already present. Idempotent.
    async fn install_deps(launcher: Launcher, cache_dir: &PathBuf) -> Result<()> {
        let marker = cache_dir.join("node_modules").join("playwright-core");
        if tokio::fs::metadata(&marker).await.is_ok() {
            return Ok(());
        }
        let (program, args) = match launcher {
            Launcher::Bun => ("bun", vec!["install", "--silent"]),
            Launcher::Node => ("npm", vec!["install", "--silent"]),
        };
        let status = Command::new(program)
            .args(&args)
            .current_dir(cache_dir)
            .status()
            .await
            .with_context(|| format!("running `{program} {}` in {cache_dir:?}", args.join(" ")))?;
        if !status.success() {
            return Err(anyhow!(
                "`{program} {}` in {cache_dir:?} exited with status {status}",
                args.join(" ")
            ));
        }
        Ok(())
    }

    /// Spawn the child process and wire stdio.
    async fn spawn(launcher: Launcher, cache_dir: &PathBuf) -> Result<Self> {
        let (program, args) = match launcher {
            Launcher::Bun => ("bun", vec!["run", "sidecar.mjs"]),
            Launcher::Node => ("node", vec!["sidecar.mjs"]),
        };
        let mut child = Command::new(program)
            .args(&args)
            .current_dir(cache_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning `{program} {}`", args.join(" ")))?;

        let mut child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("no child stdin"))?;
        let child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("no child stdout"))?;

        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
        let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        let writer_handle = tokio::spawn(async move {
            while let Some(line) = write_rx.recv().await {
                if child_stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if child_stdin.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = child_stdin.flush().await;
            }
        });

        let pending_r = pending.clone();
        let reader_handle = tokio::spawn(async move {
            let mut lines = BufReader::new(child_stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    // Unparseable NDJSON line: can't route it, so it's dropped —
                    // log (truncated) so the request it would have answered has
                    // a breadcrumb instead of hanging in silence.
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            line = %truncate_line(&line),
                            "sidecar: dropping unparseable stdout line"
                        );
                        continue;
                    }
                };
                let id = match v.get("id").and_then(|x| x.as_u64()) {
                    Some(i) => i,
                    None => {
                        tracing::debug!(
                            line = %truncate_line(&line),
                            "sidecar: dropping idless stdout line"
                        );
                        continue;
                    }
                };
                let result = if let Some(err) = v.get("error") {
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("(no message)");
                    Err(anyhow!("{msg}"))
                } else {
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                };
                let tx = {
                    let mut p = pending_r.lock().await;
                    p.remove(&id)
                };
                if let Some(tx) = tx {
                    let _ = tx.send(result);
                }
            }
            // Reader closed: drain pending with a SidecarGone-flavoured error.
            let mut p = pending_r.lock().await;
            for (_, tx) in p.drain() {
                let _ = tx.send(Err(anyhow!("sidecar stdout closed")));
            }
        });

        Ok(Self {
            next_id: Arc::new(AtomicU64::new(1)),
            pending,
            write_tx,
            _inner: Arc::new(SidecarInner {
                child: Mutex::new(Some(child)),
                reader_handle: Mutex::new(Some(reader_handle)),
                writer_handle: Mutex::new(Some(writer_handle)),
            }),
        })
    }

    /// Tell the sidecar to connect to a CDP `endpoint` (full ws://… URL
    /// from `/json/version`). Holds a Playwright `Browser` for the
    /// sidecar's lifetime; future calls reuse it.
    pub async fn connect(&self, endpoint: &str) -> Result<Value> {
        self.call("connect", json!({ "endpoint": endpoint })).await
    }

    /// Send a method call, await the response.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({ "id": id, "method": method, "params": params });
        let line = serde_json::to_string(&req)?;
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().await;
            p.insert(id, tx);
        }
        if self.write_tx.send(line).is_err() {
            let mut p = self.pending.lock().await;
            p.remove(&id);
            return Err(anyhow!("sidecar writer closed"));
        }
        match rx.await {
            Ok(r) => r,
            Err(_) => Err(anyhow!("sidecar response channel dropped")),
        }
    }
}
