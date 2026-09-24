// The scope graph, and how a machine joins one: what `init`, `create-scope`
// and `delete-scope` do to it, where a joining machine hangs, and what every
// command says on a machine that has not been initialized yet.

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
/// test the graph work has owed since the rewrite began: a run that reports it
/// created a scope means the scope exists and can be used. Declaring one in
/// `config.toml` reported success and created no bookmark, so the scope was
/// unusable and `dotsync view` broke on every machine in the fleet.
///
/// Usable means usable from another machine, which is why this ends on a
/// machine that had nothing to do with any of it reading the file: a scope
/// only earns its name by carrying config to the machines under it.
///
/// The machine that creates a scope cannot seed it, and that is the two
/// standing decisions meeting: the graph is append-only, so creating
/// `hyprland` moves no existing machine under it, and a commit may only name a
/// scope this machine holds. So a new scope is for the machines that join
/// under it afterwards, and the first of those is what puts config on it.
#[test]
fn a_scope_created_on_one_machine_is_usable_from_another() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    machine_a.init_ok();

    machine_a.run_ok("dotsync create-scope hyprland --parent linux -m 'wayland compositor config'");

    let machine_b = harness.machine("machine-b", "linux", "goof-b");
    let init_b = machine_b.init_with("--parent hyprland");
    assert!(
        init_b.status.success(),
        "a machine has to be able to join under a scope somebody created\n{}",
        render_output(&init_b)
    );
    machine_b.write_file(".config/hypr/hyprland.conf", "monitor = eDP-1\n");
    machine_b.run_ok("dotsync commit hyprland -m 'seed hyprland' -- .config/hypr/hyprland.conf");

    let machine_c = harness.machine("machine-c", "linux", "goof-c");
    let init_c = machine_c.init_with("--parent hyprland");
    assert!(
        init_c.status.success(),
        "and so has the next one\n{}",
        render_output(&init_c)
    );
    assert_eq!(
        machine_c.read_file(".config/hypr/hyprland.conf"),
        "monitor = eDP-1\n",
        "and the config on that scope has to reach it\n{}",
        render_output(&init_c)
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

    let init_output = machine.run_expecting("dotsync init", 1);

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

    let expected = r#"{"conflicts":[],"current_state":["expected repo path: {repo}; standard location: ~/.local/share/dotsync/repo"],"error":"not_initialized","message":"Dotsync could not find its hidden repo at {repo}. Run `dotsync init <remote-url>` from this home directory first.","status":"error"}
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

/// Deleting is the other half of what can happen to the graph, and the half a
/// fleet needs when a machine goes away: the branch goes from the remote, so
/// the scope stops being something every machine has to carry, cascade into
/// and show.
#[test]
fn a_deleted_scope_is_gone_from_the_remote_and_from_the_graph() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);

    machine_a.run_ok("dotsync delete-scope goof-b");

    assert!(
        !remote_branches(&machine_a).contains(&"goof-b".to_string()),
        "the scope's branch has to go from the remote, which is the only copy every machine reads: {:?}",
        remote_branches(&machine_a)
    );
    let view = machine_a.run_ok("dotsync view");
    assert!(
        !String::from_utf8_lossy(&view.stdout).contains("goof-b"),
        "and the scope has to be gone from the graph this machine reads\n{}",
        render_output(&view)
    );
}

/// Nothing tells the other machines. The scope's head on the remote is absent,
/// which is a head position like any other, so the next run each of them makes
/// picks it up the way it picks up a head that moved.
#[test]
fn another_machine_stops_seeing_a_deleted_scope() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);
    let machine_c = harness.machine("machine-c", "linux", "goof-c");
    machine_c.init_ok_under("linux");
    let before = machine_c.run_ok("dotsync view");
    assert!(
        String::from_utf8_lossy(&before.stdout).contains("goof-b"),
        "this test is about a scope the third machine can see to begin with\n{}",
        render_output(&before)
    );

    machine_a.run_ok("dotsync delete-scope goof-b");
    machine_c.run_ok("dotsync");

    let after = machine_c.run_ok("dotsync view");
    assert!(
        !String::from_utf8_lossy(&after.stdout).contains("goof-b"),
        "a machine that has synced since the deletion must not still be carrying the scope\n{}",
        render_output(&after)
    );
}

