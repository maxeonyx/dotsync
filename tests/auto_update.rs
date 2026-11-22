//! Every machine runs the newest release: before any command, dotsync asks
//! its release site for the current version and, when it is newer, replaces
//! itself and runs the command with the new binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A copy of the built dotsync in a folder of its own, as an install is.
fn installed_copy(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let installed = bin.join("dotsync");
    std::fs::copy(env!("CARGO_BIN_EXE_dotsync"), &installed).unwrap();
    installed
}

/// A release that reports `version` and says which binary ran the command.
#[cfg(unix)]
fn fake_release(path: &Path, version: &str) -> Vec<u8> {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        "#!/bin/sh\nif [ \"$1 $2\" = \"--version --json\" ]; then\n  echo '{{\"package\":\"dotsync\",\"binary\":\"dotsync\",\"version\":\"{version}\"}}'\nelse\n  echo \"release {version} ran: $*\"\nfi\n"
    );
    std::fs::write(path, &script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    script.into_bytes()
}

/// A release site laid out as Pages serves it: `version.json` and the
/// binaries under `releases/`.
#[cfg(unix)]
fn release_site(dir: &Path, version: &str) -> (String, Vec<u8>) {
    let site = dir.join("site");
    std::fs::create_dir_all(site.join("releases")).unwrap();
    std::fs::write(
        site.join("version.json"),
        format!("{{\"package\":\"dotsync\",\"binary\":\"dotsync\",\"version\":\"{version}\"}}\n"),
    )
    .unwrap();
    let bytes = fake_release(&site.join("releases/dotsync-x86_64-linux"), version);
    (format!("file://{}", site.display()), bytes)
}

fn run(installed: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    // Never this machine's home: there is a live fleet behind it.
    let home = installed.parent().unwrap().parent().unwrap().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(installed);
    command.args(args).env("HOME", &home);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
#[cfg_attr(not(unix), ignore = "the fake release is a shell script")]
fn a_newer_release_replaces_the_binary_and_runs_the_command() {
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        let installed = installed_copy(dir.path());
        let (url, release) = release_site(dir.path(), "99.0.0");

        let output = run(&installed, &["scopes"], &[("DOTSYNC_UPDATE_URL", &url)]);

        // Compared, not shown: the left side is a whole dotsync binary.
        assert!(
            std::fs::read(&installed).unwrap() == release,
            "the installed binary is not the release\n{}",
            describe(&output)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("release 99.0.0 ran: scopes"),
            "the command did not run on the new binary\n{}",
            describe(&output)
        );
    }
}

#[test]
#[cfg_attr(not(unix), ignore = "the fake release is a shell script")]
fn the_workspace_forced_update_replaces_the_binary_from_a_local_file() {
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        let installed = installed_copy(dir.path());
        let replacement = dir.path().join("replacement");
        let release = fake_release(&replacement, "99.0.0");

        let output = run(
            &installed,
            &["--version"],
            &[
                ("AGENT_TOOLS_UPDATE_FORCE", "1"),
                ("AGENT_TOOLS_UPDATE_SOURCE", replacement.to_str().unwrap()),
            ],
        );

        // Compared, not shown: the left side is a whole dotsync binary.
        assert!(
            std::fs::read(&installed).unwrap() == release,
            "the forced update did not replace the binary\n{}",
            describe(&output)
        );
    }
}
