//! One-line structured trace per CLI dispatch.
//!
//! Every CLI command builds a [`CommandTrace`] at entry and finishes it
//! at exit. The trace is emitted at `tracing::Level::INFO` so it lands
//! by default (`BROWSER_CONTROL_LOG=info`), with a fixed field schema:
//!
//! ```text
//! command   – static command name (eval / fetch / tab-open / ...)
//! browser   – resolved registered name, or empty for external URLs
//! engine    – "cdp" | "bidi" | ""
//! route     – which code path the command took (scratch / named-tab / ...)
//! tab_name  – name of the named tab if any
//! target_id – engine-specific id touched, if known
//! elapsed_ms – wall-clock duration from CommandTrace::new to .finish*
//! outcome   – "ok" | "err"
//! ```
//!
//! Agents and operators consume these by tailing stderr and grepping
//! `target=browser_control::cli`.

use std::time::Instant;

use crate::detect::Engine;

/// Mutable trace builder for a single CLI command invocation.
///
/// Construction starts the clock; calling [`CommandTrace::ok`] or
/// [`CommandTrace::err`] emits the line and consumes the value.
///
/// The builder fields are accumulated as the command progresses and
/// figures out what it's doing (browser resolution, route selection,
/// tab binding). Anything not set defaults to an empty string in the
/// emitted log line so the schema is stable.
pub struct CommandTrace {
    command: &'static str,
    start: Instant,
    browser: String,
    engine: String,
    route: &'static str,
    tab_name: String,
    target_id: String,
}

impl CommandTrace {
    /// Start a trace. The clock starts here. `command` is the static
    /// command name (`"eval"`, `"fetch"`, `"tab-open"`, …) — keep it
    /// kebab-cased so log consumers can match on a stable string.
    pub fn new(command: &'static str) -> Self {
        Self {
            command,
            start: Instant::now(),
            browser: String::new(),
            engine: String::new(),
            route: "",
            tab_name: String::new(),
            target_id: String::new(),
        }
    }

    pub fn browser(&mut self, s: impl Into<String>) -> &mut Self {
        self.browser = s.into();
        self
    }

    pub fn engine(&mut self, e: Engine) -> &mut Self {
        self.engine = match e {
            Engine::Cdp => "cdp".into(),
            Engine::Bidi => "bidi".into(),
        };
        self
    }

    /// Set the routing path. Suggested values:
    /// - `"scratch"`        — scratch-tab recovery wrapper
    /// - `"named-tab"`      — `<browser>/<tab>` via tabs registry
    /// - `"target-regex"`   — legacy `--target <regex>` selector
    /// - `"attach-for-origin"` — fetch default (origin-matched tab)
    /// - `"direct"`         — external URL fall-through
    /// - `"registry"`       — registry-only ops (cookies, wait, etc.)
    pub fn route(&mut self, r: &'static str) -> &mut Self {
        self.route = r;
        self
    }

    pub fn tab_name(&mut self, s: impl Into<String>) -> &mut Self {
        self.tab_name = s.into();
        self
    }

    pub fn target_id(&mut self, s: impl Into<String>) -> &mut Self {
        self.target_id = s.into();
        self
    }

    /// Emit the closing log line with `outcome=ok` and return `val`
    /// unchanged. Use at the success exit of every command:
    /// `Ok(trace.ok(value))`.
    pub fn ok<T>(self, val: T) -> T {
        self.emit("ok", None);
        val
    }

    /// Emit the closing log line with `outcome=err` and return `err`
    /// unchanged. Use at the error exit: `Err(trace.err(e))`.
    pub fn err(self, err: anyhow::Error) -> anyhow::Error {
        let msg = format!("{err:#}");
        self.emit("err", Some(&msg));
        err
    }

    /// Combinator collapsing the `match result { Ok => ok; Err => err }`
    /// boilerplate every command's entry point used to repeat. Emits the
    /// closing line with the matching outcome and returns `result`
    /// unchanged: `trace.finish(run_inner(...).await)`.
    pub fn finish(self, result: anyhow::Result<()>) -> anyhow::Result<()> {
        match result {
            Ok(()) => {
                self.ok(());
                Ok(())
            }
            Err(e) => Err(self.err(e)),
        }
    }

    fn emit(&self, outcome: &'static str, err_msg: Option<&str>) {
        let elapsed_ms = self.start.elapsed().as_millis() as u64;
        tracing::info!(
            target: "browser_control::cli",
            command = self.command,
            browser = self.browser.as_str(),
            engine = self.engine.as_str(),
            route = self.route,
            tab_name = self.tab_name.as_str(),
            target_id = self.target_id.as_str(),
            elapsed_ms,
            outcome,
            err = err_msg.unwrap_or(""),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::subscriber::with_default;

    /// Verify the line emits with the expected schema. We capture via a
    /// tiny custom subscriber rather than depending on `tracing_test`.
    #[test]
    fn ok_emits_one_line_with_full_schema() {
        let captured: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sub = TestSubscriber::new(captured.clone());
        with_default(sub, || {
            let mut t = CommandTrace::new("eval");
            t.browser("brave-twilight")
                .engine(Engine::Cdp)
                .route("scratch")
                .target_id("T42");
            let _ = t.ok(123);
        });
        let lines = captured.lock().unwrap();
        assert_eq!(lines.len(), 1, "exactly one line emitted");
        let line = &lines[0];
        assert!(line.contains("command=\"eval\""), "command field: {line}");
        assert!(line.contains("browser=\"brave-twilight\""));
        assert!(line.contains("engine=\"cdp\""));
        assert!(line.contains("route=\"scratch\""));
        assert!(line.contains("target_id=\"T42\""));
        assert!(line.contains("outcome=\"ok\""));
        assert!(line.contains("elapsed_ms="));
    }

    #[test]
    fn err_emits_with_outcome_err_and_msg() {
        let captured: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sub = TestSubscriber::new(captured.clone());
        with_default(sub, || {
            let mut t = CommandTrace::new("fetch");
            t.browser("chrome-pikachu")
                .engine(Engine::Cdp)
                .route("named-tab")
                .tab_name("scrape-cart");
            let _ = t.err(anyhow::anyhow!("simulated failure"));
        });
        let line = captured.lock().unwrap().pop().unwrap();
        assert!(line.contains("command=\"fetch\""));
        assert!(line.contains("outcome=\"err\""));
        assert!(line.contains("err=\"simulated failure\""));
        assert!(line.contains("tab_name=\"scrape-cart\""));
    }

    /// Tiny tracing subscriber that formats each `info!` event into a
    /// single string and pushes onto a shared Vec. Avoids pulling in a
    /// formatter crate for tests.
    struct TestSubscriber {
        captured: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl TestSubscriber {
        fn new(captured: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Self {
            Self { captured }
        }
    }
    impl tracing::Subscriber for TestSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = StringVisitor {
                fields: String::new(),
            };
            event.record(&mut visitor);
            self.captured.lock().unwrap().push(visitor.fields);
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }
    struct StringVisitor {
        fields: String,
    }
    impl tracing::field::Visit for StringVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write;
            let _ = write!(self.fields, "{}={:?} ", field.name(), value);
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            use std::fmt::Write;
            let _ = write!(self.fields, "{}={:?} ", field.name(), value);
        }
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            use std::fmt::Write;
            let _ = write!(self.fields, "{}={} ", field.name(), value);
        }
    }
}
