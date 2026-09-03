//! Page freshness helpers used before reading browser-backed auth state.

use std::time::Duration;

use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::Value;

pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(10 * 60);
pub const DEFAULT_MAX_AGE_STR: &str = "10m";
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
pub const RELOAD_READY_TIMEOUT: Duration = Duration::from_secs(20);
pub const READY_POLL_INTERVAL: Duration = Duration::from_millis(200);

pub const PAGE_FRESHNESS_EXPR: &str = r#"(() => ({
  href: location.href,
  ageMs: Math.max(0, Date.now() - performance.timeOrigin),
  readyState: document.readyState
}))()"#;

pub const READY_STATE_EXPR: &str = "document.readyState";

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PageFreshness {
    pub href: String,
    #[serde(rename = "ageMs")]
    pub age_ms: f64,
}

impl PageFreshness {
    pub fn should_reload(&self, max_age: Duration) -> bool {
        is_reloadable_url(&self.href) && self.age_ms >= max_age.as_millis() as f64
    }
}

pub fn parse_page_freshness(value: Value) -> Result<PageFreshness> {
    Ok(serde_json::from_value(value)?)
}

pub fn is_ready(value: &Value) -> bool {
    matches!(value.as_str(), Some("complete"))
}

fn is_reloadable_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Human form of a duration using the same units `parse_max_age` accepts
/// (`2h`, `30m`, `90s`; mixed values fall back to seconds).
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs > 0 && secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs > 0 && secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

pub fn parse_max_age(raw: &str) -> Result<Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("--max-age must not be empty");
    }

    let mut rest = raw;
    let mut total_ms: u128 = 0;
    while !rest.is_empty() {
        let digits_len = rest
            .char_indices()
            .take_while(|(_, ch)| ch.is_ascii_digit())
            .map(|(idx, ch)| idx + ch.len_utf8())
            .last()
            .unwrap_or(0);
        if digits_len == 0 {
            bail!("invalid --max-age `{raw}`: expected a number");
        }
        let n: u128 = rest[..digits_len].parse()?;
        rest = rest[digits_len..].trim_start();

        let unit_len = rest
            .char_indices()
            .take_while(|(_, ch)| ch.is_ascii_alphabetic())
            .map(|(idx, ch)| idx + ch.len_utf8())
            .last()
            .unwrap_or(0);
        let unit = if unit_len == 0 {
            "s"
        } else {
            &rest[..unit_len]
        };
        rest = rest[unit_len..].trim_start();

        let factor = match unit {
            "ms" => 1,
            "s" | "sec" | "secs" | "second" | "seconds" => 1_000,
            "m" | "min" | "mins" | "minute" | "minutes" => 60_000,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000,
            other => bail!("invalid --max-age unit `{other}`: expected ms, s, m, or h"),
        };
        total_ms = total_ms
            .checked_add(n.checked_mul(factor).ok_or_else(|| {
                anyhow::anyhow!("invalid --max-age `{raw}`: duration is too large")
            })?)
            .ok_or_else(|| anyhow::anyhow!("invalid --max-age `{raw}`: duration is too large"))?;
    }

    Ok(Duration::from_millis(total_ms.try_into()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_max_age_accepts_common_units() {
        assert_eq!(parse_max_age("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_max_age("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_max_age("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_max_age("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_max_age("1h 30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_max_age("42").unwrap(), Duration::from_secs(42));
    }

    #[test]
    fn page_freshness_only_reloads_http_pages_over_age() {
        let info = parse_page_freshness(json!({
            "href": "https://example.com/app",
            "ageMs": 700_000.0,
            "readyState": "complete"
        }))
        .unwrap();
        assert!(info.should_reload(Duration::from_secs(600)));

        let blank = PageFreshness {
            href: "about:blank".to_string(),
            age_ms: 700_000.0,
        };
        assert!(!blank.should_reload(Duration::from_secs(600)));
    }
}
