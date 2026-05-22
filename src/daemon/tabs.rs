//! Daemon-owned tab registry — named handles to CDP page targets.
//!
//! Agents address tabs by **name** (`brave-twilight/scrape-cart`). The
//! daemon owns the lifecycle: it creates tabs via `Target.createTarget`,
//! tracks their `last_used_at`, GCs idle ones, and recycles the LRU
//! daemon-created tab when the budget cap is hit. Agents never close tabs
//! explicitly — `tab open` is the only verb.
//!
//! Two provenance classes:
//! - **Daemon-created** (`daemon_created = true`): the daemon called
//!   `Target.createTarget`. Eligible for idle GC and budget-pressure
//!   recycling. Scratch tabs live here.
//! - **User-created** (`daemon_created = false`): discovered in the user's
//!   browser via `Target.getTargets`. **Never** GC'd, even if idle for days.
//!   Adopted by an explicit `tab open --name <name> --adopt` (not yet wired
//!   in this chunk).
//!
//! Naming: agents may pass `--name X`; if absent we generate
//! `tab-<cute-word>` via the same word list as browser names. `tab open
//! --name X` is **get-or-create** — re-running the same line returns the
//! same tab while it's `Ready`, and recreates under the same name if it
//! went `Stuck` or `Closed`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use rand::Rng;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::cdp::CdpClient;
use crate::registry::words::WORDS;

/// Per-tab health summary, mirrored to the wire as `TabHealth.state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabHealth {
    /// Available for ops. Last interaction succeeded (or this is a brand-new
    /// tab and no op has run yet).
    Ready,
    /// Last op timed out or pre-flight probe failed. Eligible for preferential
    /// recycle under budget pressure; the agent can `tab open --name <same>`
    /// to recreate.
    Stuck,
    /// Target was closed — by GC, browser death, `Target.targetDestroyed`,
    /// or `--match-name`-style explicit close. Name does not resolve.
    Closed,
}

/// In-memory tab record. The daemon holds one per named tab.
#[derive(Debug, Clone)]
pub struct TabRow {
    pub name: String,
    pub target_id: String,
    pub url: String,
    pub last_used: Instant,
    pub state: TabHealth,
    /// `true` if this daemon called `Target.createTarget` for this tab —
    /// the only class eligible for GC.
    pub daemon_created: bool,
}

/// Tuning parameters for the registry.
#[derive(Debug, Clone)]
pub struct TabConfig {
    /// Idle-sweep threshold: daemon-created tabs unused for this long are
    /// closed by the background sweep task.
    pub idle_max: Duration,
    /// Soft cap on daemon-created tabs; over this, the next `open` kicks the
    /// sweep early but still creates.
    pub soft_cap: usize,
    /// Hard cap on daemon-created tabs; at this, `open` MUST recycle the LRU
    /// (preferring `Stuck` over `Ready`) before creating.
    pub hard_cap: usize,
    /// How often the idle-sweep task runs.
    pub sweep_interval: Duration,
}

impl Default for TabConfig {
    fn default() -> Self {
        Self {
            idle_max: Duration::from_secs(60 * 60),
            soft_cap: 20,
            hard_cap: 50,
            sweep_interval: Duration::from_secs(60),
        }
    }
}

/// Possible outcomes of an `open` call. The wire layer maps these onto
/// `Errors.Error` codes.
#[derive(Debug)]
pub enum OpenError {
    /// Hard cap reached and no daemon-created tab is eligible for recycling
    /// (everything is recently used and `Ready`). Caller should back off.
    BudgetExceeded { hard_cap: usize },
    /// Upstream CDP call failed (browser unreachable, target id rejected,
    /// etc.). Wrapped error has the protocol detail.
    Upstream(anyhow::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::BudgetExceeded { hard_cap } => {
                write!(f, "tab budget exceeded (hard cap {hard_cap})")
            }
            OpenError::Upstream(e) => write!(f, "upstream: {e}"),
        }
    }
}

impl std::error::Error for OpenError {}

/// The daemon-side tab registry.
#[derive(Clone)]
pub struct TabRegistry {
    inner: Arc<Mutex<Inner>>,
    upstream: Arc<CdpClient>,
    config: TabConfig,
}

struct Inner {
    tabs: HashMap<String, TabRow>,
}

