//! browser-control build/dev tasks.
//!
//! Currently supports installing a pinned Cap'n Proto toolchain into the
//! workspace so contributors don't need a system install.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const CAPNP_VERSION: &str = "1.4.0";

/// Optional SHA-256 pins, keyed by basename. Empty entries mean "skip verification"
/// (we still print the observed digest so it can be pinned later).
const CAPNP_SHA256: &[(&str, &str)] = &[
    // ("capnproto-c++-1.4.0.tar.gz", "<sha>"),
    // ("capnproto-c++-win32-1.4.0.zip", "<sha>"),
];

#[derive(Parser)]
#[command(name = "xtask", about = "browser-control developer tasks")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install the pinned Cap'n Proto compiler into the workspace.
    InstallCapnp(InstallCapnpArgs),
}

#[derive(Parser)]
struct InstallCapnpArgs {
    /// Override the pinned version.
    #[arg(long, default_value = CAPNP_VERSION)]
    version: String,
    /// Prefix to install into. Defaults to target/tools/capnp/<version>.
    #[arg(long)]
    prefix: Option<PathBuf>,
    /// Reinstall even if already present.
    #[arg(long)]
    force: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::InstallCapnp(args) => install_capnp(args),
    }
}

fn workspace_root() -> Result<PathBuf> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .context("CARGO_MANIFEST_DIR not set; run via `cargo xtask ...`")?;
    Ok(PathBuf::from(manifest_dir)
        .parent()
        .context("xtask manifest dir has no parent")?
        .to_path_buf())
}

fn install_capnp(args: InstallCapnpArgs) -> Result<()> {
    let root = workspace_root()?;
    let prefix = args
        .prefix
        .unwrap_or_else(|| root.join("target/tools/capnp").join(&args.version));
    let bin_name = if cfg!(windows) { "capnp.exe" } else { "capnp" };
    let bin_path = prefix.join("bin").join(bin_name);

    if bin_path.exists() && !args.force {
        println!("capnp already installed at {}", bin_path.display());
        update_current_symlink(&root, &prefix)?;
        return Ok(());
    }

    fs::create_dir_all(&prefix).with_context(|| format!("create {}", prefix.display()))?;
    let tmp = tempfile::tempdir().context("create scratch dir")?;

    if cfg!(windows) {
        install_windows(&args.version, tmp.path(), &prefix)?;
    } else {
        install_unix(&args.version, tmp.path(), &prefix)?;
    }

    if !bin_path.exists() {
        bail!(
            "install completed but {} is missing - investigate",
            bin_path.display()
        );
    }
    update_current_symlink(&root, &prefix)?;
    println!("\ncapnp installed: {}", bin_path.display());
    println!("Add to PATH:");
    if cfg!(windows) {
        println!("  set PATH={};%PATH%", prefix.join("bin").display());
    } else {
        println!("  export PATH=\"{}:$PATH\"", prefix.join("bin").display());
    }
    Ok(())
}

fn install_unix(version: &str, scratch: &Path, prefix: &Path) -> Result<()> {
    let file = format!("capnproto-c++-{version}.tar.gz");
    let url = format!("https://capnproto.org/{file}");
    let archive = scratch.join(&file);
    download(&url, &archive)?;
    verify_sha256(&archive, &file)?;

    let src = scratch.join("src");
    fs::create_dir_all(&src)?;
    println!("Extracting {} ...", file);
    let f = fs::File::open(&archive)?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut tar = tar::Archive::new(gz);
    tar.unpack(&src)?;

    let extracted = src.join(format!("capnproto-c++-{version}"));
    if !extracted.exists() {
        bail!("expected extracted dir {} not found", extracted.display());
    }

    println!("Configuring (prefix {}) ...", prefix.display());
    run(
        "./configure",
        &[&format!("--prefix={}", prefix.display())],
        &extracted,
    )?;

    let jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    println!("Building with -j{jobs} ...");
    run("make", &[&format!("-j{jobs}")], &extracted)?;
    println!("Installing ...");
    run("make", &["install"], &extracted)?;
    Ok(())
}

