//! Resolve the `BROWSER_CONTROL` environment variable / `--browser` argument
//! into a concrete browser endpoint.
//!
//! Parsing priority (most-to-least specific):
//! 1. URL with scheme `ws://`, `wss://`, `http://`, `https://` → [`BrowserSelector::Url`].
//! 2. Absolute path that exists on disk → [`BrowserSelector::ExecutablePath`].
//! 3. String that matches a [`Kind`] (case-insensitive) → [`BrowserSelector::Kind`].
//! 4. Anything else → [`BrowserSelector::Name`].

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::future::BoxFuture;
use serde::Serialize;

use crate::detect::{Engine, Installed, Kind};
use crate::registry::Registry;

/// Parsed form of a `BROWSER_CONTROL` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserSelector {
    /// Direct CDP/BiDi WebSocket or HTTP URL.
    Url(url::Url),
    /// Friendly instance name like `firefox-pikachu`.
    Name(String),
    /// Browser kind like `chrome` or `firefox`.
    Kind(Kind),
    /// Absolute path to a browser executable (must exist).
    ExecutablePath(PathBuf),
}

/// Where the resolved browser came from.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum Source {
    /// An external CDP/BiDi endpoint passed by URL. No registry write.
    External,
    /// A registered browser, identified by friendly name.
    Registered { name: String },
}

/// Result of resolving a [`BrowserSelector`].
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedBrowser {
    pub endpoint: String,
    pub engine: Engine,
    pub source: Source,
}

/// I/O surface for [`resolve_with`]. Implementations provide HTTP discovery
/// (`/json/version`) and the list of installed browsers.
pub trait Resolver: Send + Sync {
    fn fetch_version<'a>(&'a self, base: &'a str) -> BoxFuture<'a, Result<String>>;
    fn list_installed(&self) -> Vec<Installed>;
}

/// Production resolver: real HTTP + on-disk browser detection.
pub struct DefaultResolver;

impl Resolver for DefaultResolver {
    fn fetch_version<'a>(&'a self, base: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .context("building reqwest client")?;
            let url = format!("{}/json/version", base.trim_end_matches('/'));
            let v: serde_json::Value = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("GET {url}"))?
                .error_for_status()
                .with_context(|| format!("GET {url}"))?
                .json()
                .await
                .with_context(|| format!("decode JSON from {url}"))?;
            let ws = v
                .get("webSocketDebuggerUrl")
                .and_then(|x| x.as_str())
                .ok_or_else(|| anyhow!("response from {url} missing webSocketDebuggerUrl"))?
                .to_string();
            Ok(ws)
        })
    }

    fn list_installed(&self) -> Vec<Installed> {
        crate::detect::list_installed()
    }
}

/// Parse a raw `BROWSER_CONTROL` value into a [`BrowserSelector`].
pub fn parse(value: &str) -> Result<BrowserSelector> {
    let v = value.trim();
    if v.is_empty() {
        bail!("BROWSER_CONTROL value is empty");
    }

    let lower = v.to_ascii_lowercase();
    if lower.starts_with("ws://")
        || lower.starts_with("wss://")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
    {
        let u = url::Url::parse(v).with_context(|| format!("parsing URL {v}"))?;
        return Ok(BrowserSelector::Url(u));
    }

    let path = Path::new(v);
    if path.is_absolute() && path.exists() {
        return Ok(BrowserSelector::ExecutablePath(path.to_path_buf()));
    }

    if let Some(k) = Kind::parse(v) {
        return Ok(BrowserSelector::Kind(k));
    }

    Ok(BrowserSelector::Name(v.to_string()))
}

/// Heuristic engine detection for a WebSocket URL.
///
/// A path beginning with `/session` (WebDriver BiDi) → [`Engine::Bidi`];
/// anything else (e.g. `/devtools/browser/<id>`) → [`Engine::Cdp`].
fn engine_from_ws_url(u: &url::Url) -> Engine {
    let path = u.path();
    if path.starts_with("/session") || path.contains("/session/") {
        Engine::Bidi
    } else {
        Engine::Cdp
    }
}