impl TabRegistry {
    /// Build a fresh registry; does not seed user tabs. Seeding is a future
    /// chunk (`tab adopt`); for now agents create their own.
    pub fn new(upstream: Arc<CdpClient>, config: TabConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                tabs: HashMap::new(),
            })),
            upstream,
            config,
        }
    }

    /// Get-or-create the daemon-owned scratch tab. Lock-free ops (`fetch`,
    /// `eval` of arbitrary expressions, cookie reads) route through this so
    /// they never touch a user tab — the architectural answer to the iLO
    /// failure mode and the reason the default `eval '1+1'` is safe.
    ///
    /// The scratch tab uses a reserved name (`_scratch`) so agents can't
    /// collide with it by passing `--name _scratch`.
    pub async fn get_or_create_scratch(&self) -> Result<TabRow, OpenError> {
        self.open(Some("_scratch"), Some("about:blank")).await
    }

    /// Get-or-create. If `name` is already known and `Ready`, returns it
    /// (navigating to `url` first if provided and different from current).
    /// If `Stuck` or `Closed`, closes and recreates under the same name.
    /// If `name` is `None`, generates `tab-<cute-word>` and creates fresh.
    pub async fn open(&self, name: Option<&str>, url: Option<&str>) -> Result<TabRow, OpenError> {
        // Fast path: name exists and is Ready. Touch and maybe navigate.
        if let Some(n) = name {
            let (is_ready, current_url, target_id) = {
                let guard = self.inner.lock().await;
                match guard.tabs.get(n) {
                    Some(r) if r.state == TabHealth::Ready => {
                        (true, r.url.clone(), Some(r.target_id.clone()))
                    }
                    Some(_) => (false, String::new(), None), // Stuck/Closed → recreate
                    None => (false, String::new(), None),    // unknown → create fresh
                }
            };
            if is_ready {
                let target_id = target_id.expect("target_id present for Ready row");
                if let Some(u) = url {
                    if u != current_url {
                        self.navigate(&target_id, u)
                            .await
                            .map_err(OpenError::Upstream)?;
                        let mut guard = self.inner.lock().await;
                        if let Some(r) = guard.tabs.get_mut(n) {
                            r.url = u.to_string();
                            r.last_used = Instant::now();
                            return Ok(r.clone());
                        }
                    }
                }
                let mut guard = self.inner.lock().await;
                if let Some(r) = guard.tabs.get_mut(n) {
                    r.last_used = Instant::now();
                    return Ok(r.clone());
                }
            }
        }

        // Budget pressure: if at hard cap, recycle LRU (preferring Stuck)
        // before opening. Only daemon-created tabs count toward the cap.
        let daemon_created_count = {
            let guard = self.inner.lock().await;
            guard.tabs.values().filter(|r| r.daemon_created).count()
        };
        if daemon_created_count >= self.config.hard_cap {
            self.recycle_lru().await?;
        }

        // Create the new tab in the browser.
        let target_url = url.unwrap_or("about:blank");
        let target_id = create_target(&self.upstream, target_url)
            .await
            .map_err(OpenError::Upstream)?;

        // Resolve a name and register.
        let assigned = match name {
            Some(n) => n.to_string(),
            None => self.fresh_name().await,
        };
        let row = TabRow {
            name: assigned.clone(),
            target_id,
            url: target_url.to_string(),
            last_used: Instant::now(),
            state: TabHealth::Ready,
            daemon_created: true,
        };
        let mut guard = self.inner.lock().await;
        guard.tabs.insert(assigned, row.clone());
        Ok(row)
    }

    /// Snapshot of all tabs (for `tab list`). Returns in stable name order.
    pub async fn list(&self) -> Vec<TabRow> {
        let guard = self.inner.lock().await;
        let mut rows: Vec<TabRow> = guard.tabs.values().cloned().collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Bump `last_used` for an existing tab. No-op if name unknown.
    pub async fn touch(&self, name: &str) {
        let mut guard = self.inner.lock().await;
        if let Some(row) = guard.tabs.get_mut(name) {
            row.last_used = Instant::now();
        }
    }

    /// Mark a tab as Stuck (so it's preferentially recycled and subsequent
    /// ops fast-fail). Used by the per-op timeout path in higher layers.
    pub async fn mark_stuck(&self, name: &str) {
        let mut guard = self.inner.lock().await;
        if let Some(row) = guard.tabs.get_mut(name) {
            row.state = TabHealth::Stuck;
        }
    }

    /// Run the idle sweep once. Returns the names of tabs that were closed.
    pub async fn sweep_idle_once(&self) -> Vec<String> {
        let now = Instant::now();
        let cutoff = self.config.idle_max;
        let victims: Vec<(String, String)> = {
            let guard = self.inner.lock().await;
            guard
                .tabs
                .values()
                .filter(|r| r.daemon_created && now.duration_since(r.last_used) > cutoff)
                .map(|r| (r.name.clone(), r.target_id.clone()))
                .collect()
        };
        let mut closed = Vec::new();
        for (name, target_id) in victims {
            if close_target(&self.upstream, &target_id).await.is_ok() {
                let mut guard = self.inner.lock().await;
                guard.tabs.remove(&name);
                closed.push(name);
            }
        }
        closed
    }

    /// Spawn the background idle-sweep task. Returns a handle; dropping it
    /// does NOT stop the task (it runs for the daemon's lifetime).
    pub fn start_idle_sweep(&self) -> tokio::task::JoinHandle<()> {
        let reg = self.clone();
        let interval = self.config.sweep_interval;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await; // first tick fires immediately; skip it
            loop {
                tick.tick().await;
                reg.sweep_idle_once().await;
            }
        })
    }

    /// Update `last_used` for diagnostics ("idle" column in `tab list`).
    pub fn config(&self) -> &TabConfig {
        &self.config
    }

    // ---- internals ----

    async fn recycle_lru(&self) -> Result<(), OpenError> {
        // Pick the LRU daemon-created tab, preferring Stuck over Ready.
        let victim: Option<(String, String)> = {
            let guard = self.inner.lock().await;
            let mut candidates: Vec<&TabRow> = guard
                .tabs
                .values()
                .filter(|r| r.daemon_created && r.state != TabHealth::Closed)
                .collect();
            // Sort: Stuck before Ready, then oldest last_used first.
            candidates.sort_by(|a, b| {
                let stuck_a = (a.state == TabHealth::Stuck) as u8;
                let stuck_b = (b.state == TabHealth::Stuck) as u8;
                // Higher stuck_* first → reverse cmp.
                stuck_b
                    .cmp(&stuck_a)
                    .then_with(|| a.last_used.cmp(&b.last_used))
            });
            candidates
                .first()
                .map(|r| (r.name.clone(), r.target_id.clone()))
        };
        let (name, target_id) = victim.ok_or(OpenError::BudgetExceeded {
            hard_cap: self.config.hard_cap,
        })?;
        close_target(&self.upstream, &target_id)
            .await
            .map_err(OpenError::Upstream)?;
        let mut guard = self.inner.lock().await;
        guard.tabs.remove(&name);
        Ok(())
    }

    async fn fresh_name(&self) -> String {
        // tab-<word>, fall back to numeric suffix on collision.
        let mut rng = rand::thread_rng();
        let guard = self.inner.lock().await;
        for _ in 0..20 {
            let word = WORDS[rng.gen_range(0..WORDS.len())];
            let base = format!("tab-{word}");
            if !guard.tabs.contains_key(&base) {
                return base;
            }
            for n in 2..=1000 {
                let cand = format!("tab-{word}-{n}");
                if !guard.tabs.contains_key(&cand) {
                    return cand;
                }
            }
        }
        // Extreme fallback: pseudo-uuid suffix.
        format!("tab-fallback-{}", rng.gen::<u32>())
    }

    async fn navigate(&self, target_id: &str, url: &str) -> Result<()> {
        // Use Target.attachToTarget to get a session, then Page.navigate.
        // Cheaper alternative: `Target.activateTarget` won't navigate; we
        // need a real Page.navigate.
        let attach: Value = self
            .upstream
            .send(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session_id = attach
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("attachToTarget returned no sessionId"))?
            .to_string();
        self.upstream
            .send_with_session("Page.navigate", json!({ "url": url }), Some(&session_id))
            .await?;
        Ok(())
    }
}

