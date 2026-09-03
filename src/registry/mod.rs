//! SQLite-backed registry of running browser instances.

pub mod bidi_lock;
mod db;
pub mod foreground;
pub mod naming;
pub mod schema;
pub mod scratches;
pub mod tabs;
pub mod words;

pub use bidi_lock::{BidiLockBusy, BidiLockGuard, BidiLockRow};
pub use foreground::ForegroundRow;
pub use scratches::ScratchRow;
pub use tabs::TabRow;

use anyhow::{anyhow, bail, Context, Result};
use fs2::FileExt;
use rusqlite::{params, OpenFlags};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::detect::{Engine, Kind};

/// One row in the `browsers` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserRow {
    pub name: String,
    pub kind: Kind,
    pub engine: Engine,
    pub pid: u32,
    pub endpoint: String,
    pub port: u16,
    pub profile_dir: PathBuf,
    pub executable: PathBuf,
    pub headless: bool,
    pub started_at: String,
}

/// Current registry liveness classification for a browser row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserLiveness {
    /// The recorded process exists and the debugging endpoint accepts TCP.
    Alive,
    /// The recorded process no longer exists. This is high-confidence stale
    /// state and can be pruned.
    DeadPid,
    /// The recorded process exists, but the debugging endpoint did not accept
    /// TCP right now. Treat as unavailable, but do not delete the row: short
    /// endpoint outages can be transient during browser startup/restart.
    EndpointUnreachable,
}

/// SQLite registry handle. SQLite-level WAL + `busy_timeout` handles
/// concurrent reader/writer coordination across processes; we no longer
/// hold a long-lived exclusive file lock for the lifetime of the
/// handle. A short exclusive lock is taken only across the
/// schema-migration window in `open_at` to serialise initial
/// `CREATE TABLE` / `CREATE INDEX` against a concurrent first open.
pub struct Registry {
    conn: rusqlite::Connection,
    /// Path the registry was opened at. Read by `bidi_lock_acquire` to
    /// stamp the `BidiLockGuard`, whose `Drop` reopens a fresh connection
    /// here to release the row (the guard outlives the borrowing handle).
    db_path: PathBuf,
}

impl Registry {
    /// Open the registry at the OS-standard location.
    pub fn open() -> Result<Self> {
        let p = crate::paths::registry_db_path()?;
        Self::open_at(&p)
    }

