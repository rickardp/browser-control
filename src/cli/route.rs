//! Shared routing preamble + named-tab dispatch for the page-evaluating CLI
//! commands (`eval`, `fetch`, `storage`).
//!
//! These commands all accept the unified `<browser>[/<tab>]` positional plus a
//! mutually-exclusive `--target <regex>` and fan out into the *same three
//! routing paths*, picked by `(tab_name, target)`:
//!
//! 1. **named-tab** (`<browser>/<tab>`, no `--target`) — resolve `<tab>` in the
//!    `tabs` table via the engine-agnostic [`TabBackend`] and run the op under
//!    [`with_named_tab_recovery`], so a tab that dies between resolve and op is
//!    recovered once. Requires a registered browser. See [`run_named_tab`].
//! 2. **bare browser** (no `--target`) — the per-command default. This arm
//!    *differs per command* (scratch tab for `eval`/`storage`, origin-bound
//!    attach for `fetch`, plus an external-endpoint fallback), so it stays in
//!    each command rather than here.
//! 3. **target-regex** (`--target <regex>`) — legacy [`PageSession::attach`]
//!    against a user tab matching the URL regex. Also per-command (it differs
//!    in await-promise flags / timeouts), so it stays in each command.
//!
//! The genuinely shared pieces — the preamble (parse, mutual-exclusion check,
//! browser resolution, registry handle, BiDi lock) and the named-tab arm — live
//! here so the three commands cannot drift on them. The per-command variation
//! (the JS expression, the await-promise flag, the timeout, and the
//! bare-browser arm) is supplied by the caller.
//!
//! [`PageSession::attach`]: crate::session::PageSession::attach
//! [`TabBackend`]: crate::session::backend::TabBackend

use std::future::Future;

use anyhow::{bail, Result};

use crate::cli::env_resolver::{self, ResolvedBrowser, Source};
use crate::cli::mcp::{acquire_bidi_lock_if_needed, resolve_browser};
use crate::cli::routing::strip_tab;
use crate::cli::trace::CommandTrace;
use crate::registry::{BidiLockGuard, Registry};
use crate::session::backend::{open_backend, TabBackend};
use crate::session::with_named_tab_recovery;

/// Outcome of the shared routing preamble. Holds everything the per-command
/// dispatch needs: the resolved browser, the open [`Registry`] handle, the held
/// BiDi lock guard (RAII — released when this struct drops), and the parsed
/// `(tab_name, browser_only)` from the positional.
///
/// The `_bidi_lock` field is never read directly; it exists to keep the lock
/// held for the lifetime of the route and releases after the command finishes
/// its dispatch.
pub struct Route {
    /// The resolved browser endpoint + engine + source.
    pub resolved: ResolvedBrowser,
    /// Single Registry handle for the route lifetime: the BiDi lock, scratch
    /// row, and named-tab resolution all share it. Re-opening would block on
    /// the per-process file lock.
    pub registry: Registry,
    /// The `<tab>` parsed out of `<browser>/<tab>`, if any.
    pub tab_name: Option<String>,
    /// The positional with any `/<tab>` suffix stripped (the bare browser).
    pub browser_only: String,
    /// Held BiDi single-session lock; RAII-released on drop. `None` for CDP
    /// engines and external URL endpoints.
    _bidi_lock: Option<BidiLockGuard>,
}

/// Run the shared preamble: parse the `<browser>[/<tab>]` positional, enforce
/// the `<tab>` / `--target` mutual exclusion (in ONE place with ONE message),
/// resolve the browser, record `trace.browser`/`trace.engine`, open the
/// registry, and acquire the BiDi lock if applicable.
///
/// `browser` is the raw positional/env value; `target` is `--target`. Returns a
/// [`Route`] the caller dispatches on via `(route.tab_name, target)`.
pub async fn preamble(
    browser: Option<String>,
    target: Option<&str>,
    trace: &mut CommandTrace,
) -> Result<Route> {
    let raw = browser.unwrap_or_default();
    let parsed = if raw.is_empty() {
        None
    } else {
        Some(env_resolver::parse_target(&raw)?)
    };
    let tab_name = parsed.as_ref().and_then(|p| p.tab.clone());
    if tab_name.is_some() && target.is_some() {
        bail!("specify the tab via either `<browser>/<name>` or `--target <regex>`, not both");
    }
    let browser_only = parsed
        .as_ref()
        .map(|p| strip_tab(&raw, p.tab.as_deref()))
        .unwrap_or_default();
    let resolved = resolve_browser(if browser_only.is_empty() {
        None
    } else {
        Some(browser_only.clone())
    })
    .await?;
    trace.browser(&browser_only).engine(resolved.engine);

    // Open the registry once for the route lifetime — used for the BiDi
    // single-session lock, scratch row, and named-tab resolution. Re-opening
    // would block on the per-process file lock.
    let registry = Registry::open()?;
    // Acquire the Firefox BiDi lock if applicable. RAII releases on drop; held
    // across whatever path the caller takes.
    let bidi_lock = acquire_bidi_lock_if_needed(&registry, &resolved)?;

    Ok(Route {
        resolved,
        registry,
        tab_name,
        browser_only,
        _bidi_lock: bidi_lock,
    })
}

/// The named-tab routing arm, identical across `eval`/`fetch`/`storage`:
/// resolve the registered browser name, open its [`TabBackend`], and run the
/// caller's `op` against the named tab under [`with_named_tab_recovery`] (which
/// recovers once if the tab dies between resolve and op).
///
/// `name` is the tab name; `op` evaluates the command-specific JS — the JS
/// expression, await-promise flag, and timeout all live inside the closure, so
/// they are the per-command variation point. `no_external_msg` is the `bail!`
/// text used when the source is not a registered browser; it is passed in
/// because the existing commands phrase it slightly differently and this is a
/// behavior-preserving refactor.
///
/// The caller is responsible for `trace.route("named-tab")` /
/// `trace.tab_name(...)` so the trace contract stays visible at the call site.
pub async fn run_named_tab<T, F, Fut>(
    route: &Route,
    name: &str,
    no_external_msg: &str,
    op: F,
) -> Result<T>
where
    F: FnMut(TabBackend, String) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let browser_name = match &route.resolved.source {
        Source::Registered { name } => name.clone(),
        _ => bail!("{no_external_msg}"),
    };
    let backend = open_backend(&route.resolved.endpoint, route.resolved.engine).await?;
    with_named_tab_recovery(&backend, &route.registry, &browser_name, name, op).await
}
