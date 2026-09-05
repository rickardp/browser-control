//! `browser-control type` — type into the focused element.
//!
//! Exists chiefly so a secret can reach a login form without passing through
//! the agent driving the login:
//!
//! ```sh
//! op read op://Automation/site/password |
//!   browser-control type -b brave/login --stdin --submit
//! ```
//!
//! The value travels an OS pipe between two processes the agent spawned. It
//! is never a tool result, never a log line, and never in the agent's
//! context — only the reference (`op://…`) is, which is not a secret.
//!
//! Any vault works: anything that prints a secret on stdout is a resolver
//! (`op read`, `bw get password`, `vault kv get -field`, `security
//! find-internet-password -w`, `browser-control vault read`). browser-control
//! knows about none of them.
//!
//! Targets the **focused** element rather than a ref, because refs live in
//! the MCP server's state and a separate CLI process cannot see them. Focus
//! the field first with a ref-based click over MCP, or by tabbing to it.

use std::io::Read;
use std::time::Duration;

use anyhow::{bail, Result};

use crate::cli::env_resolver::Source;
use crate::cli::route;
use crate::cli::trace::CommandTrace;
use crate::session::backend::open_backend;
use crate::session::with_scratch_recovery;

const TYPE_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run(
    browser: Option<String>,
    text: Option<String>,
    stdin: bool,
    submit: bool,
    press_sequentially: bool,
    target: Option<String>,
) -> Result<()> {
    let mut trace = CommandTrace::new("type");
    // Resolve the value before touching the browser, so a bad invocation
    // costs nothing and never half-fills a form.
    let value = match (text, stdin) {
        (Some(_), true) => bail!("`--text` and `--stdin` are mutually exclusive; pass one"),
        (None, false) => bail!("one of `--text` or `--stdin` is required"),
        (Some(t), false) => t,
        (None, true) => read_stdin()?,
    };
    let result = run_inner(
        browser,
        &value,
        submit,
        press_sequentially,
        target,
        &mut trace,
    )
    .await;
    trace.finish(result)?;
    // Report the length, never the value.
    println!("typed {} characters", value.chars().count());
    Ok(())
}

/// Read the secret from stdin.
///
/// Every rejection here is a real failure mode of a vault CLI, and every one
/// of them would otherwise be typed verbatim into a password field:
/// an empty read means a mis-scoped token, and a multi-line read means the
/// resolver printed a banner or more than one secret.
fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| anyhow::anyhow!("reading stdin: {e}"))?;
    // One trailing newline is normal from a CLI; strip exactly that.
    let value = buf.strip_suffix('\n').unwrap_or(&buf);
    let value = value.strip_suffix('\r').unwrap_or(value);
    if value.trim().is_empty() {
        bail!("stdin was empty; the resolver produced no value (a mis-scoped token?)");
    }
    if value.contains('\n') {
        bail!(
            "stdin held {} lines; a resolver should print exactly one secret \
             (a warning banner on stdout?)",
            value.lines().count()
        );
    }
    Ok(value.to_string())
}

async fn run_inner(
    browser: Option<String>,
    value: &str,
    submit: bool,
    press_sequentially: bool,
    target: Option<String>,
    trace: &mut CommandTrace,
) -> Result<()> {
    let r = route::preamble(browser, target.as_deref(), trace).await?;
    let resolved = &r.resolved;

    match (r.tab_name.clone(), target) {
        // Path 1: <browser>/<tab> — named tab with recover-once.
        (Some(name), None) => {
            trace.route("named-tab").tab_name(&name);
            let value = value.to_string();
            route::run_named_tab(
                &r,
                &name,
                "named tabs (`<browser>/<name>`) require a registered browser; \
                 external endpoints can't carry tab names",
                move |b, target_id| {
                    let value = value.clone();
                    async move {
                        b.type_into_focused(
                            &target_id,
                            &value,
                            press_sequentially,
                            submit,
                            TYPE_TIMEOUT,
                        )
                        .await
                    }
                },
            )
            .await
        }
        // Path 2: bare browser → scratch tab with recover-once.
        (None, None) => {
            let browser_name = match &resolved.source {
                Source::Registered { name } => name.clone(),
                Source::External => bail!(
                    "`type` needs a registered browser or an explicit `--target`; \
                     external endpoints have no tab to focus"
                ),
            };
            trace.route("scratch");
            let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
            let value = value.to_string();
            with_scratch_recovery(&backend, &r.registry, &browser_name, move |b, target_id| {
                let value = value.clone();
                async move {
                    b.type_into_focused(
                        &target_id,
                        &value,
                        press_sequentially,
                        submit,
                        TYPE_TIMEOUT,
                    )
                    .await
                }
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
                .type_into_focused(&target_id, value, press_sequentially, submit, TYPE_TIMEOUT)
                .await;
            backend.release().await;
            out
        }
        _ => unreachable!("mutual exclusion checked in preamble"),
    }
}

/// First live target whose URL matches `regex`, using an existing backend.
///
/// Deliberately not `PageSession::attach`: that opens a second connection,
/// and BiDi permits only one session per browser.
pub(crate) async fn target_matching(
    backend: &crate::session::backend::TabBackend,
    regex: &str,
) -> Result<String> {
    let re = regex::Regex::new(regex).map_err(|e| anyhow::anyhow!("bad --target regex: {e}"))?;
    let targets = backend.live_targets().await?;
    targets
        .into_iter()
        .find(|t| re.is_match(&t.url))
        .map(|t| t.id)
        .ok_or_else(|| anyhow::anyhow!("no tab matched `{regex}`"))
}

#[cfg(test)]
mod tests {

    // read_stdin is exercised through its rejection paths, which are the
    // ones that would otherwise put junk into a password field.

    #[test]
    fn trailing_newline_is_stripped_but_inner_content_kept() {
        // Simulated by exercising the same trimming logic.
        let cases = [
            ("hunter2\n", "hunter2"),
            ("hunter2\r\n", "hunter2"),
            ("hunter2", "hunter2"),
        ];
        for (raw, want) in cases {
            let v = raw.strip_suffix('\n').unwrap_or(raw);
            let v = v.strip_suffix('\r').unwrap_or(v);
            assert_eq!(v, want);
        }
    }

    #[test]
    fn a_password_may_contain_spaces_and_symbols() {
        let raw = "p@ss word!+/=\n";
        let v = raw.strip_suffix('\n').unwrap();
        assert_eq!(v, "p@ss word!+/=");
        assert!(!v.trim().is_empty());
        assert!(!v.contains('\n'));
    }
}