async fn create_target(client: &CdpClient, url: &str) -> Result<String> {
    let v = client
        .send("Target.createTarget", json!({ "url": url }))
        .await?;
    v.get("targetId")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Target.createTarget returned no targetId"))
}

async fn close_target(client: &CdpClient, target_id: &str) -> Result<()> {
    let _ = client
        .send("Target.closeTarget", json!({ "targetId": target_id }))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value as JsonValue;
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;

    /// Mock CDP server that responds to:
    /// - `Target.createTarget` with an incrementing `T1`, `T2`, … id
    /// - `Target.closeTarget` with `{}`
    /// - `Target.attachToTarget` with `{sessionId: "S<n>"}`
    /// - `Page.navigate` with `{}`
    /// - `Browser.getVersion` with a fake product string (so connect_http
    ///   would also work, though tests construct CdpClient directly)
    async fn spawn_mock() -> (String, oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut next_target = 0u32;
            let mut next_session = 0u32;
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    msg = ws.next() => {
                        let msg = match msg {
                            Some(Ok(m)) => m,
                            _ => break,
                        };
                        if let Message::Text(t) = msg {
                            let req: JsonValue = serde_json::from_str(&t).unwrap();
                            let id = req["id"].as_u64().unwrap();
                            let method = req["method"].as_str().unwrap_or("");
                            let result = match method {
                                "Target.createTarget" => {
                                    next_target += 1;
                                    json!({"targetId": format!("T{next_target}")})
                                }
                                "Target.closeTarget" => json!({"success": true}),
                                "Target.attachToTarget" => {
                                    next_session += 1;
                                    json!({"sessionId": format!("S{next_session}")})
                                }
                                "Page.navigate" => json!({}),
                                "Browser.getVersion" => json!({"product": "Chrome/138.0.0.0"}),
                                "Target.getTargets" => json!({"targetInfos": []}),
                                "Target.setDiscoverTargets" | "Target.setAutoAttach" => json!({}),
                                _ => json!({}),
                            };
                            let resp = json!({"id": id, "result": result});
                            ws.send(Message::Text(resp.to_string())).await.unwrap();
                        }
                    }
                }
            }
        });
        (format!("ws://{addr}"), stop_tx)
    }

    fn small_config() -> TabConfig {
        TabConfig {
            idle_max: Duration::from_millis(80),
            soft_cap: 2,
            hard_cap: 2,
            sweep_interval: Duration::from_millis(20),
        }
    }

    /// Test #3: `tab open` (no `--name`) assigns a cute name.
    #[tokio::test]
    async fn open_without_name_assigns_cute_name() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = TabRegistry::new(client, TabConfig::default());
        let row = reg.open(None, None).await.unwrap();
        assert!(row.name.starts_with("tab-"), "name was {}", row.name);
        assert_eq!(row.state, TabHealth::Ready);
        assert!(row.daemon_created);
        assert_eq!(row.target_id, "T1");
    }

    /// Test #4: `tab open --name X` is idempotent (get-or-create).
    #[tokio::test]
    async fn open_with_name_is_idempotent() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = TabRegistry::new(client, TabConfig::default());
        let a = reg.open(Some("scrape-cart"), None).await.unwrap();
        let b = reg.open(Some("scrape-cart"), None).await.unwrap();
        assert_eq!(a.target_id, b.target_id);
        // last_used bumped on second call.
        assert!(b.last_used >= a.last_used);
    }

    /// Test #5: `tab open --name X` recreates when the existing tab is Stuck.
    #[tokio::test]
    async fn open_with_name_recreates_when_stuck() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = TabRegistry::new(client, TabConfig::default());
        let first = reg.open(Some("flaky"), None).await.unwrap();
        reg.mark_stuck("flaky").await;
        let second = reg.open(Some("flaky"), None).await.unwrap();
        assert_ne!(first.target_id, second.target_id);
        assert_eq!(second.name, "flaky");
        assert_eq!(second.state, TabHealth::Ready);
    }

    /// Test #8: idle sweep closes daemon-created tabs after IDLE_MAX.
    #[tokio::test]
    async fn idle_sweep_closes_daemon_tabs() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = TabRegistry::new(client, small_config());
        let _ = reg.open(Some("a"), None).await.unwrap();
        let _ = reg.open(Some("b"), None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let closed = reg.sweep_idle_once().await;
        assert_eq!(closed.len(), 2);
        let remaining = reg.list().await;
        assert!(remaining.is_empty());
    }

    /// Test #9: idle sweep does NOT touch user-created tabs.
    #[tokio::test]
    async fn idle_sweep_skips_user_created_tabs() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = TabRegistry::new(client, small_config());
        // Manually inject a user-created row.
        {
            let mut guard = reg.inner.lock().await;
            guard.tabs.insert(
                "iLO".to_string(),
                TabRow {
                    name: "iLO".to_string(),
                    target_id: "U1".to_string(),
                    url: "https://192.168.2.28/".to_string(),
                    last_used: Instant::now() - Duration::from_secs(3600),
                    state: TabHealth::Ready,
                    daemon_created: false,
                },
            );
        }
        let closed = reg.sweep_idle_once().await;
        assert!(closed.is_empty(), "swept user tab: {closed:?}");
        let remaining = reg.list().await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "iLO");
    }

    /// Test #10: hard-cap budget pressure recycles the LRU daemon tab.
    #[tokio::test]
    async fn budget_pressure_recycles_lru() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        // Hard cap 2; fill it, then a 3rd open must close the LRU.
        let cfg = TabConfig {
            hard_cap: 2,
            soft_cap: 2,
            idle_max: Duration::from_secs(3600),
            sweep_interval: Duration::from_secs(3600),
        };
        let reg = TabRegistry::new(client, cfg);
        let _a = reg.open(Some("a"), None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let _b = reg.open(Some("b"), None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        // Bump b's last_used so a is LRU.
        reg.touch("b").await;
        // 3rd open under hard cap → must recycle a.
        let c = reg.open(Some("c"), None).await.unwrap();
        let remaining = reg.list().await;
        let names: Vec<String> = remaining.iter().map(|r| r.name.clone()).collect();
        assert!(
            !names.contains(&"a".to_string()),
            "a should be recycled: {names:?}"
        );
        assert!(names.contains(&"b".to_string()));
        assert_eq!(c.name, "c");
    }

    /// Test #11: stuck tabs preferentially recycled over ready under cap.
    #[tokio::test]
    async fn stuck_preferentially_recycled() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let cfg = TabConfig {
            hard_cap: 2,
            soft_cap: 2,
            idle_max: Duration::from_secs(3600),
            sweep_interval: Duration::from_secs(3600),
        };
        let reg = TabRegistry::new(client, cfg);
        // First tab is the OLDER (would normally be LRU), but we'll mark it
        // Ready and make the newer one Stuck.
        let _a = reg.open(Some("old-ready"), None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let _b = reg.open(Some("new-stuck"), None).await.unwrap();
        reg.mark_stuck("new-stuck").await;
        // 3rd open: should recycle "new-stuck" even though "old-ready" is
        // older — Stuck wins over LRU ordering.
        let _c = reg.open(Some("c"), None).await.unwrap();
        let names: Vec<String> = reg.list().await.iter().map(|r| r.name.clone()).collect();
        assert!(names.contains(&"old-ready".to_string()));
        assert!(!names.contains(&"new-stuck".to_string()));
    }

    /// Test #18: `tab list` reflects state, ownership, and idle accurately.
    /// We make several tabs, manipulate state, and assert the snapshot.
    #[tokio::test]
    async fn list_reflects_state_ownership_and_idle() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = TabRegistry::new(client, TabConfig::default());

        // Three tabs: one ready (daemon-created), one stuck (daemon-created),
        // one user-created.
        let _a = reg.open(Some("ready"), None).await.unwrap();
        let _b = reg.open(Some("flaky"), None).await.unwrap();
        reg.mark_stuck("flaky").await;
        {
            let mut g = reg.inner.lock().await;
            g.tabs.insert(
                "iLO".to_string(),
                TabRow {
                    name: "iLO".to_string(),
                    target_id: "U99".to_string(),
                    url: "https://192.168.2.28/".to_string(),
                    last_used: Instant::now() - Duration::from_secs(120),
                    state: TabHealth::Ready,
                    daemon_created: false,
                },
            );
        }

        let rows = reg.list().await;
        assert_eq!(rows.len(), 3);
        // Sorted by name.
        assert_eq!(rows[0].name, "flaky");
        assert_eq!(rows[0].state, TabHealth::Stuck);
        assert!(rows[0].daemon_created);
        assert_eq!(rows[1].name, "iLO");
        assert!(!rows[1].daemon_created);
        // iLO was set 120s ago — last_used.elapsed() > 100s; check the row
        // by querying its actual elapsed.
        let user_tab_idle = rows[1].last_used.elapsed();
        assert!(
            user_tab_idle >= Duration::from_secs(100),
            "iLO idle too low: {user_tab_idle:?}"
        );
        assert_eq!(rows[2].name, "ready");
        assert_eq!(rows[2].state, TabHealth::Ready);
        assert!(rows[2].daemon_created);
    }

    /// Test #17: open with mismatched URL navigates the existing tab.
    #[tokio::test]
    async fn open_with_mismatched_url_navigates() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = TabRegistry::new(client, TabConfig::default());
        let a = reg
            .open(Some("nav"), Some("https://a.test/"))
            .await
            .unwrap();
        let b = reg
            .open(Some("nav"), Some("https://b.test/"))
            .await
            .unwrap();
        assert_eq!(a.target_id, b.target_id);
        assert_eq!(b.url, "https://b.test/");
    }
}
