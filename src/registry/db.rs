//! Private SQLite access helpers shared across the registry submodules.
//!
//! These DRY the three boilerplate shapes that were previously copy-pasted
//! across `mod.rs`, `tabs.rs`, `scratches.rs`, and `bidi_lock.rs`:
//!   * execute-with-context: run a statement, wrap any error with a label;
//!   * query-optional: `prepare` + `query` + first row mapped to `Option<T>`;
//!   * query-vec: `prepare` + `query` + all rows mapped to `Vec<T>`.
//!
//! All helpers take a borrowed `&rusqlite::Connection` rather than `&Registry`
//! so they are reusable from `BidiLockGuard::drop`, which opens a fresh
//! connection and cannot borrow the parent `Registry`.

use anyhow::{Context, Result};
use rusqlite::{Connection, Row};
use std::fmt::Display;

/// Execute a statement with params, wrapping any error with `context`.
///
/// `context` is produced lazily so the (often `format!`-built) label is only
/// allocated on the error path, matching the previous `with_context(|| …)`
/// call sites.
pub(crate) fn execute<P, C, F>(conn: &Connection, sql: &str, params: P, context: F) -> Result<()>
where
    P: rusqlite::Params,
    C: Display + Send + Sync + 'static,
    F: FnOnce() -> C,
{
    conn.execute(sql, params).with_context(context)?;
    Ok(())
}

/// Execute a statement with params, propagating the raw error unwrapped.
///
/// Used by the call sites that historically did `self.conn.execute(…)?` with
/// no `with_context` label (touch / set_url / delete on tabs & scratches).
pub(crate) fn execute_bare<P>(conn: &Connection, sql: &str, params: P) -> Result<()>
where
    P: rusqlite::Params,
{
    conn.execute(sql, params)?;
    Ok(())
}

/// `prepare` + `query` returning the first row mapped through `map`, or `None`.
pub(crate) fn query_optional<P, T, F>(
    conn: &Connection,
    sql: &str,
    params: P,
    map: F,
) -> Result<Option<T>>
where
    P: rusqlite::Params,
    F: FnOnce(&Row<'_>) -> Result<T>,
{
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params)?;
    if let Some(r) = rows.next()? {
        Ok(Some(map(r)?))
    } else {
        Ok(None)
    }
}

/// `prepare` + `query` mapping every row through `map` into a `Vec`.
pub(crate) fn query_vec<P, T, F>(conn: &Connection, sql: &str, params: P, map: F) -> Result<Vec<T>>
where
    P: rusqlite::Params,
    F: Fn(&Row<'_>) -> Result<T>,
{
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push(map(r)?);
    }
    Ok(out)
}
