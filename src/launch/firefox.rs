//! Firefox launcher (WebDriver BiDi via `--remote-debugging-port`, Firefox 129+).

use std::fs::File;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use crate::detect::{Engine, Installed};

use super::{
    allocate_free_port, background_after_launch, configure_session_detachment, LaunchOpts,
    LaunchedHandle,
};

pub async fn launch(installed: &Installed, opts: LaunchOpts) -> Result<LaunchedHandle> {
    let port = allocate_free_port().context("allocating BiDi port")?;

    if !opts.profile_dir.exists() {
        std::fs::create_dir_all(&opts.profile_dir)
            .with_context(|| format!("creating profile dir {}", opts.profile_dir.display()))?;
    }

    if let Some(dir) = &opts.download_dir {
        if let Err(e) = super::download_prefs::apply_firefox(&opts.profile_dir, dir) {
            tracing::warn!(error = %e, "writing Firefox download prefs failed");
        }
    }

    let log_path = opts.profile_dir.join("browser.log");
    let log_file =
        File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    let log_clone = log_file
        .try_clone()
        .context("cloning log file handle for stderr")?;

    let mut cmd = Command::new(&installed.executable);
    if let Some(home) = macos_tcc_home_override(&opts.profile_dir, &installed.version) {
        tracing::info!(home = %home.display(), "Firefox app-data is TCC-protected; launching with CFFIXED_USER_HOME");
        cmd.env("CFFIXED_USER_HOME", home);
    }
    cmd.arg("-profile").arg(&opts.profile_dir).arg("-no-remote");
    if opts.headless {
        cmd.arg("-headless");
    }
    cmd.arg("--remote-debugging-port")
        .arg(port.to_string())
        .arg("about:blank");

    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_clone))
        .kill_on_drop(false);
    configure_session_detachment(&mut cmd);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", installed.executable.display()))?;
    let pid = child.id().context("child has no pid")?;

    let endpoint = wait_for_firefox_endpoint(port, &mut child, &log_path).await?;
    if !opts.headless {
        background_after_launch(pid);
    }

    Ok(LaunchedHandle {
        pid,
        port,
        endpoint,
        engine: Engine::Bidi,
        profile_dir: opts.profile_dir,
        child: Some(child),
    })
}

/// Hint shown when Firefox fails the way the macOS 27 TCC issue makes it fail.
const TCC_HINT: &str =
    "\nhint: on macOS 27, Firefox before 158 reads ~/Library/Application Support/Firefox \
     even with -profile, and macOS privacy protection (TCC) denies that folder to apps \
     launched from a terminal or agent host. Update Firefox to 158 or later, or grant the \
     host app Full Disk Access.";

/// Workaround for Firefox < 158 on macOS 27: Gecko resolves the shared
/// app-data root `~/Library/Application Support/Firefox` (profiles.ini /
/// installs.ini bookkeeping) even when `-profile` is given. macOS 27 puts
/// that folder under TCC, so a Firefox whose responsible process is a
/// terminal or agent host without Full Disk Access exits with "Could not
/// find profile folder." (headless) or hangs on a "Profile Missing" modal.
///
/// When this process is denied the same folder (it shares the responsible
/// process with the child) and Firefox is older than 158, point
/// CoreFoundation's notion of home at a browser-control-owned directory
/// next to the profile. The profile and the download folder are explicit
/// paths. Side effects inside that Firefox: file pickers start in the empty
/// fake home and native-messaging hosts under the real
/// `~/Library/Application Support/Mozilla` are not found. Returns `None`
/// (launch unchanged) when the folder is readable or absent, on Firefox
/// 158+, and off macOS.
///
/// Not covered: a machine where the folder does not exist yet but TCC
/// would still deny creating it; such a launch fails with [`TCC_HINT`].
fn macos_tcc_home_override(
    profile_dir: &std::path::Path,
    version: &str,
) -> Option<std::path::PathBuf> {
    if !cfg!(target_os = "macos") || firefox_major(version).is_some_and(|m| m >= 158) {
        return None;
    }
    if !firefox_app_data_denied() {
        return None;
    }
    let name = profile_dir.file_name()?.to_string_lossy().to_string();
    let fake = profile_dir.with_file_name(format!("{name}-home"));
    std::fs::create_dir_all(&fake).ok()?;
    Some(fake)
}

