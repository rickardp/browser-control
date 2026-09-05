//! Passive console and network capture for the MCP server.
//!
//! The MCP server keeps one CDP WebSocket open for its lifetime, and CDP
//! pushes `Runtime.*` / `Log.*` / `Network.*` events to any attached session
//! that has those domains enabled. This module keeps one long-lived flat
//! session per *touched* tab (any tab a tool call has routed to), enables
//! the domains once, and routes the pushed events into bounded per-tab ring
//! buffers that `browser_console_messages` / `browser_network_requests`
//! read on demand.
//!
//! This is the sanctioned exception to the "no idle work" rule in
//! `docs/specs/boundaries.md`: nothing here polls or wakes on a timer. The
//! only background task blocks on the event channel and does a mutex push
//! per event; when the browser is quiet, it is parked.
//!
//! Lifecycle: state for a tab is dropped when the tab is closed through the
//! MCP server (`forget`), when the browser detaches the session
//! (`Target.detachedFromTarget`), when the renderer crashes
//! (`Inspector.targetCrashed`), and wholesale on `browser_select` (`reset`)
//! or when the socket closes. Nothing in here ever returns `TabHung` /
//! `TabCrashed`: attach failures are swallowed and retried on the next touch,
//! so the recover-once flows elsewhere are unaffected.
//!
//! Firefox (WebDriver BiDi) uses the same hub with a different ingress: one
//! global `session.subscribe` per backend for `log.entryAdded`,
//! `network.beforeRequestSent` / `responseCompleted` / `fetchError`, and
//! `browsingContext.navigationStarted` / `contextDestroyed`, routed by the
//! browsing-context id, which *is* the target id. Response bodies are not
//! available on BiDi (no `getResponseBody` equivalent without browser-side
//! retention), so `browser_network_body` stays Chromium-only.
//!
//! Opt-out: `BROWSER_CONTROL_CAPTURE=0` (or `false`) disables attachment
//! entirely on both engines. `Runtime.enable` is observable by some anti-bot
//! scripts, and a user logging into such a site through `browser_show` may
//! prefer the server not to touch their tabs.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{anyhow, Result};
use base64::Engine as _;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Notify, OnceCell};

use crate::bidi::{BidiClient, BidiEvent};
use crate::cdp::{CdpClient, CdpEvent};
use crate::session::backend::TabBackend;

/// Agent-facing explanation for `browser_network_body` on Firefox.
pub const BIDI_NO_BODIES_HINT: &str = "response bodies are not captured on Firefox; use browser_fetch to re-issue the request, or switch to a Chromium browser via browser_select";

/// Console entries kept per tab.
pub const CONSOLE_CAP: usize = 1000;
/// Network entries kept per tab.
pub const NETWORK_CAP: usize = 500;
/// Bytes of rendered text kept per console entry.
pub const CONSOLE_TEXT_CAP: usize = 4096;
/// Bytes of URL kept per entry.
pub const URL_CAP: usize = 2048;
/// Bound on the whole attach + enable sequence.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a reader (or `browser_navigate`) waits for an in-flight attach.
pub const TOUCH_WAIT: Duration = Duration::from_secs(2);
/// Default cap for `browser_network_body`.
pub const BODY_DEFAULT_MAX: usize = 256 * 1024;
/// Hard cap for `browser_network_body`, aligned with `browser_curl`.
pub const BODY_HARD_MAX: usize = crate::cli::curl::MCP_RESPONSE_LIMIT;
/// Browser-side body buffer per captured tab (`Network.enable`).
const NET_MAX_TOTAL_BUFFER: u64 = 32 * 1024 * 1024;
const NET_MAX_RESOURCE_BUFFER: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warn,
    Info,
    Log,
    Debug,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Log => "log",
            Level::Debug => "debug",
        }
    }
}

/// One captured console line.
#[derive(Debug, Clone, Serialize)]
pub struct ConsoleEntry {
    pub seq: u64,
    /// Epoch milliseconds.
    pub ts_ms: f64,
    pub level: Level,
    /// `console.<type>`, `exception`, or the `Log.entryAdded` source
    /// (`network`, `security`, `deprecation`, …).
    pub source: String,
    pub text: String,
    pub url: Option<String>,
    /// 1-based.
    pub line: Option<u32>,
    pub column: Option<u32>,
    /// Document URL when the entry was captured.
    pub page_url: String,
    /// Full exception description (JSON output only).
    pub stack: Option<String>,
    pub network_request_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NetState {
    Pending,
    Finished,
    Failed,
    Redirected,
}

/// One captured network request.
#[derive(Debug, Clone, Serialize)]
pub struct NetworkEntry {
    pub seq: u64,
    /// CDP `Network.RequestId`, verbatim — pass to `browser_network_body`.
    pub request_id: String,
    /// Epoch milliseconds.
    pub ts_ms: f64,
    #[serde(skip)]
    monotonic_start: f64,
    pub method: String,
    pub url: String,
    pub resource_type: Option<String>,
    pub status: Option<u16>,
    pub status_text: Option<String>,
    pub mime_type: Option<String>,
    pub from_cache: bool,
    pub encoded_bytes: Option<u64>,
    pub duration_ms: Option<f64>,
    pub failed: Option<String>,
    pub state: NetState,
    pub page_url: String,
    pub has_post_data: bool,
}

/// Per-tab capture state.
#[derive(Debug)]
struct TabCapture {
    /// Hub-owned flat session, once attached.
    session_id: Option<String>,
    /// Domains enabled and `page_url` seeded.
    ready: bool,
    notify: Arc<Notify>,
    page_url: String,
    console: VecDeque<ConsoleEntry>,
    network: VecDeque<NetworkEntry>,
    next_seq: u64,
    dropped_console: u64,
    dropped_network: u64,
}

impl TabCapture {
    fn new() -> Self {
        Self {
            session_id: None,
            ready: false,
            notify: Arc::new(Notify::new()),
            page_url: String::new(),
            console: VecDeque::new(),
            network: VecDeque::new(),
            next_seq: 1,
            dropped_console: 0,
            dropped_network: 0,
        }
    }

    fn push_console(&mut self, mut e: ConsoleEntry) {
        e.seq = self.next_seq;
        self.next_seq += 1;
        e.page_url = self.page_url.clone();
        if self.console.len() >= CONSOLE_CAP {
            self.console.pop_front();
            self.dropped_console += 1;
        }
        self.console.push_back(e);
    }

    fn push_network(&mut self, mut e: NetworkEntry) {
        e.seq = self.next_seq;
        self.next_seq += 1;
        e.page_url = self.page_url.clone();
        if self.network.len() >= NETWORK_CAP {
            self.network.pop_front();
            self.dropped_network += 1;
        }
        self.network.push_back(e);
    }

