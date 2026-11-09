//! Download management for the MCP server (ADR-005).
//!
//! Goal: a download triggered by an agent never opens a native Save dialog
//! and always ends up at a path the agent can be told about.
//!
//! - **Managed browsers** (launched by browser-control into an owned
//!   profile): downloads land in the per-kind staging directory
//!   (`paths::downloads_staging_dir`). Launch-time prefs cover every path
//!   without a live connection; on top of that, the server re-sends the
//!   runtime override before tool calls (debounced) because Chromium resets
//!   it to the profile default whenever *another* connection that set it
//!   detaches (the Playwright sidecar, another MCP server) and nothing can
//!   observe that without polling.
//!   - Chromium: `Browser.setDownloadBehavior {allowAndName}` so files are
//!     written as `<staging>/<guid>`, then renamed here to a sanitized,
//!     never-overwriting name when `downloadProgress` reports completion.
//!     Every server sharing the browser sends identical values, so the
//!     browser-global setting has no last-writer problem; the rename is
//!     claimed with an `O_EXCL` marker so exactly one server performs it.
//!   - Firefox 149+: BiDi `browser.setDownloadBehavior {allowed}`; Firefox
//!     names files uniquely itself and reports the path in `downloadEnd`.
//! - **Unmanaged browsers** (a user's own browser, external endpoints):
//!   nothing is changed. Setting a download override in Chromium swaps in a
//!   devtools download delegate for the profile until restart, which
//!   degrades Save Page As and Safe Browsing for the human user.
//!
//! Push-only, like the capture hub: one router task blocks on the event
//! channel; nothing polls or wakes on a timer.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::json;
use tokio::sync::{broadcast, Notify};

use crate::bidi::BidiEvent;
use crate::cdp::CdpEvent;
use crate::session::backend::TabBackend;

/// Downloads remembered per server.
pub const RECORD_CAP: usize = 200;
/// Minimum spacing between runtime-override re-sends.
const REAPPLY_DEBOUNCE: Duration = Duration::from_millis(250);
/// Bound on one override RPC.
const APPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// Staging bookkeeping files older than this are removed on the first
/// prepare of a backend (on invocation, never on a timer).
const SWEEP_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Default wait for a download to begin.
pub const DEFAULT_BEGIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Default wait for a begun download to complete.
pub const DEFAULT_COMPLETE_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for either wait, below common MCP client call timeouts.
pub const MAX_WAIT: Duration = Duration::from_secs(120);

/// Env var naming the default destination for `browser_downloads`
/// `wait` / `save` (equivalent to `browser-control mcp --download-dir`).
pub const DOWNLOAD_DIR_ENV: &str = "BROWSER_CONTROL_DOWNLOAD_DIR";

/// Whether this server manages downloads for its current browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Managed { staging: PathBuf },
    Unmanaged { reason: &'static str },
}

pub const UNMANAGED_ATTACHED: &str = "this browser was not launched by browser-control (or is an external endpoint), so its download settings are left alone and downloads go wherever the browser's own settings put them (possibly a Save dialog). Start a managed browser with browser_start to get silent downloads into a staging folder";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DlState {
    InProgress,
    Completed,
    Canceled,
}

impl DlState {
    fn label(self) -> &'static str {
        match self {
            DlState::InProgress => "in_progress",
            DlState::Completed => "completed",
            DlState::Canceled => "canceled",
        }
    }
}

/// One download this server observed.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadRecord {
    /// Server-local ordering; compared against action marks.
    #[serde(skip)]
    pub seq: u64,
    /// Agent-facing id: the CDP guid, or the BiDi download/navigation id.
    pub id: String,
    pub url: String,
    pub suggested_filename: String,
    pub state: DlState,
    pub received_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    /// Final absolute path once known.
    pub path: Option<PathBuf>,
    /// Why no path is available (finalize failure, canceled, …).
    pub note: Option<String>,
    /// Started from a tab this server works in (see `Inner::owns`).
    /// Only owned downloads are reported in action results and picked by
    /// an id-less `wait`, so agents sharing a browser do not take each
    /// other's files.
    pub owned: bool,
    /// The event channel overflowed while this download ran; events for
    /// it may be missing. Cleared by any later event for it.
    #[serde(skip)]
    pub events_lost: bool,
    /// A CDP rename is running for it.
    #[serde(skip)]
    pub finalizing: bool,
    /// CDP frame id or BiDi context that started it.
    #[serde(skip)]
    pub frame: Option<String>,
    #[serde(skip)]
    pub began: Option<SystemTime>,
}

impl DownloadRecord {
    fn new(id: String, url: &str, suggested: &str, frame: Option<String>, owned: bool) -> Self {
        Self {
            seq: 0,
            id,
            url: url.to_string(),
            suggested_filename: suggested.to_string(),
            state: DlState::InProgress,
            received_bytes: None,
            total_bytes: None,
            path: None,
            note: None,
            owned,
            events_lost: false,
            finalizing: false,
            frame,
            began: Some(SystemTime::now()),
        }
    }
}

/// Bound on the target→opener map.
const OPENER_CAP: usize = 2000;

#[derive(Default)]
struct Inner {
    records: VecDeque<DownloadRecord>,
    next_seq: u64,
    /// `next_seq` value when the most recent action tool call began.
    last_action_mark: u64,
    /// Highest seq a `wait` without `id` has returned as settled; the next
    /// such `wait` takes the oldest owned download after it.
    last_returned_seq: u64,
    /// Action tool calls currently running on this server.
    in_flight: u32,
    /// Tabs (CDP target ids / BiDi contexts) this server's tools used.
    touched: HashSet<String>,
    /// Child → opener/parent (CDP `openerId`, BiDi `originalOpener` or
    /// `parent`), so a download from a popup or iframe is attributed to
    /// the tab that opened it.
    openers: HashMap<String, String>,
    /// Top-level targets the browser reported (CDP discovery / BiDi).
    known: HashSet<String>,
    /// Identity of the backend the router/mode belong to.
    backend_key: Option<usize>,
    mode: Option<Mode>,
    router: Option<tokio::task::JoinHandle<()>>,
    last_applied: Option<Instant>,
    /// BiDi: download events subscribed (once per backend).
    bidi_subscribed: bool,
    /// BiDi: `browser.setDownloadBehavior` unsupported (Firefox < 149).
    bidi_behavior_unsupported: bool,
    /// BiDi: download events unavailable (Firefox < 144/145).
    bidi_events_unavailable: bool,
    /// CDP: `Target.setDiscoverTargets` sent on this backend.
    cdp_discovering: bool,
    swept: bool,
}

