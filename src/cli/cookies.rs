//! `browser-control cookies` — export cookies from the active browser.
//!
//! Browser-wide command — tab suffixes on `--browser` are rejected.
//!
//! Output formats:
//! - `json`: pretty JSON array of normalised cookies. Always contains full values
//!   (machine-readable; user explicitly opted in).
//! - `netscape`: Netscape HTTP Cookie File (tab-separated). Always full values
//!   (the format is intended for tools like `curl --cookie`).
//! - `header`: a single `Cookie: k=v; ...` line. Values are replaced with
//!   `<redacted>` when writing to stdout unless `--reveal` is given. Writing
//!   to a file via `-o` always emits full values; the file is `0600` on Unix.
//!
//! Domain/name filters are unanchored regexes — use `^…$` for strict matching.

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::cli::env_resolver::{self, ResolvedBrowser};
use crate::cli::mcp::{acquire_bidi_lock_if_needed, resolve_browser};
use crate::cli::trace::CommandTrace;
use crate::detect::Engine;
use crate::registry::Registry;
use crate::session::targets::{open_bidi, open_cdp};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct NormalCookie {
    pub(crate) domain: String,
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) path: String,
    pub(crate) secure: bool,
    pub(crate) http_only: bool,
    pub(crate) same_site: Option<String>,
    pub(crate) expires: Option<i64>,
}

/// Fetch all cookies from the resolved browser, normalised across engines.
pub(crate) async fn fetch_cookies(resolved: &ResolvedBrowser) -> Result<Vec<NormalCookie>> {
    match resolved.engine {
        Engine::Cdp => fetch_cdp(&resolved.endpoint).await,
        Engine::Bidi => fetch_bidi(&resolved.endpoint).await,
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    browser: Option<String>,
    domain: Option<String>,
    name: Option<String>,
    format: String,
    output: Option<PathBuf>,
    reveal: bool,
    json: bool,
) -> Result<()> {
    let mut trace = CommandTrace::new("cookies");
    let result: Result<()> = async {
        let effective_format = if json { "json".to_string() } else { format };
        match effective_format.as_str() {
            "json" | "netscape" | "header" => {}
            other => {
                bail!("unknown --format `{other}`: expected one of `netscape`, `json`, `header`")
            }
        }

        let domain_re = domain
            .as_deref()
            .map(Regex::new)
            .transpose()
            .context("invalid --domain regex")?;
        let name_re = name
            .as_deref()
            .map(Regex::new)
            .transpose()
            .context("invalid --name regex")?;

        // Accept `<browser>[/<tab>]` for syntactic consistency with
        // `eval`/`fetch`/`storage`. Cookies are inherently browser-wide
        // (CDP `Storage.getCookies` / BiDi `storage.getCookies` return the
        // full cookie jar), so tab suffixes are rejected.
        let raw = browser.unwrap_or_default();
        let parsed = if raw.is_empty() {
            None
        } else {
            Some(env_resolver::parse_target(&raw)?)
        };
        if parsed.as_ref().and_then(|p| p.tab.as_ref()).is_some() {
            bail!(
                "`cookies` operates browser-wide; tab suffixes are not supported \
                 (got `{raw}`). Use a bare browser selector instead."
            );
        }
        let browser_only = parsed.as_ref().map(|_| raw.clone()).unwrap_or_default();
        let resolved = resolve_browser(if browser_only.is_empty() {
            None
        } else {
            Some(browser_only.clone())
        })
        .await?;
        trace.browser(&browser_only).engine(resolved.engine);
        trace.route("browser-wide");
        // Hold the BiDi single-session lock for the read; releases on Drop.
        // No-op for CDP and for external URL endpoints.
        let _bidi_lock = {
            let registry = Registry::open()?;
            acquire_bidi_lock_if_needed(&registry, &resolved)?
        };
        let raw = fetch_cookies(&resolved).await?;

        let cookies: Vec<NormalCookie> = raw
            .into_iter()
            .filter(|c| matches_filter(c, domain_re.as_ref(), name_re.as_ref()))
            .collect();

        // For `-o FILE` writes we always emit full values (the file is 0600).
        // Redaction only applies to stdout + `header` format without `--reveal`.
        let to_stdout = output.is_none();
        let body = match effective_format.as_str() {
            "json" => format_json(&cookies)?,
            "netscape" => format_netscape(&cookies),
            "header" => format_header(&cookies, !to_stdout || reveal),
            _ => unreachable!(),
        };

        match output {
            Some(path) => {
                write_file(&path, &body)?;
                eprintln!("wrote {} cookies to {}", cookies.len(), path.display());
            }
            None => {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                out.write_all(body.as_bytes())?;
                if !body.ends_with('\n') {
                    out.write_all(b"\n")?;
                }
            }
        }
        Ok(())
    }
    .await;
    trace.finish(result)
}

async fn fetch_cdp(endpoint: &str) -> Result<Vec<NormalCookie>> {
    let client = open_cdp(endpoint).await?;
    let result = client.get_all_cookies().await?;
    client.close().await;
    let arr = result
        .get("cookies")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("CDP cookie export: missing `cookies` array"))?;
    Ok(arr.iter().map(normalize_cdp).collect())
}

