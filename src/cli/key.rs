//! `browser-control key` — press a key on the focused element.
//!
//! Page-context command. The key goes to whatever currently has focus, so it
//! addresses a tab rather than an element; focus something first (MCP
//! `browser_click`, or `Tab` your way there).
//!
//! Native on both engines — CDP `Input.dispatchKeyEvent`, BiDi
//! `input.performActions` — so no Node and no Playwright sidecar.

use std::time::Duration;

use anyhow::Result;

use crate::cli::env_resolver::Source;
use crate::cli::route;
use crate::cli::trace::CommandTrace;
use crate::cli::type_cmd::target_matching;
use crate::session::backend::open_backend;
use crate::session::keys::{parse_chord, Chord};
use crate::session::with_scratch_recovery;

const KEY_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run(browser: Option<String>, key: String, target: Option<String>) -> Result<()> {
    let mut trace = CommandTrace::new("key");
    let result = run_inner(browser, &key, target, &mut trace).await;
    trace.finish(result)?;
    println!("pressed {key}");
    Ok(())
}

async fn run_inner(
    browser: Option<String>,
    key: &str,
    target: Option<String>,
    trace: &mut CommandTrace,
) -> Result<()> {
    // Parse before touching the browser: a typo should cost nothing and say
    // what was wrong.
    let chord = parse_chord(key)?;

    let r = route::preamble(browser, target.as_deref(), trace).await?;
    let resolved = &r.resolved;

    match (r.tab_name.clone(), target) {
        // Path 1: <browser>/<tab> — named tab with recover-once.
        (Some(name), None) => {
            trace.route("named-tab").tab_name(&name);
            let chord = chord.clone();
            route::run_named_tab(
                &r,
                &name,
                "named tabs (`<browser>/<name>`) require a registered browser; \
                 external endpoints can't carry tab names",
                move |b, target_id| {
                    let chord: Chord = chord.clone();
                    async move { b.press_key_on_tab(&target_id, &chord, KEY_TIMEOUT).await }
                },
            )
            .await
        }
        // Path 2: bare browser → scratch tab with recover-once.
        (None, None) => {
            let browser_name = match &resolved.source {
                Source::Registered { name } => name.clone(),
                // A key press only makes sense against a tab we can name; an
                // external endpoint has no registry row to key a scratch by.
                Source::External => anyhow::bail!(
                    "`key` needs a registered browser or an explicit `--target`; \
                     external endpoints have no tab to focus"
                ),
            };
            trace.route("scratch");
            let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
            with_scratch_recovery(&backend, &r.registry, &browser_name, move |b, target_id| {
                let chord: Chord = chord.clone();
                async move { b.press_key_on_tab(&target_id, &chord, KEY_TIMEOUT).await }
            })
            .await
        }
        // Path 3: bare browser, --target regex. Resolved through the backend
        // we then use, not a throwaway PageSession: BiDi permits only one
        // session per browser, so attaching twice fails with "Maximum number
        // of active sessions".
        (None, Some(regex)) => {
            trace.route("target-regex");
            let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
            let target_id = target_matching(&backend, &regex).await?;
            let out = backend
                .press_key_on_tab(&target_id, &chord, KEY_TIMEOUT)
                .await;
            backend.release().await;
            out
        }
        _ => unreachable!("mutual exclusion checked in preamble"),
    }
}
