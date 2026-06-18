use std::process::Command;

#[test]
fn top_level_help_explains_scope_and_basic_workflow() {
    let output = Command::new(env!("CARGO_BIN_EXE_dotsync"))
        .arg("--help")
        .output()
        .expect("run dotsync --help");

    assert!(output.status.success(), "{}", render_output(&output));

    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "A scope is a branch in the dotsync DAG",
        "plain `dotsync` syncs your current machine scope into home",
        "dotsync commit <scope> -m \"message\"",
        "Examples:",
    ] {
        assert!(
            stdout.contains(expected),
            "top-level help missing {expected:?}:\n{}",
            stdout
        );
    }
}

#[test]
fn init_help_explains_remote_url_and_setup() {
    let output = Command::new(env!("CARGO_BIN_EXE_dotsync"))
        .args(["init", "--help"])
        .output()
        .expect("run dotsync init --help");

    assert!(output.status.success(), "{}", render_output(&output));

    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "REMOTE_URL is the git remote that stores your dotsync repo",
        "clones the repo into ~/.local/share/dotsync/repo",
        "detects this machine",
        "sets up any missing scope branches",
    ] {
        assert!(
            stdout.contains(expected),
            "init help missing {expected:?}:\n{}",
            stdout
        );
    }
}

#[test]
fn init_help_explains_optional_remote_prompt_without_mode_labels() {
    let output = Command::new(env!("CARGO_BIN_EXE_dotsync"))
        .args(["init", "--help"])
        .output()
        .expect("run dotsync init --help");

    assert!(output.status.success(), "{}", render_output(&output));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("If REMOTE_URL is omitted, dotsync asks for it."),
        "init help should describe the user-facing behavior without implementation-mode labels:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("interactive") && !stdout.contains("non-interactive"),
        "init help should not make users reason about terminal mode:\n{}",
        stdout
    );
}

fn render_output(output: &std::process::Output) -> String {
    format!(
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
