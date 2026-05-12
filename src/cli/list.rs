//! `list-installed` and `list-running` subcommand handlers.

use anyhow::Result;

use crate::cli::output::{print_json, print_table};
use crate::detect;
use crate::registry::Registry;

/// Implements `browser-control list-installed [--json]`.
pub fn run_list_installed(json: bool) -> Result<()> {
    let installed = detect::list_installed();
    if json {
        print_json(&mut std::io::stdout(), &installed)?;
    } else {
        let headers = ["KIND", "VERSION", "ENGINE", "EXECUTABLE"];
        let rows = installed
            .iter()
            .map(|i| {
                vec![
                    i.kind.as_str().to_string(),
                    i.version.clone(),
                    format!("{:?}", i.engine).to_lowercase(),
                    i.executable.display().to_string(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&mut std::io::stdout(), &headers, &rows)?;
    }
    Ok(())
}

/// Implements `browser-control list-running [--json]`.
pub fn run_list_running(json: bool) -> Result<()> {
    let reg = Registry::open()?;
    let alive = reg.list_alive()?;
    if json {
        print_json(&mut std::io::stdout(), &alive)?;
    } else {
        let headers = [
            "NAME", "KIND", "PID", "ENGINE", "ENDPOINT", "PROFILE", "STARTED",
        ];
        let rows = alive
            .iter()
            .map(|r| {
                vec![
                    r.name.clone(),
                    r.kind.as_str().to_string(),
                    r.pid.to_string(),
                    format!("{:?}", r.engine).to_lowercase(),
                    r.endpoint.clone(),
                    r.profile_dir.display().to_string(),
                    r.started_at.clone(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&mut std::io::stdout(), &headers, &rows)?;
    }
    Ok(())
}
