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
"#;

/// Apply the schema migration. Idempotent.
pub fn apply(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)
        .context("applying registry schema")?;
    Ok(())
}
