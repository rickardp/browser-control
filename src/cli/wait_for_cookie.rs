//! `browser-control wait-for-cookie` — block until a cookie appears.
//!
//! Browser-wide command — tab suffixes on `--browser` are rejected.
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
use crate::cli::env_resolver;
use crate::cli::mcp::{acquire_bidi_lock_if_needed, resolve_browser};
use crate::cli::trace::CommandTrace;
use crate::dom::scripts::FETCH_JS;
use crate::registry::Registry;
use crate::session::evaluate_for_origin_with_recover_once;
use crate::session::freshness;

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
    max_age: String,
) -> Result<()> {
    let mut trace = CommandTrace::new("wait-for-cookie");
    let result: Result<()> = async {
        let domain_re = Regex::new(&domain).context("invalid --domain regex")?;
        let name_re = Regex::new(&name).context("invalid --name regex")?;

        // Reject `<browser>/<tab>` — `wait-for-cookie` is browser-wide.
        // The cookie poll uses `Network.getAllCookies` / `storage.getCookies`
        // which are browser-scoped; tabs don't apply.
        let raw = browser.unwrap_or_default();
        if !raw.is_empty() {
            let parsed = env_resolver::parse_target(&raw)?;
            if parsed.tab.is_some() {
                bail!(
                    "`wait-for-cookie` operates browser-wide; tab suffixes are not supported \
                     (got `{raw}`). Use a bare browser selector instead."
                );
            }
        }
        let resolved = resolve_browser(if raw.is_empty() {
            None
        } else {
            Some(raw.clone())
        })
        .await?;
        trace.browser(&raw).engine(resolved.engine);

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
            let max_age = freshness::parse_max_age(&max_age)?;
            run_validate_url(&resolved, &url, max_age, &mut trace).await?;
        } else {
            // No validate-url; only the cookie poll ran (browser-wide).
            trace.route("poll");
        }

        println!("{}", matched.name);
        Ok(())
    }
    .await;
    trace.finish(result)
}

/// Drive the `--validate-url` fetch from the requested URL's origin, with
/// recover-once by re-resolving that origin. This intentionally avoids scratch
/// / `about:blank`, which would drop cookies or trip CORS.
async fn run_validate_url(
    resolved: &crate::cli::env_resolver::ResolvedBrowser,
    url: &str,
    max_age: Duration,
    trace: &mut CommandTrace,
) -> Result<()> {
    let args = serde_json::json!({ "url": url, "method": "GET" }).to_string();
    let expr = format!("({})({})", FETCH_JS, serde_json::to_string(&args).unwrap());

    trace.route("attach-for-origin");
    let value = evaluate_for_origin_with_recover_once(
        &resolved.endpoint,
        resolved.engine,
        url,
        &expr,
        true,
        VALIDATE_TIMEOUT,
        max_age,
    )
    .await?;

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
    use crate::cli::env_resolver::{ResolvedBrowser, Source};
    use crate::detect::Engine;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_tungstenite::tungstenite::Message;

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

    async fn spawn_validate_cdp_mock() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let created_urls = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn({
            let created_urls = created_urls.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let req: Value = serde_json::from_str(&t).unwrap();
                    let id = req["id"].as_u64().unwrap();
                    let method = req["method"].as_str().unwrap_or("");
                    let result = match method {
                        "Target.getTargets" => json!({
                            "targetInfos": [{
                                "targetId": "OTHER",
                                "type": "page",
                                "url": "https://other.test/",
                            }]
                        }),
                        "Target.createTarget" => {
                            let url = req
                                .pointer("/params/url")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            created_urls.lock().await.push(url);
                            json!({"targetId": "NEW"})
                        }
                        "Target.attachToTarget" => json!({"sessionId": "S1"}),
                        "Target.detachFromTarget" => json!({}),
                        "Inspector.enable" => json!({}),
                        "Runtime.evaluate" => {
                            let expression = req
                                .pointer("/params/expression")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let value = if expression == freshness::PAGE_FRESHNESS_EXPR {
                                json!({
                                    "href": "https://example.com/",
                                    "ageMs": 0.0,
                                    "readyState": "complete"
                                })
                            } else if expression == freshness::READY_STATE_EXPR {
                                json!("complete")
                            } else {
                                json!(json!({"status": 204}).to_string())
                            };
                            json!({"result": {"value": value}})
                        }
                        _ => json!({}),
                    };
                    let resp = json!({"id": id, "result": result});
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                }
            }
        });
        (format!("ws://{addr}"), created_urls)
    }

    #[tokio::test]
    async fn validate_url_registered_browser_uses_origin_tab_not_scratch() {
        let (endpoint, created_urls) = spawn_validate_cdp_mock().await;
        let resolved = ResolvedBrowser {
            endpoint,
            engine: Engine::Cdp,
            source: Source::Registered {
                name: "chrome-test".to_string(),
            },
        };
        let mut trace = CommandTrace::new("wait-for-cookie");
        run_validate_url(
            &resolved,
            "https://example.com/api/check",
            freshness::DEFAULT_MAX_AGE,
            &mut trace,
        )
        .await
        .unwrap();
        assert_eq!(
            *created_urls.lock().await,
            vec!["https://example.com/".to_string()]
        );
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