impl Inner {
    fn find_mut(&mut self, id: &str) -> Option<&mut DownloadRecord> {
        self.records.iter_mut().rev().find(|r| r.id == id)
    }

    fn push(&mut self, mut r: DownloadRecord) {
        self.next_seq += 1;
        r.seq = self.next_seq;
        if self.records.len() >= RECORD_CAP {
            self.records.pop_front();
        }
        self.records.push_back(r);
    }

    fn staging(&self) -> Option<PathBuf> {
        match &self.mode {
            Some(Mode::Managed { staging }) => Some(staging.clone()),
            _ => None,
        }
    }

    /// Whether a download started by `frame` belongs to this server: the
    /// frame, or a tab up its opener/parent chain, is one this server's
    /// tools used. A frame the browser never announced (a CDP iframe) is
    /// attributed to whichever server has an action running at that
    /// moment; a known tab this server never used is someone else's.
    fn owns(&self, frame: Option<&str>) -> bool {
        let Some(mut cur) = frame else {
            return self.in_flight > 0;
        };
        for _ in 0..16 {
            if self.touched.contains(cur) {
                return true;
            }
            match self.openers.get(cur) {
                Some(o) => cur = o,
                None => break,
            }
        }
        if self.known.contains(cur) {
            false
        } else {
            self.in_flight > 0
        }
    }

    fn note_opener(&mut self, child: &str, opener: &str) {
        if self.openers.len() >= OPENER_CAP {
            self.openers.clear();
        }
        self.openers.insert(child.to_string(), opener.to_string());
    }
}

/// Finalize job produced by routing a CDP `completed` progress event.
struct FinalizeJob {
    id: String,
    staging: PathBuf,
    suggested: String,
    received: Option<u64>,
    began: Option<SystemTime>,
}

pub struct DownloadHub {
    inner: Arc<Mutex<Inner>>,
    notify: Arc<Notify>,
}

impl Default for DownloadHub {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DownloadHub {
    fn drop(&mut self) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(h) = g.router.take() {
                h.abort();
            }
        }
    }
}

fn backend_key(b: &TabBackend) -> usize {
    match b {
        TabBackend::Cdp(c) => Arc::as_ptr(c) as *const () as usize,
        TabBackend::Bidi(c) => Arc::as_ptr(c) as *const () as usize,
    }
}