    /// Open the registry at an explicit path.
    pub fn open_at(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating registry dir {}", parent.display()))?;
            }
        }

        // Hold an exclusive file lock only across the initial schema
        // migration. SQLite's own `CREATE TABLE IF NOT EXISTS` is
        // idempotent and safe under concurrent execution, but
        // narrowing the lock to this window keeps the historical
        // serialisation guarantee for any future schema-altering
        // change while letting steady-state CLI invocations proceed
        // in parallel.
        let lock_path = lock_path_for(path);
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening lock file {}", lock_path.display()))?;
        FileExt::lock_exclusive(&lock_file)
            .with_context(|| format!("acquiring exclusive lock on {}", lock_path.display()))?;

        let conn = rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .with_context(|| format!("opening registry db {}", path.display()))?;

        configure_conn(&conn)?;
        schema::apply(&conn)?;

        // Release the migration lock. From here on, SQLite's
        // WAL + busy_timeout handles inter-process coordination
        // for both reads and writes; CLI invocations no longer
        // serialise on browser I/O.
        let _ = FileExt::unlock(&lock_file);
        drop(lock_file);

        Ok(Self {
            conn,
            db_path: path.to_path_buf(),
        })
    }

    /// Open an in-memory registry (tests only). No file lock taken.
    pub fn open_in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory().context("opening in-memory registry")?;
        // WAL is not supported for :memory:; only set synchronous.
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        schema::apply(&conn)?;
        Ok(Self {
            conn,
            db_path: PathBuf::from(":memory:"),
        })
    }

    /// Insert (or replace) a row.
    pub fn insert(&self, row: &BrowserRow) -> Result<()> {
        db::execute(
            &self.conn,
            "INSERT OR REPLACE INTO browsers
                    (name, kind, engine, pid, endpoint, port, profile_dir, executable, headless, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                row.name,
                kind_to_str(row.kind),
                engine_to_str(row.engine),
                row.pid as i64,
                row.endpoint,
                row.port as i64,
                row.profile_dir.to_string_lossy(),
                row.executable.to_string_lossy(),
                row.headless as i64,
                row.started_at,
            ],
            || format!("inserting registry row {}", row.name),
        )
    }

    /// Delete a row by name. No error if it does not exist.
    pub fn delete(&self, name: &str) -> Result<()> {
        db::execute(
            &self.conn,
            "DELETE FROM browsers WHERE name = ?1",
            params![name],
            || format!("deleting registry row {name}"),
        )
    }

    /// Look up a row by name.
    pub fn get_by_name(&self, name: &str) -> Result<Option<BrowserRow>> {
        db::query_optional(
            &self.conn,
            "SELECT name, kind, engine, pid, endpoint, port, profile_dir, executable, headless, started_at FROM browsers WHERE name = ?1",
            params![name],
            row_from_sqlite,
        )
    }

    /// All rows, no liveness check, ordered by started_at DESC.
    pub fn list_all(&self) -> Result<Vec<BrowserRow>> {
        db::query_vec(
            &self.conn,
            "SELECT name, kind, engine, pid, endpoint, port, profile_dir, executable, headless, started_at
             FROM browsers ORDER BY started_at DESC",
            [],
            row_from_sqlite,
        )
    }

    /// All rows of a given kind ordered by started_at DESC, without liveness check.
    pub(crate) fn list_by_kind_all(&self, kind: Kind) -> Result<Vec<BrowserRow>> {
        db::query_vec(
            &self.conn,
            "SELECT name, kind, engine, pid, endpoint, port, profile_dir, executable, headless, started_at
             FROM browsers WHERE kind = ?1 ORDER BY started_at DESC",
            params![kind_to_str(kind)],
            row_from_sqlite,
        )
    }

    /// All alive rows. Dead-PID rows are deleted as a side-effect; rows whose
    /// endpoint is temporarily unreachable are omitted but retained.
    pub fn list_alive(&self) -> Result<Vec<BrowserRow>> {
        let all = self.list_all()?;
        let mut alive = Vec::with_capacity(all.len());
        for row in all {
            match liveness(&row) {
                BrowserLiveness::Alive => alive.push(row),
                BrowserLiveness::DeadPid => {
                    self.delete(&row.name)?;
                }
                BrowserLiveness::EndpointUnreachable => {}
            }
        }
        Ok(alive)
    }

    /// First alive row of the given kind (most recent first). Dead-PID matches
    /// are pruned; endpoint-unreachable rows are retained.
    pub fn first_alive_by_kind(&self, kind: Kind) -> Result<Option<BrowserRow>> {
        for row in self.list_by_kind_all(kind)? {
            match liveness(&row) {
                BrowserLiveness::Alive => return Ok(Some(row)),
                BrowserLiveness::DeadPid => {
                    self.delete(&row.name)?;
                }
                BrowserLiveness::EndpointUnreachable => {}
            }
        }
        Ok(None)
    }

    /// Most recently started alive row across all kinds.
    pub fn most_recent_alive(&self) -> Result<Option<BrowserRow>> {
        for row in self.list_all()? {
            match liveness(&row) {
                BrowserLiveness::Alive => return Ok(Some(row)),
                BrowserLiveness::DeadPid => {
                    self.delete(&row.name)?;
                }
                BrowserLiveness::EndpointUnreachable => {}
            }
        }
        Ok(None)
    }
}

