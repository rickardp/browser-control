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
use crate::cli::mcp::resolve_browser;
use crate::dom::scripts::FETCH_JS;
use crate::session::PageSession;

pub async fn run(
    browser: Option<String>,
    domain: String,
    name: String,
    timeout: u64,
    poll_interval: u64,
    validate_url: Option<String>,
) -> Result<()> {
    let domain_re = Regex::new(&domain).context("invalid --domain regex")?;
    let name_re = Regex::new(&name).context("invalid --name regex")?;

    let resolved = resolve_browser(browser).await?;

    let deadline = Instant::now() + Duration::from_secs(timeout);
    let interval = Duration::from_secs(poll_interval.max(1));

    let matched = loop {
        let cookies = fetch_cookies(&resolved).await?;
        if let Some(c) = cookies.into_iter().find(|c| cookie_matches(c, &domain_re, &name_re)) {
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
        let session = PageSession::attach(&resolved.endpoint, resolved.engine, None).await?;
        let result = validate_via_page(&session, &url).await;
        session.close().await;
        result?;
    }

    println!("{}", matched.name);
    Ok(())
}

/// Returns true when both regexes match the cookie's domain and name. Both
/// regexes are unanchored (`Regex::is_match` semantics).
pub(crate) fn cookie_matches(c: &NormalCookie, domain_re: &Regex, name_re: &Regex) -> bool {
    domain_re.is_match(&c.domain) && name_re.is_match(&c.name)
}

async fn validate_via_page(session: &PageSession, url: &str) -> Result<()> {
    let args = serde_json::json!({ "url": url, "method": "GET" }).to_string();
    let expr = format!("({})({})", FETCH_JS, serde_json::to_string(&args).unwrap());
    let value = session.evaluate(&expr, true).await?;
    let json_str = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("validate-url: page returned non-string from fetch script"))?;
    let parsed: Value = serde_json::from_str(json_str)
        .context("validate-url: failed to parse fetch response envelope")?;
    let status = parsed
        .get("status")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("validate-url: missing `status` in fetch response"))?;
    validate_status(status)
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
        assert!(cookie_matches(&cookie("www.example.com", "session_id"), &d, &n));
        assert!(cookie_matches(&cookie(".example.com", "my_session"), &d, &n));
    }

    #[test]
    fn cookie_matches_requires_both() {
        let d = Regex::new(r"example\.com").unwrap();
        let n = Regex::new(r"^session$").unwrap();
        // wrong name
        assert!(!cookie_matches(&cookie("example.com", "session_id"), &d, &n));
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
            if let Some(c) = cookies.into_iter().find(|c| cookie_matches(c, domain_re, name_re)) {
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
        let got = wait_loop(fetch, &d, &n, Duration::from_secs(10), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(got.name, "sid");
        assert_eq!(got.domain, "www.example.com");
    }
}
