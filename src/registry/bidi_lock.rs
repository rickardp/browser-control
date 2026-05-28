//! `bidi_locks` SQLite table: arbitrates the Firefox single-BiDi-session
//! limit across concurrent CLI processes.
//!
//! Firefox allows one BiDi session per browser at a time. Two `browser-control`
//! invocations targeting the same Firefox would otherwise race on
//! `session.new` and one would lose with "Maximum number of active
//! sessions". The current mitigation (`session.end` on close +
//! retry-on-collision) handles the common case but leaves a tight race
//! window. This lock closes it: a CLI acquires `bidi_locks(browser_name)`
//! before opening a BiDi session and releases on `Drop`.
//!
//! Crashed-CLI safety: each row carries the holder's PID. On contention,
//! we check `pid_alive(holder_pid)`; if dead, the row is evicted and the
//! contender takes the lock. This avoids the need for any background
//! cleanup task.
//!
//! Granularity: per `browser_name`, not engine. Chromium callers don't
//! touch this table; only `PageSession::attach` on the BiDi engine path
//! acquires.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use thiserror::Error;

use crate::registry::{now_epoch_s, pid_alive, Registry};

/// Default poll interval while waiting on a contended lock. Short enough
/// to feel responsive on release, long enough to avoid burning CPU.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Returned by [`Registry::bidi_lock_acquire`] when `timeout` elapses
/// before the lock can be taken. Carries the holder PID for diagnostics
/// so an agent can surface "another `browser-control` (PID N) is using
/// Firefox BiDi" instead of a generic "timeout".
#[derive(Debug, Error)]
#[error("Firefox BiDi for {browser_name} is held by PID {holder_pid} (waited {waited_ms}ms)")]
pub struct BidiLockBusy {
    pub browser_name: String,
    pub holder_pid: u32,
    pub waited_ms: u64,
}

/// RAII guard for an acquired BiDi lock. Drop releases the row.
///
/// The release uses CAS-style `DELETE WHERE browser_name=? AND
/// holder_pid=?` so a double-release or a release after the row has been
/// evicted by a stale-PID sweep is harmless (won't delete someone else's
/// row).
#[derive(Debug)]
pub struct BidiLockGuard {
    browser_name: String,
    holder_pid: u32,
    db_path: std::path::PathBuf,
}

impl BidiLockGuard {
    pub fn browser_name(&self) -> &str {
        &self.browser_name
    }
    pub fn holder_pid(&self) -> u32 {
        self.holder_pid
    }
}

impl Drop for BidiLockGuard {
    fn drop(&mut self) {
        // Talk to SQLite directly via a raw `rusqlite::Connection`. We
        // can't reach back into the parent `Registry` from Drop without
        // significant lifetime gymnastics, and the DELETE is a single
        // atomic statement — SQLite's own concurrency (WAL +
        // busy_timeout) covers serialisation against other writers.
        if let Ok(conn) = rusqlite::Connection::open(&self.db_path) {
            let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
            let _ = conn.execute(
                "DELETE FROM bidi_locks WHERE browser_name = ?1 AND holder_pid = ?2",
                rusqlite::params![self.browser_name, self.holder_pid],
            );
        }
    }
}

