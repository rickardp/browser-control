//! SQLite schema for the browser registry.

use anyhow::{Context, Result};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS browsers (
  name        TEXT PRIMARY KEY,
  kind        TEXT NOT NULL,
  engine      TEXT NOT NULL,
  pid         INTEGER NOT NULL,
  endpoint    TEXT NOT NULL,
  port        INTEGER NOT NULL,
  profile_dir TEXT NOT NULL,
  executable  TEXT NOT NULL,
  headless    INTEGER NOT NULL,
  started_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS browsers_kind_started ON browsers(kind, started_at DESC);

-- One scratch tab per browser. Used by lock-free ops (eval/fetch with no
-- explicit tab) so they never touch a user-visible tab. The single-row
-- shape is deliberate: concurrent CLI calls share the same scratch row
-- (CDP allows multiple sessions per target), and recovery just rewrites
-- target_id when the existing scratch is dead.
CREATE TABLE IF NOT EXISTS scratches (
  browser_name        TEXT PRIMARY KEY,
  target_id           TEXT NOT NULL,
  last_used_at_epoch_s INTEGER NOT NULL
);

-- Named tabs, addressed as `<browser>/<name>` across the CLI. `daemon_created`
-- distinguishes tabs the CLI opened (eligible for sweep / LRU recycle) from
-- tabs adopted from the user (kept verbatim, never GC'd).
CREATE TABLE IF NOT EXISTS tabs (
  browser_name         TEXT NOT NULL,
  name                 TEXT NOT NULL,
  target_id            TEXT NOT NULL,
  last_url             TEXT NOT NULL,
  last_used_at_epoch_s INTEGER NOT NULL,
  daemon_created       INTEGER NOT NULL,
  PRIMARY KEY (browser_name, name)
);
CREATE INDEX IF NOT EXISTS tabs_lru ON tabs(browser_name, daemon_created, last_used_at_epoch_s);

-- Firefox BiDi allows one session per browser. This table arbitrates among
-- concurrent CLI processes: holder_pid is the winning process, released
-- on Drop (DELETE WHERE browser_name=? AND holder_pid=?). Stale rows from
-- crashed CLIs are evicted on acquire via pid_alive().
CREATE TABLE IF NOT EXISTS bidi_locks (
  browser_name         TEXT PRIMARY KEY,
  holder_pid           INTEGER NOT NULL,
  acquired_at_epoch_s  INTEGER NOT NULL
);

-- Foreground emulation holders (ADR-004): one detached
-- `browser-control tab foreground-hold` process per emulated tab. `pid` is
-- the holder; rows whose pid is dead are evicted on read.
CREATE TABLE IF NOT EXISTS foreground_holders (
  browser_name         TEXT NOT NULL,
  target_id            TEXT NOT NULL,
  pid                  INTEGER NOT NULL,
  started_at_epoch_s   INTEGER NOT NULL,
  expires_at_epoch_s   INTEGER NOT NULL,
  PRIMARY KEY (browser_name, target_id)
);
"#;

/// Apply the schema migration. Idempotent.
pub fn apply(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)
        .context("applying registry schema")?;
    Ok(())
}