async fn fetch_bidi(endpoint: &str) -> Result<Vec<NormalCookie>> {
    let client = open_bidi(endpoint).await?;
    client.session_new().await?;
    let result = client.send("storage.getCookies", json!({})).await;
    let _ = client.session_end().await;
    let result = result?;
    let arr = result
        .get("cookies")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("BiDi storage.getCookies: missing `cookies` array"))?;
    Ok(arr.iter().map(normalize_bidi).collect())
}

fn str_field(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

fn bool_field(v: &Value, k: &str) -> bool {
    v.get(k).and_then(|x| x.as_bool()).unwrap_or(false)
}

pub(crate) fn normalize_cdp(v: &Value) -> NormalCookie {
    let expires =
        v.get("expires")
            .and_then(|x| x.as_i64())
            .and_then(|n| if n < 0 { None } else { Some(n) });
    let same_site = v
        .get("sameSite")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    NormalCookie {
        domain: str_field(v, "domain"),
        name: str_field(v, "name"),
        value: str_field(v, "value"),
        path: str_field(v, "path"),
        secure: bool_field(v, "secure"),
        http_only: bool_field(v, "httpOnly"),
        same_site,
        expires,
    }
}

pub(crate) fn normalize_bidi(v: &Value) -> NormalCookie {
    let value = v
        .get("value")
        .and_then(|inner| inner.get("value").and_then(|x| x.as_str()))
        .unwrap_or_default()
        .to_string();
    let expires = v.get("expiry").and_then(|x| x.as_i64());
    let same_site = v
        .get("sameSite")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    NormalCookie {
        domain: str_field(v, "domain"),
        name: str_field(v, "name"),
        value,
        path: str_field(v, "path"),
        secure: bool_field(v, "secure"),
        http_only: bool_field(v, "httpOnly"),
        same_site,
        expires,
    }
}

fn matches_filter(c: &NormalCookie, domain: Option<&Regex>, name: Option<&Regex>) -> bool {
    domain.map_or(true, |re| re.is_match(&c.domain)) && name.map_or(true, |re| re.is_match(&c.name))
}

fn format_json(cookies: &[NormalCookie]) -> Result<String> {
    Ok(serde_json::to_string_pretty(cookies)?)
}

fn format_netscape(cookies: &[NormalCookie]) -> String {
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for c in cookies {
        let include_sub = if c.domain.starts_with('.') {
            "TRUE"
        } else {
            "FALSE"
        };
        let secure = if c.secure { "TRUE" } else { "FALSE" };
        let expires = c.expires.unwrap_or(0);
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            c.domain, include_sub, c.path, secure, expires, c.name, c.value
        ));
    }
    out
}

fn format_header(cookies: &[NormalCookie], reveal: bool) -> String {
    let parts: Vec<String> = cookies
        .iter()
        .map(|c| {
            let v = if reveal {
                c.value.as_str()
            } else {
                "<redacted>"
            };
            format!("{}={}", c.name, v)
        })
        .collect();
    format!("Cookie: {}", parts.join("; "))
}

