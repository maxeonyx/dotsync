// `dotsync init` and `dotsync create-scope`: what they create, where a joining
// machine hangs, and what every command says on a machine that has not been
// initialized yet.

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
    // init that is every file the scope holds, and a new fleet's scopes hold
    // nothing at all.
    let init_output = machine.init();
    assert_stderr_snapshot(
        &init_output,
        "dotsync: initialized mx-xps-cy and synced 0 file(s)\n",
    );
}

/// Creating a scope is the whole of what can be done to the graph, and the
/// test the graph work has owed since PLAN §2.3 step 1: a run that reports it
/// created a scope means the scope exists and can be used. Declaring one in
/// `config.toml` reported success and created no bookmark, so the scope was
/// unusable and `dotsync view` broke on every machine in the fleet.
///
/// Usable means usable from another machine, which is why this ends on a
/// second machine reading the file: a scope only earns its name by carrying
/// config to the machines under it.
#[test]
fn a_scope_created_on_one_machine_is_usable_from_another() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    machine_a.init_ok();

    machine_a.run_ok("dotsync create-scope hyprland --parent linux -m 'wayland compositor config'");
    machine_a.write_file(".config/hypr/hyprland.conf", "monitor = eDP-1\n");
    machine_a.run_ok("dotsync commit hyprland -m 'seed hyprland' -- .config/hypr/hyprland.conf");

    let machine_b = harness.machine("machine-b", "linux", "goof-b");
    let init_b = machine_b.init_with("--parent hyprland");
    assert!(
        init_b.status.success(),
        "a machine has to be able to join under a scope somebody created\n{}",
        render_output(&init_b)
    );
    assert_eq!(
        machine_b.read_file(".config/hypr/hyprland.conf"),
        "monitor = eDP-1\n",
        "and the config on that scope has to reach it\n{}",
        render_output(&init_b)
    );
}

/// A hostname cannot say whether this machine is a `home-linux` or a
/// `work-linux`, so joining a fleet that already has scopes means naming the
/// one this machine's config hangs off. A name that is not there is a mistake
/// dotsync can see immediately, and the graph is append-only — a machine
/// hung in the wrong place cannot be moved — so this is the last moment the
/// mistake is cheap.
#[test]
fn init_refuses_a_parent_scope_that_does_not_exist() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    machine_a.init_ok();

    let machine_b = harness.machine("machine-b", "linux", "goof-b");
    let joined = machine_b.init_with("--parent hyprland");

    assert_eq!(
        joined.status.code(),
        Some(1),
        "joining under a scope that does not exist has to stop\n{}",
        render_output(&joined)
    );
    let stderr = String::from_utf8_lossy(&joined.stderr).into_owned();
    for expected in ["hyprland", "linux", "create-scope"] {
        assert!(
            stderr.contains(expected),
            "the stop has to say which scopes there are and how to make one; missing {expected:?}\n{stderr}"
        );
    }
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

    let expected = r#"{"conflicts":[],"current_state":["expected repo path: {repo}; standard location: ~/.local/share/dotsync/repo"],"drifts":[],"error":"not_initialized","message":"Dotsync could not find its hidden repo at {repo}. Run `dotsync init <remote-url>` from this home directory first.","status":"error"}
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
