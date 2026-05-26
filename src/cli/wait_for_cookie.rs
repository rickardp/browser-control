//! `browser-control wait-for-cookie` — block until a cookie appears.
//!
//! v1 strategy: **polling only**. The plan envisions an event-driven path via
//! CDP `Network.responseReceived` / BiDi `network.responseCompleted`, but for
//! v1 simplicity we poll `Network.getAllCookies` / `storage.getCookies` at a
//! fixed interval until the matching cookie appears or the timeout elapses.
//!
//! After a match, an optional `--validate-url` performs a `fetch()` from the
//! page context (credentials included) and requires a 2xx status.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::Value;
use tokio::time::sleep;

use crate::cli::cookies::{fetch_cookies, NormalCookie};
use crate::cli::env_resolver::{self, Source};
use crate::cli::mcp::{acquire_bidi_lock_if_needed, resolve_browser};
use crate::cli::trace::CommandTrace;
use crate::dom::scripts::FETCH_JS;
use crate::registry::Registry;
use crate::session::backend::open_backend;
use crate::session::{with_named_tab_recovery, with_scratch_recovery, PageSession};

/// Per-fetch timeout when `--validate-url` drives a `fetch()` from the
/// page context. 30 s catches a wedged renderer; the outer polling loop
/// reruns this periodically.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run(
    browser: Option<String>,
    domain: String,
    name: String,
    timeout: u64,
    poll_interval: u64,
    validate_url: Option<String>,
) -> Result<()> {
    let mut trace = CommandTrace::new("wait-for-cookie");
    let result: Result<()> = async {
        let domain_re = Regex::new(&domain).context("invalid --domain regex")?;
        let name_re = Regex::new(&name).context("invalid --name regex")?;

        // Parse `<browser>[/<tab>]` so the optional `--validate-url` leg can
        // route through a named tab. The cookie poll itself is browser-wide
        // and uses no tab context.
        let raw = browser.unwrap_or_default();
        let parsed = if raw.is_empty() {
            None
        } else {
            Some(env_resolver::parse_target(&raw)?)
        };
        let tab_name = parsed.as_ref().and_then(|p| p.tab.clone());
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

        // Hold the Firefox BiDi single-session lock across the whole poll
        // loop (and the optional validate-url leg). Each `fetch_cookies`
        // call opens + closes a BiDi session, so without the lock two
        // concurrent CLI processes would race on `session.new`. No-op on CDP.
        let registry = Registry::open()?;
        let _bidi_lock = acquire_bidi_lock_if_needed(&registry, &resolved)?;

        let deadline = Instant::now() + Duration::from_secs(timeout);
        let interval = Duration::from_secs(poll_interval.max(1));

        let matched = loop {
            let cookies = fetch_cookies(&resolved).await?;
            if let Some(c) = cookies
                .into_iter()
                .find(|c| cookie_matches(c, &domain_re, &name_re))
            {
                break c;
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for cookie");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let nap = std::cmp::min(interval, remaining);
            if nap.is_zero() {
                bail!("timed out waiting for cookie");
            }
            sleep(nap).await;
        };

        eprintln!("cookie {} appeared on {}", matched.name, matched.domain);

        if let Some(url) = validate_url {
            // Three-path routing for the validate-url fetch:
            //   - <browser>/<tab>  → named-tab path
            //   - bare <browser>   → scratch path (or direct for external URL)
            // The cookie poll above already finished, so this is the only
            // place a tab context matters.
            run_validate_url(&resolved, &registry, tab_name.as_deref(), &url, &mut trace).await?;
        } else {
            // No validate-url; only the cookie poll ran (browser-wide).
            trace.route("poll");
        }

        println!("{}", matched.name);
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            trace.ok(());
            Ok(())
        }
        Err(e) => Err(trace.err(e)),
    }
}

/// Strip `/<tab>` suffix from a raw `<browser>[/<tab>]` positional.
fn strip_tab(raw: &str, tab: Option<&str>) -> String {
    match tab {
        Some(name) => raw
            .strip_suffix(&format!("/{name}"))
            .unwrap_or(raw)
            .to_string(),
        None => raw.to_string(),
    }
}

/// Drive the `--validate-url` fetch through the right routing path:
/// named-tab (with recover-once), scratch (with recover-once), or direct
/// attach for external URL endpoints.
async fn run_validate_url(
    resolved: &crate::cli::env_resolver::ResolvedBrowser,
    registry: &Registry,
    tab_name: Option<&str>,
    url: &str,
    trace: &mut CommandTrace,
) -> Result<()> {
    let args = serde_json::json!({ "url": url, "method": "GET" }).to_string();
    let expr = format!("({})({})", FETCH_JS, serde_json::to_string(&args).unwrap());

    let value = match tab_name {
        Some(name) => {
            trace.route("named-tab").tab_name(name);
            let browser_name = match &resolved.source {
                Source::Registered { name } => name.clone(),
                _ => bail!(
                    "named tabs (`<browser>/<name>`) require a registered browser; \
                     external endpoints can't carry tab names"
                ),
            };
            let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
            let expr = expr.clone();
            with_named_tab_recovery(
                &backend,
                registry,
                &browser_name,
                name,
                move |b, target_id| {
                    let expr = expr.clone();
                    async move { b.evaluate(&target_id, &expr, true, VALIDATE_TIMEOUT).await }
                },
            )
            .await?
        }
        None => {
            if matches!(resolved.source, Source::External) {
                trace.route("direct");
                let session =
                    PageSession::attach(&resolved.endpoint, resolved.engine, None).await?;
                let res = session
                    .evaluate_with_timeout(&expr, true, Some(VALIDATE_TIMEOUT))
                    .await;
                session.close().await;
                res?
            } else {
                trace.route("scratch");
                let browser_name = match &resolved.source {
                    Source::Registered { name } => name.clone(),
                    _ => unreachable!("Source::External branch handled above"),
                };
                let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
                let expr = expr.clone();
                with_scratch_recovery(&backend, registry, &browser_name, move |b, target_id| {
                    let expr = expr.clone();
                    async move { b.evaluate(&target_id, &expr, true, VALIDATE_TIMEOUT).await }
                })
                .await?
            }
        }
    };

    let json_str = value.as_str().ok_or_else(|| {
        anyhow::anyhow!("validate-url: page returned non-string from fetch script")
    })?;
    let parsed: Value = serde_json::from_str(json_str)
        .context("validate-url: failed to parse fetch response envelope")?;
    let status = parsed
        .get("status")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("validate-url: missing `status` in fetch response"))?;
    validate_status(status)
}