impl DownloadHub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            notify: Arc::new(Notify::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Mode for `backend`, if already resolved for it.
    pub fn mode_for(&self, backend: &TabBackend) -> Option<Mode> {
        let g = self.lock();
        (g.backend_key == Some(backend_key(backend)))
            .then(|| g.mode.clone())
            .flatten()
    }

    /// Current mode regardless of backend (tool output).
    pub fn mode(&self) -> Option<Mode> {
        self.lock().mode.clone()
    }

    /// Install the mode for a (new) backend. Drops router state that
    /// belonged to a previous backend.
    pub fn set_mode(&self, backend: &TabBackend, mode: Mode) {
        let mut g = self.lock();
        let key = backend_key(backend);
        if g.backend_key != Some(key) {
            if let Some(h) = g.router.take() {
                h.abort();
            }
            g.backend_key = Some(key);
            g.last_applied = None;
            g.bidi_subscribed = false;
            g.bidi_behavior_unsupported = false;
            g.bidi_events_unavailable = false;
            g.cdp_discovering = false;
            g.known.clear();
            g.swept = false;
        }
        g.mode = Some(mode);
    }

    /// Forget everything (browser switch).
    pub fn reset(&self) {
        let mut g = self.lock();
        if let Some(h) = g.router.take() {
            h.abort();
        }
        *g = Inner {
            next_seq: g.next_seq,
            last_action_mark: g.next_seq,
            last_returned_seq: g.next_seq,
            in_flight: g.in_flight,
            ..Default::default()
        };
    }

    /// Record the start of an action tool call; returns the mark used to
    /// report downloads that began during it.
    ///
    /// Also clears the re-apply debounce: an action is exactly when a
    /// download can start, so the override is always re-sent for it.
    pub fn begin_action(&self) -> u64 {
        let mut g = self.lock();
        g.last_action_mark = g.next_seq;
        g.last_applied = None;
        g.in_flight += 1;
        g.next_seq
    }

    /// Pair of [`Self::begin_action`].
    pub fn end_action(&self) {
        let mut g = self.lock();
        g.in_flight = g.in_flight.saturating_sub(1);
    }

    /// Record that this server's tools work in `target_id`.
    pub fn touch(&self, target_id: &str) {
        let mut g = self.lock();
        if !g.touched.contains(target_id) {
            g.touched.insert(target_id.to_string());
        }
    }

    /// Force the next `apply` to re-send the override (another connection
    /// that set it, such as the sidecar, just went away).
    pub fn invalidate(&self) {
        self.lock().last_applied = None;
    }

    /// Downloads from this server's tabs that began after `mark`.
    pub fn since(&self, mark: u64) -> Vec<DownloadRecord> {
        self.lock()
            .records
            .iter()
            .filter(|r| r.seq > mark && r.owned)
            .cloned()
            .collect()
    }

    pub fn list(&self) -> Vec<DownloadRecord> {
        let mut out: Vec<_> = self.lock().records.iter().cloned().collect();
        for r in &mut out {
            resolve_lazily(r, &self.inner);
        }
        out
    }

    /// Make sure the runtime override and event routing are in place for
    /// `backend`. Debounced unless `force`. Never errors: failures are
    /// logged and the launch-time prefs remain the safety net.
    pub async fn apply(&self, backend: &TabBackend, force: bool) {
        let staging = {
            let mut g = self.lock();
            if g.backend_key != Some(backend_key(backend)) {
                return;
            }
            let Some(staging) = g.staging() else {
                return;
            };
            if !force
                && g.last_applied
                    .is_some_and(|t| t.elapsed() < REAPPLY_DEBOUNCE)
            {
                return;
            }
            g.last_applied = Some(Instant::now());
            if g.router.is_none() {
                let weak = Arc::downgrade(&self.inner);
                let notify = self.notify.clone();
                g.router = Some(match backend {
                    TabBackend::Cdp(c) => tokio::spawn(run_cdp_router(c.subscribe(), weak, notify)),
                    TabBackend::Bidi(c) => {
                        tokio::spawn(run_bidi_router(c.subscribe(), weak, notify))
                    }
                });
            }
            if !g.swept {
                g.swept = true;
                let dir = staging.clone();
                tokio::task::spawn_blocking(move || sweep_staging(&dir));
            }
            staging
        };
        let _ = std::fs::create_dir_all(&staging);
        let dir = staging.to_string_lossy().to_string();
        match backend {
            TabBackend::Cdp(c) => {
                if !self.lock().cdp_discovering {
                    // Push-only target events: openers for popup
                    // attribution and the set of known tabs.
                    let fut = c.send("Target.setDiscoverTargets", json!({ "discover": true }));
                    if matches!(tokio::time::timeout(APPLY_TIMEOUT, fut).await, Ok(Ok(_))) {
                        self.lock().cdp_discovering = true;
                    }
                }
                let fut = c.send(
                    "Browser.setDownloadBehavior",
                    json!({
                        "behavior": "allowAndName",
                        "downloadPath": dir,
                        "eventsEnabled": true,
                    }),
                );
                match tokio::time::timeout(APPLY_TIMEOUT, fut).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::debug!(error = %e, "setDownloadBehavior failed"),
                    Err(_) => tracing::debug!("setDownloadBehavior timed out"),
                }
            }
            TabBackend::Bidi(c) => {
                let (subscribe, behavior) = {
                    let g = self.lock();
                    (!g.bidi_subscribed, !g.bidi_behavior_unsupported)
                };
                if subscribe {
                    let fut = c.send(
                        "session.subscribe",
                        json!({ "events": [
                            "browsingContext.downloadWillBegin",
                            "browsingContext.downloadEnd",
                            "browsingContext.contextCreated",
                        ] }),
                    );
                    match tokio::time::timeout(APPLY_TIMEOUT, fut).await {
                        Ok(Ok(_)) => self.lock().bidi_subscribed = true,
                        // A protocol error means this Firefox lacks the
                        // events; a timeout is retried on the next call.
                        Ok(Err(e)) => {
                            tracing::debug!(error = %e, "BiDi download events unavailable");
                            let mut g = self.lock();
                            g.bidi_subscribed = true;
                            g.bidi_events_unavailable = true;
                        }
                        Err(_) => tracing::debug!("BiDi download subscribe timed out"),
                    }
                }
                if behavior {
                    let fut = c.send(
                        "browser.setDownloadBehavior",
                        json!({ "downloadBehavior": {
                            "type": "allowed",
                            "destinationFolder": dir,
                        } }),
                    );
                    match tokio::time::timeout(APPLY_TIMEOUT, fut).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => {
                            let msg = format!("{e:#}");
                            tracing::debug!(error = %msg, "BiDi setDownloadBehavior failed");
                            if msg.contains("unknown command") || msg.contains("unknown method") {
                                self.lock().bidi_behavior_unsupported = true;
                            }
                        }
                        Err(_) => tracing::debug!("BiDi setDownloadBehavior timed out"),
                    }
                }
            }
        }
    }

    /// Whether download events can be observed on the current backend.
    pub fn events_available(&self) -> bool {
        let g = self.lock();
        !(g.bidi_subscribed && g.bidi_events_unavailable)
    }

    /// Wait for a download. `id`: that download; otherwise the newest one
    /// no earlier id-less `wait` has returned (waiting up to
    /// `begin_timeout` for one to begin). Then waits up to
    /// `complete_timeout` for it to finish. Returns `None` when nothing
    /// began; an unfinished download is returned in its current state.
    pub async fn wait(
        &self,
        id: Option<&str>,
        begin_timeout: Duration,
        complete_timeout: Duration,
    ) -> Result<Option<DownloadRecord>> {
        let begin_deadline = tokio::time::Instant::now() + begin_timeout;
        let target_id = loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let g = self.lock();
                // Without an id: the oldest download from this server's tabs
                // that no earlier `wait` returned. Not tied to the last
                // action, because a download often begins after the click
                // returns and the agent may act again before waiting; oldest
                // first so a burst ("download all") is handed out in order.
                let found = match id {
                    Some(id) => g.records.iter().rev().find(|r| r.id == id),
                    None => g
                        .records
                        .iter()
                        .find(|r| r.owned && r.seq > g.last_returned_seq),
                };
                if let Some(r) = found {
                    break r.id.clone();
                }
                if id.is_some() {
                    bail!(
                        "no download with id `{}` is known to this MCP server; call browser_downloads with action `list`",
                        id.unwrap_or_default()
                    );
                }
            }
            if tokio::time::timeout_at(begin_deadline, notified)
                .await
                .is_err()
            {
                return Ok(None);
            }
        };
        let done_deadline = tokio::time::Instant::now() + complete_timeout;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let snapshot = {
                let g = self.lock();
                g.records.iter().rev().find(|r| r.id == target_id).cloned()
            };
            let Some(mut r) = snapshot else {
                bail!(
                    "download `{target_id}` is no longer known to this MCP server (the browser was switched, or more than {RECORD_CAP} downloads followed); check the staging folder"
                );
            };
            let settled = match r.state {
                // Events were lost: more may never arrive, so report now
                // rather than block. Cleared if a later event shows up.
                DlState::InProgress => r.events_lost,
                DlState::Canceled => true,
                DlState::Completed => {
                    r.path.is_some()
                        || (!r.finalizing
                            && (r.note.is_some() || {
                                resolve_lazily(&mut r, &self.inner);
                                r.path.is_some()
                            }))
                }
            };
            if settled {
                if id.is_none() {
                    let mut g = self.lock();
                    g.last_returned_seq = g.last_returned_seq.max(r.seq);
                }
                return Ok(Some(r));
            }
            if tokio::time::timeout_at(done_deadline, notified)
                .await
                .is_err()
            {
                resolve_lazily(&mut r, &self.inner);
                return Ok(Some(r));
            }
        }
    }

    /// Move a completed download into `dest_dir` (absolute; created if
    /// missing) without overwriting. Updates the record.
    pub async fn save_to(&self, id: &str, dest_dir: &Path) -> Result<DownloadRecord> {
        if !dest_dir.is_absolute() {
            bail!(
                "`save_to` must be an absolute directory path (got {})",
                dest_dir.display()
            );
        }
        let mut r = self
            .lock()
            .records
            .iter()
            .rev()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("no download with id `{id}` is known to this MCP server"))?;
        resolve_lazily(&mut r, &self.inner);
        if r.state != DlState::Completed {
            bail!(
                "download `{id}` is {}; wait for it to complete first",
                r.state.label()
            );
        }
        let src = r
            .path
            .clone()
            .ok_or_else(|| anyhow!("download `{id}` has no file path yet"))?;
        if src.parent() == Some(dest_dir) {
            return Ok(r);
        }
        let dest_dir = dest_dir.to_path_buf();
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".into());
        let staging = self.lock().staging();
        let rid = id.to_string();
        let moved = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
            let moved = move_unique(&src, &dest_dir, &name)?;
            // Keep other servers' lazy resolution pointing at the file.
            if let Some(staging) = staging {
                if is_safe_id(&rid) && done_marker(&staging, &rid).exists() {
                    let _ = std::fs::write(
                        done_marker(&staging, &rid),
                        moved.to_string_lossy().as_bytes(),
                    );
                }
            }
            Ok(moved)
        })
        .await??;
        let mut g = self.lock();
        if let Some(rec) = g.find_mut(id) {
            rec.path = Some(moved.clone());
        }
        r.path = Some(moved);
        Ok(r)
    }
}