impl Registry {
    /// Acquire the BiDi lock for `browser_name`. Blocks until granted or
    /// `timeout` elapses. On contention, evicts a stale row whose holder
    /// PID is no longer alive.
    ///
    /// Polling rather than file-locking on the row: the registry's
    /// process-level file lock already serializes connections, and the
    /// acquire path is single-row + atomic. We re-poll every
    /// [`POLL_INTERVAL`] until success or `timeout`.
    pub fn bidi_lock_acquire(
        &self,
        browser_name: &str,
        timeout: Duration,
    ) -> Result<BidiLockGuard> {
        let my_pid = std::process::id();
        let start = Instant::now();
        loop {
            // Try to insert our row. If a row already exists for this
            // browser, the INSERT fails with a UNIQUE constraint — that's
            // the contention path.
            let now = now_epoch_s();
            let attempt = self.conn.execute(
                "INSERT INTO bidi_locks (browser_name, holder_pid, acquired_at_epoch_s) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![browser_name, my_pid, now],
            );
            match attempt {
                Ok(_) => {
                    return Ok(BidiLockGuard {
                        browser_name: browser_name.to_string(),
                        holder_pid: my_pid,
                        db_path: self.db_path.clone(),
                    });
                }
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    // Contended. Check if the holder is still alive; if not,
                    // evict and retry immediately.
                    if let Some(existing) = self.bidi_lock_holder(browser_name)? {
                        if !pid_alive(existing.holder_pid) {
                            // Stale row — evict by CAS so we don't race
                            // with the actual holder if they came back.
                            let _ = self.conn.execute(
                                "DELETE FROM bidi_locks \
                                 WHERE browser_name = ?1 AND holder_pid = ?2",
                                rusqlite::params![browser_name, existing.holder_pid],
                            );
                            continue;
                        }
                        // Still alive. If we've hit the timeout, escalate
                        // with the holder's PID in the typed error.
                        if start.elapsed() >= timeout {
                            return Err(BidiLockBusy {
                                browser_name: browser_name.to_string(),
                                holder_pid: existing.holder_pid,
                                waited_ms: start.elapsed().as_millis() as u64,
                            }
                            .into());
                        }
                        // Wait and retry.
                        std::thread::sleep(POLL_INTERVAL);
                    } else {
                        // Row vanished between INSERT and lookup — race
                        // with another contender. Retry immediately.
                        continue;
                    }
                }
                Err(e) => {
                    return Err(anyhow!(e))
                        .with_context(|| format!("acquire bidi_lock for {browser_name}"));
                }
            }
        }
    }

    /// Read the current lock holder for diagnostics / tests. Returns
    /// `None` if no row exists.
    pub fn bidi_lock_holder(&self, browser_name: &str) -> Result<Option<BidiLockRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT browser_name, holder_pid, acquired_at_epoch_s \
             FROM bidi_locks WHERE browser_name = ?1",
        )?;
        let mut rows = stmt.query([browser_name])?;
        if let Some(r) = rows.next()? {
            Ok(Some(BidiLockRow {
                browser_name: r.get(0)?,
                holder_pid: r.get::<_, i64>(1)? as u32,
                acquired_at_epoch_s: r.get(2)?,
            }))
        } else {
            Ok(None)
        }
    }
}

/// Read-only view of a row in `bidi_locks`. Used by `bidi_lock_holder`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BidiLockRow {
    pub browser_name: String,
    pub holder_pid: u32,
    pub acquired_at_epoch_s: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_at(path: &std::path::Path) -> Registry {
        Registry::open_at(path).unwrap()
    }

    #[test]
    fn acquire_when_unlocked_succeeds_immediately() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let reg = fresh_at(tmp.path());
        let guard = reg
            .bidi_lock_acquire("brave", Duration::from_secs(1))
            .unwrap();
        assert_eq!(guard.holder_pid(), std::process::id());
        let holder = reg.bidi_lock_holder("brave").unwrap().unwrap();
        assert_eq!(holder.holder_pid, std::process::id());
    }

    #[test]
    fn drop_releases_the_lock() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let reg = fresh_at(tmp.path());
        {
            let _guard = reg.bidi_lock_acquire("b", Duration::from_secs(1)).unwrap();
            assert!(reg.bidi_lock_holder("b").unwrap().is_some());
        }
        // After drop, the row should be gone.
        assert!(reg.bidi_lock_holder("b").unwrap().is_none());
    }

    #[test]
    fn contention_against_self_pid_times_out_with_typed_error() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let reg = fresh_at(tmp.path());
        let _g = reg.bidi_lock_acquire("b", Duration::from_secs(1)).unwrap();
        // Acquire again immediately — the existing row is our own PID
        // (still alive). Should time out with BidiLockBusy.
        let start = Instant::now();
        let err = reg
            .bidi_lock_acquire("b", Duration::from_millis(250))
            .expect_err("must time out");
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(200));
        assert!(elapsed < Duration::from_secs(1));
        let typed = err
            .downcast_ref::<BidiLockBusy>()
            .expect("typed BidiLockBusy");
        assert_eq!(typed.holder_pid, std::process::id());
    }

    #[test]
    fn stale_pid_holder_is_evicted_on_acquire() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let reg = fresh_at(tmp.path());
        // Plant a synthetic row with a PID that almost certainly does
        // not exist on this machine. `u32::MAX` is reserved on most
        // platforms; using a sentinel keeps the test cheap.
        let dead_pid: u32 = 1; // PID 1 is init; not us. We use a clearly-bogus number instead.
        let bogus_pid: u32 = 999_999_999;
        reg.conn
            .execute(
                "INSERT INTO bidi_locks (browser_name, holder_pid, acquired_at_epoch_s) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params!["b", bogus_pid, crate::registry::now_epoch_s()],
            )
            .unwrap();
        let _ = dead_pid;
        // Acquire should evict the bogus row and grant the lock to us.
        let guard = reg.bidi_lock_acquire("b", Duration::from_secs(2)).unwrap();
        assert_eq!(guard.holder_pid(), std::process::id());
    }
}
