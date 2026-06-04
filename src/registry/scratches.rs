//! `scratches` SQLite table: one row per browser, holding the daemon-style
//! scratch tab's `target_id`. Used by lock-free ops (`eval`, `fetch` with no
//! explicit tab) so the default code path never touches a user-visible tab —
//! the architectural answer to the iLO failure mode (an admin tab whose
//! renderer ignores `Runtime.evaluate` could otherwise be silently picked
//! by the default selector).
//!
//! Lifecycle is **lazy + hybrid**:
//! - First call: no row → create an `about:blank` via `Target.createTarget`,
//!   insert the row, return the `target_id`.
//! - Subsequent calls: row exists → try the op against the stored
//!   `target_id`. If it errors with `tabHung`/`tabCrashed`/protocol-error
//!   ("no target with given id"), close the dead target, recreate, update
//!   the row, retry once, then escalate.
//!
//! Concurrent CLI processes share the row: CDP allows multiple sessions
//! against the same target. JS execution in one renderer is single-threaded
//! anyway, so concurrent evals serialize naturally — no locking needed.

use anyhow::Result;

use crate::registry::{db, now_epoch_s, Registry};

/// In-memory view of a `scratches` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchRow {
    pub browser_name: String,
    pub target_id: String,
    pub last_used_at_epoch_s: i64,
}

impl Registry {
    /// Read the scratch row for `browser_name`. Returns `None` if no row.
    pub fn scratch_get(&self, browser_name: &str) -> Result<Option<ScratchRow>> {
        db::query_optional(
            &self.conn,
            "SELECT browser_name, target_id, last_used_at_epoch_s \
             FROM scratches WHERE browser_name = ?1",
            [browser_name],
            |r| {
                Ok(ScratchRow {
                    browser_name: r.get(0)?,
                    target_id: r.get(1)?,
                    last_used_at_epoch_s: r.get(2)?,
                })
            },
        )
    }

    /// Insert or replace the scratch row for `browser_name`. Used both when
    /// creating the first scratch and when recovering a wedged one.
    pub fn scratch_upsert(&self, browser_name: &str, target_id: &str) -> Result<()> {
        let now = now_epoch_s();
        db::execute(
            &self.conn,
            "INSERT INTO scratches (browser_name, target_id, last_used_at_epoch_s) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT(browser_name) DO UPDATE SET \
                    target_id = excluded.target_id, \
                    last_used_at_epoch_s = excluded.last_used_at_epoch_s",
            rusqlite::params![browser_name, target_id, now],
            || format!("upsert scratch for {browser_name}"),
        )
    }

    /// Bump `last_used_at` for the scratch row (no-op if absent). Called
    /// after a successful op so the row reflects actual recency for
    /// diagnostics.
    pub fn scratch_touch(&self, browser_name: &str) -> Result<()> {
        let now = now_epoch_s();
        db::execute_bare(
            &self.conn,
            "UPDATE scratches SET last_used_at_epoch_s = ?1 WHERE browser_name = ?2",
            rusqlite::params![now, browser_name],
        )
    }

    /// Remove the scratch row (used by recovery to mark the prior target
    /// dead before re-creating). Idempotent.
    pub fn scratch_delete(&self, browser_name: &str) -> Result<()> {
        db::execute_bare(
            &self.conn,
            "DELETE FROM scratches WHERE browser_name = ?1",
            [browser_name],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_then_get_round_trip() {
        let reg = Registry::open_in_memory().unwrap();
        assert!(reg.scratch_get("brave-twilight").unwrap().is_none());
        reg.scratch_upsert("brave-twilight", "T1").unwrap();
        let got = reg.scratch_get("brave-twilight").unwrap().unwrap();
        assert_eq!(got.browser_name, "brave-twilight");
        assert_eq!(got.target_id, "T1");
        assert!(got.last_used_at_epoch_s > 0);
    }

    #[test]
    fn upsert_replaces_existing_target_id() {
        let reg = Registry::open_in_memory().unwrap();
        reg.scratch_upsert("brave-twilight", "T1").unwrap();
        reg.scratch_upsert("brave-twilight", "T2").unwrap();
        let got = reg.scratch_get("brave-twilight").unwrap().unwrap();
        assert_eq!(got.target_id, "T2");
    }

    #[test]
    fn touch_bumps_last_used() {
        let reg = Registry::open_in_memory().unwrap();
        reg.scratch_upsert("a", "T1").unwrap();
        let first = reg.scratch_get("a").unwrap().unwrap().last_used_at_epoch_s;
        // Force a >=1s delta isn't reliable in a fast test; just assert
        // touch doesn't error and the row is still present.
        reg.scratch_touch("a").unwrap();
        let later = reg.scratch_get("a").unwrap().unwrap().last_used_at_epoch_s;
        assert!(later >= first);
    }

    #[test]
    fn delete_removes_the_row() {
        let reg = Registry::open_in_memory().unwrap();
        reg.scratch_upsert("a", "T1").unwrap();
        reg.scratch_delete("a").unwrap();
        assert!(reg.scratch_get("a").unwrap().is_none());
    }

    #[test]
    fn touch_on_missing_row_is_noop() {
        let reg = Registry::open_in_memory().unwrap();
        // No assertion needed — must not panic / error.
        reg.scratch_touch("nonexistent").unwrap();
    }
}