/// Fill `path` for a completed CDP download finalized by another server
/// (or by a finalize that raced this read) from the staging marker.
fn resolve_lazily(r: &mut DownloadRecord, inner: &Arc<Mutex<Inner>>) {
    // A recorded path that no longer exists was moved by another server
    // (`save_to`), which rewrote the marker.
    if r.state != DlState::Completed || r.path.as_ref().is_some_and(|p| p.exists()) {
        return;
    }
    let staging = inner.lock().unwrap_or_else(|p| p.into_inner()).staging();
    let Some(staging) = staging else {
        return;
    };
    if let Some(p) = read_done_marker(&staging, &r.id).filter(|p| Some(p) != r.path.as_ref()) {
        r.path = Some(p.clone());
        let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(rec) = g.find_mut(&r.id) {
            rec.path = Some(p);
        }
    }
}

// ---------------------------------------------------------------------------
// Routing.
// ---------------------------------------------------------------------------

impl Inner {
    /// Route one CDP event. Pure and synchronous (unit-testable); a
    /// completion that needs a rename is returned as a job.
    fn route_cdp(&mut self, ev: &CdpEvent) -> Option<FinalizeJob> {
        if ev.session_id.is_some() {
            return None;
        }
        let p = &ev.params;
        match ev.method.as_str() {
            "Target.targetCreated" | "Target.targetInfoChanged" => {
                let info = &p["targetInfo"];
                let tid = info["targetId"].as_str()?;
                if info["type"].as_str() == Some("page") {
                    self.known.insert(tid.to_string());
                }
                if let Some(op) = info["openerId"].as_str() {
                    self.note_opener(tid, op);
                }
                None
            }
            "Target.targetDestroyed" => {
                // Openers stay: a popup that became a download closes
                // around the time its download begins.
                self.known.remove(p["targetId"].as_str()?);
                None
            }
            "Browser.downloadWillBegin" => {
                let id = p["guid"].as_str()?.to_string();
                if self.find_mut(&id).is_some() {
                    return None;
                }
                let frame = p["frameId"].as_str().map(String::from);
                let owned = self.owns(frame.as_deref());
                self.push(DownloadRecord::new(
                    id,
                    p["url"].as_str().unwrap_or_default(),
                    p["suggestedFilename"].as_str().unwrap_or_default(),
                    frame,
                    owned,
                ));
                None
            }
            "Browser.downloadProgress" => {
                let id = p["guid"].as_str()?;
                let staging = self.staging();
                if self.find_mut(id).is_none() {
                    // Began before this server connected (or its begin
                    // event was lost). Still rename it when it completes so
                    // no GUID-named file is left behind.
                    if p["state"].as_str() != Some("completed") {
                        return None;
                    }
                    let short: String = id.chars().take(8).collect();
                    let mut r = DownloadRecord::new(
                        id.to_string(),
                        "",
                        &format!("download-{short}"),
                        None,
                        false,
                    );
                    r.began = None;
                    self.push(r);
                }
                let r = self.find_mut(id)?;
                r.events_lost = false;
                r.received_bytes = p["receivedBytes"].as_f64().map(|v| v as u64);
                r.total_bytes = p["totalBytes"]
                    .as_f64()
                    .filter(|v| *v > 0.0)
                    .map(|v| v as u64);
                match p["state"].as_str() {
                    Some("completed") if r.state != DlState::Completed => {
                        r.state = DlState::Completed;
                        let staging = staging?;
                        r.finalizing = true;
                        Some(FinalizeJob {
                            id: r.id.clone(),
                            staging,
                            suggested: r.suggested_filename.clone(),
                            received: r.received_bytes,
                            began: r.began,
                        })
                    }
                    Some("canceled") => {
                        r.state = DlState::Canceled;
                        r.note = Some("canceled or interrupted by the browser".into());
                        None
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Route one BiDi event. Firefox names and places the file itself.
    fn route_bidi(&mut self, ev: &BidiEvent) {
        let p = &ev.params;
        // `download` (Firefox 154+) or `navigation` correlate begin and end.
        // Without either, synthesize a unique id at begin and match the end
        // to the newest in-progress record for the same URL.
        let explicit = p["download"]
            .as_str()
            .or_else(|| p["navigation"].as_str())
            .map(String::from);
        let url = p["url"].as_str().unwrap_or_default();
        match ev.method.as_str() {
            "browsingContext.contextCreated" => {
                let Some(ctx) = p["context"].as_str() else {
                    return;
                };
                let link = p["parent"]
                    .as_str()
                    .or_else(|| p["originalOpener"].as_str());
                match link {
                    Some(l) => self.note_opener(ctx, l),
                    None => {
                        self.known.insert(ctx.to_string());
                    }
                }
            }
            "browsingContext.downloadWillBegin" => {
                let id = match explicit {
                    Some(id) if self.find_mut(&id).is_some() => return,
                    Some(id) => id,
                    None => format!("bidi-{}", self.next_seq + 1),
                };
                let frame = p["context"].as_str().map(String::from);
                let owned = self.owns(frame.as_deref());
                self.push(DownloadRecord::new(
                    id,
                    url,
                    p["suggestedFilename"].as_str().unwrap_or_default(),
                    frame,
                    owned,
                ));
            }
            "browsingContext.downloadEnd" => {
                let r = match explicit {
                    Some(id) => self.find_mut(&id),
                    None => self
                        .records
                        .iter_mut()
                        .rev()
                        .find(|r| r.state == DlState::InProgress && r.url == url),
                };
                let Some(r) = r else {
                    return;
                };
                r.events_lost = false;
                if p["status"].as_str() == Some("complete") {
                    r.state = DlState::Completed;
                    r.path = p["filepath"].as_str().map(PathBuf::from);
                    if r.path.is_none() {
                        r.note = Some("the browser did not report a file path".into());
                    }
                } else {
                    r.state = DlState::Canceled;
                    r.note = Some("canceled by the browser".into());
                }
            }
            _ => {}
        }
    }
}

async fn run_cdp_router(
    mut rx: broadcast::Receiver<CdpEvent>,
    weak: Weak<Mutex<Inner>>,
    notify: Arc<Notify>,
) {
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if !(ev.method.starts_with("Browser.download")
                    || (ev.session_id.is_none()
                        && matches!(
                            ev.method.as_str(),
                            "Target.targetCreated"
                                | "Target.targetInfoChanged"
                                | "Target.targetDestroyed"
                        )))
                {
                    continue;
                }
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let job = inner
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .route_cdp(&ev);
                notify.notify_waiters();
                if let Some(job) = job {
                    let weak = weak.clone();
                    let notify = notify.clone();
                    tokio::spawn(async move {
                        let id = job.id.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            match finalize(
                                &job.staging,
                                &job.id,
                                &job.suggested,
                                job.received,
                                job.began,
                            )? {
                                Some(p) => Ok(Some(p)),
                                // Another server holds the claim: give it a
                                // bounded moment to record the final path.
                                None => {
                                    for _ in 0..40 {
                                        if let Some(p) = read_done_marker(&job.staging, &job.id) {
                                            return Ok(Some(p));
                                        }
                                        std::thread::sleep(Duration::from_millis(50));
                                    }
                                    Ok(None)
                                }
                            }
                        })
                        .await
                        .map_err(|e| anyhow!("finalize task failed: {e}"))
                        .and_then(|r| r);
                        if let Some(inner) = weak.upgrade() {
                            let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
                            if let Some(r) = g.find_mut(&id) {
                                r.finalizing = false;
                                match res {
                                    Ok(Some(path)) => r.path = Some(path),
                                    // Another server claimed it; path is
                                    // resolved lazily from its marker.
                                    Ok(None) => {
                                        r.note = Some(
                                            "renamed by another MCP server sharing this browser, which did not report the path in time; call browser_downloads list".into(),
                                        )
                                    }
                                    Err(e) => r.note = Some(format!("{e:#}")),
                                }
                            }
                        }
                        notify.notify_waiters();
                    });
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!(lost = n, "download router lagged");
                mark_events_lost(&weak, &notify);
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// The event channel overflowed: a `completed` for any in-progress
/// download may be gone. Say so instead of letting `wait` hang.
fn mark_events_lost(weak: &Weak<Mutex<Inner>>, notify: &Notify) {
    if let Some(inner) = weak.upgrade() {
        let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
        for r in g
            .records
            .iter_mut()
            .filter(|r| r.state == DlState::InProgress)
        {
            r.events_lost = true;
        }
    }
    notify.notify_waiters();
}

async fn run_bidi_router(
    mut rx: broadcast::Receiver<BidiEvent>,
    weak: Weak<Mutex<Inner>>,
    notify: Arc<Notify>,
) {
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if !(ev.method.starts_with("browsingContext.download")
                    || ev.method == "browsingContext.contextCreated")
                {
                    continue;
                }
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                inner
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .route_bidi(&ev);
                notify.notify_waiters();
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!(lost = n, "download router lagged");
                mark_events_lost(&weak, &notify);
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// Files.
// ---------------------------------------------------------------------------

fn claim_marker(staging: &Path, id: &str) -> PathBuf {
    staging.join(format!(".{id}.claim"))
}

fn done_marker(staging: &Path, id: &str) -> PathBuf {
    staging.join(format!(".{id}.done"))
}

fn read_done_marker(staging: &Path, id: &str) -> Option<PathBuf> {
    let s = std::fs::read_to_string(done_marker(staging, id)).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// Rename `<staging>/<id>` to a sanitized, unique name in `staging`.
/// Returns `Ok(None)` when another server already claimed it.
fn finalize(
    staging: &Path,
    id: &str,
    suggested: &str,
    received: Option<u64>,
    began: Option<SystemTime>,
) -> Result<Option<PathBuf>> {
    if !is_safe_id(id) {
        bail!("refusing unexpected download id `{id}`");
    }
    let claim = claim_marker(staging, id);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&claim)
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("claiming {}", claim.display())),
    }
    let name = sanitize_filename(suggested);
    let src = staging.join(id);
    if !src.exists() {
        // The override was reset (another connection detached) before this
        // download began, so the browser saved it under its own name.
        if let Some(found) = find_browser_named(staging, &name, received, began) {
            std::fs::write(done_marker(staging, id), found.to_string_lossy().as_bytes())
                .context("writing download marker")?;
            return Ok(Some(found));
        }
        bail!(
            "the browser reported the download complete but {} is missing and no matching {name} was found; check {}",
            src.display(),
            staging.display()
        );
    }
    let path = move_unique(&src, staging, &name)?;
    std::fs::write(done_marker(staging, id), path.to_string_lossy().as_bytes())
        .with_context(|| "writing download marker".to_string())?;
    Ok(Some(path))
}

/// The file the browser itself named `name` or `name (N)` for this
/// download: written after it began and, when known, of the reported size.
/// The newest match wins; `None` rather than a guess.
fn find_browser_named(
    staging: &Path,
    name: &str,
    received: Option<u64>,
    began: Option<SystemTime>,
) -> Option<PathBuf> {
    let (stem, ext) = split_ext(name);
    let since = began?.checked_sub(Duration::from_secs(2))?;
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(staging).ok()?.flatten() {
        let fname = e.file_name().to_string_lossy().to_string();
        let matches_name = fname == name
            || fname
                .strip_prefix(&format!("{stem} ("))
                .and_then(|rest| rest.strip_suffix(ext))
                .and_then(|rest| rest.strip_suffix(')'))
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if !matches_name {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime < since || received.is_some_and(|n| n != meta.len()) {
            continue;
        }
        if best.as_ref().map_or(true, |(t, _)| mtime > *t) {
            best = Some((mtime, e.path()));
        }
    }
    best.map(|(_, p)| p)
}

/// Chromium guids are hex + dashes; never let an id escape the directory.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Move `src` into `dir` as `name`, or `name (1)`, `name (2)`, … — never
/// overwriting. The destination is reserved with `create_new` first so
/// concurrent movers cannot pick the same name; cross-device moves fall
/// back to copy + remove.
pub fn move_unique(src: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let (stem, ext) = split_ext(name);
    for n in 0..1000u32 {
        let candidate = if n == 0 {
            dir.join(name)
        } else {
            dir.join(format!("{stem} ({n}){ext}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", candidate.display())),
        }
        if std::fs::rename(src, &candidate).is_err() {
            // Cross-device (EXDEV) or Windows sharing quirks: copy over the
            // reserved file, then remove the source.
            if let Err(e) = std::fs::copy(src, &candidate) {
                let _ = std::fs::remove_file(&candidate);
                return Err(e).with_context(|| {
                    format!("moving {} to {}", src.display(), candidate.display())
                });
            }
            let _ = std::fs::remove_file(src);
        }
        return Ok(candidate);
    }
    bail!("no free file name for {name} in {}", dir.display())
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// Make a page-supplied filename safe to create: no path components, no
/// control characters, no leading dots, no Windows reserved names, bounded
/// length. `allowAndName` bypasses Chromium's own name generation, so this
/// is the only gate between `Content-Disposition` and the filesystem.
pub fn sanitize_filename(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut s: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    s = s
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .to_string();
    // Windows reserves the device name before the *first* dot.
    let stem = s.split('.').next().unwrap_or_default().trim_end();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM0", "LPT0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6",
        "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
        "LPT9",
    ];
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        s = format!("_{s}");
    }
    if s.is_empty() {
        s = "download".into();
    }
    if s.len() > 200 {
        let (stem, ext) = split_ext(&s);
        let ext = if ext.len() <= 16 { ext } else { "" };
        let mut cut = 200 - ext.len();
        while !stem.is_char_boundary(cut.min(stem.len())) {
            cut -= 1;
        }
        s = format!("{}{ext}", &stem[..cut.min(stem.len())]);
    }
    s
}

/// Remove stale rename bookkeeping (`.<id>.claim` / `.<id>.done`) from
/// staging. Downloads themselves are never deleted, whatever their name:
/// a GUID-named file is usually a finished download nobody renamed.
fn sweep_staging(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let marker = name.starts_with('.') && (name.ends_with(".claim") || name.ends_with(".done"));
        if !marker {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > SWEEP_AGE);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

// ---------------------------------------------------------------------------
// Formatting.
// ---------------------------------------------------------------------------

fn fmt_bytes(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

pub fn format_record(r: &DownloadRecord) -> String {
    let mut s = format!("{} {}", r.id, r.state.label());
    match (r.received_bytes, r.total_bytes, r.state) {
        (Some(rx), Some(t), DlState::InProgress) => {
            s.push_str(&format!(" {}/{}", fmt_bytes(rx), fmt_bytes(t)))
        }
        (Some(rx), _, _) if rx > 0 => s.push_str(&format!(" {}", fmt_bytes(rx))),
        _ => {}
    }
    if !r.suggested_filename.is_empty() {
        s.push_str(&format!(" {:?}", r.suggested_filename));
    }
    if let Some(p) = &r.path {
        s.push_str(&format!(" -> {}", p.display()));
    }
    if let Some(n) = &r.note {
        s.push_str(&format!(" ({n})"));
    }
    if r.events_lost && r.state == DlState::InProgress {
        s.push_str(&format!(
            " (browser events were lost while it ran; check the staging folder, where a Chromium file may still be named {})",
            r.id
        ));
    }
    if !r.owned {
        s.push_str(" [another tab]");
    }
    if !r.url.is_empty() && !r.url.starts_with("data:") {
        let url: String = r.url.chars().take(200).collect();
        s.push_str(&format!(" from {url}"));
    }
    s
}

/// Text appended to an action tool's result when downloads began during
/// the call.
pub fn action_note(records: &[DownloadRecord]) -> Option<String> {
    if records.is_empty() {
        return None;
    }
    let mut s = String::from("download started:\n");
    for r in records {
        s.push_str("  ");
        s.push_str(&format_record(r));
        s.push('\n');
    }
    s.push_str("call browser_downloads with action \"wait\" for the saved path (add save_to to move it into a specific folder)");
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn cev(method: &str, params: Value) -> CdpEvent {
        CdpEvent {
            method: method.into(),
            params,
            session_id: None,
        }
    }

    fn managed(dir: &Path) -> Inner {
        Inner {
            mode: Some(Mode::Managed {
                staging: dir.to_path_buf(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn sanitize_strips_paths_controls_and_reserved_names() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\..\\Cargo.toml"), "Cargo.toml");
        assert_eq!(sanitize_filename(".bashrc"), "bashrc");
        assert_eq!(sanitize_filename("a\u{0}b\nc.txt"), "a_b_c.txt");
        assert_eq!(sanitize_filename("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_filename("con.tar.gz"), "_con.tar.gz");
        assert_eq!(sanitize_filename("CONSOLE.txt"), "CONSOLE.txt");
        assert_eq!(sanitize_filename(""), "download");
        assert_eq!(sanitize_filename("..."), "download");
        assert_eq!(sanitize_filename("re:port?.csv"), "re_port_.csv");
        let long = format!("{}.pdf", "é".repeat(300));
        let s = sanitize_filename(&long);
        assert!(s.len() <= 200 && s.ends_with(".pdf"), "{s}");
    }

    #[test]
    fn move_unique_never_overwrites() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("report.csv"), "existing").unwrap();
        let a = dir.join("a");
        std::fs::write(&a, "new").unwrap();
        let p = move_unique(&a, dir, "report.csv").unwrap();
        assert_eq!(p, dir.join("report (1).csv"));
        assert_eq!(
            std::fs::read_to_string(dir.join("report.csv")).unwrap(),
            "existing"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        assert!(!a.exists());
    }

    #[test]
    fn finalize_is_claimed_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let id = "0b9a3c1e-1111-2222-3333-444455556666";
        std::fs::write(dir.join(id), "x").unwrap();
        let first = finalize(dir, id, "../evil.txt", None, None).unwrap();
        assert_eq!(first, Some(dir.join("evil.txt")));
        assert_eq!(finalize(dir, id, "evil.txt", None, None).unwrap(), None);
        assert_eq!(read_done_marker(dir, id), Some(dir.join("evil.txt")));
        assert!(finalize(dir, "../x", "y", None, None).is_err());
    }

    #[test]
    fn cdp_events_track_lifecycle_and_emit_finalize_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut h = managed(tmp.path());
        // Session-scoped events (Page.* on a tab session) are ignored.
        let mut scoped = cev("Browser.downloadWillBegin", json!({"guid": "g0"}));
        scoped.session_id = Some("S".into());
        assert!(h.route_cdp(&scoped).is_none());
        assert!(h.records.is_empty());

        h.route_cdp(&cev(
            "Browser.downloadWillBegin",
            json!({"guid": "g1", "url": "https://x.test/r.csv", "suggestedFilename": "r.csv", "frameId": "F"}),
        ));
        assert_eq!(h.records.len(), 1);
        assert!(h
            .route_cdp(&cev(
                "Browser.downloadProgress",
                json!({"guid": "g1", "receivedBytes": 5, "totalBytes": 10, "state": "inProgress"}),
            ))
            .is_none());
        let job = h
            .route_cdp(&cev(
                "Browser.downloadProgress",
                json!({"guid": "g1", "receivedBytes": 10, "totalBytes": 10, "state": "completed"}),
            ))
            .expect("finalize job");
        assert_eq!(job.suggested, "r.csv");
        // A repeated completed event does not finalize twice.
        assert!(h
            .route_cdp(&cev(
                "Browser.downloadProgress",
                json!({"guid": "g1", "state": "completed"}),
            ))
            .is_none());
        h.route_cdp(&cev(
            "Browser.downloadWillBegin",
            json!({"guid": "g2", "suggestedFilename": "b.bin"}),
        ));
        h.route_cdp(&cev(
            "Browser.downloadProgress",
            json!({"guid": "g2", "state": "canceled"}),
        ));
        assert_eq!(h.records[1].state, DlState::Canceled);
    }

    #[test]
    fn bidi_events_use_reported_filepath() {
        let mut h = Inner::default();
        h.route_bidi(&BidiEvent {
            method: "browsingContext.downloadWillBegin".into(),
            params: json!({"context": "C", "navigation": "N1", "url": "https://x/a.pdf", "suggestedFilename": "a.pdf"}),
        });
        h.route_bidi(&BidiEvent {
            method: "browsingContext.downloadEnd".into(),
            params: json!({"context": "C", "navigation": "N1", "status": "complete", "filepath": "/s/a.pdf"}),
        });
        assert_eq!(h.records[0].state, DlState::Completed);
        assert_eq!(h.records[0].path.as_deref(), Some(Path::new("/s/a.pdf")));
    }

    fn rec(id: &str, state: DlState) -> DownloadRecord {
        let mut r = DownloadRecord::new(id.into(), "https://x/f", "f", None, true);
        r.state = state;
        r.path = Some(PathBuf::from(format!("/s/{id}")));
        r
    }

    #[tokio::test]
    async fn wait_finds_download_despite_intervening_action_and_moves_on_after_return() {
        let hub = DownloadHub::new();
        hub.begin_action(); // click
        hub.lock().push(rec("g1", DlState::Completed)); // began after click returned
        hub.begin_action(); // agent runs another action before waiting
        let short = Duration::from_millis(20);
        let r = hub.wait(None, short, short).await.unwrap().unwrap();
        assert_eq!(r.id, "g1");
        // Already returned: the next wait does not hand it out again.
        assert!(hub.wait(None, short, short).await.unwrap().is_none());
        // But an explicit id still works.
        assert_eq!(
            hub.wait(Some("g1"), short, short)
                .await
                .unwrap()
                .unwrap()
                .id,
            "g1"
        );
    }

    #[tokio::test]
    async fn lost_events_settle_in_progress_wait() {
        let hub = DownloadHub::new();
        let mut r = rec("g1", DlState::InProgress);
        r.path = None;
        hub.lock().push(r);
        mark_events_lost(&Arc::downgrade(&hub.inner), &hub.notify);
        let got = hub
            .wait(None, Duration::from_millis(20), Duration::from_secs(5))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.state, DlState::InProgress);
        assert!(got.events_lost);
        assert!(format_record(&got).contains("lost"));
    }

    #[test]
    fn lost_flag_clears_on_later_event_and_completion_still_finalizes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut h = managed(tmp.path());
        h.route_cdp(&cev(
            "Browser.downloadWillBegin",
            json!({"guid": "g1", "suggestedFilename": "r.csv"}),
        ));
        h.records[0].events_lost = true;
        let job = h.route_cdp(&cev(
            "Browser.downloadProgress",
            json!({"guid": "g1", "receivedBytes": 3, "state": "completed"}),
        ));
        assert!(job.is_some());
        assert!(!h.records[0].events_lost);
        // Completed but still renaming: not settled until the job reports.
        assert!(h.records[0].finalizing);
    }

    #[tokio::test]
    async fn completed_but_finalizing_is_not_settled() {
        let hub = DownloadHub::new();
        let mut r = rec("g1", DlState::Completed);
        r.path = None;
        r.finalizing = true;
        hub.lock().push(r);
        let got = hub
            .wait(
                Some("g1"),
                Duration::from_millis(10),
                Duration::from_millis(50),
            )
            .await
            .unwrap()
            .unwrap();
        // Returned on timeout, not as settled: the id-less cursor did not move.
        assert!(got.path.is_none());
        assert_eq!(hub.lock().last_returned_seq, 0);
    }

    #[tokio::test]
    async fn idless_wait_hands_out_oldest_first_and_skips_other_tabs() {
        let hub = DownloadHub::new();
        {
            let mut g = hub.lock();
            g.push(rec("a", DlState::Completed));
            let mut other = rec("x", DlState::Completed);
            other.owned = false;
            g.push(other);
            g.push(rec("b", DlState::Completed));
        }
        let short = Duration::from_millis(10);
        assert_eq!(hub.wait(None, short, short).await.unwrap().unwrap().id, "a");
        assert_eq!(hub.wait(None, short, short).await.unwrap().unwrap().id, "b");
        assert!(hub.wait(None, short, short).await.unwrap().is_none());
        // Another tab's download is still reachable by id.
        assert_eq!(
            hub.wait(Some("x"), short, short).await.unwrap().unwrap().id,
            "x"
        );
    }

    #[test]
    fn ownership_follows_touched_tabs_and_openers() {
        let mut h = Inner::default();
        h.touched.insert("T1".into());
        h.route_cdp(&cev(
            "Target.targetCreated",
            json!({"targetInfo": {"targetId": "T1", "type": "page"}}),
        ));
        h.route_cdp(&cev(
            "Target.targetCreated",
            json!({"targetInfo": {"targetId": "T2", "type": "page"}}),
        ));
        h.route_cdp(&cev(
            "Target.targetCreated",
            json!({"targetInfo": {"targetId": "P", "type": "page", "openerId": "T1"}}),
        ));
        assert!(h.owns(Some("T1")));
        assert!(h.owns(Some("P")), "popup opened by our tab");
        assert!(!h.owns(Some("T2")), "another agent's tab");
        // Unknown frame (an iframe): owned only while an action runs here.
        assert!(!h.owns(Some("iframe-1")));
        h.in_flight = 1;
        assert!(h.owns(Some("iframe-1")));
        assert!(!h.owns(Some("T2")));
    }

    #[test]
    fn completion_of_unseen_download_is_still_renamed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut h = managed(tmp.path());
        let job = h
            .route_cdp(&cev(
                "Browser.downloadProgress",
                json!({"guid": "0b9a3c1e-1111-2222-3333-444455556666", "state": "completed"}),
            ))
            .expect("job for unseen guid");
        assert_eq!(job.suggested, "download-0b9a3c1e");
        assert!(!h.records[0].owned);
        // Progress for an unseen guid that is not complete is ignored.
        assert!(h
            .route_cdp(&cev(
                "Browser.downloadProgress",
                json!({"guid": "other", "state": "inProgress"}),
            ))
            .is_none());
    }

    #[test]
    fn bidi_without_navigation_tracks_repeat_downloads_separately() {
        let mut h = Inner::default();
        for _ in 0..2 {
            h.route_bidi(&BidiEvent {
                method: "browsingContext.downloadWillBegin".into(),
                params: json!({"context": "C", "navigation": null, "url": "https://x/a", "suggestedFilename": "a"}),
            });
        }
        assert_eq!(h.records.len(), 2);
        h.route_bidi(&BidiEvent {
            method: "browsingContext.downloadEnd".into(),
            params: json!({"context": "C", "navigation": null, "url": "https://x/a", "status": "complete", "filepath": "/s/a (1)"}),
        });
        assert_eq!(h.records[1].state, DlState::Completed);
        assert_eq!(h.records[0].state, DlState::InProgress);
    }

    #[test]
    fn sweep_removes_only_old_markers_never_downloads() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let old = SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
        for name in [
            "0b9a3c1e-1111-2222-3333-444455556666",
            "report.csv",
            ".g1.claim",
            ".g1.done",
        ] {
            let f = std::fs::File::create(dir.join(name)).unwrap();
            f.set_modified(old).unwrap();
        }
        std::fs::write(dir.join(".fresh.done"), "x").unwrap();
        sweep_staging(dir);
        assert!(dir.join("0b9a3c1e-1111-2222-3333-444455556666").exists());
        assert!(dir.join("report.csv").exists());
        assert!(!dir.join(".g1.claim").exists());
        assert!(!dir.join(".g1.done").exists());
        assert!(dir.join(".fresh.done").exists());
    }

    #[test]
    fn finalize_fallback_picks_the_matching_browser_named_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let old = SystemTime::now() - Duration::from_secs(60);
        // An older report.csv that predates this download.
        let f = std::fs::File::create(dir.join("r.csv")).unwrap();
        f.set_modified(old).unwrap();
        // The browser saved this download as "r (1).csv" (5 bytes).
        let began = SystemTime::now() - Duration::from_secs(1);
        std::fs::write(dir.join("r (1).csv"), "hello").unwrap();
        let p = finalize(dir, "abcd-1", "r.csv", Some(5), Some(began))
            .unwrap()
            .unwrap();
        assert_eq!(p, dir.join("r (1).csv"));
        // Size mismatch or nothing written since it began: no guess.
        assert!(finalize(dir, "abcd-2", "r.csv", Some(999), Some(began)).is_err());
        assert!(finalize(dir, "abcd-3", "missing.csv", None, Some(began)).is_err());
    }

    #[tokio::test]
    async fn save_to_by_another_server_is_picked_up_from_the_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("r.csv"), "x").unwrap();
        let mk = || {
            let hub = DownloadHub::new();
            {
                let mut g = hub.lock();
                g.mode = Some(Mode::Managed {
                    staging: staging.clone(),
                });
                let mut r = rec("abcd-1", DlState::Completed);
                r.path = Some(staging.join("r.csv"));
                g.push(r);
            }
            hub
        };
        let (a, b) = (mk(), mk());
        std::fs::write(
            done_marker(&staging, "abcd-1"),
            staging.join("r.csv").to_string_lossy().as_bytes(),
        )
        .unwrap();
        let moved = a.save_to("abcd-1", &dest).await.unwrap();
        assert_eq!(moved.path, Some(dest.join("r.csv")));
        // Server B still has the old staging path; list re-reads the marker.
        assert_eq!(b.list()[0].path, Some(dest.join("r.csv")));
    }

    #[tokio::test]
    async fn wait_returns_none_when_nothing_begins_and_finds_new_download() {
        let hub = DownloadHub::new();
        let r = hub
            .wait(None, Duration::from_millis(20), Duration::from_millis(20))
            .await
            .unwrap();
        assert!(r.is_none());

        let mark = hub.begin_action();
        {
            let mut g = hub.lock();
            g.push(DownloadRecord::new("g1".into(), "", "a", None, true));
        }
        assert_eq!(hub.since(mark).len(), 1);
        let inner = hub.inner.clone();
        let notify = hub.notify.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut g = inner.lock().unwrap();
            let r = g.find_mut("g1").unwrap();
            r.state = DlState::Completed;
            r.path = Some(PathBuf::from("/s/a"));
            drop(g);
            notify.notify_waiters();
        });
        let r = hub
            .wait(None, Duration::from_millis(500), Duration::from_secs(2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.state, DlState::Completed);
        assert_eq!(r.path.as_deref(), Some(Path::new("/s/a")));
    }
}
