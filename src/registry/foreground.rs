//! `foreground_holders` SQLite table: one row per tab whose foreground
//! emulation is currently held by a `browser-control tab foreground-hold`
//! process (see `crate::session::foreground`). Rows whose `pid` is dead are
//! evicted lazily on read, like `bidi_locks`.

use anyhow::Result;

use crate::registry::{db, now_epoch_s, pid_alive, Registry};

/// In-memory view of a `foreground_holders` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundRow {
    pub browser_name: String,
    pub target_id: String,
    pub pid: u32,
    pub started_at_epoch_s: i64,
    /// When the holder stops on its own.
    pub expires_at_epoch_s: i64,
}

fn row_to_fg(r: &rusqlite::Row<'_>) -> Result<ForegroundRow> {
    Ok(ForegroundRow {
        browser_name: r.get(0)?,
        target_id: r.get(1)?,
        pid: r.get::<_, i64>(2)? as u32,
        started_at_epoch_s: r.get(3)?,
        expires_at_epoch_s: r.get(4)?,
    })
}

impl Registry {
    /// Record that `pid` holds foreground emulation for a tab.
    pub fn foreground_upsert(
        &self,
        browser_name: &str,
        target_id: &str,
        pid: u32,
        expires_at_epoch_s: i64,
    ) -> Result<()> {
        db::execute(
            &self.conn,
            "INSERT INTO foreground_holders \
                (browser_name, target_id, pid, started_at_epoch_s, expires_at_epoch_s) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(browser_name, target_id) DO UPDATE SET \
                pid = excluded.pid, \
                started_at_epoch_s = excluded.started_at_epoch_s, \
                expires_at_epoch_s = excluded.expires_at_epoch_s",
            rusqlite::params![
                browser_name,
                target_id,
                pid as i64,
                now_epoch_s(),
                expires_at_epoch_s
            ],
            || format!("upsert foreground holder {browser_name}/{target_id}"),
        )
    }

    /// The live holder for a tab, evicting a stale row whose process died.
    pub fn foreground_get(
        &self,
        browser_name: &str,
        target_id: &str,
    ) -> Result<Option<ForegroundRow>> {
        let row = db::query_optional(
            &self.conn,
            "SELECT browser_name, target_id, pid, started_at_epoch_s, expires_at_epoch_s \
             FROM foreground_holders WHERE browser_name = ?1 AND target_id = ?2",
            rusqlite::params![browser_name, target_id],
            row_to_fg,
        )?;
        match row {
            Some(r) if pid_alive(r.pid) => Ok(Some(r)),
            Some(r) => {
                self.foreground_delete(browser_name, target_id)?;
                let _ = r;
                Ok(None)
            }
            None => Ok(None),
        }
    }

    /// All live holders for a browser; dead rows are pruned.
    pub fn foreground_list(&self, browser_name: &str) -> Result<Vec<ForegroundRow>> {
        let rows = db::query_vec(
            &self.conn,
            "SELECT browser_name, target_id, pid, started_at_epoch_s, expires_at_epoch_s \
             FROM foreground_holders WHERE browser_name = ?1 ORDER BY target_id",
            [browser_name],
            row_to_fg,
        )?;
        let mut live = Vec::with_capacity(rows.len());
        for r in rows {
            if pid_alive(r.pid) {
                live.push(r);
            } else {
                self.foreground_delete(&r.browser_name, &r.target_id)?;
            }
        }
        Ok(live)
    }

    /// Delete a holder row. Idempotent.
    pub fn foreground_delete(&self, browser_name: &str, target_id: &str) -> Result<()> {
        db::execute_bare(
            &self.conn,
            "DELETE FROM foreground_holders WHERE browser_name = ?1 AND target_id = ?2",
            rusqlite::params![browser_name, target_id],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_registry() -> (tempfile::TempDir, Registry) {
        let td = tempfile::TempDir::new().unwrap();
        let reg = Registry::open_at(&td.path().join("registry.db")).unwrap();
        (td, reg)
    }

    #[test]
    fn upsert_get_list_and_prune_dead_pid() {
        let (_td, reg) = tmp_registry();
        let me = std::process::id();
        reg.foreground_upsert("brave-x", "T1", me, 0).unwrap();
        reg.foreground_upsert("brave-x", "T2", 999_999_999, 0)
            .unwrap();
        let live = reg.foreground_get("brave-x", "T1").unwrap().unwrap();
        assert_eq!(live.pid, me);
        assert!(reg.foreground_get("brave-x", "T2").unwrap().is_none());
        let list = reg.foreground_list("brave-x").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].target_id, "T1");
        reg.foreground_delete("brave-x", "T1").unwrap();
        assert!(reg.foreground_list("brave-x").unwrap().is_empty());
    }
}