fn firefox_major(version: &str) -> Option<u32> {
    version
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Whether this process is denied the real Firefox app-data folder.
fn firefox_app_data_denied() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let Some(home) = account_home() else {
        return false;
    };
    let app_data = home.join("Library/Application Support/Firefox");
    matches!(
        std::fs::read_dir(&app_data),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied
    )
}

/// The account's home directory from the user database, which is what
/// Firefox resolves; `$HOME` is often rewritten by agent sandboxes.
fn account_home() -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    {
        use std::ffi::CStr;
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: getpwuid_r writes into `pwd`/`buf`, both owned here and
        // sized as passed; `out` is either null or points at `pwd`.
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut pwd,
                buf.as_mut_ptr(),
                buf.len(),
                &mut out,
            )
        };
        if rc == 0 && !out.is_null() && !pwd.pw_dir.is_null() {
            // SAFETY: pw_dir points into `buf`, NUL-terminated by libc.
            let dir = unsafe { CStr::from_ptr(pwd.pw_dir) };
            let path = std::path::PathBuf::from(dir.to_string_lossy().into_owned());
            if path.is_absolute() {
                return Some(path);
            }
        }
    }
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Wait for Firefox to print "WebDriver BiDi listening on ws://..." to its
/// log file (we redirect stderr there because Firefox does not expose a
/// `/json/version` HTTP endpoint).
async fn wait_for_firefox_endpoint(
    port: u16,
    child: &mut tokio::process::Child,
    log_path: &std::path::Path,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut seen = 0usize;

    loop {
        if let Some(status) = child.try_wait().context("polling child status")? {
            let log = std::fs::read_to_string(log_path).unwrap_or_default();
            let hint = if log.contains("Could not find profile folder") {
                TCC_HINT
            } else {
                ""
            };
            bail!(
                "firefox exited before BiDi endpoint was advertised (status: {status}); \
                 log ({}):\n{}{hint}",
                log_path.display(),
                log
            );
        }

        if let Ok(s) = std::fs::read_to_string(log_path) {
            if s.len() > seen {
                for line in s[seen..].lines() {
                    if let Some(url) = parse_bidi_url(line) {
                        return Ok(url);
                    }
                }
                seen = s.len();
            }
        }

        if tokio::time::Instant::now() >= deadline {
            let _ = child.start_kill();
            // A windowed Firefox hit by the TCC issue does not exit: it
            // waits on a "Profile Missing" dialog until this timeout.
            let hint = if firefox_app_data_denied() {
                TCC_HINT
            } else {
                ""
            };
            bail!(
                "timed out waiting for Firefox WebDriver BiDi endpoint on port {port}; \
                 see log at {}{hint}",
                log_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn parse_bidi_url(line: &str) -> Option<String> {
    // Match: "WebDriver BiDi listening on ws://HOST:PORT" (path may be absent;
    // Firefox typically logs without a trailing path. The actual WS endpoint
    // for the browser is `<base>/session`).
    let needle = "WebDriver BiDi listening on ";
    let idx = line.find(needle)?;
    let rest = &line[idx + needle.len()..];
    let url = rest.split_whitespace().next()?.trim();
    if !url.starts_with("ws://") && !url.starts_with("wss://") {
        return None;
    }
    let trimmed = url.trim_end_matches('/');
    Some(format!("{trimmed}/session"))
}

#[cfg(test)]
mod tests {
    use super::{firefox_major, parse_bidi_url};

    #[test]
    fn parses_firefox_major_version() {
        assert_eq!(firefox_major("157.0"), Some(157));
        assert_eq!(firefox_major("158.0.1"), Some(158));
        assert_eq!(firefox_major("159.0b3"), Some(159));
        assert_eq!(firefox_major("fake"), None);
    }

    #[test]
    fn parses_bidi_listening_line() {
        let l = "WebDriver BiDi listening on ws://127.0.0.1:9876";
        assert_eq!(
            parse_bidi_url(l).as_deref(),
            Some("ws://127.0.0.1:9876/session")
        );
    }

    #[test]
    fn ignores_unrelated_lines() {
        assert!(parse_bidi_url("*** You are running in headless mode.").is_none());
        assert!(parse_bidi_url("[GFX1-]: noise").is_none());
    }

    #[test]
    fn handles_trailing_slash() {
        let l = "WebDriver BiDi listening on ws://127.0.0.1:1234/";
        assert_eq!(
            parse_bidi_url(l).as_deref(),
            Some("ws://127.0.0.1:1234/session")
        );
    }
}