fn write_file(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, body).with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod 600 {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(domain: &str, name: &str, value: &str) -> NormalCookie {
        NormalCookie {
            domain: domain.to_string(),
            name: name.to_string(),
            value: value.to_string(),
            path: "/".to_string(),
            secure: false,
            http_only: false,
            same_site: None,
            expires: None,
        }
    }

    #[test]
    fn netscape_byte_for_byte() {
        let cookies = vec![
            NormalCookie {
                domain: ".example.com".to_string(),
                name: "a".to_string(),
                value: "1".to_string(),
                path: "/".to_string(),
                secure: true,
                http_only: false,
                same_site: None,
                expires: Some(1700000000),
            },
            NormalCookie {
                domain: "host.test".to_string(),
                name: "b".to_string(),
                value: "two".to_string(),
                path: "/x".to_string(),
                secure: false,
                http_only: true,
                same_site: Some("Lax".to_string()),
                expires: None,
            },
        ];
        let got = format_netscape(&cookies);
        let want = "# Netscape HTTP Cookie File\n\
            .example.com\tTRUE\t/\tTRUE\t1700000000\ta\t1\n\
            host.test\tFALSE\t/x\tFALSE\t0\tb\ttwo\n";
        assert_eq!(got, want);
    }

    #[test]
    fn header_output_revealed() {
        let cookies = vec![c("x.test", "a", "1"), c("x.test", "b", "2")];
        assert_eq!(format_header(&cookies, true), "Cookie: a=1; b=2");
    }

    #[test]
    fn header_output_redacted() {
        let cookies = vec![c("x.test", "a", "1"), c("x.test", "b", "2")];
        assert_eq!(
            format_header(&cookies, false),
            "Cookie: a=<redacted>; b=<redacted>"
        );
    }

    #[test]
    fn domain_regex_filter_unanchored() {
        // Pattern `\.example\.com$` matches `.example.com` literally and
        // `www.example.com` (`.example.com` is a substring at the end).
        let re = Regex::new(r"\.example\.com$").unwrap();
        let dot = c(".example.com", "a", "1");
        let www = c("www.example.com", "a", "1");
        let other = c("evil.test", "a", "1");
        assert!(matches_filter(&dot, Some(&re), None));
        assert!(matches_filter(&www, Some(&re), None));
        assert!(!matches_filter(&other, Some(&re), None));
    }

    #[test]
    fn name_regex_filter() {
        let re = Regex::new(r"^session_").unwrap();
        let yes = c("x.test", "session_id", "1");
        let no = c("x.test", "csrf", "2");
        assert!(matches_filter(&yes, None, Some(&re)));
        assert!(!matches_filter(&no, None, Some(&re)));
    }

    #[test]
    fn json_output_round_trips() {
        let cookies = vec![NormalCookie {
            domain: ".example.com".to_string(),
            name: "a".to_string(),
            value: "v".to_string(),
            path: "/".to_string(),
            secure: true,
            http_only: true,
            same_site: Some("Strict".to_string()),
            expires: Some(42),
        }];
        let s = format_json(&cookies).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v[0]["domain"], ".example.com");
        assert_eq!(v[0]["name"], "a");
        assert_eq!(v[0]["value"], "v");
        assert_eq!(v[0]["secure"], true);
        assert_eq!(v[0]["http_only"], true);
        assert_eq!(v[0]["same_site"], "Strict");
        assert_eq!(v[0]["expires"], 42);
    }

    #[test]
    fn normalize_cdp_maps_fields() {
        let v = json!({
            "name": "sid",
            "value": "abc",
            "domain": ".example.com",
            "path": "/",
            "expires": 1700000000_i64,
            "httpOnly": true,
            "secure": true,
            "sameSite": "Lax",
            "session": false
        });
        let n = normalize_cdp(&v);
        assert_eq!(n.name, "sid");
        assert_eq!(n.value, "abc");
        assert_eq!(n.domain, ".example.com");
        assert_eq!(n.path, "/");
        assert_eq!(n.expires, Some(1700000000));
        assert!(n.http_only);
        assert!(n.secure);
        assert_eq!(n.same_site.as_deref(), Some("Lax"));
    }

    #[test]
    fn normalize_cdp_session_cookie_has_no_expires() {
        let v = json!({
            "name": "s",
            "value": "x",
            "domain": "host.test",
            "path": "/",
            "expires": -1_i64,
            "httpOnly": false,
            "secure": false,
            "session": true
        });
        let n = normalize_cdp(&v);
        assert_eq!(n.expires, None);
        assert_eq!(n.same_site, None);
    }

    #[test]
    fn normalize_bidi_maps_fields() {
        let v = json!({
            "name": "sid",
            "value": {"type": "string", "value": "abc"},
            "domain": "example.com",
            "path": "/",
            "expiry": 1700000000_i64,
            "httpOnly": true,
            "secure": true,
            "sameSite": "lax",
            "size": 10
        });
        let n = normalize_bidi(&v);
        assert_eq!(n.name, "sid");
        assert_eq!(n.value, "abc");
        assert_eq!(n.domain, "example.com");
        assert_eq!(n.expires, Some(1700000000));
        assert!(n.http_only);
        assert!(n.secure);
        assert_eq!(n.same_site.as_deref(), Some("lax"));
    }

    #[test]
    fn normalize_bidi_no_expiry_is_session() {
        let v = json!({
            "name": "s",
            "value": {"type": "string", "value": "x"},
            "domain": "example.com",
            "path": "/",
            "httpOnly": false,
            "secure": false,
            "size": 1
        });
        let n = normalize_bidi(&v);
        assert_eq!(n.expires, None);
    }
}
