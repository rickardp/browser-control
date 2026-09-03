//! Foreground emulation for background tabs (ADR-004).
//!
//! Chromium treats a tab in a minimized window, or any tab while the
//! display is locked, as hidden: `requestAnimationFrame` never fires,
//! timers are throttled, `document.visibilityState` is `hidden` and
//! `document.hasFocus()` is false. `Emulation.setFocusEmulationEnabled`
//! flips all of that, but only for as long as the CDP session that enabled
//! it stays attached.
//!
//! So both the CLI (`browser-control tab foreground <browser>/<tab> on`) and
//! the MCP tool (`browser_tab_foreground`) use the same thing: a small
//! detached **holder process** (`browser-control tab foreground-hold`, a
//! hidden subcommand) that attaches one session, enables the emulation, and
//! blocks until it is stopped, the tab goes away, or the browser exits. Its
//! PID is recorded in the registry so `off` can stop it and tab listings can
//! show the flag from either surface. The holder is push-only: it waits on
//! CDP events and a termination signal; nothing polls or wakes on a timer.
//! This is the documented exception to the daemonless rule in ADR-002.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use tokio::sync::broadcast;

use crate::cdp::{CdpClient, CdpEvent};
use crate::detect::Engine;
use crate::registry::{ForegroundRow, Registry};
use crate::session::backend::{open_backend, TabBackend};

/// How long `spawn_holder` waits for the child to register itself.
const SPAWN_WAIT: Duration = Duration::from_secs(8);
const SPAWN_POLL: Duration = Duration::from_millis(50);

/// Make Chromium treat the tab as focused and visible (or stop doing so).
/// `setFocusEmulationEnabled` is what flips `document.visibilityState`,
/// `document.hasFocus()`, `requestAnimationFrame`, and timer throttling for
/// a minimized window or a locked display; `setIdleOverride` additionally
/// answers the Idle Detection API with "active, unlocked".
pub async fn apply(client: &CdpClient, session_id: &str, enabled: bool) -> Result<()> {
    client
        .send_with_session(
            "Emulation.setFocusEmulationEnabled",
            json!({ "enabled": enabled }),
            Some(session_id),
        )
        .await?;
    let idle = if enabled {
        client
            .send_with_session(
                "Emulation.setIdleOverride",
                json!({ "isUserActive": true, "isScreenUnlocked": true }),
                Some(session_id),
            )
            .await
    } else {
        client
            .send_with_session("Emulation.clearIdleOverride", json!({}), Some(session_id))
            .await
    };
    if let Err(e) = idle {
        tracing::debug!(error = %e, "idle override unavailable");
    }
    Ok(())
}

/// Whether a live holder exists for the tab.
pub fn status(
    registry: &Registry,
    browser_name: &str,
    target_id: &str,
) -> Result<Option<ForegroundRow>> {
    registry.foreground_get(browser_name, target_id)
}

/// Target ids under foreground emulation for a browser.
pub fn active_targets(registry: &Registry, browser_name: &str) -> Result<Vec<String>> {
    Ok(registry
        .foreground_list(browser_name)?
        .into_iter()
        .map(|r| r.target_id)
        .collect())
}

/// Default lifetime of a holder. Agents forget to turn things off; an hour
/// covers a long debugging session without keeping a game at 60 fps in the
/// background forever.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Start a detached holder for the tab that expires after `timeout`, or
/// return the existing one. Returns `(pid, created)`.
pub fn spawn_holder(
    registry: &Registry,
    browser_name: &str,
    target_id: &str,
    timeout: Duration,
) -> Result<(u32, bool)> {
    if let Some(existing) = registry.foreground_get(browser_name, target_id)? {
        return Ok((existing.pid, false));
    }
    let exe = std::env::var_os("BROWSER_CONTROL_BIN")
        .map(std::path::PathBuf::from)
        .map(Ok)
        .unwrap_or_else(std::env::current_exe)
        .context("locating the browser-control executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args([
        "tab",
        "foreground-hold",
        browser_name,
        target_id,
        "--timeout-s",
        &timeout.as_secs().to_string(),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group: survives the parent's shell job control and
        // Ctrl-C in the terminal that started it.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    let mut child = cmd.spawn().context("spawning the foreground holder")?;
    let pid = child.id();
    let deadline = std::time::Instant::now() + SPAWN_WAIT;
    loop {
        if let Some(row) = registry.foreground_get(browser_name, target_id)? {
            if row.pid == pid {
                // Reap the child whenever it exits so it never lingers as a
                // zombie (which `pid_alive` would otherwise keep reporting
                // as a live holder in a long-lived MCP server).
                std::thread::Builder::new()
                    .name("foreground-holder-reaper".into())
                    .spawn(move || {
                        let _ = child.wait();
                    })
                    .ok();
                return Ok((pid, true));
            }
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "foreground holder exited before attaching ({status}); is the tab still open and the browser a Chromium?"
            );
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            bail!("foreground holder did not attach within {SPAWN_WAIT:?}");
        }
        std::thread::sleep(SPAWN_POLL);
    }
}

