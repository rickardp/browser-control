//! Shared error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BrowserControlError {
    #[error("browser not found: {0}")]
    NotFound(String),
    #[error("no running browser matches the selector")]
    NoRunningMatch,
    #[error("invalid BROWSER_CONTROL value: {0}")]
    InvalidSelector(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Typed errors raised from the page-session layer. These exist so the daemon
/// (and tests) can pattern-match on the failure category — in particular,
/// `TabHung` is the catch-all for the alive-but-unresponsive renderer case
/// that has no protocol event signal.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Op exceeded its per-call timeout without a reply and without a crash
    /// event. The most likely cause is a wedged renderer (service-worker
    /// suspended page, infinite loop, modal dialog, etc.). The agent should
    /// treat the tab as unusable until reloaded or recreated.
    #[error("tab hung: op exceeded {timeout_ms}ms without reply ({hint}) [target={target_id:?} url={url:?}]")]
    TabHung {
        target_id: Option<String>,
        url: Option<String>,
        timeout_ms: u64,
        hint: &'static str,
    },
    /// Renderer crashed mid-op (observed via `Target.targetCrashed` /
    /// `Inspector.targetCrashed`). Distinct from `TabHung` so callers can
    /// distinguish "definitely dead" from "presumed wedged."
    #[error("tab crashed: {reason} [target={target_id:?}]")]
    TabCrashed { target_id: String, reason: String },
}
