//! Unit tests for the sidecar runtime.
//!
//! These tests exercise the JSON-RPC plumbing using a *mock* sidecar
//! (a Bash/Python/Node one-liner that speaks the same NDJSON protocol)
//! rather than spawning real Playwright. Real-Playwright integration is
//! tested at the MCP-tool level.

use super::*;

/// Build a `Sidecar` directly from a child process whose stdio speaks the
/// NDJSON RPC protocol. Skips the install/spawn dance — purely for unit
/// tests of the request/response wiring.
async fn from_child(mut child: Child) -> Result<Sidecar> {
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("no child stdin"))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no child stdout"))?;
    let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
    let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let writer_handle = tokio::spawn(async move {
        while let Some(line) = write_rx.recv().await {
            if child_stdin.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if child_stdin.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = child_stdin.flush().await;
        }
    });

    let pending_r = pending.clone();
    let reader_handle = tokio::spawn(async move {
        let mut lines = BufReader::new(child_stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let id = match v.get("id").and_then(|x| x.as_u64()) {
                Some(i) => i,
                None => continue,
            };
            let result = if let Some(err) = v.get("error") {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("(no message)");
                Err(anyhow!("{msg}"))
            } else {
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            };
            let tx = {
                let mut p = pending_r.lock().await;
                p.remove(&id)
            };
            if let Some(tx) = tx {
                let _ = tx.send(result);
            }
        }
        let mut p = pending_r.lock().await;
        for (_, tx) in p.drain() {
            let _ = tx.send(Err(anyhow!("sidecar stdout closed")));
        }
    });

    Ok(Sidecar {
        next_id: Arc::new(AtomicU64::new(1)),
        pending,
        write_tx,
        _inner: Arc::new(SidecarInner {
            child: Mutex::new(Some(child)),
            reader_handle: Mutex::new(Some(reader_handle)),
            writer_handle: Mutex::new(Some(writer_handle)),
        }),
    })
}

/// Mock sidecar via a tiny Node one-liner: read NDJSON, echo back with
/// `result.method = <method>`. We only need this if Node is installed.
#[tokio::test]
async fn call_roundtrips_via_node_echo() {
    if which::which("node").is_err() {
        eprintln!("skipping: node not on PATH");
        return;
    }
    let child = Command::new("node")
        .arg("-e")
        .arg(
            r#"const rl = require('readline').createInterface({input: process.stdin});
            rl.on('line', l => {
                try {
                    const r = JSON.parse(l);
                    process.stdout.write(JSON.stringify({id: r.id, result: {method: r.method, params: r.params}}) + "\n");
                } catch {}
            });"#,
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn node");

    let sc = from_child(child).await.unwrap();
    let v = sc
        .call("ping", json!({"hello": "world"}))
        .await
        .expect("call ok");
    assert_eq!(v["method"], "ping");
    assert_eq!(v["params"]["hello"], "world");
}

/// Mock sidecar that returns an error response. We verify the error is
/// surfaced as `anyhow::Error` with the JSON-RPC `message` text.
#[tokio::test]
async fn error_response_surfaces_as_anyhow() {
    if which::which("node").is_err() {
        eprintln!("skipping: node not on PATH");
        return;
    }
    let child = Command::new("node")
        .arg("-e")
        .arg(
            r#"const rl = require('readline').createInterface({input: process.stdin});
            rl.on('line', l => {
                const r = JSON.parse(l);
                process.stdout.write(JSON.stringify({id: r.id, error: {message: "boom"}}) + "\n");
            });"#,
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn node");

    let sc = from_child(child).await.unwrap();
    let err = sc.call("explode", json!({})).await.expect_err("must error");
    assert!(err.to_string().contains("boom"), "got: {err}");
}
