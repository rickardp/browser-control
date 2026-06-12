//! `show` subcommand: explicitly reveal a controlled browser for login or
//! debugging. Normal automation keeps browser windows/tabs in the background.

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::env_resolver::Source;
use crate::cli::mcp::{acquire_bidi_lock_if_needed, resolve_browser};
use crate::cli::output::print_json;
use crate::detect::Engine;
use crate::registry::Registry;
use crate::session::backend::open_backend;

#[derive(Debug, Serialize)]
struct ShowResult {
    name: String,
    engine: Engine,
    endpoint: String,
    target_id: String,
    os_activated: bool,
}

pub async fn run(browser: Option<String>, json: bool) -> Result<()> {
    let registry = Registry::open()?;
    let resolved = resolve_browser(browser).await?;
    let name = match &resolved.source {
        Source::Registered { name } => name.clone(),
        Source::External => "<external>".to_string(),
    };

    let _bidi_lock = acquire_bidi_lock_if_needed(&registry, &resolved)?;
    let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
    let target_id = backend.target_for_show().await?;

    let os_activated = activate_resolved_app(&registry, &resolved.source)
        .with_context(|| format!("activating browser app for {name}"))?;
    backend.show_tab(&target_id).await?;

    let result = ShowResult {
        name,
        engine: resolved.engine,
        endpoint: resolved.endpoint,
        target_id,
        os_activated,
    };
    if json {
        print_json(&mut std::io::stdout(), &result)?;
    } else {
        println!("Shown {}", result.name);
        println!("  engine:    {:?}", result.engine);
        println!("  endpoint:  {}", result.endpoint);
        println!("  target_id: {}", result.target_id);
    }
    Ok(())
}

pub(crate) fn activate_resolved_app(registry: &Registry, source: &Source) -> Result<bool> {
    let Source::Registered { name } = source else {
        return Ok(false);
    };
    let Some(row) = registry.get_by_name(name)? else {
        return Ok(false);
    };
    platform_activate(&row.executable)
}

#[cfg(target_os = "macos")]
fn platform_activate(executable: &std::path::Path) -> Result<bool> {
    let Some(app) = app_bundle_for_executable(executable) else {
        return Ok(false);
    };
    let status = std::process::Command::new("open")
        .arg(&app)
        .status()
        .with_context(|| format!("running `open {}`", app.display()))?;
    if status.success() {
        Ok(true)
    } else {
        anyhow::bail!("`open {}` exited with {status}", app.display())
    }
}

#[cfg(not(target_os = "macos"))]
fn platform_activate(_executable: &std::path::Path) -> Result<bool> {
    Ok(false)
}

#[cfg(target_os = "macos")]
fn app_bundle_for_executable(executable: &std::path::Path) -> Option<std::path::PathBuf> {
    executable
        .ancestors()
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("app"))
        .map(std::path::Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn finds_app_bundle_for_macos_executable() {
        let path =
            std::path::Path::new("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser");
        assert_eq!(
            super::app_bundle_for_executable(path).as_deref(),
            Some(std::path::Path::new("/Applications/Brave Browser.app"))
        );
    }
}
