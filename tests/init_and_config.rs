// `dotsync init`: what it creates, the config.toml it writes and preserves
// when a second machine joins, and what every command says on a machine that
// has not been initialized yet.

mod harness;
use harness::*;

#[test]
fn init_creates_no_visible_git_directory() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    assert!(
        !machine.repo_dir.join(".git").exists(),
        "dotsync init should not create a .git directory — agents must not see git and assume they can commit directly"
    );
    assert!(
        machine.repo_dir.join(".jj").exists(),
        "dotsync init should create a .jj directory for internal state"
    );
}

#[test]
fn v03_init_creates_hidden_repo_not_dotfiles() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init_ok();

    assert!(
        machine
            .home_dir
            .join(".local/share/dotsync/repo/.jj")
            .exists(),
        "v0.3 init should create a hidden bare repo under ~/.local/share/dotsync/repo\n{}",
        render_output(&init_output)
    );
    assert!(
        !machine.home_dir.join("dotfiles").exists(),
        "v0.3 init should not create ~/dotfiles\n{}",
        render_output(&init_output)
    );
}

#[test]
fn init_reports_no_drift() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    // A machine with no sync state has no record of putting anything in home,
    // so it cannot claim a file missing from home was deleted there. On a fresh
    // init that is every file the scope holds.
    let init_output = machine.init();
    assert_stderr_snapshot(
        &init_output,
        "dotsync: initialized mx-xps-cy and synced 1 file(s)\n",
    );
}

/// The comments in this file are load-bearing: they are how an agent with no
/// memory of this machine learns that hyprland config goes on `hyprland` and
/// not on `linux`. `init` used to generate the scope list with no comments at
/// all, so the mechanism DESIGN.md and the dotfiles skill both send agents to
/// produced nothing to read.
#[test]
fn init_writes_a_config_whose_comments_teach_scope_choice() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let config = machine.read_file(".config/dotsync/config.toml");
    for expected in [
        "Every scope is a branch",
        "root-est scope",
        "`all` — every machine",
        "`linux` — every machine whose OS is linux",
        "`mx-xps-cy` — only the machine called mx-xps-cy",
    ] {
        assert!(
            config.contains(expected),
            "the generated config must explain the scopes it created; missing {expected:?}:\n{config}"
        );
    }
    assert!(
        config.matches("What belongs here:").count() >= 3,
        "every scope needs somewhere to write what it is for:\n{config}"
    );

    let status = machine.run("dotsync status");
    assert!(
        status.status.success(),
        "and the commented file still has to be the file dotsync reads\n{}",
        render_output(&status)
    );
}

/// A second machine joining adds its own scope to the shared config. It used to
/// re-render that file from the parsed scope graph, which threw away every
/// comment anyone had written — so the load-bearing comments survived exactly
/// until the next machine ran `init`.
#[test]
fn a_machine_joining_keeps_the_comments_already_in_the_config() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");

    machine_a.init_ok();

    let described = machine_a.read_file(".config/dotsync/config.toml").replace(
        "[sync]",
        "# hand-written: hyprland and fish config live on `linux`.\n[sync]",
    );
    machine_a.write_file(".config/dotsync/config.toml", &described);
    machine_a.run_ok("dotsync commit all -m 'describe linux' -- .config/dotsync/config.toml");

    let machine_b = harness.machine("machine-b", "linux", "goof-b");
    machine_b.init_ok();

    let joined = machine_b.read_file(".config/dotsync/config.toml");
    assert!(
        joined.contains("# hand-written: hyprland and fish config live on `linux`."),
        "joining a remote must not throw away what the config says:\n{joined}"
    );
    assert!(
        joined.contains("goof-b = { parents = [\"linux\"] }"),
        "while still adding the joining machine's scope:\n{joined}"
    );
    assert!(
        joined.contains("`goof-b` — only the machine called goof-b"),
        "with the same explanation init writes for a scope it creates:\n{joined}"
    );
}

