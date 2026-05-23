//! `tabs` SQLite table: named tab handles addressable as `<browser>/<name>`
//! across the CLI. Backs the `tab open` / `tab list` subcommands and the
//! cross-command `<browser>/<name>` positional routing.
//!
//! Provenance distinguishes daemon-style (CLI-created) tabs that are
//! eligible for sweep / LRU recycle from user-created tabs the CLI just
//! adopted, which are never GC'd.
//!
//! This module is pure SQL CRUD. Orchestration (talking to CDP to
//! create/navigate/close real tabs, and the sweep-on-read of stale
//! `target_id`s) lives in `crate::session::tabs` so the registry layer
//! stays narrow and testable in isolation.

use anyhow::{Context, Result};

use crate::registry::{now_epoch_s, Registry};

/// In-memory view of a `tabs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabRow {
    pub browser_name: String,
    pub name: String,
    pub target_id: String,
    pub last_url: String,
    pub last_used_at_epoch_s: i64,
    pub daemon_created: bool,
}

impl Registry {
    /// Read a single tab by composite key.
    pub fn tab_get(&self, browser_name: &str, name: &str) -> Result<Option<TabRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT browser_name, name, target_id, last_url, last_used_at_epoch_s, daemon_created \
             FROM tabs WHERE browser_name = ?1 AND name = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![browser_name, name])?;
        if let Some(r) = rows.next()? {
            Ok(Some(row_to_tab(r)?))
        } else {
            Ok(None)
        }
    }

    /// All tabs for a browser, sorted by name (stable for `tab list` output).
    pub fn tabs_list_for(&self, browser_name: &str) -> Result<Vec<TabRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT browser_name, name, target_id, last_url, last_used_at_epoch_s, daemon_created \
             FROM tabs WHERE browser_name = ?1 ORDER BY name ASC",
        )?;
        let mut rows = stmt.query([browser_name])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(row_to_tab(r)?);
        }
        Ok(out)
    }

    /// Insert or replace a tab. Bumps `last_used_at` to "now".
    pub fn tab_upsert(
        &self,
        browser_name: &str,
        name: &str,
        target_id: &str,
        last_url: &str,
        daemon_created: bool,
    ) -> Result<()> {
        let now = now_epoch_s();
        self.conn
            .execute(
                "INSERT INTO tabs \
                   (browser_name, name, target_id, last_url, last_used_at_epoch_s, daemon_created) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(browser_name, name) DO UPDATE SET \
                    target_id = excluded.target_id, \
                    last_url = excluded.last_url, \
                    last_used_at_epoch_s = excluded.last_used_at_epoch_s, \
                    daemon_created = excluded.daemon_created",
                rusqlite::params![
                    browser_name,
                    name,
                    target_id,
                    last_url,
                    now,
                    daemon_created as i32,
                ],
            )
            .with_context(|| format!("upsert tab {browser_name}/{name}"))?;
        Ok(())
    }

    /// Bump `last_used_at` without rewriting other columns. No-op if absent.
    pub fn tab_touch(&self, browser_name: &str, name: &str) -> Result<()> {
        let now = now_epoch_s();
        self.conn.execute(
            "UPDATE tabs SET last_used_at_epoch_s = ?1 \
             WHERE browser_name = ?2 AND name = ?3",
            rusqlite::params![now, browser_name, name],
        )?;
        Ok(())
    }

    /// Rewrite just `last_url` (after a successful `Page.navigate`).
    pub fn tab_set_url(&self, browser_name: &str, name: &str, url: &str) -> Result<()> {
        let now = now_epoch_s();
        self.conn.execute(
            "UPDATE tabs SET last_url = ?1, last_used_at_epoch_s = ?2 \
             WHERE browser_name = ?3 AND name = ?4",
            rusqlite::params![url, now, browser_name, name],
        )?;
        Ok(())
    }

    /// Delete by composite key. Idempotent.
    pub fn tab_delete(&self, browser_name: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM tabs WHERE browser_name = ?1 AND name = ?2",
            rusqlite::params![browser_name, name],
        )?;
        Ok(())
    }

    /// Count `daemon_created = true` rows for a browser. Used by the
    /// orchestration layer to decide when to recycle under budget pressure.
    pub fn tabs_count_daemon_created(&self, browser_name: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tabs \
             WHERE browser_name = ?1 AND daemon_created = 1",
            [browser_name],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// LRU daemon-created row for a browser. Returns the row with the
    /// smallest `last_used_at_epoch_s`, or `None` if no eligible row.
    pub fn tabs_lru_daemon_created(&self, browser_name: &str) -> Result<Option<TabRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT browser_name, name, target_id, last_url, last_used_at_epoch_s, daemon_created \
             FROM tabs \
             WHERE browser_name = ?1 AND daemon_created = 1 \
             ORDER BY last_used_at_epoch_s ASC, name ASC \
             LIMIT 1",
        )?;
        let mut rows = stmt.query([browser_name])?;
        if let Some(r) = rows.next()? {
            Ok(Some(row_to_tab(r)?))
        } else {
            Ok(None)
        }
    }
}