fn configure_conn(conn: &rusqlite::Connection) -> Result<()> {
    // WAL allows concurrent readers + one writer per process group.
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("setting journal_mode = WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .context("setting synchronous = NORMAL")?;
    // `busy_timeout` is what makes concurrent invocations safe now that
    // we no longer hold the long-lived advisory file lock. SQLite will
    // retry a contended write for up to this duration before returning
    // SQLITE_BUSY. Five seconds is generous for our workloads (single
    // INSERT/UPDATE per CLI invocation) and short enough that a stuck
    // process surfaces visibly instead of hanging forever.
    conn.busy_timeout(Duration::from_secs(5))
        .context("setting busy_timeout")?;
    Ok(())
}

fn lock_path_for(db: &Path) -> PathBuf {
    let mut name = db
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("registry.db"));
    name.push(".lock");
    match db.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => PathBuf::from(name),
    }
}

fn row_from_sqlite(r: &rusqlite::Row<'_>) -> Result<BrowserRow> {
    let name: String = r.get(0)?;
    let kind_s: String = r.get(1)?;
    let engine_s: String = r.get(2)?;
    let pid: i64 = r.get(3)?;
    let endpoint: String = r.get(4)?;
    let port: i64 = r.get(5)?;
    let profile_dir: String = r.get(6)?;
    let executable: String = r.get(7)?;
    let headless: i64 = r.get(8)?;
    let started_at: String = r.get(9)?;

    Ok(BrowserRow {
        name,
        kind: parse_kind(&kind_s)?,
        engine: parse_engine(&engine_s)?,
        pid: pid as u32,
        endpoint,
        port: port as u16,
        profile_dir: PathBuf::from(profile_dir),
        executable: PathBuf::from(executable),
        headless: headless != 0,
        started_at,
    })
}

fn kind_to_str(k: Kind) -> &'static str {
    k.as_str()
}

fn parse_kind(s: &str) -> Result<Kind> {
    Kind::parse(s).ok_or_else(|| anyhow!("invalid kind {s}"))
}

fn engine_to_str(e: Engine) -> &'static str {
    match e {
        Engine::Cdp => "cdp",
        Engine::Bidi => "bidi",
    }
}

fn parse_engine(s: &str) -> Result<Engine> {
    match s {
        "cdp" => Ok(Engine::Cdp),
        "bidi" => Ok(Engine::Bidi),
        _ => bail!("invalid engine {s}"),
    }
}

/// Liveness check: PID exists AND a TCP connect to the local port succeeds.
pub fn is_alive(row: &BrowserRow) -> bool {
    liveness(row) == BrowserLiveness::Alive
}

/// Classify row liveness. Only [`BrowserLiveness::DeadPid`] is safe to prune.
pub fn liveness(row: &BrowserRow) -> BrowserLiveness {
    let pid = sysinfo::Pid::from_u32(row.pid);
    let mut sys = sysinfo::System::new();
    // Refresh only the target PID rather than the whole process table:
    // these run per-row in `list_alive`/`first_alive_by_kind`/etc., and we
    // only ever query this one PID below.
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    if sys.process(pid).is_none() {
        return BrowserLiveness::DeadPid;
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], row.port));
    if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
        BrowserLiveness::Alive
    } else {
        BrowserLiveness::EndpointUnreachable
    }
}

/// Cheap check: is process `pid` still running on this machine? Used by
/// stale-row eviction in `scratches`, `tabs`, and `bidi_locks` to avoid
/// keeping rows registered to a crashed CLI process.
pub fn pid_alive(pid: u32) -> bool {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    // Refresh only this PID, not the entire process table — this is hit
    // per-row by stale-row eviction across scratches/tabs/bidi_locks.
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid).is_some()
}

/// Current Unix epoch seconds.
pub fn now_epoch_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// -- ISO-8601 helpers --------------------------------------------------------

/// Current time formatted as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
pub fn now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_unix_seconds_as_iso8601(secs)
}

/// Format a Unix epoch second count as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Uses Howard Hinnant's `civil_from_days` algorithm.
pub fn format_unix_seconds_as_iso8601(secs: i64) -> String {
    // Split into days and time-of-day, handling negative seconds correctly.
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let hour = (tod / 3600) as u32;
    let minute = ((tod % 3600) / 60) as u32;
    let second = (tod % 60) as u32;

    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hour, minute, second
    )
}

