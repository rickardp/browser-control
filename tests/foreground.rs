//! End-to-end test of the foreground holder against a mock CDP browser:
//! `browser-control tab foreground-hold` is spawned as a real detached
//! process through `spawn_holder`, must register itself, send the emulation
//! commands, exit cleanly on `stop_holder`, and exit on its own when the
//! target is destroyed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use browser_control::detect::{Engine, Kind};
use browser_control::registry::{BrowserRow, Registry};
use browser_control::session::foreground;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

/// Mock browser endpoint: HTTP `/json/version` for discovery is not needed
/// because the registry row carries a `ws://` endpoint. Records every
/// request method and can push `Target.targetDestroyed`.
async fn spawn_mock() -> (u16, Arc<Mutex<Vec<Value>>>, watch::Sender<bool>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let (destroy_tx, destroy_rx) = watch::channel(false);
    tokio::spawn({
        let seen = seen.clone();
        async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen.clone();
                let mut destroy_rx = destroy_rx.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    loop {
                        tokio::select! {
                            changed = destroy_rx.changed() => {
                                if changed.is_ok() && *destroy_rx.borrow() {
                                    let ev = json!({"method": "Target.targetDestroyed", "params": {"targetId": "T1"}});
                                    let _ = ws.send(Message::Text(ev.to_string())).await;
                                }
                            }
                            msg = ws.next() => {
                                let Some(Ok(Message::Text(t))) = msg else { return; };
                                let req: Value = serde_json::from_str(&t).unwrap();
                                seen.lock().unwrap().push(req.clone());
                                let id = req["id"].as_u64().unwrap();
                                let result = match req["method"].as_str().unwrap_or("") {
                                    "Target.attachToTarget" => json!({"sessionId": "S1"}),
                                    _ => json!({}),
                                };
                                let _ = ws.send(Message::Text(json!({"id": id, "result": result}).to_string())).await;
                            }
                        }
                    }
                });
            }
        }
    });
    (port, seen, destroy_tx)
}

