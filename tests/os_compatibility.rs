//! A scope's content reaches every machine below it. Content a Windows
//! machine cannot hold — a name Windows cannot spell, a symlink — stays off
//! any scope a Windows machine takes, refused at the write rather than found
//! later on the machine that cannot hold it.

mod harness;
use harness::*;

/// A Linux machine and a Windows one, both under `all`.
fn linux_and_windows(harness: &TestHarness) -> (MachineEnvironment, MachineEnvironment) {
    let linux = harness.machine("linux-box", "linux", "goof-linux");
    linux.init_ok();
    linux.run_ok("dotsync create-scope windows --parent all -m 'windows machines'");
    let windows = harness.machine("windows-box", "windows", "goof-windows");
    windows.init_ok_under("windows");
    linux.run_ok("dotsync");
    (linux, windows)
}

fn assert_refused_for_windows(output: &std::process::Output, path: &str) {
    let text = render_output(output);
    assert!(
        !output.status.success(),
        "the write was not refused\n{text}"
    );
    assert!(
        text.contains("goof-windows") && text.contains(path),
        "the refusal does not name the Windows machine and the path\n{text}"
    );
}

#[test]
#[cfg_attr(not(unix), ignore = "Windows cannot create a file with this name")]
fn a_name_windows_cannot_spell_stays_off_a_scope_a_windows_machine_takes() {
    let harness = TestHarness::new();
    let (linux, _windows) = linux_and_windows(&harness);
    linux.write_file("notes\\today.md", "on Linux this is one file\n");

    let output = linux.run("dotsync commit all -m 'notes' -- 'notes\\today.md'");

    assert_refused_for_windows(&output, "notes\\today.md");
    let shown = linux.run("dotsync show all 'notes\\today.md'");
    assert!(
        !shown.status.success(),
        "`all` holds the file anyway\n{}",
        render_output(&shown)
    );
}

#[test]
#[cfg_attr(not(unix), ignore = "Windows cannot create a file with this name")]
fn moving_a_name_windows_cannot_spell_onto_a_shared_scope_is_refused() {
    let harness = TestHarness::new();
    let (linux, _windows) = linux_and_windows(&harness);
    linux.write_file("cache:lock", "linux only\n");
    linux.run_ok("dotsync commit linux -m 'lock' -- cache:lock");

    let output = linux.run("dotsync move --from linux --to all -m 'share' cache:lock");

    assert_refused_for_windows(&output, "cache:lock");
}

#[test]
#[cfg_attr(not(unix), ignore = "symlink fixtures are unix-only")]
fn a_symlink_stays_off_a_scope_a_windows_machine_takes() {
    let harness = TestHarness::new();
    let (linux, _windows) = linux_and_windows(&harness);
    symlink_at(
        std::path::Path::new(".bashrc"),
        &linux.home_dir.join(".profile"),
    );

    let output = linux.run("dotsync commit all -m 'profile' -- .profile");

    assert_refused_for_windows(&output, ".profile");
}