/// Convert days since 1970-01-01 to (year, month, day) using Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(name: &str, kind: Kind, port: u16, started_at: &str) -> BrowserRow {
        BrowserRow {
            name: name.to_string(),
            kind,
            engine: kind.engine(),
            pid: 99_999_999, // unlikely to exist
            endpoint: format!("http://127.0.0.1:{port}"),
            port,
            profile_dir: PathBuf::from(format!("/tmp/profiles/{name}")),
            executable: PathBuf::from("/usr/bin/example"),
            headless: false,
            started_at: started_at.to_string(),
        }
    }

    #[test]
    fn insert_then_get_round_trip() {
        let reg = Registry::open_in_memory().unwrap();
        let row = sample_row("alpha-bravo", Kind::Chrome, 9222, "2024-01-02T03:04:05Z");
        reg.insert(&row).unwrap();
        let got = reg.get_by_name("alpha-bravo").unwrap().unwrap();
        assert_eq!(got, row);
        assert!(reg.get_by_name("missing").unwrap().is_none());
    }

    #[test]
    fn list_all_returns_all_rows() {
        let reg = Registry::open_in_memory().unwrap();
        reg.insert(&sample_row("a", Kind::Chrome, 9001, "2024-01-01T00:00:00Z"))
            .unwrap();
        reg.insert(&sample_row(
            "b",
            Kind::Firefox,
            9002,
            "2024-01-02T00:00:00Z",
        ))
        .unwrap();
        reg.insert(&sample_row("c", Kind::Edge, 9003, "2024-01-03T00:00:00Z"))
            .unwrap();
        let all = reg.list_all().unwrap();
        assert_eq!(all.len(), 3);
        // ordered DESC by started_at
        assert_eq!(all[0].name, "c");
        assert_eq!(all[2].name, "a");
    }

    #[test]
    fn delete_removes_row() {
        let reg = Registry::open_in_memory().unwrap();
        let row = sample_row("x", Kind::Brave, 9010, "2024-05-05T05:05:05Z");
        reg.insert(&row).unwrap();
        reg.delete("x").unwrap();
        assert!(reg.get_by_name("x").unwrap().is_none());
        // deleting a missing row is a no-op
        reg.delete("ghost").unwrap();
    }

    #[test]
    fn first_alive_by_kind_returns_most_recent() {
        // We can't easily mock `is_alive`. Instead verify the underlying SQL ordering
        // via the pub(crate) helper.
        let reg = Registry::open_in_memory().unwrap();
        reg.insert(&sample_row(
            "older",
            Kind::Chrome,
            9101,
            "2024-01-01T00:00:00Z",
        ))
        .unwrap();
        reg.insert(&sample_row(
            "newer",
            Kind::Chrome,
            9102,
            "2024-06-01T00:00:00Z",
        ))
        .unwrap();
        reg.insert(&sample_row(
            "ff",
            Kind::Firefox,
            9103,
            "2024-07-01T00:00:00Z",
        ))
        .unwrap();
        let chromes = reg.list_by_kind_all(Kind::Chrome).unwrap();
        assert_eq!(chromes.len(), 2);
        assert_eq!(chromes[0].name, "newer");
        assert_eq!(chromes[1].name, "older");

        // first_alive_by_kind on these synthetic rows should find none alive
        // and prune dead-process entries.
        assert!(reg.first_alive_by_kind(Kind::Chrome).unwrap().is_none());
        assert!(reg.list_by_kind_all(Kind::Chrome).unwrap().is_empty());
    }

    #[test]
    fn list_alive_prunes_stale() {
        let reg = Registry::open_in_memory().unwrap();
        reg.insert(&sample_row("a", Kind::Chrome, 9201, "2024-01-01T00:00:00Z"))
            .unwrap();
        reg.insert(&sample_row("b", Kind::Chrome, 9202, "2024-01-02T00:00:00Z"))
            .unwrap();
        let alive = reg.list_alive().unwrap();
        assert!(alive.is_empty());
        assert!(reg.list_all().unwrap().is_empty());
    }

    #[test]
    fn list_alive_retains_live_pid_with_unreachable_endpoint() {
        let reg = Registry::open_in_memory().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut row = sample_row(
            "temporarily-unreachable",
            Kind::Chrome,
            port,
            "2024-01-01T00:00:00Z",
        );
        row.pid = std::process::id();
        reg.insert(&row).unwrap();

        let alive = reg.list_alive().unwrap();
        assert!(alive.is_empty());
        assert!(reg
            .get_by_name("temporarily-unreachable")
            .unwrap()
            .is_some());
    }

    #[test]
    fn most_recent_alive_with_no_live_rows_is_none() {
        let reg = Registry::open_in_memory().unwrap();
        reg.insert(&sample_row("a", Kind::Chrome, 9301, "2024-01-01T00:00:00Z"))
            .unwrap();
        assert!(reg.most_recent_alive().unwrap().is_none());
    }

    #[test]
    fn now_iso8601_format() {
        let s = now_iso8601();
        assert_eq!(s.len(), 20, "got {s}");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");

        // Epoch sanity.
        assert_eq!(format_unix_seconds_as_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_known_dates() {
        let cases = [
            (0_i64, "1970-01-01T00:00:00Z"),
            (951_782_400, "2000-02-29T00:00:00Z"), // leap day
            (1_700_000_000, "2023-11-14T22:13:20Z"),
            (1_583_020_799, "2020-02-29T23:59:59Z"), // last second of leap day
            (1_583_020_800, "2020-03-01T00:00:00Z"),
            (1_577_836_799, "2019-12-31T23:59:59Z"), // end of year
        ];
        for (secs, want) in cases {
            assert_eq!(format_unix_seconds_as_iso8601(secs), want, "epoch {secs}");
        }
    }

    #[test]
    fn concurrent_file_lock_serializes() {
        use std::thread;

        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("registry.db");

        let p1 = db_path.clone();
        let p2 = db_path.clone();
        let t1 = thread::spawn(move || {
            let reg = Registry::open_at(&p1).unwrap();
            reg.insert(&BrowserRow {
                name: "one".to_string(),
                kind: Kind::Chrome,
                engine: Engine::Cdp,
                pid: 1,
                endpoint: "http://127.0.0.1:9001".to_string(),
                port: 9001,
                profile_dir: PathBuf::from("/tmp/p1"),
                executable: PathBuf::from("/usr/bin/chrome"),
                headless: false,
                started_at: "2024-01-01T00:00:00Z".to_string(),
            })
            .unwrap();
        });
        let t2 = thread::spawn(move || {
            let reg = Registry::open_at(&p2).unwrap();
            reg.insert(&BrowserRow {
                name: "two".to_string(),
                kind: Kind::Firefox,
                engine: Engine::Bidi,
                pid: 2,
                endpoint: "ws://127.0.0.1:9002".to_string(),
                port: 9002,
                profile_dir: PathBuf::from("/tmp/p2"),
                executable: PathBuf::from("/usr/bin/firefox"),
                headless: false,
                started_at: "2024-01-02T00:00:00Z".to_string(),
            })
            .unwrap();
        });
        t1.join().unwrap();
        t2.join().unwrap();

        let reg = Registry::open_at(&db_path).unwrap();
        let all = reg.list_all().unwrap();
        assert_eq!(all.len(), 2);
        let names: Vec<&str> = all.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"one"));
        assert!(names.contains(&"two"));
    }

    #[test]
    fn open_at_creates_parent_dir_and_db() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nested = tmp.path().join("a/b/c/registry.db");
        let reg = Registry::open_at(&nested).unwrap();
        reg.insert(&sample_row("x", Kind::Chrome, 9999, "2024-01-01T00:00:00Z"))
            .unwrap();
        assert!(nested.exists());
        let lock = nested.parent().unwrap().join("registry.db.lock");
        assert!(lock.exists());
    }
}