/// What only that scope held goes with it — nobody else ever had it — and what
/// it merely inherited stays where it is. A deletion that took a shared file
/// down with it would empty out home on machines that have nothing to do with
/// the one that left.
#[test]
fn deleting_a_scope_takes_the_files_only_it_had_and_nothing_else() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);
    machine_b.write_file(".config/vps-tunnel.conf", "port = 2222\n");
    machine_b.run_ok("dotsync commit goof-b -m 'tunnel config' -- .config/vps-tunnel.conf");
    machine_a.run_ok("dotsync");

    let deleted = machine_a.run_ok("dotsync --output json delete-scope goof-b");

    let payload = parse_stdout_json(&deleted);
    assert_eq!(
        payload["files_gone"],
        serde_json::json!([".config/vps-tunnel.conf"]),
        "the run has to say which files went with the scope\n{}",
        render_output(&deleted)
    );
    assert!(
        remote_branch_holds(&machine_a, "all", ".apprc"),
        "and must not take anything off the scopes that own it: {:?}",
        remote_branch_entries(&machine_a, "all")
    );
    assert_eq!(
        machine_a.read_file(".apprc"),
        "ui_theme = dark\nfont = mono\n",
        "deleting somebody else's scope moves nothing this machine syncs from, so home cannot change"
    );
    let status = machine_a.run_ok("dotsync --output json status");
    assert_eq!(
        parse_stdout_json(&status)["changes"],
        serde_json::json!([]),
        "and it must leave this machine with nothing to report\n{}",
        render_output(&status)
    );
}

/// Deleting a scope something hangs off is the reparenting nobody has
/// designed: every machine under it would silently start taking its config
/// from the scopes above, keeping whatever the deleted scope had already
/// merged into its history with no scope left to change it on.
#[test]
fn a_scope_other_scopes_hang_off_cannot_be_deleted() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);

    let refused = machine_a.run_expecting("dotsync delete-scope linux", 1);

    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    for expected in ["linux", "goof-a", "goof-b"] {
        assert!(
            stderr.contains(expected),
            "the stop has to name what hangs off it, because that is the whole reason; missing {expected:?}\n{stderr}"
        );
    }
    assert!(
        remote_branches(&machine_a).contains(&"linux".to_string()),
        "and a refused deletion must change nothing: {:?}",
        remote_branches(&machine_a)
    );
    machine_a.run_ok("dotsync");
}

/// Home is materialized from this machine's own scope, so a machine that
/// deleted it would have nothing left to sync from and no way to undo it.
#[test]
fn a_machine_cannot_delete_the_scope_it_syncs_from() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);

    let refused = machine_a.run_expecting("dotsync delete-scope goof-a", 1);

    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        stderr.contains("goof-a"),
        "the stop has to name the scope\n{stderr}"
    );
    assert!(
        stderr.contains("another machine"),
        "and say where the deletion can happen instead, or it is a dead end\n{stderr}"
    );
    assert!(
        remote_branches(&machine_a).contains(&"goof-a".to_string()),
        "and a refused deletion must change nothing: {:?}",
        remote_branches(&machine_a)
    );
    machine_a.run_ok("dotsync");
}

/// A name that is not a scope is a typo, and the fleet is small enough that
/// the answer is to go and look at it.
#[test]
fn deleting_a_name_that_is_not_a_scope_says_where_to_find_the_ones_that_are() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);
    let before = remote_branches(&machine_a);

    let refused = machine_a.run_expecting("dotsync delete-scope hyprland", 1);

    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        stderr.contains("hyprland") && stderr.contains("dotsync view"),
        "the stop has to name what was asked for and where to see what there is\n{stderr}"
    );
    assert_eq!(
        remote_branches(&machine_a),
        before,
        "and must leave the remote exactly as it was"
    );
}

/// The machine a deletion is about is the one that is not supposed to exist
/// any more. If it turns up anyway it has to be told what happened and how to
/// come back, rather than left with commands that fail.
#[test]
fn the_machine_whose_scope_was_deleted_is_told_how_to_get_one_back() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.run_ok("dotsync delete-scope goof-b");

    let stopped = machine_b.run_expecting("dotsync", 1);
    let stderr = String::from_utf8_lossy(&stopped.stderr).into_owned();
    assert!(
        stderr.contains("dotsync create-scope goof-b"),
        "the machine that lost its scope has to be told how to have one again\n{stderr}"
    );
}

/// A machine that cascaded into a scope and never got to publish it holds a
/// position for a scope the remote no longer has. Publishing that position
/// would put the scope back under everybody's feet, saying nothing, and the
/// deletion would have to be done again — from a machine that may never run
/// again.
#[test]
fn a_machine_holding_an_unpublished_cascade_does_not_bring_a_deleted_scope_back() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    let machine_c = harness.machine("machine-c", "linux", "goof-c");
    machine_c.init_ok_under("linux");
    machine_b.run_ok("dotsync");

    block_remote_pushes(&machine_b);
    machine_b.write_file(".apprc", "ui_theme = dark\n");
    machine_b.run("dotsync commit all -m 'seed apprc' -- .apprc");
    allow_remote_pushes(&machine_b);
    assert_ne!(
        bookmark_revision(&machine_b, "goof-c"),
        remote_branch_revision(&machine_b, "goof-c"),
        "this test needs a machine holding a cascade into `goof-c` that the remote never saw"
    );

    machine_a.run_ok("dotsync delete-scope goof-c");
    machine_b.run_ok("dotsync");

    assert!(
        !remote_branches(&machine_b).contains(&"goof-c".to_string()),
        "the deletion has to win: {:?}",
        remote_branches(&machine_b)
    );
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".apprc"),
        "ui_theme = dark\n",
        "and the work that machine was holding still has to get out"
    );
}
