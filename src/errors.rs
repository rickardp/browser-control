//! Shared error types.

use thiserror::Error;

/// Typed errors raised from the page-session layer so callers (and tests) can
/// pattern-match on the failure category — in particular, `TabHung` is the
/// catch-all for the alive-but-unresponsive renderer case that has no
/// protocol event signal.
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
    /// `<browser>/<name>` referenced a tab that doesn't exist in the
    /// `tabs` registry. Agents see this when they reference a tab they
    /// haven't `tab open`'d yet — the recovery is to open it first.
    /// Distinct from `TabHung` because the tab was never there to wedge.
    #[error("no tab `{name}` registered for browser `{browser}` — run `browser-control tab open {browser}/{name}` first")]
    TabNotFound { browser: String, name: String },
    /// Protocol-level error indicating the underlying target/session/context
    /// referenced by the request no longer exists in the browser. Raised by
    /// the CDP/BiDi client layer when the server returns a recognised
    /// "gone" code (e.g. `no target with given id`, `no such frame`). The
    /// recover-once wrappers treat this as a signal to recreate the tab
    /// and retry; otherwise it would surface as a generic protocol error.
    #[error("{kind:?} target gone: {details}")]
    TargetGone { kind: TargetKind, details: String },
    /// The requested operation cannot run against the currently selected
    /// browser engine. Used by the Playwright sidecar tools (snapshot,
    /// click, type, etc.) when the active browser is BiDi (Firefox) —
    /// Playwright can't drive a user-launched Firefox. The agent's
    /// recovery is to `browser_select` a Chromium-family browser, or
    /// to use the engine-agnostic tools (`browser_get_html`,
    /// `browser_fetch`, `browser_navigate`, etc.) which work on both engines.
    #[error("tool `{tool}` requires {required_engine} engine; current browser uses {current_engine} ({hint})")]
    EngineUnsupported {
        tool: String,
        required_engine: String,
        current_engine: String,
        hint: &'static str,
    },
}

/// Which protocol surfaced a `TargetGone`. Useful for diagnostics; the
/// recovery path treats both kinds identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Cdp,
    Bidi,
}

/// Substrings that, when found in a CDP error `message`, indicate the
/// target/session referenced by the call no longer exists. Centralised so
/// the client-layer classifier and the recovery-wrapper fallback agree.
pub const CDP_TARGET_GONE_NEEDLES: &[&str] = &[
    "no target with given id",
    "session is gone",
    "no session with given id",
    "target closed",
];

/// Substrings that indicate a BiDi context/frame/session is gone. BiDi
/// also surfaces context-gone via the dedicated error codes
/// `no such frame` / `no such context` etc.; we match on message text so
/// we catch both shapes.
pub const BIDI_TARGET_GONE_NEEDLES: &[&str] = &[
    "no such frame",
    "no such node",
    "no such context",
    "invalid session id",
];

/// Returns true if `message` matches any CDP "target gone" indicator.
pub fn is_cdp_target_gone(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    CDP_TARGET_GONE_NEEDLES.iter().any(|n| m.contains(n))
}

/// Returns true if `code` or `message` indicates a BiDi context is gone.
/// BiDi codes are stable per spec (`no such frame`, etc.), but we also
/// scan the message in case the code is generic.
pub fn is_bidi_target_gone(code: &str, message: &str) -> bool {
    let c = code.to_ascii_lowercase();
    if BIDI_TARGET_GONE_NEEDLES.iter().any(|n| c.contains(n)) {
        return true;
    }
    let m = message.to_ascii_lowercase();
    BIDI_TARGET_GONE_NEEDLES.iter().any(|n| m.contains(n))
}

/// Single shared predicate for "the tab the op targeted is dead; one
/// recover-and-retry round is appropriate." Used by `with_scratch_recovery`,
/// `with_named_tab_recovery`, and the origin-bound evaluate helper.
/// Keeping these in one place avoids the call
/// sites drifting apart on what counts as recoverable.
pub fn is_recoverable_tab_failure(err: &anyhow::Error) -> bool {
    if let Some(se) = err.downcast_ref::<SessionError>() {
        return matches!(
            se,
            SessionError::TabHung { .. }
                | SessionError::TabCrashed { .. }
                | SessionError::TargetGone { .. }
        );
    }
    let msg = format!("{err:#}").to_ascii_lowercase();
    CDP_TARGET_GONE_NEEDLES.iter().any(|n| msg.contains(n))
        || BIDI_TARGET_GONE_NEEDLES.iter().any(|n| msg.contains(n))
}
