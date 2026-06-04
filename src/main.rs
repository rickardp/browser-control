use anyhow::Result;
use browser_control::cli::{
    agent_instructions, cookies, eval, fetch, list, set, storage, targets as cli_targets, wait,
    wait_for_cookie,
};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "browser-control", version, about, long_about = None, arg_required_else_help = true)]
struct Cli {
    /// Print canonical instructions for agents using browser-control.
    #[arg(long)]
    agent_instructions: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List browsers installed on this machine.
    ListInstalled {
        #[arg(long)]
        json: bool,
    },
    /// List browsers currently registered and alive.
    ListRunning {
        #[arg(long)]
        json: bool,
    },
    /// Start a browser and register it.
    Start {
        /// Browser kind (chrome, edge, chromium, brave, firefox) or friendly name.
        browser: Option<String>,
        #[arg(long)]
        headless: bool,
        /// Do not wait for the browser's debugging endpoint to be reachable.
        #[arg(long)]
        no_wait: bool,
        /// Seconds to wait for the endpoint when not using --no-wait.
        #[arg(long, default_value_t = 30)]
        wait_timeout: u64,
        #[arg(long)]
        json: bool,
    },
    /// Start the MCP server on stdio.
    Mcp {
        /// Browser to target. Overrides $BROWSER_CONTROL.
        #[arg(long, short = 'b', env = "BROWSER_CONTROL")]
        browser: Option<String>,
        /// Override the `playwright-core` version used by the internal
        /// Playwright sidecar (defaults to the pinned version in the
        /// bundled `package.json`). Useful for picking up an upstream
        /// API change without waiting for a browser-control release.
        #[arg(long)]
        playwright_version: Option<String>,
    },
    /// List page targets in the active browser.
    Targets {
        #[arg(long, short = 'b', env = "BROWSER_CONTROL")]
        browser: Option<String>,
        /// Filter pages whose URL matches this regex.
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Export cookies from the active browser.
    Cookies {
        #[arg(long, short = 'b', env = "BROWSER_CONTROL")]
        browser: Option<String>,
        /// Filter by cookie domain regex.
        #[arg(long)]
        domain: Option<String>,
        /// Filter by cookie name regex.
        #[arg(long)]
        name: Option<String>,
        /// Output format.
        #[arg(long, default_value = "json")]
        format: String,
        /// Write to FILE (chmod 0600) instead of stdout.
        #[arg(long, short = 'o')]
        output: Option<std::path::PathBuf>,
        /// Print cookie values in human output (default: redacted).
        #[arg(long)]
        reveal: bool,
        #[arg(long)]
        json: bool,
    },
    /// Run an HTTP request from the browser page context.
    Fetch {
        #[arg(long, short = 'b', env = "BROWSER_CONTROL")]
        browser: Option<String>,
        url: String,
        #[arg(long, short = 'X', default_value = "GET")]
        method: String,
        /// Repeat -H to add multiple headers; format `Key: Value`.
        #[arg(long = "header", short = 'H')]
        headers: Vec<String>,
        #[arg(long, short = 'd')]
        data: Option<String>,
        /// Select a page target by URL regex (default: first page).
        #[arg(long)]
        target: Option<String>,
        /// Prepend HTTP status + headers to stdout, like `curl -i`.
        #[arg(long, short = 'i')]
        include: bool,
        #[arg(long, short = 'o')]
        output: Option<std::path::PathBuf>,
        /// Per-call timeout in milliseconds for the in-page fetch. Default
        /// 60 s is generous for slow networks; lower bounds catch wedged
        /// renderers faster.
        #[arg(long, default_value_t = 60_000)]
        timeout_ms: u64,
        /// Reload the page first if its document is older than this duration.
        #[arg(long, default_value = browser_control::session::freshness::DEFAULT_MAX_AGE_STR)]
        max_age: String,
    },
    /// Read or write localStorage / sessionStorage.
    Storage {
        #[command(subcommand)]
        action: storage::StorageCmd,
    },
    /// Evaluate a JavaScript expression in the active page.
    Eval {
        #[arg(long, short = 'b', env = "BROWSER_CONTROL")]
        browser: Option<String>,
        /// JavaScript expression.
        expression: String,
        #[arg(long)]
        target: Option<String>,
        /// Print the full Runtime.evaluate envelope.
        #[arg(long)]
        json: bool,
        /// Treat the expression as a Promise and await it.
        #[arg(long, default_value_t = true)]
        await_promise: bool,
        /// Per-call timeout in milliseconds. The default catches wedged
        /// renderers (service-worker-paused tabs, busy event loops, embedded
        /// admin UIs that ignore Runtime.evaluate) so the CLI fails fast
        /// instead of dragging through the upstream 30 s protocol timeout.
        #[arg(long, default_value_t = 10_000)]
        timeout_ms: u64,
        /// Reload the page first if its document is older than this duration.
        #[arg(long, default_value = browser_control::session::freshness::DEFAULT_MAX_AGE_STR)]
        max_age: String,
    },
    /// Wait until the browser endpoint is reachable.
    Wait {
        #[arg(long, short = 'b', env = "BROWSER_CONTROL")]
        browser: Option<String>,
        /// Deprecated no-op kept for backward compatibility. Readiness is now
        /// the default (and only) mode.
        #[arg(long, hide = true)]
        ready: bool,
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Wait until a cookie matching the filter appears.
    WaitForCookie {
        #[arg(long, short = 'b', env = "BROWSER_CONTROL")]
        browser: Option<String>,
        #[arg(long)]
        domain: String,
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        #[arg(long, default_value_t = 1)]
        poll_interval: u64,
        /// After the cookie appears, GET this URL through the page and require 2xx.
        #[arg(long)]
        validate_url: Option<String>,
        /// Reload the validation page first if its document is older than this duration.
        #[arg(long, default_value = browser_control::session::freshness::DEFAULT_MAX_AGE_STR)]
        max_age: String,
    },
    /// Set a persistent setting (e.g. `set default firefox`).
    Set {
        #[arg(value_enum)]
        key: set::Key,
        /// Value to assign. Omit and use `unset` instead to clear.
        value: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print a persistent setting.
    Get {
        #[arg(value_enum)]
        key: set::Key,
        #[arg(long)]
        json: bool,
    },
    /// Clear a persistent setting.
    Unset {
        #[arg(value_enum)]
        key: set::Key,
        #[arg(long)]
        json: bool,
    },
    /// Open or navigate tabs: `tab open <browser>/<name> <url>` (re-running
    /// with a new url navigates the existing tab), `tab list`, `tab adopt`.
    /// This is the way to go to a URL — do not navigate by evaling
    /// `location.href`. Addressable as `--browser <browser>/<name>` in
    /// page-context commands.
    Tab {
        #[command(subcommand)]
        cmd: browser_control::cli::tab::TabCmd,
    },
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("BROWSER_CONTROL_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    if cli.agent_instructions {
        agent_instructions::print();
        return Ok(());
    }

    let Some(command) = cli.command else {
        return Ok(());
    };

    match command {
        Command::ListInstalled { json } => list::run_list_installed(json),
        Command::ListRunning { json } => list::run_list_running(json).await,
        Command::Start {
            browser,
            headless,
            no_wait,
            wait_timeout,
            json,
        } => browser_control::cli::start::run(browser, headless, no_wait, wait_timeout, json).await,
        Command::Mcp {
            browser,
            playwright_version,
        } => browser_control::cli::mcp::run_cli(browser, playwright_version).await,
        Command::Set { key, value, json } => set::run_set(key, value, json),
        Command::Get { key, json } => set::run_get(key, json),
        Command::Unset { key, json } => set::run_unset(key, json),
        Command::Targets { browser, url, json } => cli_targets::run(browser, url, json).await,
        Command::Cookies {
            browser,
            domain,
            name,
            format,
            output,
            reveal,
            json,
        } => cookies::run(browser, domain, name, format, output, reveal, json).await,
        Command::Fetch {
            browser,
            url,
            method,
            headers,
            data,
            target,
            include,
            output,
            timeout_ms,
            max_age,
        } => {
            fetch::run(
                browser, url, method, headers, data, target, include, output, timeout_ms, max_age,
            )
            .await
        }
        Command::Storage { action } => storage::run(action).await,
        Command::Eval {
            browser,
            expression,
            target,
            json,
            await_promise,
            timeout_ms,
            max_age,
        } => {
            eval::run(
                browser,
                expression,
                target,
                json,
                await_promise,
                timeout_ms,
                max_age,
            )
            .await
        }
        Command::Wait {
            browser,
            ready,
            timeout,
        } => wait::run(browser, ready, timeout).await,
        Command::WaitForCookie {
            browser,
            domain,
            name,
            timeout,
            poll_interval,
            validate_url,
            max_age,
        } => {
            wait_for_cookie::run(
                browser,
                domain,
                name,
                timeout,
                poll_interval,
                validate_url,
                max_age,
            )
            .await
        }
        Command::Tab { cmd } => browser_control::cli::tab::run(cmd).await,
    }
}