/// `repo already exists at <path>` was a one-line dead end: it named a
/// directory the agent is told never to touch, and said nothing about what to
/// do with an already-initialized machine.
#[test]
fn init_on_an_initialized_machine_says_what_to_run_instead() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let again = machine.init();
    assert_eq!(again.status.code(), Some(1), "{}", render_output(&again));
    let stderr = String::from_utf8_lossy(&again.stderr).into_owned();
    assert!(
        stderr.contains("already initialized"),
        "the stop has to say what state the machine is in\n{stderr}"
    );
    assert!(
        stderr.contains("run `dotsync`") && stderr.contains("dotsync status"),
        "and what to run instead\n{stderr}"
    );
    assert!(
        stderr.contains("Correct flow:"),
        "laid out like every other teaching error\n{stderr}"
    );
}

#[test]
fn init_without_remote_noninteractive_matches_full_recovery_message() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.run_expecting("dotsync init", 2);

    let stderr = String::from_utf8_lossy(&init_output.stderr);
    let expected = "dotsync: init needs the repo remote URL

Usage:
  dotsync init <remote-url>

The remote URL is the git remote that stores your dotsync repo.

Example:
  dotsync init git@github.com:maxeonyx/dotfiles.git
";
    assert_eq!(stderr, expected, "{}", render_output(&init_output));
}

#[test]
fn status_before_init_matches_full_recovery_message() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let status_output = machine.run_expecting("dotsync status", 1);

    let stderr = String::from_utf8_lossy(&status_output.stderr);
    let expected = format!(
        "dotsync: not initialized

What dotsync does:
Dotsync keeps your config in a hidden repo at ~/.local/share/dotsync/repo and syncs the scopes this machine belongs to into your home directory. Every command works against that repo.

This flow:
This flow opened that repo to find out what this machine's scopes hold.

Expected:
It expects `dotsync init <remote-url>` to have been run in this home directory already, which is what creates the repo.

Current state found:
expected repo path: {}; standard location: ~/.local/share/dotsync/repo

Why dotsync stopped:
There is nothing to compare your home directory against, so dotsync cannot answer for it.

Correct flow:
- run `dotsync init <remote-url>` from this home directory. The remote URL is the git remote that stores your dotsync repo.
- then rerun `dotsync status`.
",
        machine.repo_dir.display()
    );
    assert_eq!(stderr, expected, "{}", render_output(&status_output));
}

#[test]
fn status_before_init_json_matches_recovery_message() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let status_output = machine.run_expecting("dotsync --output json status", 1);

    let expected = r#"{"conflicts":[],"current_state":["expected repo path: {repo}; standard location: ~/.local/share/dotsync/repo"],"drifts":[],"error":"not_initialized","forced_overwrites":[],"message":"Dotsync could not find its hidden repo at {repo}. Run `dotsync init <remote-url>` from this home directory first.","status":"error"}
"#
    .replace("{repo}", &machine.repo_dir.display().to_string());
    let stdout = String::from_utf8_lossy(&status_output.stdout);
    assert_eq!(stdout, expected, "{}", render_output(&status_output));
}

/// "Then rerun `dotsync status`" was the advice whatever you had run, so an
/// agent that ran `dotsync commit` was told to finish by running something
/// else. The message was also the one structured error written in a shape of
/// its own.
#[test]
fn the_not_initialized_stop_names_the_command_you_ran() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let output = machine.run_expecting("dotsync commit all -m 'add bashrc' -- .bashrc", 1);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.contains("rerun `dotsync commit`"),
        "the advice must name the command that was run\n{stderr}"
    );
    assert!(
        !stderr.contains("rerun `dotsync status`"),
        "and must not name a command the agent never ran\n{stderr}"
    );
    for section in [
        "What dotsync does:",
        "This flow:",
        "Expected:",
        "Current state found:",
        "Why dotsync stopped:",
        "Correct flow:",
    ] {
        assert!(
            stderr.contains(section),
            "not-initialized must be laid out like every other teaching error; missing {section:?}\n{stderr}"
        );
    }
}

#[test]
fn missing_home_is_reported_as_an_environment_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let status_output = machine.run_without_home("dotsync status");
    assert_eq!(
        status_output.status.code(),
        Some(1),
        "{}",
        render_output(&status_output)
    );
    assert_stderr_snapshot(
        &status_output,
        "dotsync: HOME is not set, so dotsync cannot find your home directory. Set HOME to the home directory dotsync should manage, then rerun.\n",
    );
}