fn methods(seen: &Arc<Mutex<Vec<Value>>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|r| r["method"].as_str().map(String::from))
        .collect()
}

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn holder_attaches_emulates_and_stops() {
    let (port, seen, destroy) = spawn_mock().await;
    let data = tempfile::TempDir::new().unwrap();
    // The holder child reads the same registry through this env var.
    std::env::set_var("BROWSER_CONTROL_DATA_DIR", data.path());
    std::env::set_var(
        "BROWSER_CONTROL_BIN",
        assert_cmd::cargo::cargo_bin("browser-control"),
    );
    let registry = Registry::open().unwrap();
    registry
        .insert(&BrowserRow {
            name: "brave-test".into(),
            kind: Kind::Brave,
            engine: Engine::Cdp,
            pid: std::process::id(),
            endpoint: format!("ws://127.0.0.1:{port}/devtools/browser/mock"),
            port,
            profile_dir: data.path().join("profile"),
            executable: "/usr/bin/true".into(),
            headless: true,
            started_at: "2026-09-03T00:00:00Z".into(),
        })
        .unwrap();

    // Spawning is blocking (thread sleeps); keep it off the runtime threads.
    let (pid, created) = tokio::task::spawn_blocking(move || {
        let registry = Registry::open().unwrap();
        foreground::spawn_holder(&registry, "brave-test", "T1", Duration::from_secs(600)).unwrap()
    })
    .await
    .unwrap();
    assert!(created);
    assert!(browser_control::registry::pid_alive(pid));
    let row = registry
        .foreground_get("brave-test", "T1")
        .unwrap()
        .unwrap();
    assert_eq!(row.pid, pid);
    assert!(row.expires_at_epoch_s > browser_control::registry::now_epoch_s() + 500);
    assert_eq!(
        foreground::active_targets(&registry, "brave-test").unwrap(),
        vec!["T1".to_string()]
    );
    {
        let m = methods(&seen);
        assert!(m.iter().any(|x| x == "Target.setDiscoverTargets"), "{m:?}");
        let attach = seen
            .lock()
            .unwrap()
            .iter()
            .find(|r| r["method"] == "Target.attachToTarget")
            .cloned()
            .unwrap();
        assert_eq!(attach["params"]["targetId"], "T1");
        let focus = seen
            .lock()
            .unwrap()
            .iter()
            .find(|r| r["method"] == "Emulation.setFocusEmulationEnabled")
            .cloned()
            .unwrap();
        assert_eq!(focus["params"]["enabled"], true);
        assert_eq!(focus["sessionId"], "S1");
        assert!(m.iter().any(|x| x == "Emulation.setIdleOverride"));
    }

    // Idempotent: a second `on` reuses the holder.
    let (pid2, created2) = tokio::task::spawn_blocking(move || {
        let registry = Registry::open().unwrap();
        foreground::spawn_holder(&registry, "brave-test", "T1", Duration::from_secs(600)).unwrap()
    })
    .await
    .unwrap();
    assert_eq!((pid2, created2), (pid, false));

    // Off: SIGTERM → the holder disables emulation, detaches, removes its row.
    let stopped = tokio::task::spawn_blocking(move || {
        let registry = Registry::open().unwrap();
        foreground::stop_holder(&registry, "brave-test", "T1").unwrap()
    })
    .await
    .unwrap();
    assert!(stopped);
    assert!(wait_until(
        || !browser_control::registry::pid_alive(pid),
        Duration::from_secs(5)
    ));
    assert!(registry
        .foreground_get("brave-test", "T1")
        .unwrap()
        .is_none());
    assert!(wait_until(
        || {
            let s = seen.lock().unwrap();
            s.iter().any(|r| {
                r["method"] == "Emulation.setFocusEmulationEnabled"
                    && r["params"]["enabled"] == false
            }) && s.iter().any(|r| r["method"] == "Target.detachFromTarget")
        },
        Duration::from_secs(5)
    ));
    let stopped_again = tokio::task::spawn_blocking(move || {
        let registry = Registry::open().unwrap();
        foreground::stop_holder(&registry, "brave-test", "T1").unwrap()
    })
    .await
    .unwrap();
    assert!(!stopped_again);

    // A holder exits on its own when the tab is destroyed; stop_all covers it.
    let (pid3, _) = tokio::task::spawn_blocking(move || {
        let registry = Registry::open().unwrap();
        foreground::spawn_holder(&registry, "brave-test", "T1", Duration::from_secs(600)).unwrap()
    })
    .await
    .unwrap();
    destroy.send(true).unwrap();
    assert!(wait_until(
        || !browser_control::registry::pid_alive(pid3),
        Duration::from_secs(5)
    ));
    assert!(registry.foreground_list("brave-test").unwrap().is_empty());
    let n = tokio::task::spawn_blocking(move || {
        let registry = Registry::open().unwrap();
        foreground::stop_all(&registry, "brave-test").unwrap()
    })
    .await
    .unwrap();
    assert_eq!(n, 0);

    std::env::remove_var("BROWSER_CONTROL_DATA_DIR");
    std::env::remove_var("BROWSER_CONTROL_BIN");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn holder_expires_after_timeout() {
    let (port, _seen, _destroy) = spawn_mock().await;
    let data = tempfile::TempDir::new().unwrap();
    // Separate registry so the two tests never share env state; pass the
    // data dir explicitly instead of through the process env.
    let db = data.path().join("registry.db");
    let registry = Registry::open_at(&db).unwrap();
    registry
        .insert(&BrowserRow {
            name: "brave-ttl".into(),
            kind: Kind::Brave,
            engine: Engine::Cdp,
            pid: std::process::id(),
            endpoint: format!("ws://127.0.0.1:{port}/devtools/browser/mock"),
            port,
            profile_dir: data.path().join("profile"),
            executable: "/usr/bin/true".into(),
            headless: true,
            started_at: "2026-09-03T00:00:00Z".into(),
        })
        .unwrap();
    // Run the holder subcommand directly with a 1 s timeout.
    let status = std::process::Command::new(assert_cmd::cargo::cargo_bin("browser-control"))
        .args([
            "tab",
            "foreground-hold",
            "brave-ttl",
            "T1",
            "--timeout-s",
            "1",
        ])
        .env("BROWSER_CONTROL_DATA_DIR", data.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "{status}");
    assert!(registry.foreground_list("brave-ttl").unwrap().is_empty());
}