    fn network_mut(&mut self, request_id: &str) -> Option<&mut NetworkEntry> {
        self.network
            .iter_mut()
            .rev()
            .find(|e| e.request_id == request_id)
    }
}

/// What the one-time BiDi `session.subscribe` managed to arm.
#[derive(Debug, Clone, Copy)]
struct BidiCapabilities {
    /// `network.*` events were accepted (Firefox 124+).
    network: bool,
}

#[derive(Default)]
struct HubInner {
    tabs: HashMap<String, TabCapture>,
    /// CDP only: hub session id → target id. BiDi routes by context id.
    sessions: HashMap<String, String>,
    /// Either engine's router (only one backend is live at a time).
    router: Option<tokio::task::JoinHandle<()>>,
    /// BiDi only: the global subscription, performed once per backend by
    /// the first attach task and shared by later ones; replaced on `reset`.
    bidi: Option<Arc<OnceCell<BidiCapabilities>>>,
    lost_events: u64,
    disabled: bool,
}

impl HubInner {
    /// Route one CDP event. Pure and synchronous so it is unit-testable
    /// with synthetic events.
    fn route(&mut self, ev: CdpEvent) {
        let Some(sid) = ev.session_id.as_deref() else {
            if ev.method == "Target.detachedFromTarget" {
                if let Some(sid) = ev.params.get("sessionId").and_then(Value::as_str) {
                    if let Some(tid) = self.sessions.remove(sid) {
                        self.tabs.remove(&tid);
                    }
                }
            }
            return;
        };
        // Fast-path discard of chatty events before any map lookup.
        if matches!(
            ev.method.as_str(),
            "Network.dataReceived"
                | "Network.requestServedFromCache"
                | "Network.resourceChangedPriority"
                | "Network.responseReceivedExtraInfo"
                | "Network.requestWillBeSentExtraInfo"
                | "Runtime.executionContextCreated"
                | "Runtime.executionContextDestroyed"
                | "Runtime.executionContextsCleared"
                | "Page.lifecycleEvent"
                | "Page.frameStartedLoading"
                | "Page.frameStoppedLoading"
                | "Page.domContentEventFired"
                | "Page.loadEventFired"
        ) {
            return;
        }
        let Some(tid) = self.sessions.get(sid).cloned() else {
            return;
        };
        if matches!(
            ev.method.as_str(),
            "Inspector.targetCrashed" | "Inspector.detached"
        ) {
            self.sessions.remove(sid);
            self.tabs.remove(&tid);
            return;
        }
        let Some(tab) = self.tabs.get_mut(&tid) else {
            return;
        };
        let p = &ev.params;
        match ev.method.as_str() {
            "Runtime.consoleAPICalled" => {
                if let Some(e) = console_entry_from_api(p) {
                    tab.push_console(e);
                }
            }
            "Runtime.exceptionThrown" => tab.push_console(console_entry_from_exception(p)),
            "Log.entryAdded" => {
                if let Some(e) = console_entry_from_log(p) {
                    tab.push_console(e);
                }
            }
            "Network.requestWillBeSent" => {
                let rid = p["requestId"].as_str().unwrap_or_default().to_string();
                if let Some(redirect) = p.get("redirectResponse") {
                    if let Some(prev) = tab.network_mut(&rid) {
                        apply_response(prev, redirect);
                        prev.state = NetState::Redirected;
                        prev.duration_ms =
                            duration_ms(prev.monotonic_start, p["timestamp"].as_f64());
                    }
                }
                let req = &p["request"];
                tab.push_network(NetworkEntry {
                    seq: 0,
                    request_id: rid,
                    ts_ms: p["wallTime"].as_f64().unwrap_or(0.0) * 1000.0,
                    monotonic_start: p["timestamp"].as_f64().unwrap_or(0.0),
                    method: req["method"].as_str().unwrap_or("GET").to_string(),
                    url: truncate(req["url"].as_str().unwrap_or_default(), URL_CAP),
                    resource_type: p["type"].as_str().map(String::from),
                    status: None,
                    status_text: None,
                    mime_type: None,
                    from_cache: false,
                    encoded_bytes: None,
                    duration_ms: None,
                    failed: None,
                    state: NetState::Pending,
                    page_url: String::new(),
                    has_post_data: req["hasPostData"].as_bool().unwrap_or(false),
                });
            }
            "Network.responseReceived" => {
                let rid = p["requestId"].as_str().unwrap_or_default();
                let rtype = p["type"].as_str().map(String::from);
                if let Some(e) = tab.network_mut(rid) {
                    apply_response(e, &p["response"]);
                    if e.resource_type.is_none() {
                        e.resource_type = rtype;
                    }
                }
            }
            "Network.loadingFinished" => {
                let rid = p["requestId"].as_str().unwrap_or_default();
                if let Some(e) = tab.network_mut(rid) {
                    e.encoded_bytes = p["encodedDataLength"].as_f64().map(|b| b as u64);
                    e.duration_ms = duration_ms(e.monotonic_start, p["timestamp"].as_f64());
                    if e.state == NetState::Pending {
                        e.state = NetState::Finished;
                    }
                }
            }
            "Network.loadingFailed" => {
                let rid = p["requestId"].as_str().unwrap_or_default();
                if let Some(e) = tab.network_mut(rid) {
                    let mut why = p["errorText"].as_str().unwrap_or("failed").to_string();
                    if p["canceled"].as_bool().unwrap_or(false) {
                        why.push_str(" (canceled)");
                    }
                    if let Some(b) = p["blockedReason"].as_str() {
                        why.push_str(&format!(" blocked: {b}"));
                    }
                    e.failed = Some(why);
                    e.state = NetState::Failed;
                    e.duration_ms = duration_ms(e.monotonic_start, p["timestamp"].as_f64());
                }
            }
            "Page.frameNavigated" => {
                let frame = &p["frame"];
                if frame.get("parentId").is_none() {
                    if let Some(u) = frame["url"].as_str() {
                        tab.page_url = truncate(u, URL_CAP);
                    }
                }
            }
            _ => {}
        }
    }
}

impl HubInner {
    /// Route one WebDriver BiDi event. The browsing-context id is the
    /// target id, so no session map is involved; events for untouched
    /// contexts (including child frames) cost one hash miss.
    fn route_bidi(&mut self, ev: BidiEvent) {
        let p = &ev.params;
        let ctx = match ev.method.as_str() {
            "log.entryAdded" => p["source"]["context"].as_str(),
            _ => p["context"].as_str(),
        };
        let Some(ctx) = ctx else {
            return;
        };
        if ev.method == "browsingContext.contextDestroyed" {
            self.tabs.remove(ctx);
            return;
        }
        let Some(tab) = self.tabs.get_mut(ctx) else {
            return;
        };
        match ev.method.as_str() {
            "log.entryAdded" => {
                if let Some(e) = console_entry_from_bidi(p) {
                    tab.push_console(e);
                }
            }
            "network.beforeRequestSent" => {
                let req = &p["request"];
                let rid = req["request"].as_str().unwrap_or_default().to_string();
                let start = bidi_secs(p);
                if p["redirectCount"].as_u64().unwrap_or(0) > 0 {
                    if let Some(prev) = tab.network_mut(&rid) {
                        if prev.state != NetState::Redirected {
                            prev.state = NetState::Redirected;
                            if prev.duration_ms.is_none() {
                                prev.duration_ms = duration_ms(prev.monotonic_start, start);
                            }
                        }
                    }
                }
                let is_navigation = p["navigation"].as_str().is_some();
                tab.push_network(NetworkEntry {
                    seq: 0,
                    request_id: rid,
                    ts_ms: p["timestamp"].as_f64().unwrap_or(0.0),
                    monotonic_start: start.unwrap_or(0.0),
                    method: req["method"].as_str().unwrap_or("GET").to_string(),
                    url: truncate(req["url"].as_str().unwrap_or_default(), URL_CAP),
                    resource_type: bidi_resource_type(
                        req,
                        is_navigation,
                        p["initiator"]["type"].as_str(),
                        None,
                    ),
                    status: None,
                    status_text: None,
                    mime_type: None,
                    from_cache: false,
                    encoded_bytes: None,
                    duration_ms: None,
                    failed: None,
                    state: NetState::Pending,
                    page_url: String::new(),
                    has_post_data: req["bodySize"].as_u64().unwrap_or(0) > 0,
                });
            }
            "network.responseCompleted" => {
                let rid = p["request"]["request"].as_str().unwrap_or_default();
                let end = bidi_secs(p);
                if let Some(e) = tab.network_mut(rid) {
                    apply_bidi_response(e, &p["response"]);
                    e.duration_ms = duration_ms(e.monotonic_start, end);
                    if e.resource_type.is_none() {
                        e.resource_type =
                            bidi_resource_type(&p["request"], false, None, e.mime_type.as_deref());
                    }
                    if e.state == NetState::Pending {
                        e.state = NetState::Finished;
                    }
                }
            }
            "network.fetchError" => {
                let rid = p["request"]["request"].as_str().unwrap_or_default();
                let end = bidi_secs(p);
                if let Some(e) = tab.network_mut(rid) {
                    e.failed = Some(p["errorText"].as_str().unwrap_or("failed").to_string());
                    e.state = NetState::Failed;
                    e.duration_ms = duration_ms(e.monotonic_start, end);
                }
            }
            "browsingContext.navigationStarted" => {
                if let Some(u) = p["url"].as_str() {
                    tab.page_url = truncate(u, URL_CAP);
                }
            }
            _ => {}
        }
    }
}

/// BiDi timestamps are epoch milliseconds; the shared `duration_ms` works
/// in seconds.
fn bidi_secs(p: &Value) -> Option<f64> {
    p["timestamp"].as_f64().map(|t| t / 1000.0)
}

fn apply_bidi_response(e: &mut NetworkEntry, r: &Value) {
    e.status = r["status"].as_u64().map(|s| s as u16);
    e.status_text = r["statusText"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    e.mime_type = r["mimeType"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    e.from_cache = r["fromCache"].as_bool().unwrap_or(false);
    e.encoded_bytes = r["bytesReceived"].as_f64().map(|b| b as u64);
}

/// Derive a CDP-style resource label for a BiDi request so
/// `resource_type` filters behave the same on both engines. Firefox
/// hard-codes `initiator.type = "other"`, so it is only consulted for
/// preflights; `request.initiatorType` / `destination` (Firefox 129+) and,
/// at response time, the MIME type do the real work.
fn bidi_resource_type(
    req: &Value,
    is_navigation: bool,
    initiator_type: Option<&str>,
    mime: Option<&str>,
) -> Option<String> {
    if is_navigation {
        return Some("Document".into());
    }
    if initiator_type == Some("preflight") {
        return Some("Preflight".into());
    }
    let destination = req["destination"].as_str().unwrap_or("");
    let by_initiator = match req["initiatorType"].as_str().unwrap_or("") {
        "xmlhttprequest" => Some("XHR"),
        "fetch" => Some("Fetch"),
        "script" => Some("Script"),
        "css" => Some("Stylesheet"),
        "img" | "image" | "input" => Some("Image"),
        "font" => Some("Font"),
        "iframe" | "frame" => Some("Document"),
        "beacon" => Some("Ping"),
        "audio" | "video" | "track" => Some("Media"),
        "link" if destination == "style" => Some("Stylesheet"),
        _ => None,
    };
    if let Some(t) = by_initiator {
        return Some(t.into());
    }
    let by_destination = match destination {
        "document" | "iframe" | "frame" => Some("Document"),
        "script" | "worker" | "sharedworker" | "serviceworker" => Some("Script"),
        "style" => Some("Stylesheet"),
        "image" => Some("Image"),
        "font" => Some("Font"),
        "manifest" => Some("Manifest"),
        "audio" | "video" => Some("Media"),
        _ => None,
    };
    if let Some(t) = by_destination {
        return Some(t.into());
    }
    let m = mime?.to_ascii_lowercase();
    let by_mime = if m.starts_with("text/css") {
        "Stylesheet"
    } else if m.contains("javascript") || m.contains("ecmascript") {
        "Script"
    } else if m.starts_with("image/") {
        "Image"
    } else if m.starts_with("font/") || m.starts_with("application/font") {
        "Font"
    } else if m.starts_with("application/json") {
        "Fetch"
    } else {
        return None;
    };
    Some(by_mime.into())
}

/// `log.entryAdded` → console entry. Console entries take their level
/// from the console method (BiDi reports `console.log` as `info`);
/// JavaScript errors become `exception` entries with a rendered stack.
fn console_entry_from_bidi(p: &Value) -> Option<ConsoleEntry> {
    let fallback_level = match p["level"].as_str() {
        Some("error") => Level::Error,
        Some("warn") => Level::Warn,
        Some("debug") => Level::Debug,
        _ => Level::Info,
    };
    let (level, source, text, stack) = if p["type"].as_str() == Some("javascript") {
        let full = p["text"].as_str().unwrap_or("Uncaught exception");
        let first = full.lines().next().unwrap_or(full).to_string();
        (
            Level::Error,
            "exception".to_string(),
            first,
            Some(bidi_stack_string(full, &p["stackTrace"])),
        )
    } else {
        let method = p["method"].as_str().unwrap_or("log");
        let level = match method {
            "error" | "assert" => Level::Error,
            "warn" => Level::Warn,
            "info" => Level::Info,
            "debug" | "trace" => Level::Debug,
            "clear" | "group" | "groupCollapsed" | "groupEnd" | "profile" | "profileEnd" => {
                return None
            }
            "log" | "dir" | "dirxml" | "table" | "count" | "countReset" | "timeEnd" | "timeLog" => {
                Level::Log
            }
            _ => fallback_level,
        };
        // `warn` → `console.warning` so a `pattern` matches on both engines.
        let source = format!(
            "console.{}",
            if method == "warn" { "warning" } else { method }
        );
        // Firefox formats object arguments in `text` as `[object Object]`,
        // so render the structured `args` when present and fall back to
        // `text` only when the entry carries none.
        let text = match p["args"].as_array().filter(|a| !a.is_empty()) {
            Some(args) => args
                .iter()
                .map(|a| render_bidi_value(a, false))
                .collect::<Vec<_>>()
                .join(" "),
            None => p["text"].as_str().unwrap_or_default().to_string(),
        };
        (level, source, text, None)
    };
    let (url, line, column) = location(&p["stackTrace"]["callFrames"][0]);
    Some(ConsoleEntry {
        seq: 0,
        ts_ms: p["timestamp"].as_f64().unwrap_or(0.0),
        level,
        source,
        text: truncate(&text, CONSOLE_TEXT_CAP),
        url,
        line,
        column,
        page_url: String::new(),
        stack,
        network_request_id: None,
    })
}

/// `text` plus one `    at fn (url:line:col)` line per BiDi stack frame
/// (1-based), mirroring CDP's `exception.description`.
fn bidi_stack_string(text: &str, stack: &Value) -> String {
    let mut out = text.to_string();
    if let Some(frames) = stack["callFrames"].as_array() {
        for f in frames {
            let (url, line, col) = location(f);
            out.push_str(&format!(
                "\n    at {} ({}:{}:{})",
                f["functionName"].as_str().unwrap_or("<anonymous>"),
                url.unwrap_or_default(),
                line.unwrap_or(0),
                col.unwrap_or(0)
            ));
        }
    }
    out
}

/// Render a BiDi `script.RemoteValue` for console output.
pub fn render_bidi_remote_value(v: &Value) -> String {
    render_bidi_value(v, false)
}

fn render_bidi_value(v: &Value, nested: bool) -> String {
    let join = |items: &[Value]| {
        items
            .iter()
            .map(|i| render_bidi_value(i, true))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let pairs = |items: &[Value], sep: &str| {
        items
            .iter()
            .map(|pair| {
                let k = match pair.get(0) {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => render_bidi_value(other, true),
                    None => String::new(),
                };
                let val = pair
                    .get(1)
                    .map(|x| render_bidi_value(x, true))
                    .unwrap_or_default();
                format!("{k}{sep}{val}")
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    match v["type"].as_str().unwrap_or("undefined") {
        "string" => {
            let s = v["value"].as_str().unwrap_or("");
            if nested {
                json!(s).to_string()
            } else {
                s.to_string()
            }
        }
        "number" => match &v["value"] {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => "NaN".into(),
        },
        "boolean" => v["value"]
            .as_bool()
            .map(|b| b.to_string())
            .unwrap_or_default(),
        "null" => "null".into(),
        "undefined" => "undefined".into(),
        "bigint" => format!("{}n", v["value"].as_str().unwrap_or("0")),
        "array" => match v["value"].as_array() {
            Some(items) => format!("[{}]", join(items)),
            None => "Array".into(),
        },
        "object" => match v["value"].as_array() {
            Some(items) => format!("{{{}}}", pairs(items, ": ")),
            None => "Object".into(),
        },
        "map" => match v["value"].as_array() {
            Some(items) => format!("Map {{{}}}", pairs(items, " => ")),
            None => "Map".into(),
        },
        "set" => match v["value"].as_array() {
            Some(items) => format!("Set {{{}}}", join(items)),
            None => "Set".into(),
        },
        "regexp" => format!(
            "/{}/{}",
            v["value"]["pattern"].as_str().unwrap_or(""),
            v["value"]["flags"].as_str().unwrap_or("")
        ),
        "date" => v["value"].as_str().unwrap_or("Date").to_string(),
        "error" => "Error".into(),
        "node" => v["value"]["localName"]
            .as_str()
            .map(|n| format!("<{n}>"))
            .unwrap_or_else(|| "Node".into()),
        "function" => "function".into(),
        other => other.to_string(),
    }
}

fn duration_ms(start: f64, end: Option<f64>) -> Option<f64> {
    end.filter(|_| start > 0.0)
        .map(|e| ((e - start) * 1000.0).max(0.0))
}

fn apply_response(e: &mut NetworkEntry, r: &Value) {
    e.status = r["status"].as_u64().map(|s| s as u16);
    e.status_text = r["statusText"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    e.mime_type = r["mimeType"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    e.from_cache = r["fromDiskCache"].as_bool().unwrap_or(false)
        || r["fromServiceWorker"].as_bool().unwrap_or(false)
        || r["fromPrefetchCache"].as_bool().unwrap_or(false);
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

fn location(frame: &Value) -> (Option<String>, Option<u32>, Option<u32>) {
    let url = frame["url"]
        .as_str()
        .filter(|u| !u.is_empty())
        .map(|u| truncate(u, URL_CAP));
    let line = frame["lineNumber"].as_u64().map(|l| l as u32 + 1);
    let col = frame["columnNumber"].as_u64().map(|c| c as u32 + 1);
    (url, line, col)
}

fn console_entry_from_api(p: &Value) -> Option<ConsoleEntry> {
    let kind = p["type"].as_str().unwrap_or("log");
    let level = match kind {
        "error" | "assert" => Level::Error,
        "warning" => Level::Warn,
        "info" => Level::Info,
        "debug" => Level::Debug,
        "clear" | "startGroup" | "startGroupCollapsed" | "endGroup" | "profile" | "profileEnd" => {
            return None
        }
        _ => Level::Log,
    };
    let text = p["args"]
        .as_array()
        .map(|args| {
            args.iter()
                .map(render_remote_object)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let (url, line, column) = location(&p["stackTrace"]["callFrames"][0]);
    Some(ConsoleEntry {
        seq: 0,
        ts_ms: p["timestamp"].as_f64().unwrap_or(0.0),
        level,
        source: format!("console.{kind}"),
        text: truncate(&text, CONSOLE_TEXT_CAP),
        url,
        line,
        column,
        page_url: String::new(),
        stack: None,
        network_request_id: None,
    })
}

fn console_entry_from_exception(p: &Value) -> ConsoleEntry {
    let d = &p["exceptionDetails"];
    let description = d["exception"]["description"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    let text = description
        .as_deref()
        .and_then(|s| s.lines().next())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| {
            d["text"]
                .as_str()
                .unwrap_or("Uncaught exception")
                .to_string()
        });
    let (mut url, mut line, mut column) = location(&d["stackTrace"]["callFrames"][0]);
    if url.is_none() {
        url = d["url"]
            .as_str()
            .filter(|u| !u.is_empty())
            .map(String::from);
        line = d["lineNumber"].as_u64().map(|l| l as u32 + 1);
        column = d["columnNumber"].as_u64().map(|c| c as u32 + 1);
    }
    ConsoleEntry {
        seq: 0,
        ts_ms: p["timestamp"].as_f64().unwrap_or(0.0),
        level: Level::Error,
        source: "exception".into(),
        text: truncate(&text, CONSOLE_TEXT_CAP),
        url,
        line,
        column,
        page_url: String::new(),
        stack: description,
        network_request_id: None,
    }
}

fn console_entry_from_log(p: &Value) -> Option<ConsoleEntry> {
    let e = &p["entry"];
    let level = match e["level"].as_str().unwrap_or("info") {
        "error" => Level::Error,
        "warning" => Level::Warn,
        "verbose" => Level::Debug,
        _ => Level::Info,
    };
    let text = e["text"].as_str()?.to_string();
    let (mut url, mut line, mut column) = location(&e["stackTrace"]["callFrames"][0]);
    if url.is_none() {
        url = e["url"]
            .as_str()
            .filter(|u| !u.is_empty())
            .map(|u| truncate(u, URL_CAP));
        line = e["lineNumber"].as_u64().map(|l| l as u32 + 1);
        column = None;
    }
    Some(ConsoleEntry {
        seq: 0,
        ts_ms: e["timestamp"].as_f64().unwrap_or(0.0),
        level,
        source: e["source"].as_str().unwrap_or("other").to_string(),
        text: truncate(&text, CONSOLE_TEXT_CAP),
        url,
        line,
        column,
        page_url: String::new(),
        stack: None,
        network_request_id: e["networkRequestId"].as_str().map(String::from),
    })
}

/// Render a `Runtime.RemoteObject` the way DevTools' console preview
/// does, without any round trip: primitives verbatim, objects from their
/// `preview`, everything else by `description`.
pub fn render_remote_object(o: &Value) -> String {
    if o["type"] == "string" {
        if let Some(s) = o["value"].as_str() {
            return s.to_string();
        }
    }
    if let Some(v) = o.get("value") {
        if !v.is_null() || o["type"] == "object" {
            return v.to_string();
        }
    }
    if let Some(u) = o["unserializableValue"].as_str() {
        return u.to_string();
    }
    if let Some(preview) = o.get("preview") {
        return render_preview(preview);
    }
    if let Some(d) = o["description"].as_str() {
        return d.to_string();
    }
    o["type"].as_str().unwrap_or("undefined").to_string()
}

fn render_preview(preview: &Value) -> String {
    if preview["subtype"] == "error" {
        if let Some(d) = preview["description"].as_str() {
            return d.lines().next().unwrap_or(d).to_string();
        }
    }
    let is_array = preview["subtype"] == "array";
    let props: Vec<String> = preview["properties"]
        .as_array()
        .map(|ps| {
            ps.iter()
                .map(|p| {
                    let val = match p.get("valuePreview") {
                        Some(vp) => render_preview(vp),
                        None => match p["type"].as_str() {
                            Some("string") => json!(p["value"].as_str().unwrap_or("")).to_string(),
                            Some("object") | Some("function") => p["value"]
                                .as_str()
                                .map(String::from)
                                .unwrap_or_else(|| p["type"].as_str().unwrap_or("").to_string()),
                            _ => p["value"].as_str().unwrap_or("undefined").to_string(),
                        },
                    };
                    if is_array {
                        val
                    } else {
                        format!("{}: {val}", p["name"].as_str().unwrap_or("?"))
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let overflow = preview["overflow"].as_bool().unwrap_or(false);
    let mut body = props.join(", ");
    if overflow {
        body.push_str(if props.is_empty() { "…" } else { ", …" });
    }
    if is_array {
        format!("[{body}]")
    } else {
        let desc = preview["description"].as_str().unwrap_or("Object");
        if desc == "Object" {
            format!("{{{body}}}")
        } else {
            format!("{desc} {{{body}}}")
        }
    }
}

/// Epoch milliseconds → `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn fmt_iso_ms(ts_ms: f64) -> String {
    if ts_ms <= 0.0 {
        return "-".into();
    }
    let secs = (ts_ms / 1000.0).floor() as i64;
    let millis = (ts_ms - secs as f64 * 1000.0).round().clamp(0.0, 999.0) as u32;
    let base = crate::registry::format_unix_seconds_as_iso8601(secs);
    format!("{}.{millis:03}Z", base.trim_end_matches('Z'))
}

fn fmt_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n}B")
    } else if n < 1024 * 1024 {
        format!("{:.1}KB", n as f64 / 1024.0)
    } else {
        format!("{:.1}MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// One console entry as a single text line (no page separator).
pub fn format_console_line(e: &ConsoleEntry) -> String {
    let loc = match (&e.url, e.line, e.column) {
        (Some(u), Some(l), Some(c)) => format!("{u}:{l}:{c}"),
        (Some(u), Some(l), None) => format!("{u}:{l}"),
        (Some(u), None, _) => u.clone(),
        (None, _, _) => e.source.clone(),
    };
    format!(
        "[{}] {} {loc}  {}",
        e.level.label(),
        fmt_iso_ms(e.ts_ms),
        e.text.replace('\n', "\\n")
    )
}

/// One network entry as a single text line.
pub fn format_network_line(e: &NetworkEntry) -> String {
    let outcome = match e.state {
        NetState::Pending => "→ pending".to_string(),
        NetState::Failed => format!("→ failed {}", e.failed.as_deref().unwrap_or("")),
        NetState::Finished | NetState::Redirected => {
            let mut s = format!(
                "→ {}",
                e.status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "?".into())
            );
            if let Some(m) = &e.mime_type {
                s.push(' ');
                s.push_str(m);
            }
            if let Some(b) = e.encoded_bytes {
                s.push(' ');
                s.push_str(&fmt_bytes(b));
            }
            if let Some(d) = e.duration_ms {
                s.push_str(&format!(" {}ms", d.round() as u64));
            }
            if e.from_cache {
                s.push_str(" (cache)");
            }
            if e.state == NetState::Redirected {
                s.push_str(" [redirect]");
            }
            s
        }
    };
    let rtype = e
        .resource_type
        .as_deref()
        .map(|t| format!(" [{t}]"))
        .unwrap_or_default();
    format!(
        "{}  {:<6} {}  {outcome}{rtype}",
        e.request_id, e.method, e.url
    )
}

/// Filters for `read_console`.
#[derive(Debug, Default)]
pub struct ConsoleQuery {
    pub pattern: Option<Regex>,
    pub only_errors: bool,
    pub limit: usize,
    pub clear: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Exact(u16),
    /// `2xx` → 2, etc.
    Class(u16),
    Failed,
    Pending,
}

impl StatusFilter {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "failed" => return Ok(StatusFilter::Failed),
            "pending" => return Ok(StatusFilter::Pending),
            _ => {}
        }
        if let Some(cls) = s.strip_suffix("xx") {
            if let Ok(c) = cls.parse::<u16>() {
                if (1..=5).contains(&c) {
                    return Ok(StatusFilter::Class(c));
                }
            }
        }
        s.parse::<u16>().map(StatusFilter::Exact).map_err(|_| {
            anyhow!("`status` must be a code (404), a class (4xx), \"failed\", or \"pending\"")
        })
    }

    fn matches(self, e: &NetworkEntry) -> bool {
        match self {
            StatusFilter::Failed => e.state == NetState::Failed,
            StatusFilter::Pending => e.state == NetState::Pending,
            StatusFilter::Exact(c) => e.status == Some(c),
            StatusFilter::Class(c) => e.status.is_some_and(|s| s / 100 == c),
        }
    }
}

/// Filters for `read_network`.
#[derive(Debug, Default)]
pub struct NetworkQuery {
    pub url_pattern: Option<Regex>,
    pub method: Option<String>,
    pub status: Option<StatusFilter>,
    pub resource_type: Option<String>,
    pub limit: usize,
    pub clear: bool,
}

/// Result of a console read.
#[derive(Debug, Serialize)]
pub struct ConsoleReport {
    pub target_id: String,
    pub page_url: String,
    pub matched: usize,
    pub buffered: usize,
    pub evicted: u64,
    pub lost_events: u64,
    pub entries: Vec<ConsoleEntry>,
}

/// Result of a network read.
#[derive(Debug, Serialize)]
pub struct NetworkReport {
    pub target_id: String,
    pub page_url: String,
    pub matched: usize,
    pub buffered: usize,
    pub evicted: u64,
    pub lost_events: u64,
    pub entries: Vec<NetworkEntry>,
}

/// Result of a body fetch.
#[derive(Debug)]
pub struct BodyResult {
    pub request_id: String,
    pub url: String,
    pub status: Option<u16>,
    pub mime_type: Option<String>,
    pub bytes: Vec<u8>,
    pub total_bytes: usize,
    pub truncated: bool,
}

/// Counters shared by the console and network header lines.
struct HeaderStats<'a> {
    target_id: &'a str,
    page_url: &'a str,
    shown: usize,
    matched: usize,
    buffered: usize,
    evicted: u64,
    lost: u64,
}

fn header_line(kind: &str, s: &HeaderStats<'_>) -> String {
    format!(
        "{kind} tab={} page={}  showing {} of {} matched ({} buffered, {} evicted, {} events lost)\n",
        s.target_id,
        if s.page_url.is_empty() {
            "-"
        } else {
            s.page_url
        },
        s.shown,
        s.matched,
        s.buffered,
        s.evicted,
        s.lost
    )
}

/// Render a console report as text with `-- page: <url> --` separators.
pub fn format_console_text(r: &ConsoleReport) -> String {
    let mut out = header_line(
        "console",
        &HeaderStats {
            target_id: &r.target_id,
            page_url: &r.page_url,
            shown: r.entries.len(),
            matched: r.matched,
            buffered: r.buffered,
            evicted: r.evicted,
            lost: r.lost_events,
        },
    );
    let mut last_page: Option<&str> = None;
    for e in &r.entries {
        if last_page != Some(e.page_url.as_str()) {
            out.push_str(&format!("-- page: {} --\n", e.page_url));
            last_page = Some(&e.page_url);
        }
        out.push_str(&format_console_line(e));
        out.push('\n');
    }
    out
}

/// Render a network report as text.
pub fn format_network_text(r: &NetworkReport) -> String {
    let mut out = header_line(
        "network",
        &HeaderStats {
            target_id: &r.target_id,
            page_url: &r.page_url,
            shown: r.entries.len(),
            matched: r.matched,
            buffered: r.buffered,
            evicted: r.evicted,
            lost: r.lost_events,
        },
    );
    let mut last_page: Option<&str> = None;
    for e in &r.entries {
        if last_page != Some(e.page_url.as_str()) {
            out.push_str(&format!("-- page: {} --\n", e.page_url));
            last_page = Some(&e.page_url);
        }
        out.push_str(&format_network_line(e));
        out.push('\n');
    }
    out
}

/// The capture hub. Cheap to clone via `Arc` on `ServerState`.
pub struct CaptureHub {
    inner: Arc<Mutex<HubInner>>,
}

impl Default for CaptureHub {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CaptureHub {
    fn drop(&mut self) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(h) = g.router.take() {
                h.abort();
            }
        }
    }
}

fn capture_disabled_error() -> anyhow::Error {
    anyhow!("console/network capture is disabled for this MCP server (BROWSER_CONTROL_CAPTURE=0); unset it and restart the server to capture")
}

fn no_capture_error(target_id: &str) -> anyhow::Error {
    anyhow!(
        "no capture for tab {target_id}: the MCP server attaches to a tab when a tool first touches it (browser_navigate, browser_tab_select, …); navigate or select the tab, act, then read again. Capture runs on Chromium (CDP) and Firefox (BiDi) and can be disabled with BROWSER_CONTROL_CAPTURE=0"
    )
}

impl CaptureHub {
    pub fn new() -> Self {
        let disabled = std::env::var("BROWSER_CONTROL_CAPTURE")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "0" || v == "false" || v == "off"
            })
            .unwrap_or(false);
        Self {
            inner: Arc::new(Mutex::new(HubInner {
                disabled,
                ..Default::default()
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether capture is disabled by environment.
    pub fn disabled(&self) -> bool {
        self.lock().disabled
    }

    /// Start capturing `target_id` if not already. Synchronous and
    /// non-blocking: the attach (CDP) or subscribe + seed (BiDi) runs on a
    /// background task.
    pub fn touch(&self, backend: &TabBackend, target_id: &str) {
        let weak = Arc::downgrade(&self.inner);
        let mut g = self.lock();
        if g.disabled || g.tabs.contains_key(target_id) {
            return;
        }
        g.tabs.insert(target_id.to_string(), TabCapture::new());
        match backend {
            TabBackend::Cdp(client) => {
                if g.router.is_none() {
                    let rx = client.subscribe();
                    g.router = Some(tokio::spawn(run_router(rx, weak.clone(), HubInner::route)));
                }
                drop(g);
                tokio::spawn(attach_task(client.clone(), target_id.to_string(), weak));
            }
            TabBackend::Bidi(client) => {
                if g.router.is_none() {
                    let rx = client.subscribe();
                    g.router = Some(tokio::spawn(run_router(
                        rx,
                        weak.clone(),
                        HubInner::route_bidi,
                    )));
                }
                let cell = g
                    .bidi
                    .get_or_insert_with(|| Arc::new(OnceCell::new()))
                    .clone();
                drop(g);
                tokio::spawn(bidi_attach_task(
                    client.clone(),
                    target_id.to_string(),
                    cell,
                    weak,
                ));
            }
        }
    }

    /// `touch`, then wait (bounded by [`TOUCH_WAIT`]) until the tab's
    /// domains are enabled so events from the caller's next action are
    /// captured. Never errors.
    pub async fn touch_and_wait(&self, backend: &TabBackend, target_id: &str) {
        self.touch(backend, target_id);
        self.wait_ready(target_id).await;
    }

    async fn wait_ready(&self, target_id: &str) {
        let deadline = tokio::time::Instant::now() + TOUCH_WAIT;
        loop {
            let notify = {
                let g = self.lock();
                match g.tabs.get(target_id) {
                    None => return,
                    Some(t) if t.ready => return,
                    Some(t) => t.notify.clone(),
                }
            };
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            // Re-check after arming so a completion between the two locks
            // cannot be missed.
            {
                let g = self.lock();
                match g.tabs.get(target_id) {
                    None => return,
                    Some(t) if t.ready => return,
                    Some(_) => {}
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }

    /// Drop the state for a closed tab and detach the hub session.
    pub fn forget(&self, backend: &TabBackend, target_id: &str) {
        let sid = {
            let mut g = self.lock();
            let sid = g.tabs.remove(target_id).and_then(|t| t.session_id);
            g.sessions.retain(|_, t| t != target_id);
            sid
        };
        if let (Some(sid), TabBackend::Cdp(client)) = (sid, backend) {
            let client = client.clone();
            tokio::spawn(async move {
                let _ = client
                    .send("Target.detachFromTarget", json!({ "sessionId": sid }))
                    .await;
            });
        }
    }

    /// Forget everything (browser switch). No RPCs: dropping the backend
    /// closes the socket, and the browser discards its sessions and
    /// subscriptions.
    pub fn reset(&self) {
        let mut g = self.lock();
        if let Some(h) = g.router.take() {
            h.abort();
        }
        g.tabs.clear();
        g.sessions.clear();
        g.bidi = None;
        g.lost_events = 0;
    }

    /// Number of tabs currently captured (diagnostics / tests).
    pub fn captured_tabs(&self) -> Vec<String> {
        self.lock().tabs.keys().cloned().collect()
    }

    /// Read (and optionally clear) the console buffer of a tab.
    pub async fn read_console(&self, target_id: &str, q: &ConsoleQuery) -> Result<ConsoleReport> {
        self.wait_ready(target_id).await;
        let mut g = self.lock();
        if g.disabled {
            return Err(capture_disabled_error());
        }
        let lost = g.lost_events;
        let tab = g
            .tabs
            .get_mut(target_id)
            .ok_or_else(|| no_capture_error(target_id))?;
        let buffered = tab.console.len();
        let all: Vec<&ConsoleEntry> = tab
            .console
            .iter()
            .filter(|e| !q.only_errors || e.level == Level::Error)
            .filter(|e| match &q.pattern {
                Some(re) => re.is_match(&format!("{} {}", format_console_line(e), e.page_url)),
                None => true,
            })
            .collect();
        let matched = all.len();
        let skip = matched.saturating_sub(q.limit);
        let entries: Vec<ConsoleEntry> = all.into_iter().skip(skip).cloned().collect();
        let report = ConsoleReport {
            target_id: target_id.to_string(),
            page_url: tab.page_url.clone(),
            matched,
            buffered,
            evicted: tab.dropped_console,
            lost_events: lost,
            entries,
        };
        if q.clear {
            tab.console.clear();
            tab.dropped_console = 0;
        }
        Ok(report)
    }

    /// Read (and optionally clear) the network buffer of a tab.
    pub async fn read_network(&self, target_id: &str, q: &NetworkQuery) -> Result<NetworkReport> {
        self.wait_ready(target_id).await;
        let mut g = self.lock();
        if g.disabled {
            return Err(capture_disabled_error());
        }
        let lost = g.lost_events;
        let network_armed = g
            .bidi
            .as_ref()
            .and_then(|c| c.get())
            .map(|c| c.network)
            .unwrap_or(true);
        let tab = g
            .tabs
            .get_mut(target_id)
            .ok_or_else(|| no_capture_error(target_id))?;
        if !network_armed {
            return Err(anyhow!(
                "network capture is unavailable on this Firefox: session.subscribe for network.* was rejected (needs Firefox 124 or newer); console capture still works"
            ));
        }
        let buffered = tab.network.len();
        let method = q.method.as_ref().map(|m| m.to_ascii_uppercase());
        let rtype = q.resource_type.as_ref().map(|t| t.to_ascii_lowercase());
        let all: Vec<&NetworkEntry> = tab
            .network
            .iter()
            .filter(|e| match &q.url_pattern {
                Some(re) => re.is_match(&e.url),
                None => true,
            })
            .filter(|e| match &method {
                Some(m) => e.method.eq_ignore_ascii_case(m),
                None => true,
            })
            .filter(|e| match &rtype {
                Some(t) => e
                    .resource_type
                    .as_deref()
                    .is_some_and(|r| r.eq_ignore_ascii_case(t)),
                None => true,
            })
            .filter(|e| q.status.map(|s| s.matches(e)).unwrap_or(true))
            .collect();
        let matched = all.len();
        let skip = matched.saturating_sub(q.limit);
        let entries: Vec<NetworkEntry> = all.into_iter().skip(skip).cloned().collect();
        let report = NetworkReport {
            target_id: target_id.to_string(),
            page_url: tab.page_url.clone(),
            matched,
            buffered,
            evicted: tab.dropped_network,
            lost_events: lost,
            entries,
        };
        if q.clear {
            tab.network.clear();
            tab.dropped_network = 0;
        }
        Ok(report)
    }

    /// Fetch a captured response body through the hub's session.
    pub async fn response_body(
        &self,
        backend: &TabBackend,
        target_id: &str,
        request_id: &str,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<BodyResult> {
        let TabBackend::Cdp(client) = backend else {
            return Err(anyhow!("{BIDI_NO_BODIES_HINT}"));
        };
        self.wait_ready(target_id).await;
        let (sid, url, status, mime_type) = {
            let g = self.lock();
            let tab = g
                .tabs
                .get(target_id)
                .ok_or_else(|| no_capture_error(target_id))?;
            let sid = tab
                .session_id
                .clone()
                .ok_or_else(|| no_capture_error(target_id))?;
            let entry = tab
                .network
                .iter()
                .rev()
                .find(|e| e.request_id == request_id)
                .ok_or_else(|| {
                    anyhow!(
                        "unknown request id `{request_id}` for tab {target_id}: it was evicted from the {NETWORK_CAP}-entry buffer or belongs to another tab; list requests with browser_network_requests first"
                    )
                })?;
            if entry.state == NetState::Pending {
                return Err(anyhow!(
                    "response for request `{request_id}` has not finished yet; retry after the request completes"
                ));
            }
            (
                sid,
                entry.url.clone(),
                entry.status,
                entry.mime_type.clone(),
            )
        };
        let v = match tokio::time::timeout(
            timeout,
            client.send_with_session(
                "Network.getResponseBody",
                json!({ "requestId": request_id }),
                Some(&sid),
            ),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                let msg = format!("{e:#}").to_ascii_lowercase();
                if msg.contains("no resource with given identifier")
                    || msg.contains("no data found for resource")
                {
                    return Err(anyhow!(
                        "body for request `{request_id}` is no longer available in the browser (evicted after navigation or buffer overflow); re-issue the request and fetch the body promptly"
                    ));
                }
                return Err(e);
            }
            Err(_) => {
                return Err(anyhow!(
                    "Network.getResponseBody for `{request_id}` timed out after {:?}",
                    timeout
                ))
            }
        };
        let raw = v["body"].as_str().unwrap_or_default();
        let mut bytes = if v["base64Encoded"].as_bool().unwrap_or(false) {
            base64::engine::general_purpose::STANDARD
                .decode(raw)
                .map_err(|e| anyhow!("decoding response body: {e}"))?
        } else {
            raw.as_bytes().to_vec()
        };
        let total_bytes = bytes.len();
        let cap = max_bytes.min(BODY_HARD_MAX);
        let truncated = total_bytes > cap;
        if truncated {
            bytes.truncate(cap);
        }
        Ok(BodyResult {
            request_id: request_id.to_string(),
            url,
            status,
            mime_type,
            bytes,
            total_bytes,
            truncated,
        })
    }
}

/// Attach to the target and enable the capture domains. Registers the
/// session id *before* enabling so replayed events are routed.
async fn attach_task(client: Arc<CdpClient>, target_id: String, weak: Weak<Mutex<HubInner>>) {
    let attempt = async {
        let sid = client.attach_to_target(&target_id).await?;
        let registered = {
            let Some(inner) = weak.upgrade() else {
                return Err(anyhow!("hub gone"));
            };
            let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
            match g.tabs.get_mut(&target_id) {
                Some(tab) => {
                    tab.session_id = Some(sid.clone());
                    g.sessions.insert(sid.clone(), target_id.clone());
                    true
                }
                None => false,
            }
        };
        if !registered {
            // Forgotten while attaching: release the session.
            let _ = client
                .send("Target.detachFromTarget", json!({ "sessionId": sid }))
                .await;
            return Err(anyhow!("tab forgotten during attach"));
        }
        // Seed the document URL *before* enabling domains: `Runtime.enable`
        // replays existing console messages immediately, and they must be
        // stamped with the page they came from.
        let page_url = client
            .send_with_session("Page.getNavigationHistory", json!({}), Some(&sid))
            .await
            .ok()
            .and_then(|v| {
                let idx = v["currentIndex"].as_u64()? as usize;
                v["entries"][idx]["url"].as_str().map(String::from)
            })
            .unwrap_or_default();
        if let Some(inner) = weak.upgrade() {
            let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(tab) = g.tabs.get_mut(&target_id) {
                if tab.page_url.is_empty() {
                    tab.page_url = truncate(&page_url, URL_CAP);
                }
            }
        }
        for (method, params) in [
            ("Inspector.enable", json!({})),
            ("Page.enable", json!({})),
            ("Runtime.enable", json!({})),
            ("Log.enable", json!({})),
            (
                "Network.enable",
                json!({
                    "maxTotalBufferSize": NET_MAX_TOTAL_BUFFER,
                    "maxResourceBufferSize": NET_MAX_RESOURCE_BUFFER,
                    "maxPostDataSize": 0,
                }),
            ),
        ] {
            if let Err(e) = client.send_with_session(method, params, Some(&sid)).await {
                tracing::debug!(target = %target_id, %method, error = %e, "capture enable failed");
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let outcome = tokio::time::timeout(ATTACH_TIMEOUT, attempt).await;
    finish_attach(&weak, &target_id, outcome);
}

/// Shared tail of the attach tasks: mark the tab ready, or drop it so the
/// next touch retries. Waiters are notified either way.
fn finish_attach(
    weak: &Weak<Mutex<HubInner>>,
    target_id: &str,
    outcome: Result<Result<()>, tokio::time::error::Elapsed>,
) {
    let Some(inner) = weak.upgrade() else {
        return;
    };
    let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
    match outcome {
        Ok(Ok(())) => {
            if let Some(tab) = g.tabs.get_mut(target_id) {
                tab.ready = true;
                tab.notify.notify_waiters();
            }
            return;
        }
        Ok(Err(e)) => {
            tracing::debug!(target = %target_id, error = %e, "capture attach failed");
        }
        Err(_) => {
            tracing::debug!(target = %target_id, "capture attach timed out");
        }
    }
    if let Some(tab) = g.tabs.remove(target_id) {
        tab.notify.notify_waiters();
    }
    g.sessions.retain(|_, t| t != target_id);
}

/// One global `session.subscribe` per BiDi backend. The console call is
/// required; the network call is optional so a Firefox older than 124
/// degrades to console-only capture.
async fn bidi_subscribe(client: Arc<BidiClient>) -> Result<BidiCapabilities> {
    client
        .send(
            "session.subscribe",
            json!({ "events": [
                "log.entryAdded",
                "browsingContext.navigationStarted",
                "browsingContext.contextDestroyed",
            ] }),
        )
        .await?;
    let network = match client
        .send(
            "session.subscribe",
            json!({ "events": [
                "network.beforeRequestSent",
                "network.responseCompleted",
                "network.fetchError",
            ] }),
        )
        .await
    {
        Ok(_) => true,
        Err(e) => {
            tracing::debug!(error = %e, "BiDi network capture unavailable");
            false
        }
    };
    Ok(BidiCapabilities { network })
}

/// BiDi counterpart of `attach_task`: arm the global subscription (first
/// caller only) and seed the tab's document URL from `getTree`.
async fn bidi_attach_task(
    client: Arc<BidiClient>,
    target_id: String,
    sub: Arc<OnceCell<BidiCapabilities>>,
    weak: Weak<Mutex<HubInner>>,
) {
    let attempt = async {
        // Seed the document URL before arming the subscription so replayed
        // or immediate events are stamped with the right page. When the
        // subscription already exists (second tab), events may land in the
        // brief window before the seed; those are backfilled below.
        let v = client
            .send(
                "browsingContext.getTree",
                json!({ "root": target_id, "maxDepth": 0 }),
            )
            .await?;
        let page_url = truncate(
            v["contexts"][0]["url"].as_str().unwrap_or_default(),
            URL_CAP,
        );
        if let Some(inner) = weak.upgrade() {
            let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(tab) = g.tabs.get_mut(&target_id) {
                if tab.page_url.is_empty() {
                    tab.page_url = page_url.clone();
                }
                for e in tab.console.iter_mut().filter(|e| e.page_url.is_empty()) {
                    e.page_url = page_url.clone();
                }
                for e in tab.network.iter_mut().filter(|e| e.page_url.is_empty()) {
                    e.page_url = page_url.clone();
                }
            }
        }
        sub.get_or_try_init(|| bidi_subscribe(client.clone()))
            .await?;
        Ok::<_, anyhow::Error>(())
    };
    let outcome = tokio::time::timeout(ATTACH_TIMEOUT, attempt).await;
    finish_attach(&weak, &target_id, outcome);
}

/// Drain a broadcast channel into the hub through `route`. Exits when the
/// socket closes or the hub is dropped.
async fn run_router<E: Clone + Send + 'static>(
    mut rx: broadcast::Receiver<E>,
    weak: Weak<Mutex<HubInner>>,
    route: fn(&mut HubInner, E),
) {
    loop {
        match rx.recv().await {
            Ok(ev) => {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
                route(&mut g, ev);
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                if let Some(inner) = weak.upgrade() {
                    let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
                    g.lost_events += n;
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                if let Some(inner) = weak.upgrade() {
                    let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
                    g.tabs.clear();
                    g.sessions.clear();
                    g.bidi = None;
                    g.router = None;
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    fn ev(method: &str, session: Option<&str>, params: Value) -> CdpEvent {
        CdpEvent {
            method: method.into(),
            params,
            session_id: session.map(String::from),
        }
    }

    fn hub_with_tab() -> HubInner {
        let mut h = HubInner::default();
        let mut tab = TabCapture::new();
        tab.session_id = Some("S9".into());
        tab.ready = true;
        tab.page_url = "https://app.test/x".into();
        h.tabs.insert("T1".into(), tab);
        h.sessions.insert("S9".into(), "T1".into());
        h
    }

    #[test]
    fn console_api_renders_args_and_location() {
        let mut h = hub_with_tab();
        h.route(ev(
            "Runtime.consoleAPICalled",
            Some("S9"),
            json!({
                "type": "warning",
                "timestamp": 1756816496120.0,
                "args": [
                    {"type": "string", "value": "Deprecated"},
                    {"type": "number", "value": 3},
                    {"type": "object", "preview": {"description": "Object", "overflow": true,
                        "properties": [{"name": "a", "type": "number", "value": "1"},
                                       {"name": "b", "type": "string", "value": "x"}]}},
                    {"type": "object", "subtype": "array", "preview": {"subtype": "array", "overflow": false,
                        "properties": [{"name": "0", "type": "number", "value": "1"}]}},
                    {"type": "undefined"},
                    {"type": "number", "unserializableValue": "NaN"},
                    {"type": "function", "description": "function f() {}"}
                ],
                "stackTrace": {"callFrames": [{"url": "https://app.test/a.js", "lineNumber": 11, "columnNumber": 4}]}
            }),
        ));
        let tab = &h.tabs["T1"];
        assert_eq!(tab.console.len(), 1);
        let e = &tab.console[0];
        assert_eq!(e.level, Level::Warn);
        assert_eq!(e.source, "console.warning");
        assert_eq!(
            e.text,
            "Deprecated 3 {a: 1, b: \"x\", …} [1] undefined NaN function f() {}"
        );
        assert_eq!(e.line, Some(12));
        assert_eq!(e.column, Some(5));
        assert_eq!(e.page_url, "https://app.test/x");
        assert_eq!(
            format_console_line(e),
            "[warn] 2025-09-02T12:34:56.120Z https://app.test/a.js:12:5  Deprecated 3 {a: 1, b: \"x\", …} [1] undefined NaN function f() {}"
        );
    }

    #[test]
    fn exception_uses_first_description_line_and_keeps_stack() {
        let mut h = hub_with_tab();
        h.route(ev(
            "Runtime.exceptionThrown",
            Some("S9"),
            json!({
                "timestamp": 1.0,
                "exceptionDetails": {
                    "text": "Uncaught",
                    "url": "https://app.test/a.js",
                    "lineNumber": 39, "columnNumber": 8,
                    "exception": {"description": "TypeError: x is not a function\n    at f (a.js:40:9)"}
                }
            }),
        ));
        let e = &h.tabs["T1"].console[0];
        assert_eq!(e.level, Level::Error);
        assert_eq!(e.source, "exception");
        assert_eq!(e.text, "TypeError: x is not a function");
        assert!(e.stack.as_deref().unwrap().contains("at f"));
        assert_eq!((e.line, e.column), (Some(40), Some(9)));
    }

    #[test]
    fn log_entry_maps_levels_and_group_types_skipped() {
        let mut h = hub_with_tab();
        h.route(ev(
            "Log.entryAdded",
            Some("S9"),
            json!({"entry": {"source": "network", "level": "error", "text": "Failed to load resource: 404",
                             "timestamp": 2.0, "url": "https://app.test/api/me", "networkRequestId": "1.7"}}),
        ));
        h.route(ev(
            "Runtime.consoleAPICalled",
            Some("S9"),
            json!({"type": "startGroup", "args": [], "timestamp": 3.0}),
        ));
        let tab = &h.tabs["T1"];
        assert_eq!(tab.console.len(), 1);
        let e = &tab.console[0];
        assert_eq!(e.source, "network");
        assert_eq!(e.network_request_id.as_deref(), Some("1.7"));
        assert!(format_console_line(e).contains("https://app.test/api/me  Failed"));
    }

    fn net_triplet(h: &mut HubInner, rid: &str, url: &str, status: u64) {
        h.route(ev(
            "Network.requestWillBeSent",
            Some("S9"),
            json!({"requestId": rid, "timestamp": 100.0, "wallTime": 1756816496.0, "type": "XHR",
                   "request": {"method": "get", "url": url, "hasPostData": false}}),
        ));
        h.route(ev(
            "Network.responseReceived",
            Some("S9"),
            json!({"requestId": rid, "type": "XHR",
                   "response": {"status": status, "statusText": "OK", "mimeType": "application/json"}}),
        ));
        h.route(ev(
            "Network.loadingFinished",
            Some("S9"),
            json!({"requestId": rid, "timestamp": 100.084, "encodedDataLength": 312}),
        ));
    }

    #[test]
    fn network_lifecycle_failed_and_redirect() {
        let mut h = hub_with_tab();
        net_triplet(&mut h, "1.1", "https://app.test/api/me", 401);
        h.route(ev(
            "Network.requestWillBeSent",
            Some("S9"),
            json!({"requestId": "1.2", "timestamp": 200.0, "wallTime": 1756816497.0, "type": "Script",
                   "request": {"method": "GET", "url": "https://cdn.test/app.js"}}),
        ));
        h.route(ev(
            "Network.loadingFailed",
            Some("S9"),
            json!({"requestId": "1.2", "timestamp": 200.5, "errorText": "net::ERR_BLOCKED_BY_CLIENT", "canceled": false}),
        ));
        // Redirect chain reuses the request id.
        h.route(ev(
            "Network.requestWillBeSent",
            Some("S9"),
            json!({"requestId": "1.3", "timestamp": 300.0, "wallTime": 1.0, "type": "Document",
                   "request": {"method": "GET", "url": "https://app.test/old"}}),
        ));
        h.route(ev(
            "Network.requestWillBeSent",
            Some("S9"),
            json!({"requestId": "1.3", "timestamp": 300.1, "wallTime": 1.1, "type": "Document",
                   "redirectResponse": {"status": 302, "mimeType": "text/html"},
                   "request": {"method": "GET", "url": "https://app.test/new"}}),
        ));
        let tab = &h.tabs["T1"];
        assert_eq!(tab.network.len(), 4);
        let a = &tab.network[0];
        assert_eq!(a.method, "get");
        assert_eq!(a.state, NetState::Finished);
        assert_eq!(a.status, Some(401));
        assert_eq!(a.encoded_bytes, Some(312));
        assert_eq!(a.duration_ms.map(|d| d.round()), Some(84.0));
        assert_eq!(
            format_network_line(a),
            "1.1  get    https://app.test/api/me  → 401 application/json 312B 84ms [XHR]"
        );
        let b = &tab.network[1];
        assert_eq!(b.state, NetState::Failed);
        assert_eq!(
            format_network_line(b),
            "1.2  GET    https://cdn.test/app.js  → failed net::ERR_BLOCKED_BY_CLIENT [Script]"
        );
        let c = &tab.network[2];
        assert_eq!(c.state, NetState::Redirected);
        assert_eq!(c.status, Some(302));
        assert!(
            format_network_line(c).ends_with("→ 302 text/html 100ms [redirect] [Document]"),
            "{}",
            format_network_line(c)
        );
        assert_eq!(tab.network[3].state, NetState::Pending);
        assert!(format_network_line(&tab.network[3]).contains("→ pending"));
    }

    #[test]
    fn unknown_sessions_ignored_and_detach_or_crash_drops_tab() {
        let mut h = hub_with_tab();
        h.route(ev(
            "Runtime.consoleAPICalled",
            Some("S-other"),
            json!({"type": "log", "args": [{"type": "string", "value": "x"}]}),
        ));
        assert!(h.tabs["T1"].console.is_empty());
        h.route(ev("Inspector.targetCrashed", Some("S9"), json!({})));
        assert!(h.tabs.is_empty());
        assert!(h.sessions.is_empty());

        let mut h = hub_with_tab();
        h.route(ev(
            "Target.detachedFromTarget",
            None,
            json!({"sessionId": "S9", "targetId": "T1"}),
        ));
        assert!(h.tabs.is_empty());
    }

    #[test]
    fn frame_navigated_updates_page_url_for_main_frame_only() {
        let mut h = hub_with_tab();
        h.route(ev(
            "Page.frameNavigated",
            Some("S9"),
            json!({"frame": {"id": "child", "parentId": "main", "url": "https://iframe.test/"}}),
        ));
        assert_eq!(h.tabs["T1"].page_url, "https://app.test/x");
        h.route(ev(
            "Page.frameNavigated",
            Some("S9"),
            json!({"frame": {"id": "main", "url": "https://app.test/login"}}),
        ));
        assert_eq!(h.tabs["T1"].page_url, "https://app.test/login");
        h.route(ev(
            "Runtime.consoleAPICalled",
            Some("S9"),
            json!({"type": "log", "args": [{"type": "string", "value": "after"}], "timestamp": 5.0}),
        ));
        assert_eq!(h.tabs["T1"].console[0].page_url, "https://app.test/login");
    }

    #[test]
    fn buffers_evict_at_cap_and_count() {
        let mut h = hub_with_tab();
        for i in 0..(CONSOLE_CAP + 3) {
            h.route(ev(
                "Runtime.consoleAPICalled",
                Some("S9"),
                json!({"type": "log", "args": [{"type": "string", "value": format!("m{i}")}], "timestamp": 1.0}),
            ));
        }
        let tab = &h.tabs["T1"];
        assert_eq!(tab.console.len(), CONSOLE_CAP);
        assert_eq!(tab.dropped_console, 3);
        assert_eq!(tab.console.front().unwrap().text, "m3");
    }

    #[test]
    fn status_filter_parses_and_matches() {
        assert_eq!(
            StatusFilter::parse("404").unwrap(),
            StatusFilter::Exact(404)
        );
        assert_eq!(StatusFilter::parse("4xx").unwrap(), StatusFilter::Class(4));
        assert_eq!(StatusFilter::parse("FAILED").unwrap(), StatusFilter::Failed);
        assert!(StatusFilter::parse("nope").is_err());
        assert!(StatusFilter::parse("9xx").is_err());
    }

    #[test]
    fn fmt_iso_ms_renders_millis() {
        assert_eq!(fmt_iso_ms(1756816496120.0), "2025-09-02T12:34:56.120Z");
        assert_eq!(fmt_iso_ms(0.0), "-");
    }

    #[tokio::test]
    async fn read_console_filters_limits_and_clears() {
        let hub = CaptureHub::new();
        {
            let mut g = hub.lock();
            *g = hub_with_tab();
        }
        {
            let mut g = hub.lock();
            for (i, lvl) in ["log", "error", "log", "error"].iter().enumerate() {
                g.route(ev(
                    "Runtime.consoleAPICalled",
                    Some("S9"),
                    json!({"type": lvl, "args": [{"type": "string", "value": format!("msg{i}")}], "timestamp": 1.0}),
                ));
            }
        }
        let r = hub
            .read_console(
                "T1",
                &ConsoleQuery {
                    pattern: None,
                    only_errors: true,
                    limit: 1,
                    clear: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(r.matched, 2);
        assert_eq!(r.buffered, 4);
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].text, "msg3");
        let text = format_console_text(&r);
        assert!(text.starts_with("console tab=T1 page=https://app.test/x  showing 1 of 2 matched (4 buffered, 0 evicted, 0 events lost)\n-- page: https://app.test/x --\n[error]"));

        let r = hub
            .read_console(
                "T1",
                &ConsoleQuery {
                    pattern: Some(Regex::new("msg[02]").unwrap()),
                    only_errors: false,
                    limit: 100,
                    clear: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(r.matched, 2);
        let r = hub
            .read_console(
                "T1",
                &ConsoleQuery {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(r.buffered, 0);

        let err = hub
            .read_console("T-none", &ConsoleQuery::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no capture for tab T-none"));
    }

    #[tokio::test]
    async fn read_network_filters() {
        let hub = CaptureHub::new();
        {
            let mut g = hub.lock();
            *g = hub_with_tab();
            net_triplet(&mut g, "1.1", "https://app.test/api/me", 401);
            net_triplet(&mut g, "1.2", "https://app.test/api/list", 200);
            net_triplet(&mut g, "1.3", "https://cdn.test/x.png", 200);
        }
        let r = hub
            .read_network(
                "T1",
                &NetworkQuery {
                    url_pattern: Some(Regex::new("app\\.test").unwrap()),
                    status: Some(StatusFilter::Class(2)),
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(r.matched, 1);
        assert_eq!(r.entries[0].request_id, "1.2");
        let r = hub
            .read_network(
                "T1",
                &NetworkQuery {
                    method: Some("GET".into()),
                    resource_type: Some("xhr".into()),
                    limit: 2,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(r.matched, 3);
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[0].request_id, "1.2");
    }

    /// Mock that answers attach, records enables, and pushes capture
    /// events on the attached session right after `Network.enable`.
    async fn spawn_event_mock(fail_attach: bool) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        tokio::spawn({
            let seen = seen.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let req: Value = serde_json::from_str(&t).unwrap();
                    let id = req["id"].as_u64().unwrap();
                    let method = req["method"].as_str().unwrap_or("").to_string();
                    seen.lock().unwrap().push(method.clone());
                    let sid = req["sessionId"].as_str().unwrap_or("").to_string();
                    let resp = match method.as_str() {
                        "Target.attachToTarget" if fail_attach => {
                            json!({"id": id, "error": {"code": -32000, "message": "No target with given id found"}})
                        }
                        "Target.attachToTarget" => json!({"id": id, "result": {"sessionId": "S9"}}),
                        "Page.getNavigationHistory" => json!({"id": id, "result": {
                            "currentIndex": 0, "entries": [{"url": "https://app.test/x"}]}}),
                        "Network.getResponseBody" => json!({"id": id, "result": {
                            "body": base64::engine::general_purpose::STANDARD.encode(b"{\"ok\":true}"),
                            "base64Encoded": true}}),
                        _ => json!({"id": id, "result": {}}),
                    };
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                    if method == "Network.enable" {
                        for ev in [
                            json!({"method": "Runtime.consoleAPICalled", "sessionId": sid,
                                   "params": {"type": "error", "timestamp": 1.0,
                                              "args": [{"type": "string", "value": "boom"}]}}),
                            json!({"method": "Network.requestWillBeSent", "sessionId": sid,
                                   "params": {"requestId": "7.1", "timestamp": 1.0, "wallTime": 1.0, "type": "Fetch",
                                              "request": {"method": "GET", "url": "https://app.test/api"}}}),
                            json!({"method": "Network.responseReceived", "sessionId": sid,
                                   "params": {"requestId": "7.1", "type": "Fetch",
                                              "response": {"status": 200, "mimeType": "application/json"}}}),
                            json!({"method": "Network.loadingFinished", "sessionId": sid,
                                   "params": {"requestId": "7.1", "timestamp": 1.05, "encodedDataLength": 11}}),
                        ] {
                            ws.send(Message::Text(ev.to_string())).await.unwrap();
                        }
                    }
                }
            }
        });
        (format!("ws://{addr}"), seen)
    }

    #[tokio::test]
    async fn attach_enables_domains_routes_events_and_fetches_body() {
        let (url, seen) = spawn_event_mock(false).await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let backend = TabBackend::Cdp(client);
        let hub = CaptureHub::new();
        hub.touch_and_wait(&backend, "T1").await;
        // Events are pushed by the mock right after Network.enable; give the
        // router a moment to drain them (test-side only).
        for _ in 0..50 {
            if hub
                .read_network(
                    "T1",
                    &NetworkQuery {
                        limit: 10,
                        ..Default::default()
                    },
                )
                .await
                .map(|r| {
                    r.entries
                        .first()
                        .is_some_and(|e| e.state == NetState::Finished)
                })
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        {
            let methods = seen.lock().unwrap();
            let enables: Vec<&String> = methods.iter().filter(|m| m.ends_with(".enable")).collect();
            assert_eq!(
                enables,
                vec![
                    "Inspector.enable",
                    "Page.enable",
                    "Runtime.enable",
                    "Log.enable",
                    "Network.enable"
                ]
            );
        }
        let c = hub
            .read_console(
                "T1",
                &ConsoleQuery {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.entries[0].text, "boom");
        assert_eq!(c.page_url, "https://app.test/x");
        let n = hub
            .read_network(
                "T1",
                &NetworkQuery {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(n.entries.len(), 1);
        assert_eq!(n.entries[0].state, NetState::Finished);

        let body = hub
            .response_body(&backend, "T1", "7.1", 4, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(body.total_bytes, 11);
        assert!(body.truncated);
        assert_eq!(body.bytes, b"{\"ok");
        let err = hub
            .response_body(&backend, "T1", "nope", 100, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown request id"));

        hub.forget(&backend, "T1");
        assert!(hub.captured_tabs().is_empty());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "Target.detachFromTarget"));
    }

    #[tokio::test]
    async fn attach_failure_leaves_no_state_and_retries_on_next_touch() {
        let (url, seen) = spawn_event_mock(true).await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let backend = TabBackend::Cdp(client);
        let hub = CaptureHub::new();
        hub.touch_and_wait(&backend, "T1").await;
        assert!(hub.captured_tabs().is_empty());
        hub.touch_and_wait(&backend, "T1").await;
        assert_eq!(
            seen.lock()
                .unwrap()
                .iter()
                .filter(|m| *m == "Target.attachToTarget")
                .count(),
            2
        );
        let err = hub
            .read_console("T1", &ConsoleQuery::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no capture"));
    }

    #[tokio::test]
    async fn disabled_via_env_never_attaches() {
        let hub = {
            let _guard = crate::test_support::ENV_LOCK.lock().unwrap();
            std::env::set_var("BROWSER_CONTROL_CAPTURE", "0");
            let hub = CaptureHub::new();
            std::env::remove_var("BROWSER_CONTROL_CAPTURE");
            hub
        };
        assert!(hub.disabled());
        let (url, seen) = spawn_event_mock(false).await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let backend = TabBackend::Cdp(client);
        hub.touch_and_wait(&backend, "T1").await;
        assert!(hub.captured_tabs().is_empty());
        assert!(seen.lock().unwrap().is_empty());
    }

    // -- WebDriver BiDi ingress ----------------------------------------------

    fn bev(method: &str, params: Value) -> BidiEvent {
        BidiEvent {
            method: method.into(),
            params,
        }
    }

    fn bidi_hub_with_tab() -> HubInner {
        let mut h = HubInner::default();
        let mut tab = TabCapture::new();
        tab.ready = true;
        tab.page_url = "https://app.test/x".into();
        h.tabs.insert("C1".into(), tab);
        h
    }

    #[test]
    fn bidi_console_uses_text_and_location() {
        let mut h = bidi_hub_with_tab();
        h.route_bidi(bev(
            "log.entryAdded",
            json!({
                "type": "console", "level": "warn", "method": "warn",
                "text": "Deprecated 3", "args": [],
                "timestamp": 1756816496120.0,
                "source": {"realm": "R1", "context": "C1"},
                "stackTrace": {"callFrames": [{"url": "https://app.test/a.js", "lineNumber": 11, "columnNumber": 4, "functionName": "f"}]}
            }),
        ));
        let e = &h.tabs["C1"].console[0];
        assert_eq!(e.level, Level::Warn);
        assert_eq!(e.source, "console.warning");
        assert_eq!((e.line, e.column), (Some(12), Some(5)));
        assert_eq!(e.page_url, "https://app.test/x");
        assert_eq!(
            format_console_line(e),
            "[warn] 2025-09-02T12:34:56.120Z https://app.test/a.js:12:5  Deprecated 3"
        );
    }

    #[test]
    fn bidi_console_null_text_renders_args() {
        let mut h = bidi_hub_with_tab();
        h.route_bidi(bev(
            "log.entryAdded",
            json!({
                "type": "console", "level": "info", "method": "log", "text": null, "timestamp": 1.0,
                "source": {"context": "C1"},
                "args": [
                    {"type": "string", "value": "Deprecated"},
                    {"type": "number", "value": 3},
                    {"type": "number", "value": "NaN"},
                    {"type": "boolean", "value": true},
                    {"type": "null"},
                    {"type": "undefined"},
                    {"type": "object", "value": [["a", {"type": "number", "value": 1}], ["b", {"type": "string", "value": "x"}]]},
                    {"type": "array", "value": [{"type": "number", "value": 1}]},
                    {"type": "node", "value": {"localName": "div"}},
                    {"type": "function"},
                    {"type": "map", "value": [[{"type": "string", "value": "k"}, {"type": "number", "value": 2}]]},
                    {"type": "object"}
                ]
            }),
        ));
        let e = &h.tabs["C1"].console[0];
        assert_eq!(e.level, Level::Log);
        assert_eq!(e.source, "console.log");
        assert_eq!(
            e.text,
            "Deprecated 3 NaN true null undefined {a: 1, b: \"x\"} [1] <div> function Map {\"k\" => 2} Object"
        );
        assert!(e.url.is_none());
    }

    #[test]
    fn bidi_console_group_methods_skipped() {
        let mut h = bidi_hub_with_tab();
        for m in ["group", "groupEnd", "clear"] {
            h.route_bidi(bev(
                "log.entryAdded",
                json!({"type": "console", "level": "info", "method": m, "text": "x", "timestamp": 1.0, "source": {"context": "C1"}}),
            ));
        }
        assert!(h.tabs["C1"].console.is_empty());
    }

    #[test]
    fn bidi_javascript_error_is_exception_with_stack() {
        let mut h = bidi_hub_with_tab();
        h.route_bidi(bev(
            "log.entryAdded",
            json!({
                "type": "javascript", "level": "error",
                "text": "TypeError: x is not a function", "timestamp": 2.0,
                "source": {"context": "C1"},
                "stackTrace": {"callFrames": [{"url": "https://app.test/a.js", "lineNumber": 11, "columnNumber": 4, "functionName": "f"}]}
            }),
        ));
        let e = &h.tabs["C1"].console[0];
        assert_eq!(e.level, Level::Error);
        assert_eq!(e.source, "exception");
        assert_eq!(e.text, "TypeError: x is not a function");
        assert!(e
            .stack
            .as_deref()
            .unwrap()
            .contains("at f (https://app.test/a.js:12:5)"));
        assert_eq!((e.line, e.column), (Some(12), Some(5)));
    }

    fn bidi_net_triplet(h: &mut HubInner, rid: &str, url: &str, status: u64) {
        h.route_bidi(bev(
            "network.beforeRequestSent",
            json!({"context": "C1", "navigation": null, "redirectCount": 0, "timestamp": 1756816496000.0,
                   "initiator": {"type": "other"},
                   "request": {"request": rid, "url": url, "method": "get", "bodySize": 0, "initiatorType": "xmlhttprequest"}}),
        ));
        h.route_bidi(bev(
            "network.responseCompleted",
            json!({"context": "C1", "timestamp": 1756816496084.0, "redirectCount": 0,
                   "request": {"request": rid, "url": url, "method": "get"},
                   "response": {"status": status, "statusText": "OK", "mimeType": "application/json", "fromCache": false, "bytesReceived": 312}}),
        ));
    }

    #[test]
    fn bidi_network_triplet_failed_and_redirect() {
        let mut h = bidi_hub_with_tab();
        bidi_net_triplet(&mut h, "1", "https://app.test/api/me", 401);
        h.route_bidi(bev(
            "network.beforeRequestSent",
            json!({"context": "C1", "navigation": null, "redirectCount": 0, "timestamp": 200000.0,
                   "initiator": {"type": "other"},
                   "request": {"request": "2", "url": "https://cdn.test/app.js", "method": "GET", "bodySize": 0, "destination": "script"}}),
        ));
        h.route_bidi(bev(
            "network.fetchError",
            json!({"context": "C1", "timestamp": 200500.0, "errorText": "NS_ERROR_ABORT",
                   "request": {"request": "2"}}),
        ));
        h.route_bidi(bev(
            "network.beforeRequestSent",
            json!({"context": "C1", "navigation": "N1", "redirectCount": 0, "timestamp": 300000.0,
                   "initiator": {"type": "other"},
                   "request": {"request": "3", "url": "https://app.test/old", "method": "GET", "bodySize": 12}}),
        ));
        h.route_bidi(bev(
            "network.responseCompleted",
            json!({"context": "C1", "timestamp": 300100.0, "redirectCount": 0,
                   "request": {"request": "3"},
                   "response": {"status": 302, "mimeType": "text/html", "bytesReceived": 0}}),
        ));
        h.route_bidi(bev(
            "network.beforeRequestSent",
            json!({"context": "C1", "navigation": "N1", "redirectCount": 1, "timestamp": 300100.0,
                   "initiator": {"type": "other"},
                   "request": {"request": "3", "url": "https://app.test/new", "method": "GET", "bodySize": 0}}),
        ));
        let tab = &h.tabs["C1"];
        assert_eq!(tab.network.len(), 4);
        assert_eq!(
            format_network_line(&tab.network[0]),
            "1  get    https://app.test/api/me  → 401 application/json 312B 84ms [XHR]"
        );
        assert_eq!(
            format_network_line(&tab.network[1]),
            "2  GET    https://cdn.test/app.js  → failed NS_ERROR_ABORT [Script]"
        );
        let c = &tab.network[2];
        assert_eq!(c.state, NetState::Redirected);
        assert_eq!(c.status, Some(302));
        assert!(c.has_post_data);
        assert!(
            format_network_line(c).ends_with("→ 302 text/html 0B 100ms [redirect] [Document]"),
            "{}",
            format_network_line(c)
        );
        assert_eq!(tab.network[3].state, NetState::Pending);
        assert_eq!(tab.network[3].resource_type.as_deref(), Some("Document"));
    }

    #[test]
    fn bidi_resource_type_tiers() {
        let t = |req: Value, nav: bool, init: Option<&str>, mime: Option<&str>| {
            bidi_resource_type(&req, nav, init, mime)
        };
        assert_eq!(t(json!({}), true, None, None).as_deref(), Some("Document"));
        assert_eq!(
            t(json!({}), false, Some("preflight"), None).as_deref(),
            Some("Preflight")
        );
        assert_eq!(
            t(json!({"initiatorType": "fetch"}), false, None, None).as_deref(),
            Some("Fetch")
        );
        assert_eq!(
            t(
                json!({"initiatorType": "link", "destination": "style"}),
                false,
                None,
                None
            )
            .as_deref(),
            Some("Stylesheet")
        );
        assert_eq!(
            t(json!({"destination": "image"}), false, None, None).as_deref(),
            Some("Image")
        );
        assert_eq!(
            t(json!({}), false, None, Some("text/css")).as_deref(),
            Some("Stylesheet")
        );
        assert_eq!(
            t(
                json!({}),
                false,
                None,
                Some("application/json; charset=utf-8")
            )
            .as_deref(),
            Some("Fetch")
        );
        assert_eq!(t(json!({}), false, Some("other"), Some("text/plain")), None);
    }

    #[test]
    fn bidi_navigation_started_updates_top_level_page_url_only() {
        let mut h = bidi_hub_with_tab();
        h.route_bidi(bev(
            "browsingContext.navigationStarted",
            json!({"context": "C-child", "navigation": "N2", "url": "https://iframe.test/"}),
        ));
        assert_eq!(h.tabs["C1"].page_url, "https://app.test/x");
        h.route_bidi(bev(
            "browsingContext.navigationStarted",
            json!({"context": "C1", "navigation": "N3", "url": "https://app.test/login"}),
        ));
        assert_eq!(h.tabs["C1"].page_url, "https://app.test/login");
        h.route_bidi(bev(
            "log.entryAdded",
            json!({"type": "console", "level": "info", "method": "log", "text": "after", "timestamp": 5.0, "source": {"context": "C1"}}),
        ));
        assert_eq!(h.tabs["C1"].console[0].page_url, "https://app.test/login");
    }

    #[test]
    fn bidi_untouched_context_ignored_and_context_destroyed_drops() {
        let mut h = bidi_hub_with_tab();
        h.route_bidi(bev(
            "log.entryAdded",
            json!({"type": "console", "level": "info", "method": "log", "text": "x", "timestamp": 1.0, "source": {"context": "C-other"}}),
        ));
        h.route_bidi(bev(
            "network.beforeRequestSent",
            json!({"context": null, "request": {"request": "9", "url": "u"}, "timestamp": 1.0}),
        ));
        assert!(h.tabs["C1"].console.is_empty());
        assert!(h.tabs["C1"].network.is_empty());
        h.route_bidi(bev(
            "browsingContext.contextDestroyed",
            json!({"context": "C1", "url": "https://app.test/x", "children": []}),
        ));
        assert!(h.tabs.is_empty());
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Reject {
        None,
        Network,
        Console,
    }

    /// BiDi-framed mock: records requests, answers `session.subscribe`
    /// (optionally rejecting one of the two calls), `getTree {root}`, and
    /// pushes console/network/foreign-context events right after the last
    /// accepted subscribe.
    async fn spawn_bidi_event_mock(reject: Reject) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        tokio::spawn({
            let seen = seen.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let req: Value = serde_json::from_str(&t).unwrap();
                    seen.lock().unwrap().push(req.clone());
                    let id = req["id"].as_u64().unwrap();
                    let method = req["method"].as_str().unwrap_or("").to_string();
                    let first_event = req["params"]["events"][0]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    let is_network_sub =
                        method == "session.subscribe" && first_event.starts_with("network.");
                    let is_console_sub =
                        method == "session.subscribe" && first_event.starts_with("log.");
                    let rejected = (is_network_sub && reject == Reject::Network)
                        || (is_console_sub && reject == Reject::Console);
                    let resp = if rejected {
                        json!({"type": "error", "id": id, "error": "invalid argument", "message": "unknown event"})
                    } else {
                        let result = match method.as_str() {
                            "session.subscribe" => json!({"subscription": "SUB1"}),
                            "browsingContext.getTree" => json!({"contexts": [
                                {"context": "C1", "url": "https://app.test/x", "children": []}
                            ]}),
                            _ => json!({}),
                        };
                        json!({"type": "success", "id": id, "result": result})
                    };
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                    let push_now = (is_network_sub && reject != Reject::Network)
                        || (is_console_sub && reject == Reject::Network);
                    if push_now {
                        for ev in [
                            json!({"type": "event", "method": "log.entryAdded", "params": {
                                "type": "console", "level": "error", "method": "error", "text": "boom",
                                "timestamp": 1.0, "source": {"context": "C1"}}}),
                            json!({"type": "event", "method": "log.entryAdded", "params": {
                                "type": "console", "level": "info", "method": "log", "text": "foreign",
                                "timestamp": 1.0, "source": {"context": "C-other"}}}),
                            json!({"type": "event", "method": "network.beforeRequestSent", "params": {
                                "context": "C1", "navigation": null, "redirectCount": 0, "timestamp": 1000.0,
                                "initiator": {"type": "other"},
                                "request": {"request": "7.1", "url": "https://app.test/api", "method": "GET", "bodySize": 0, "initiatorType": "fetch"}}}),
                            json!({"type": "event", "method": "network.responseCompleted", "params": {
                                "context": "C1", "timestamp": 1050.0, "redirectCount": 0,
                                "request": {"request": "7.1"},
                                "response": {"status": 200, "mimeType": "application/json", "bytesReceived": 11}}}),
                        ] {
                            ws.send(Message::Text(ev.to_string())).await.unwrap();
                        }
                    }
                }
            }
        });
        (format!("ws://{addr}"), seen)
    }

    async fn bidi_backend(url: &str) -> TabBackend {
        TabBackend::Bidi(Arc::new(BidiClient::connect(url).await.unwrap()))
    }

    /// Wait until the request has reached its **terminal** state, not merely
    /// until an entry exists.
    ///
    /// `network.beforeRequestSent` creates the entry in `Pending`;
    /// `network.responseCompleted` is a second event that moves it to
    /// `Finished`. Returning as soon as `matched > 0` therefore let the test
    /// proceed between the two, and the later `assert_eq!(state, Finished)`
    /// failed as `Pending` — intermittently, and only when the machine was
    /// loaded enough to interleave them. Poll for the state the assertions
    /// actually depend on.
    async fn poll_network(hub: &CaptureHub, target: &str) -> usize {
        for _ in 0..100 {
            if let Ok(r) = hub
                .read_network(
                    target,
                    &NetworkQuery {
                        limit: 10,
                        ..Default::default()
                    },
                )
                .await
            {
                if r.entries.iter().any(|e| e.state == NetState::Finished) {
                    return r.matched;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        0
    }

    #[tokio::test]
    async fn bidi_touch_subscribes_seeds_page_url_and_routes_events() {
        let (url, seen) = spawn_bidi_event_mock(Reject::None).await;
        let backend = bidi_backend(&url).await;
        let hub = CaptureHub::new();
        hub.touch_and_wait(&backend, "C1").await;
        assert_eq!(poll_network(&hub, "C1").await, 1);
        {
            let reqs = seen.lock().unwrap();
            let subs: Vec<&Value> = reqs
                .iter()
                .filter(|r| r["method"] == "session.subscribe")
                .collect();
            assert_eq!(subs.len(), 2);
            assert_eq!(
                subs[0]["params"]["events"],
                json!([
                    "log.entryAdded",
                    "browsingContext.navigationStarted",
                    "browsingContext.contextDestroyed"
                ])
            );
            assert_eq!(
                subs[1]["params"]["events"],
                json!([
                    "network.beforeRequestSent",
                    "network.responseCompleted",
                    "network.fetchError"
                ])
            );
            let tree = reqs
                .iter()
                .find(|r| r["method"] == "browsingContext.getTree")
                .expect("getTree");
            assert_eq!(tree["params"]["root"], "C1");
            assert_eq!(tree["params"]["maxDepth"], 0);
        }
        let c = hub
            .read_console(
                "C1",
                &ConsoleQuery {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.entries[0].text, "boom");
        assert_eq!(c.entries[0].page_url, "https://app.test/x");
        let n = hub
            .read_network(
                "C1",
                &NetworkQuery {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(n.entries[0].state, NetState::Finished);
        assert_eq!(n.entries[0].resource_type.as_deref(), Some("Fetch"));

        let err = hub
            .response_body(&backend, "C1", "7.1", 100, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("browser_fetch"));

        // A second tab reuses the subscription.
        hub.touch_and_wait(&backend, "C2").await;
        let before = seen.lock().unwrap().len();
        hub.forget(&backend, "C2");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let reqs = seen.lock().unwrap();
        assert_eq!(
            reqs.iter()
                .filter(|r| r["method"] == "session.subscribe")
                .count(),
            2
        );
        assert_eq!(reqs.len(), before, "forget must not send RPCs on BiDi");
    }

    #[tokio::test]
    async fn bidi_network_subscribe_rejected_degrades_to_console_only() {
        let (url, _seen) = spawn_bidi_event_mock(Reject::Network).await;
        let backend = bidi_backend(&url).await;
        let hub = CaptureHub::new();
        hub.touch_and_wait(&backend, "C1").await;
        let mut text = String::new();
        for _ in 0..100 {
            let c = hub
                .read_console(
                    "C1",
                    &ConsoleQuery {
                        limit: 10,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            if let Some(e) = c.entries.first() {
                text = e.text.clone();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(text, "boom");
        let err = hub
            .read_network("C1", &NetworkQuery::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Firefox 124"), "{err}");
    }

    #[tokio::test]
    async fn bidi_console_subscribe_failure_leaves_no_state_and_retries() {
        let (url, seen) = spawn_bidi_event_mock(Reject::Console).await;
        let backend = bidi_backend(&url).await;
        let hub = CaptureHub::new();
        hub.touch_and_wait(&backend, "C1").await;
        assert!(hub.captured_tabs().is_empty());
        hub.touch_and_wait(&backend, "C1").await;
        assert!(hub.captured_tabs().is_empty());
        assert_eq!(
            seen.lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"] == "session.subscribe")
                .count(),
            2
        );
    }
}