/// Stop the holder for the tab, if any. Returns whether one was running.
pub fn stop_holder(registry: &Registry, browser_name: &str, target_id: &str) -> Result<bool> {
    let Some(row) = registry.foreground_get(browser_name, target_id)? else {
        return Ok(false);
    };
    terminate(row.pid);
    // The holder deletes its own row on a clean exit; make sure it is gone
    // even if it was killed hard.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while crate::registry::pid_alive(row.pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(SPAWN_POLL);
    }
    registry.foreground_delete(browser_name, target_id)?;
    Ok(true)
}

/// Stop every holder on a browser. Returns how many were running.
pub fn stop_all(registry: &Registry, browser_name: &str) -> Result<usize> {
    let rows = registry.foreground_list(browser_name)?;
    let mut n = 0;
    for r in rows {
        if stop_holder(registry, browser_name, &r.target_id)? {
            n += 1;
        }
    }
    Ok(n)
}

/// Ask a holder to exit. SIGTERM on Unix so it can disable the emulation
/// and detach cleanly; `TerminateProcess` on Windows (the session drop
/// reverts the emulation anyway).
fn terminate(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: plain syscall with a pid we recorded ourselves.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let mut sys = sysinfo::System::new();
        let p = sysinfo::Pid::from_u32(pid);
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[p]), true);
        if let Some(proc_) = sys.process(p) {
            proc_.kill();
        }
    }
}

/// Body of the hidden `tab foreground-hold` subcommand: attach, emulate,
/// record, and block until told to stop or the tab/browser goes away.
pub async fn hold(browser_name: &str, target_id: &str, timeout: Duration) -> Result<()> {
    let registry = Registry::open()?;
    let row = registry
        .get_by_name(browser_name)?
        .ok_or_else(|| anyhow!("no registered browser named {browser_name}"))?;
    if row.engine != Engine::Cdp {
        bail!("foreground emulation is Chromium-only (Firefox has no BiDi equivalent)");
    }
    let TabBackend::Cdp(client) = open_backend(&row.endpoint, Engine::Cdp).await? else {
        bail!("expected a CDP backend");
    };
    let mut events = client.subscribe();
    // Browser-level target events tell us when our tab is closed.
    let _ = client
        .send("Target.setDiscoverTargets", json!({ "discover": true }))
        .await;
    let session_id = client.attach_to_target(target_id).await?;
    let _ = client
        .send_with_session("Inspector.enable", json!({}), Some(&session_id))
        .await;
    apply(&client, &session_id, true).await?;
    let expires_at = crate::registry::now_epoch_s() + timeout.as_secs() as i64;
    registry.foreground_upsert(browser_name, target_id, std::process::id(), expires_at)?;
    tracing::info!(
        browser = browser_name,
        target = target_id,
        ?timeout,
        "foreground emulation held"
    );

    let reason = wait_for_exit(&mut events, target_id, &session_id, timeout).await;
    tracing::info!(browser = browser_name, target = target_id, %reason, "foreground holder exiting");
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let _ = apply(&client, &session_id, false).await;
        let _ = client
            .send(
                "Target.detachFromTarget",
                json!({ "sessionId": session_id }),
            )
            .await;
    })
    .await;
    registry.foreground_delete(browser_name, target_id)?;
    Ok(())
}

/// Block until a stop signal or a CDP event says the tab or browser is gone.
async fn wait_for_exit(
    events: &mut broadcast::Receiver<CdpEvent>,
    target_id: &str,
    session_id: &str,
    timeout: Duration,
) -> &'static str {
    let stop = stop_signal();
    tokio::pin!(stop);
    // One deadline, requested by the user: the holder's own expiry.
    let expiry = tokio::time::sleep(timeout);
    tokio::pin!(expiry);
    loop {
        tokio::select! {
            _ = &mut stop => return "stop requested",
            _ = &mut expiry => return "timeout reached",
            ev = events.recv() => match ev {
                Ok(ev) => {
                    let ours_session = ev.session_id.as_deref() == Some(session_id);
                    match ev.method.as_str() {
                        "Target.targetDestroyed" | "Target.targetCrashed"
                            if ev.params["targetId"].as_str() == Some(target_id) =>
                        {
                            return "tab closed";
                        }
                        "Target.detachedFromTarget"
                            if ev.params["sessionId"].as_str() == Some(session_id) =>
                        {
                            return "session detached";
                        }
                        "Inspector.detached" | "Inspector.targetCrashed" if ours_session => {
                            return "tab detached";
                        }
                        _ => {}
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return "browser connection closed",
            },
        }
    }
}

#[cfg(unix)]
async fn stop_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn stop_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