/// Returns true when both regexes match the cookie's domain and name. Both
/// regexes are unanchored (`Regex::is_match` semantics).
pub(crate) fn cookie_matches(c: &NormalCookie, domain_re: &Regex, name_re: &Regex) -> bool {
    domain_re.is_match(&c.domain) && name_re.is_match(&c.name)
}

/// Require a 2xx status; otherwise produce an error.
pub(crate) fn validate_status(status: i64) -> Result<()> {
    if (200..=299).contains(&status) {
        Ok(())
    } else {
        bail!("validate-url failed: status {status}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(domain: &str, name: &str) -> NormalCookie {
        NormalCookie {
            domain: domain.to_string(),
            name: name.to_string(),
            value: "v".to_string(),
            path: "/".to_string(),
            secure: false,
            http_only: false,
            same_site: None,
            expires: None,
        }
    }

    #[test]
    fn cookie_matches_unanchored_domain_and_name() {
        let d = Regex::new(r"example\.com").unwrap();
        let n = Regex::new(r"session").unwrap();
        assert!(cookie_matches(
            &cookie("www.example.com", "session_id"),
            &d,
            &n
        ));
        assert!(cookie_matches(
            &cookie(".example.com", "my_session"),
            &d,
            &n
        ));
    }

    #[test]
    fn cookie_matches_requires_both() {
        let d = Regex::new(r"example\.com").unwrap();
        let n = Regex::new(r"^session$").unwrap();
        // wrong name
        assert!(!cookie_matches(
            &cookie("example.com", "session_id"),
            &d,
            &n
        ));
        // wrong domain
        assert!(!cookie_matches(&cookie("other.test", "session"), &d, &n));
        // both ok
        assert!(cookie_matches(&cookie("example.com", "session"), &d, &n));
    }

    #[test]
    fn cookie_matches_anchored_regex() {
        // `^csrf$` strictly matches the literal name `csrf`.
        let d = Regex::new(r".*").unwrap();
        let n = Regex::new(r"^csrf$").unwrap();
        assert!(cookie_matches(&cookie("a.test", "csrf"), &d, &n));
        assert!(!cookie_matches(&cookie("a.test", "csrf_token"), &d, &n));
    }

    #[test]
    fn validate_status_2xx_passes() {
        assert!(validate_status(200).is_ok());
        assert!(validate_status(204).is_ok());
        assert!(validate_status(299).is_ok());
    }

    #[test]
    fn validate_status_non_2xx_fails() {
        assert!(validate_status(199).is_err());
        assert!(validate_status(300).is_err());
        assert!(validate_status(404).is_err());
        assert!(validate_status(500).is_err());
        let err = validate_status(403).unwrap_err().to_string();
        assert!(err.contains("403"), "error should mention status: {err}");
    }

    /// Pure poll-loop helper mirroring `run`'s timing logic, parameterised
    /// over a synchronous fetch closure so it can be tested without a browser.
    async fn wait_loop<F>(
        mut fetch: F,
        domain_re: &Regex,
        name_re: &Regex,
        timeout: Duration,
        interval: Duration,
    ) -> Result<NormalCookie>
    where
        F: FnMut() -> Vec<NormalCookie>,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let cookies = fetch();
            if let Some(c) = cookies
                .into_iter()
                .find(|c| cookie_matches(c, domain_re, name_re))
            {
                return Ok(c);
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for cookie");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let nap = std::cmp::min(interval, remaining);
            if nap.is_zero() {
                bail!("timed out waiting for cookie");
            }
            sleep(nap).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn wait_loop_times_out_when_cookie_never_appears() {
        let d = Regex::new(r"example\.com").unwrap();
        let n = Regex::new(r"^sid$").unwrap();
        let err = wait_loop(
            Vec::new,
            &d,
            &n,
            Duration::from_secs(3),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_loop_returns_first_match() {
        let d = Regex::new(r"example\.com").unwrap();
        let n = Regex::new(r"^sid$").unwrap();
        let mut calls = 0;
        let fetch = move || {
            calls += 1;
            if calls >= 2 {
                vec![cookie("www.example.com", "sid")]
            } else {
                vec![cookie("www.example.com", "other")]
            }
        };
        let got = wait_loop(
            fetch,
            &d,
            &n,
            Duration::from_secs(10),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(got.name, "sid");
        assert_eq!(got.domain, "www.example.com");
    }
}