fn install_windows(version: &str, scratch: &Path, prefix: &Path) -> Result<()> {
    let file = format!("capnproto-c++-win32-{version}.zip");
    let url = format!("https://capnproto.org/{file}");
    let archive = scratch.join(&file);
    download(&url, &archive)?;
    verify_sha256(&archive, &file)?;

    println!("Extracting {} ...", file);
    let f = fs::File::open(&archive)?;
    let mut zip = zip::ZipArchive::new(f)?;

    let bin_dir = prefix.join("bin");
    fs::create_dir_all(&bin_dir)?;
    // Tool exe basenames we need at minimum.
    let wanted = ["capnp.exe", "capnpc-c++.exe", "capnpc-capnp.exe"];
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        let base = Path::new(&name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if wanted.contains(&base) {
            let out = bin_dir.join(base);
            let mut sink = fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut sink)?;
        }
    }
    // Optionally also copy the schema include tree so `import "/capnp/stream.capnp"` works.
    let include_dir = prefix.join("include");
    fs::create_dir_all(&include_dir)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        if let Some(idx) = name.find("/src/capnp/") {
            let rel = &name[idx + 1..];
            let out = include_dir.join(rel);
            if entry.is_dir() {
                fs::create_dir_all(&out)?;
            } else {
                if let Some(p) = out.parent() {
                    fs::create_dir_all(p)?;
                }
                let mut sink = fs::File::create(&out)?;
                std::io::copy(&mut entry, &mut sink)?;
            }
        }
    }
    Ok(())
}

fn download(url: &str, dest: &Path) -> Result<()> {
    println!("Downloading {url} ...");
    let resp = reqwest::blocking::get(url)
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error for {url}"))?;
    let bytes = resp.bytes().context("read response body")?;
    let mut out = fs::File::create(dest)?;
    out.write_all(&bytes)?;
    Ok(())
}

fn verify_sha256(path: &Path, basename: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let observed = hex::encode(hasher.finalize());
    let expected = CAPNP_SHA256
        .iter()
        .find(|(name, _)| *name == basename)
        .map(|(_, sha)| *sha);
    match expected {
        Some(exp) if !exp.is_empty() => {
            if observed != exp {
                bail!("SHA-256 mismatch for {basename}:\n  expected {exp}\n  observed {observed}");
            }
            println!("SHA-256 verified: {observed}");
        }
        _ => {
            eprintln!(
                "warning: no SHA-256 pinned for {basename}; observed {observed}\n  \
                 (add to CAPNP_SHA256 in xtask/src/main.rs to pin)"
            );
        }
    }
    Ok(())
}

fn run(cmd: &str, args: &[&str], cwd: &Path) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("spawn {cmd}"))?;
    if !status.success() {
        bail!("{cmd} {:?} failed with status {status}", args);
    }
    Ok(())
}

#[cfg(unix)]
fn update_current_symlink(root: &Path, prefix: &Path) -> Result<()> {
    let link = root.join("target/tools/capnp/current");
    if link.exists() || link.symlink_metadata().is_ok() {
        let _ = fs::remove_file(&link);
    }
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    std::os::unix::fs::symlink(prefix, &link)
        .with_context(|| format!("symlink {} -> {}", link.display(), prefix.display()))?;
    Ok(())
}

#[cfg(windows)]
fn update_current_symlink(root: &Path, prefix: &Path) -> Result<()> {
    let link = root.join("target/tools/capnp/current");
    if link.exists() {
        let _ = fs::remove_dir(&link);
    }
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    // Junction-style directory symlink; requires Developer Mode or admin on older Windows.
    std::os::windows::fs::symlink_dir(prefix, &link)
        .or_else(|_| {
            // Fall back to copying the prefix-relative bin into current/bin.
            let bin_src = prefix.join("bin");
            let bin_dst = link.join("bin");
            fs::create_dir_all(&bin_dst)?;
            for entry in fs::read_dir(&bin_src)? {
                let entry = entry?;
                fs::copy(entry.path(), bin_dst.join(entry.file_name()))?;
            }
            Ok::<_, std::io::Error>(())
        })
        .with_context(|| format!("install current marker at {}", link.display()))?;
    Ok(())
}
