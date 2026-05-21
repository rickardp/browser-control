//! Daemon bringup primitives: socket path resolution, atomic state file,
//! single-bringup lock, stale detection.
//!
//! Layout under `<data_dir>/daemon/<browser-name>/`:
//! - `bringup.lock` — flock'd while a process attempts to start the daemon
//! - `state.json`   — atomic state document (written via tempfile + rename)
//! - `daemon.sock`  — UDS endpoint (Unix only)
//! - `pipe.name`    — pipe name (Windows; file content is the pipe name)

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::paths::data_dir;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DaemonState {
    Starting,
    Ready,
    Stopping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRecord {
    pub browser_name: String,
    pub pid: u32,
    pub state: DaemonState,
    pub endpoint: PathBuf,
    pub started_at_epoch_s: i64,
    pub daemon_version: String,
    pub schema_version: u32,
}

pub fn daemon_dir(browser_name: &str) -> Result<PathBuf> {
    let dir = data_dir()?.join("daemon").join(sanitize(browser_name));
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}

pub fn endpoint_path(browser_name: &str) -> Result<PathBuf> {
    let dir = daemon_dir(browser_name)?;
    #[cfg(unix)]
    {
        Ok(dir.join("daemon.sock"))
    }
    #[cfg(windows)]
    {
        // We still keep a file under the data dir; transport translates it
        // to a pipe name automatically.
        Ok(dir.join(format!("browser-control-{}", sanitize(browser_name))))
    }
}

pub fn state_path(browser_name: &str) -> Result<PathBuf> {
    Ok(daemon_dir(browser_name)?.join("state.json"))
}

pub fn bringup_lock_path(browser_name: &str) -> Result<PathBuf> {
    Ok(daemon_dir(browser_name)?.join("bringup.lock"))
}

/// Acquire the bringup lock. Returns a guard which releases on drop. Blocks
/// until the lock can be acquired.
pub fn acquire_bringup_lock(browser_name: &str) -> Result<BringupLock> {
    let path = bringup_lock_path(browser_name)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open bringup lock {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("flock {}", path.display()))?;
    Ok(BringupLock { file })
}

pub struct BringupLock {
    file: File,
}

impl Drop for BringupLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Atomically write the state document. Done via tempfile + rename in the
/// same directory so readers either see the old or the new record, never a
/// torn one.
pub fn write_state(browser_name: &str, record: &DaemonRecord) -> Result<()> {
    let target = state_path(browser_name)?;
    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("state path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    let json = serde_json::to_vec_pretty(record)?;
    tmp.write_all(&json)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(&target)
        .map_err(|e| anyhow!("persist state file: {}", e.error))?;
    Ok(())
}

pub fn read_state(browser_name: &str) -> Result<Option<DaemonRecord>> {
    let p = state_path(browser_name)?;
    let bytes = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", p.display())),
    };
    let rec: DaemonRecord =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", p.display()))?;
    Ok(Some(rec))
}

/// Remove the state file (e.g. on graceful shutdown).
pub fn clear_state(browser_name: &str) -> Result<()> {
    let p = state_path(browser_name)?;
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Returns true if `pid` corresponds to a live process owned by the current user.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // signal 0 = no-op probe; succeeds iff process exists and we have perms.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        // Open with limited query rights; if it succeeds the process exists.
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            CloseHandle(h);
            true
        }
    }
}

pub fn now_epoch_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Heuristic stale check: a record is considered stale if its PID is no
/// longer alive, or it's been in `Starting` for too long.
pub fn is_stale(record: &DaemonRecord, starting_grace: Duration) -> bool {
    if !pid_alive(record.pid) {
        return true;
    }
    if matches!(record.state, DaemonState::Starting) {
        let age = now_epoch_s() - record.started_at_epoch_s;
        if age > starting_grace.as_secs() as i64 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn with_env<F: FnOnce()>(dir: &TempDir, f: F) {
        let _guard = crate::test_support::ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("BROWSER_CONTROL_DATA_DIR");
        // SAFETY: tests serialize via ENV_LOCK.
        unsafe { std::env::set_var("BROWSER_CONTROL_DATA_DIR", dir.path()) };
        f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("BROWSER_CONTROL_DATA_DIR", v),
                None => std::env::remove_var("BROWSER_CONTROL_DATA_DIR"),
            }
        }
    }

    #[test]
    fn state_roundtrip() {
        let tmp = TempDir::new().unwrap();
        with_env(&tmp, || {
            let rec = DaemonRecord {
                browser_name: "firefox-lemur".into(),
                pid: 42,
                state: DaemonState::Ready,
                endpoint: endpoint_path("firefox-lemur").unwrap(),
                started_at_epoch_s: now_epoch_s(),
                daemon_version: "0.0.0".into(),
                schema_version: 1,
            };
            write_state("firefox-lemur", &rec).unwrap();
            let got = read_state("firefox-lemur").unwrap().unwrap();
            assert_eq!(got.pid, 42);
            assert_eq!(got.state, DaemonState::Ready);
            clear_state("firefox-lemur").unwrap();
            assert!(read_state("firefox-lemur").unwrap().is_none());
        });
    }

    #[test]
    fn bringup_lock_serializes() {
        let tmp = TempDir::new().unwrap();
        with_env(&tmp, || {
            let l1 = acquire_bringup_lock("brave-alpha").unwrap();
            // Same-thread re-acquire would deadlock; instead just check that
            // the lock file exists and dropping releases it.
            drop(l1);
            let _l2 = acquire_bringup_lock("brave-alpha").unwrap();
        });
    }

    #[test]
    fn stale_when_pid_dead() {
        let rec = DaemonRecord {
            browser_name: "x".into(),
            pid: 1, // PID 1 exists, but on most CI systems we can't signal it,
            // so kill(1, 0) returns EPERM (not ESRCH) and is_stale=false.
            // Use a clearly-dead PID instead:
            state: DaemonState::Ready,
            endpoint: PathBuf::from("/tmp/x.sock"),
            started_at_epoch_s: now_epoch_s(),
            daemon_version: "0.0.0".into(),
            schema_version: 1,
        };
        let mut r = rec;
        r.pid = u32::MAX - 1;
        assert!(is_stale(&r, Duration::from_secs(30)));
    }
}