/// Resolve a selector using the real environment.
pub async fn resolve(selector: BrowserSelector, registry: &Registry) -> Result<ResolvedBrowser> {
    resolve_with(selector, registry, &DefaultResolver).await
}

/// Resolve a selector using an injected [`Resolver`] for I/O.
pub async fn resolve_with<R: Resolver>(
    selector: BrowserSelector,
    registry: &Registry,
    r: &R,
) -> Result<ResolvedBrowser> {
    match selector {
        BrowserSelector::Url(u) => resolve_url(u, r).await,
        BrowserSelector::Name(name) => resolve_name(&name, registry),
        BrowserSelector::Kind(k) => resolve_kind(k, registry),
        BrowserSelector::ExecutablePath(p) => resolve_path(&p, registry, r),
    }
}

async fn resolve_url<R: Resolver>(u: url::Url, r: &R) -> Result<ResolvedBrowser> {
    match u.scheme() {
        "ws" | "wss" => Ok(ResolvedBrowser {
            engine: engine_from_ws_url(&u),
            endpoint: u.to_string(),
            source: Source::External,
        }),
        "http" | "https" => {
            let base = u.as_str().trim_end_matches('/').to_string();
            let ws = r.fetch_version(&base).await?;
            let ws_url = url::Url::parse(&ws)
                .with_context(|| format!("parsing webSocketDebuggerUrl {ws}"))?;
            Ok(ResolvedBrowser {
                engine: engine_from_ws_url(&ws_url),
                endpoint: ws,
                source: Source::External,
            })
        }
        other => bail!("unsupported URL scheme: {other}"),
    }
}

fn resolve_name(name: &str, registry: &Registry) -> Result<ResolvedBrowser> {
    let row = registry
        .get_by_name(name)
        .with_context(|| format!("looking up browser {name}"))?
        .ok_or_else(|| anyhow!("no registered browser named {name}"))?;
    Ok(ResolvedBrowser {
        endpoint: row.endpoint,
        engine: row.engine,
        source: Source::Registered { name: row.name },
    })
}

fn resolve_kind(kind: Kind, registry: &Registry) -> Result<ResolvedBrowser> {
    let row = registry
        .first_alive_by_kind(kind)
        .with_context(|| format!("looking up alive {kind} browser"))?
        .ok_or_else(|| anyhow!("no running {kind} browser found in registry"))?;
    Ok(ResolvedBrowser {
        endpoint: row.endpoint,
        engine: row.engine,
        source: Source::Registered { name: row.name },
    })
}