fn row_to_tab(r: &rusqlite::Row<'_>) -> Result<TabRow> {
    Ok(TabRow {
        browser_name: r.get(0)?,
        name: r.get(1)?,
        target_id: r.get(2)?,
        last_url: r.get(3)?,
        last_used_at_epoch_s: r.get(4)?,
        daemon_created: r.get::<_, i64>(5)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Registry {
        Registry::open_in_memory().unwrap()
    }

    #[test]
    fn upsert_then_get_round_trip() {
        let reg = fresh();
        reg.tab_upsert("brave", "scrape-cart", "T1", "https://shop", true)
            .unwrap();
        let got = reg.tab_get("brave", "scrape-cart").unwrap().unwrap();
        assert_eq!(got.target_id, "T1");
        assert_eq!(got.last_url, "https://shop");
        assert!(got.daemon_created);
    }

    #[test]
    fn upsert_replaces_target_id_and_url() {
        let reg = fresh();
        reg.tab_upsert("b", "n", "T1", "u1", true).unwrap();
        reg.tab_upsert("b", "n", "T2", "u2", false).unwrap();
        let got = reg.tab_get("b", "n").unwrap().unwrap();
        assert_eq!(got.target_id, "T2");
        assert_eq!(got.last_url, "u2");
        assert!(!got.daemon_created, "provenance flips on upsert");
    }

    #[test]
    fn list_returns_rows_sorted_by_name() {
        let reg = fresh();
        reg.tab_upsert("b", "zebra", "T1", "", true).unwrap();
        reg.tab_upsert("b", "alpha", "T2", "", true).unwrap();
        reg.tab_upsert("b", "monk", "T3", "", true).unwrap();
        let names: Vec<String> = reg
            .tabs_list_for("b")
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["alpha", "monk", "zebra"]);
    }

    #[test]
    fn list_is_scoped_per_browser() {
        let reg = fresh();
        reg.tab_upsert("a", "x", "T1", "", true).unwrap();
        reg.tab_upsert("b", "y", "T2", "", true).unwrap();
        assert_eq!(reg.tabs_list_for("a").unwrap().len(), 1);
        assert_eq!(reg.tabs_list_for("b").unwrap().len(), 1);
    }

    #[test]
    fn count_excludes_user_created() {
        let reg = fresh();
        reg.tab_upsert("b", "agent1", "T1", "", true).unwrap();
        reg.tab_upsert("b", "agent2", "T2", "", true).unwrap();
        reg.tab_upsert("b", "user-tab", "T3", "", false).unwrap();
        assert_eq!(reg.tabs_count_daemon_created("b").unwrap(), 2);
    }

    #[test]
    fn lru_picks_oldest_daemon_created() {
        let reg = fresh();
        reg.tab_upsert("b", "old-daemon", "T1", "", true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        reg.tab_upsert("b", "new-daemon", "T2", "", true).unwrap();
        reg.tab_upsert("b", "user-tab", "T3", "", false).unwrap();
        let lru = reg.tabs_lru_daemon_created("b").unwrap().unwrap();
        assert_eq!(lru.name, "old-daemon", "LRU honors last_used_at order");
    }

    #[test]
    fn delete_removes_only_named_row() {
        let reg = fresh();
        reg.tab_upsert("b", "x", "T1", "", true).unwrap();
        reg.tab_upsert("b", "y", "T2", "", true).unwrap();
        reg.tab_delete("b", "x").unwrap();
        assert!(reg.tab_get("b", "x").unwrap().is_none());
        assert!(reg.tab_get("b", "y").unwrap().is_some());
    }

    #[test]
    fn set_url_updates_just_the_url() {
        let reg = fresh();
        reg.tab_upsert("b", "n", "T1", "u1", true).unwrap();
        reg.tab_set_url("b", "n", "u2").unwrap();
        let got = reg.tab_get("b", "n").unwrap().unwrap();
        assert_eq!(got.target_id, "T1");
        assert_eq!(got.last_url, "u2");
    }
}
