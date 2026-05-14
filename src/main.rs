use anyhow::Result;
use browser_control::cli::{list, set};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "browser-control", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
        #[arg(long)]
        profile: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Start the MCP server on stdio.
    Mcp {
        /// Browser to target. Overrides $BROWSER_CONTROL.
        #[arg(env = "BROWSER_CONTROL")]
        browser: Option<String>,
        /// Stdio-forward to the official Playwright MCP instead of exposing our tools.
        #[arg(long)]
        playwright: bool,
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
    match cli.command {
        Command::ListInstalled { json } => list::run_list_installed(json),
        Command::ListRunning { json } => list::run_list_running(json),
        Command::Start {
            browser,
            headless,
            profile,
            json,
        } => browser_control::cli::start::run(browser, headless, profile, json).await,
        Command::Mcp {
            browser,
            playwright,
        } => browser_control::cli::mcp::run_cli(browser, playwright).await,
        Command::Set { key, value, json } => set::run_set(key, value, json),
        Command::Get { key, json } => set::run_get(key, json),
        Command::Unset { key, json } => set::run_unset(key, json),
    }
}