fn resolve_path<R: Resolver>(path: &Path, registry: &Registry, r: &R) -> Result<ResolvedBrowser> {
    let installed = r.list_installed();
    let kind = installed
        .iter()
        .find(|i| i.executable == path)
        .map(|i| i.kind)
        .ok_or_else(|| {
            anyhow!(
                "executable {} does not match any known installed browser",
                path.display()
            )
        })?;
    resolve_kind(kind, registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::BrowserRow;
    use std::sync::Mutex;

    fn row(name: &str, kind: Kind, port: u16, started_at: &str) -> BrowserRow {
        BrowserRow {
            name: name.to_string(),
            kind,
            engine: kind.engine(),
            pid: std::process::id(),
            endpoint: format!("ws://127.0.0.1:{port}/devtools/browser/abcd"),
            port,
            profile_dir: PathBuf::from(format!("/tmp/profiles/{name}")),
            executable: PathBuf::from("/usr/bin/example"),
            headless: false,
            started_at: started_at.to_string(),
        }
    }

    /// Bind a TCP listener on a random local port and return (listener, port).
    /// Keep the listener alive for the duration of liveness checks.
    fn alive_listener() -> (std::net::TcpListener, u16) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        (l, port)
    }

    struct FakeResolver {
        fetch_version_result: Mutex<Option<Result<String>>>,
        installed: Vec<Installed>,
    }

    impl FakeResolver {
        fn ok(ws: &str) -> Self {
            Self {
                fetch_version_result: Mutex::new(Some(Ok(ws.to_string()))),
                installed: Vec::new(),
            }
        }
        fn empty() -> Self {
            Self {
                fetch_version_result: Mutex::new(None),
                installed: Vec::new(),
            }
        }
        fn with_installed(installed: Vec<Installed>) -> Self {
            Self {
                fetch_version_result: Mutex::new(None),
                installed,
            }
        }
    }

    impl Resolver for FakeResolver {
        fn fetch_version<'a>(&'a self, _base: &'a str) -> BoxFuture<'a, Result<String>> {
            let taken = self
                .fetch_version_result
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err(anyhow!("fetch_version not configured")));
            Box::pin(async move { taken })
        }
        fn list_installed(&self) -> Vec<Installed> {
            self.installed.clone()
        }
    }

    // --- parse() ----------------------------------------------------------

    #[test]
    fn parse_ws_url() {
        let s = "ws://x:9222/devtools/browser/abc";
        match parse(s).unwrap() {
            BrowserSelector::Url(u) => assert_eq!(u.as_str(), s),
            other => panic!("expected Url, got {other:?}"),
        }
    }

    #[test]
    fn parse_http_url() {
        match parse("http://x:9222").unwrap() {
            BrowserSelector::Url(u) => assert_eq!(u.scheme(), "http"),
            other => panic!("expected Url, got {other:?}"),
        }
    }

    #[test]
    fn parse_absolute_nonexistent_path_falls_through_to_name() {
        // Absolute path that does not exist on disk falls through to Name per spec.
        let s = "/non/existent/path/to/nothing-xyz-12345";
        match parse(s).unwrap() {
            BrowserSelector::Name(n) => assert_eq!(n, s),
            other => panic!("expected Name, got {other:?}"),
        }
    }

    #[test]
    fn parse_existing_absolute_path_is_executable_path() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let p = f.path().to_path_buf();
        assert!(p.is_absolute());
        match parse(p.to_str().unwrap()).unwrap() {
            BrowserSelector::ExecutablePath(got) => assert_eq!(got, p),
            other => panic!("expected ExecutablePath, got {other:?}"),
        }
    }

    #[test]
    fn parse_kind_lowercase() {
        assert_eq!(
            parse("chrome").unwrap(),
            BrowserSelector::Kind(Kind::Chrome)
        );
    }

    #[test]
    fn parse_kind_case_insensitive() {
        assert_eq!(
            parse("FIREFOX").unwrap(),
            BrowserSelector::Kind(Kind::Firefox)
        );
    }

    #[test]
    fn parse_friendly_name() {
        assert_eq!(
            parse("firefox-pikachu").unwrap(),
            BrowserSelector::Name("firefox-pikachu".to_string())
        );
    }

    // --- resolve_with() ---------------------------------------------------

    #[tokio::test]
    async fn url_ws_returned_verbatim_cdp_engine() {
        let reg = Registry::open_in_memory().unwrap();
        let r = FakeResolver::empty();
        let sel = parse("ws://127.0.0.1:9222/devtools/browser/abc").unwrap();
        let got = resolve_with(sel, &reg, &r).await.unwrap();
        assert_eq!(got.endpoint, "ws://127.0.0.1:9222/devtools/browser/abc");
        assert_eq!(got.engine, Engine::Cdp);
        assert_eq!(got.source, Source::External);
    }

    #[tokio::test]
    async fn url_ws_session_path_is_bidi() {
        let reg = Registry::open_in_memory().unwrap();
        let r = FakeResolver::empty();
        let sel = parse("ws://127.0.0.1:9222/session/abc123").unwrap();
        let got = resolve_with(sel, &reg, &r).await.unwrap();
        assert_eq!(got.engine, Engine::Bidi);
    }

    #[tokio::test]
    async fn url_http_invokes_fetch_version() {
        let reg = Registry::open_in_memory().unwrap();
        let r = FakeResolver::ok("ws://127.0.0.1:9222/devtools/browser/discovered");
        let sel = parse("http://127.0.0.1:9222").unwrap();
        let got = resolve_with(sel, &reg, &r).await.unwrap();
        assert_eq!(
            got.endpoint,
            "ws://127.0.0.1:9222/devtools/browser/discovered"
        );
        assert_eq!(got.engine, Engine::Cdp);
        assert_eq!(got.source, Source::External);
    }

    #[tokio::test]
    async fn kind_with_one_running_resolves() {
        let reg = Registry::open_in_memory().unwrap();
        let (_listener, port) = alive_listener();
        let r = row("chrome-foxtrot", Kind::Chrome, port, "2024-06-01T00:00:00Z");
        reg.insert(&r).unwrap();
        let fake = FakeResolver::empty();
        let got = resolve_with(BrowserSelector::Kind(Kind::Chrome), &reg, &fake)
            .await
            .unwrap();
        assert_eq!(got.endpoint, r.endpoint);
        assert_eq!(got.engine, Engine::Cdp);
        assert_eq!(
            got.source,
            Source::Registered {
                name: "chrome-foxtrot".to_string()
            }
        );
    }

    #[tokio::test]
    async fn kind_with_none_running_errors() {
        let reg = Registry::open_in_memory().unwrap();
        let fake = FakeResolver::empty();
        let err = resolve_with(BrowserSelector::Kind(Kind::Chrome), &reg, &fake)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").to_lowercase().contains("chrome"));
    }

    #[tokio::test]
    async fn name_lookup_hit() {
        let reg = Registry::open_in_memory().unwrap();
        let r = row(
            "firefox-pikachu",
            Kind::Firefox,
            9111,
            "2024-06-01T00:00:00Z",
        );
        reg.insert(&r).unwrap();
        let fake = FakeResolver::empty();
        let got = resolve_with(
            BrowserSelector::Name("firefox-pikachu".to_string()),
            &reg,
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(got.endpoint, r.endpoint);
        assert_eq!(got.engine, Engine::Bidi);
        assert_eq!(
            got.source,
            Source::Registered {
                name: "firefox-pikachu".to_string()
            }
        );
    }

    #[tokio::test]
    async fn name_lookup_miss_errors() {
        let reg = Registry::open_in_memory().unwrap();
        let fake = FakeResolver::empty();
        let err = resolve_with(BrowserSelector::Name("nope".to_string()), &reg, &fake)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("nope"));
    }

    #[tokio::test]
    async fn executable_path_maps_to_kind_then_errors_when_no_running() {
        let reg = Registry::open_in_memory().unwrap();
        let exe = PathBuf::from("/opt/myorg/chrome");
        let installed = vec![Installed {
            kind: Kind::Chrome,
            executable: exe.clone(),
            version: "130.0.0.0".to_string(),
            engine: Engine::Cdp,
        }];
        let fake = FakeResolver::with_installed(installed);
        let err = resolve_with(BrowserSelector::ExecutablePath(exe), &reg, &fake)
            .await
            .unwrap_err();
        // Mapped to Chrome, but nothing running.
        assert!(format!("{err:#}").to_lowercase().contains("chrome"));
    }

    #[tokio::test]
    async fn executable_path_resolves_via_kind_when_running() {
        let reg = Registry::open_in_memory().unwrap();
        let (_listener, port) = alive_listener();
        let r = row("chrome-x", Kind::Chrome, port, "2024-06-01T00:00:00Z");
        reg.insert(&r).unwrap();

        let exe = PathBuf::from("/opt/myorg/chrome");
        let installed = vec![Installed {
            kind: Kind::Chrome,
            executable: exe.clone(),
            version: "130.0.0.0".to_string(),
            engine: Engine::Cdp,
        }];
        let fake = FakeResolver::with_installed(installed);
        let got = resolve_with(BrowserSelector::ExecutablePath(exe), &reg, &fake)
            .await
            .unwrap();
        assert_eq!(got.endpoint, r.endpoint);
    }

    #[tokio::test]
    async fn executable_path_unknown_errors() {
        let reg = Registry::open_in_memory().unwrap();
        let fake = FakeResolver::with_installed(Vec::new());
        let err = resolve_with(
            BrowserSelector::ExecutablePath(PathBuf::from("/totally/unknown")),
            &reg,
            &fake,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("/totally/unknown"));
    }
}
