//! Keeps every machine on the newest release. A fix that sits on one machine
//! while another goes on writing with the old binary is how a bug fixed once
//! keeps landing in the fleet, so before any command dotsync asks its release
//! site for the current version and, when it is newer, replaces itself and
//! runs the command on the new binary.
//!
//! Release builds ask the published site; a debug build asks only when
//! `DOTSYNC_UPDATE_URL` names one, so `cargo run` and the test suite never
//! replace the binary they are running. Not being able to reach the site is
//! the ordinary offline case and says nothing; a newer release that cannot be
//! installed says why, and the command runs on the old binary.
//!
//! `AGENT_TOOLS_UPDATE_FORCE=1` with `AGENT_TOOLS_UPDATE_SOURCE=<file>` is the
//! agent-tools workspace's forced update: install that file, whatever it is.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

const PUBLISHED_SITE: &str = "https://dotsync.maxeonyx.com";
const FORCE_ENV: &str = "AGENT_TOOLS_UPDATE_FORCE";
const SOURCE_ENV: &str = "AGENT_TOOLS_UPDATE_SOURCE";

/// Brings this binary up to date, then, if it was replaced, runs the command
/// on the replacement and exits with its status. Returns when the command
/// should run here.
pub fn update_then_run() {
    let Ok(installed) = std::env::current_exe() else {
        return;
    };
    // What a Windows update set aside last time: a running exe can be
    // renamed but not deleted.
    let _ = std::fs::remove_file(set_aside(&installed));

    let replaced = match update(&installed) {
        Ok(replaced) => replaced,
        Err(reason) => {
            eprintln!(
                "dotsync: {reason}; running {} instead",
                env!("CARGO_PKG_VERSION")
            );
            false
        }
    };
    if !replaced {
        return;
    }
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match Command::new(&installed)
        .args(&args)
        .env_remove(FORCE_ENV)
        .env_remove(SOURCE_ENV)
        .status()
    {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(err) => eprintln!(
            "dotsync: updated, but could not start the new binary ({err}); running {} instead",
            env!("CARGO_PKG_VERSION")
        ),
    }
}

/// Whether this binary was replaced.
fn update(installed: &Path) -> Result<bool, String> {
    if std::env::var_os(FORCE_ENV).is_some_and(|value| value == "1") {
        let source = std::env::var_os(SOURCE_ENV)
            .ok_or_else(|| format!("{FORCE_ENV} is set without {SOURCE_ENV}"))?;
        let staged = staged(installed);
        std::fs::copy(&source, &staged)
            .map_err(|err| format!("could not copy the forced update: {err}"))?;
        install(installed, &staged)?;
        return Ok(true);
    }

    let Some(site) = site() else {
        return Ok(false);
    };
    let Some(asset) = asset() else {
        return Ok(false);
    };
    let Some(latest) = fetch(&format!("{site}/version.json"))
        .ok()
        .and_then(|body| reported_version(&body))
    else {
        return Ok(false);
    };
    if parse(&latest) <= parse(env!("CARGO_PKG_VERSION")) {
        return Ok(false);
    }

    let staged = staged(installed);
    download(&format!("{site}/releases/{asset}"), &staged)
        .map_err(|err| format!("could not download {latest}: {err}"))?;
    make_executable(&staged)?;
    // A download that does not run as the release it claims to be never
    // replaces a binary that works.
    let ran = Command::new(&staged)
        .args(["--version", "--json"])
        .output()
        .ok()
        .and_then(|output| reported_version(&String::from_utf8_lossy(&output.stdout)));
    if ran.as_deref() != Some(latest.as_str()) {
        let _ = std::fs::remove_file(&staged);
        return Err(format!("the download of {latest} did not run as {latest}"));
    }
    install(installed, &staged)?;
    Ok(true)
}

fn site() -> Option<String> {
    match std::env::var("DOTSYNC_UPDATE_URL") {
        Ok(url) => Some(url.trim_end_matches('/').to_owned()),
        Err(_) if cfg!(debug_assertions) => None,
        Err(_) => Some(PUBLISHED_SITE.to_owned()),
    }
}

/// The release asset for this platform, if one is published.
fn asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("dotsync-x86_64-linux"),
        ("windows", "x86_64") => Some("dotsync-x86_64-windows.exe"),
        _ => None,
    }
}

/// `curl`, as `git` is for fetching: on every Linux and on Windows since 10.
fn curl() -> Command {
    let mut command = Command::new("curl");
    command.args([
        "--silent",
        "--show-error",
        "--fail",
        "--location",
        "--connect-timeout",
        "3",
        "--max-time",
        "5",
    ]);
    command
}

fn fetch(url: &str) -> Result<String, String> {
    let output = curl()
        .arg(url)
        .output()
        .map_err(|err| format!("could not run curl: {err}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn download(url: &str, to: &Path) -> Result<(), String> {
    let output = curl()
        .args(["--max-time", "300", "--output"])
        .arg(to)
        .arg(url)
        .output()
        .map_err(|err| format!("could not run curl: {err}"))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(to);
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(())
}

fn reported_version(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(value.get("version")?.as_str()?.to_owned())
}

/// `major.minor.patch` as numbers; anything else sorts below every release,
/// so a version this binary cannot read never triggers an update.
fn parse(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| part.parse().ok())
        .collect::<Option<Vec<u64>>>()
        .unwrap_or_default()
}

fn staged(installed: &Path) -> PathBuf {
    sibling(installed, "new")
}

fn set_aside(installed: &Path) -> PathBuf {
    sibling(installed, "old")
}

fn sibling(installed: &Path, suffix: &str) -> PathBuf {
    let mut name = installed.file_name().unwrap_or_default().to_owned();
    name.push(format!(".{suffix}"));
    installed.with_file_name(name)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|err| format!("could not make the download executable: {err}"))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Puts `staged` where `installed` is. Unix replaces a running binary by
/// renaming over it; Windows can only rename a running exe, so it moves out
/// of the way first.
fn install(installed: &Path, staged: &Path) -> Result<(), String> {
    make_executable(staged)?;
    if cfg!(windows) {
        let aside = set_aside(installed);
        std::fs::rename(installed, &aside)
            .map_err(|err| format!("could not move {} aside: {err}", installed.display()))?;
        if let Err(err) = std::fs::rename(staged, installed) {
            let _ = std::fs::rename(&aside, installed);
            return Err(format!("could not install the update: {err}"));
        }
        return Ok(());
    }
    std::fs::rename(staged, installed).map_err(|err| {
        let _ = std::fs::remove_file(staged);
        format!(
            "could not install the update over {}: {err}",
            installed.display()
        )
    })
}
