// The run's one conversation with the remote: exactly one fetch per run, and
// what every command does when the remote cannot be reached at all.

mod harness;
use harness::*;

/// `view` reports on every scope, and the report is one answer about one
/// moment — so it is one run, and a run fetches once. Fetching per scope also
/// makes `view` write an operation per scope into the repo's op log, which is
/// the opposite of the read-only command DESIGN describes.
#[test]
fn view_reaches_the_remote_once() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();
    add_hyprland_scope(&machine);
    machine.run_ok("dotsync");

    let (view_output, git_calls) = machine.run_recording_git("dotsync view");
    assert!(
        view_output.status.success(),
        "{}",
        render_output(&view_output)
    );

    let fetches = git_calls
        .iter()
        .filter(|call| call.split_whitespace().any(|word| word == "fetch"))
        .count();
    assert_eq!(
        fetches, 1,
        "one `dotsync view` over 4 scopes must fetch once, not once per scope; git was called: {git_calls:?}"
    );
}

/// One fetch per run is a property of having a session, not a habit of the
/// code that happens to hold today. `view` grew an N+1 fetch without anyone
/// noticing because nothing counted, and every command is one refactor away
/// from the same thing.
#[test]
fn status_diff_sync_and_commit_each_reach_the_remote_once() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();
    add_hyprland_scope(&machine);
    machine.run_ok("dotsync");
    machine.write_file(".bashrc", "export DOTSYNC=1\n");

    for command in [
        "dotsync status",
        "dotsync diff",
        "dotsync",
        "dotsync commit all -m 'add bashrc' -- .bashrc",
    ] {
        let (output, fetches) = machine.fetches_during(command);
        assert_eq!(
            fetches,
            1,
            "`{command}` must reach the remote exactly once\n{}",
            render_output(&output)
        );
    }
}

#[test]
fn read_only_commands_report_against_the_last_fetched_state_when_the_remote_is_unreachable() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();
    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    machine.run_ok("dotsync");

    machine.write_file(".bashrc", "export DOTSYNC=edited-here\n");
    harness.disconnect_remote();

    let status_output = machine.run("dotsync status");
    assert_eq!(
        status_output.status.code(),
        Some(0),
        "status must report against the last-fetched state rather than fail\n{}",
        render_output(&status_output)
    );
    let status_stderr = String::from_utf8_lossy(&status_output.stderr).into_owned();
    assert!(
        status_stderr.contains("could not reach the remote"),
        "status must say which state it is reporting against\n{status_stderr}"
    );
    assert!(
        status_stderr.contains(".bashrc"),
        "status must still report the local edit\n{status_stderr}"
    );

    let diff_output = machine.run("dotsync diff");
    assert_eq!(
        diff_output.status.code(),
        Some(1),
        "diff must still answer, and still exit 1 for drift\n{}",
        render_output(&diff_output)
    );
    let diff_stderr = String::from_utf8_lossy(&diff_output.stderr).into_owned();
    assert!(
        diff_stderr.contains("could not reach the remote"),
        "diff must say which state it is reporting against\n{diff_stderr}"
    );
    assert!(
        diff_stderr.contains("export DOTSYNC=edited-here"),
        "diff must still show the drift\n{diff_stderr}"
    );

    let view_output = machine.run("dotsync view");
    assert_eq!(
        view_output.status.code(),
        Some(0),
        "view must still list what is checked in\n{}",
        render_output(&view_output)
    );
    assert!(
        String::from_utf8_lossy(&view_output.stdout).contains(".bashrc"),
        "{}",
        render_output(&view_output)
    );
    assert!(
        String::from_utf8_lossy(&view_output.stderr).contains("could not reach the remote"),
        "{}",
        render_output(&view_output)
    );

    let json_output = machine.run_expecting("dotsync --output json status", 0);
    let json = parse_stdout_json(&json_output);
    assert_eq!(json["status"], "ok");
    assert!(
        json["remote_unreachable"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "the JSON report must carry why the remote was out of reach\n{}",
        render_output(&json_output)
    );
}

#[test]
fn work_done_offline_reaches_the_remote_on_the_next_online_run() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    harness.disconnect_remote();
    machine.write_file(".config/offline.conf", "mode = offline\n");

    let commit_output = machine
        .run("dotsync --output json commit linux -m 'add offline conf' -- .config/offline.conf");
    assert_eq!(
        commit_output.status.code(),
        Some(0),
        "a commit made offline is ordinary local-ahead work, not a failure\n{}",
        render_output(&commit_output)
    );
    let commit_stderr = String::from_utf8_lossy(&commit_output.stderr).into_owned();
    assert!(
        commit_stderr.contains("could not reach the remote"),
        "the commit must say it could not reach the remote\n{commit_stderr}"
    );
    let commit_json = parse_stdout_json(&commit_output);
    let unpushed = commit_json["unpushed_scopes"]
        .as_array()
        .expect("unpushed_scopes should be an array");
    assert!(
        unpushed.iter().any(|scope| scope == "linux"),
        "the commit must report what did not reach the remote\n{}",
        render_output(&commit_output)
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".config/offline.conf"),
        "mode = offline\n",
        "the cascade must land locally even though nothing can be published"
    );

    // Plain `dotsync` offline is the same story: it syncs home from what is
    // already here and leaves the unpublished scopes for the next online run.
    let offline_sync = machine.run("dotsync");
    assert_eq!(
        offline_sync.status.code(),
        Some(0),
        "plain sync must work offline\n{}",
        render_output(&offline_sync)
    );

    harness.reconnect_remote();
    let online_sync = machine.run_expecting("dotsync", 0);
    assert!(
        !String::from_utf8_lossy(&online_sync.stderr).contains("could not reach the remote"),
        "a run that reached the remote must not claim otherwise\n{}",
        render_output(&online_sync)
    );
    for scope in ["linux", "mx-xps-cy"] {
        assert_eq!(
            remote_branch_file_contents(&machine, scope, ".config/offline.conf"),
            "mode = offline\n",
            "the next online run must publish what was committed offline on `{scope}`"
        );
    }
}

/// A run that stops still has to say which state it stopped against. A
/// conflict is the stop a sync can reach, and one of the things it offers is
/// `--force` — overwrite home with what the scope holds — so a reader who is
/// not told the scope snapshot is however old this machine's last fetch was
/// cannot judge that advice.
#[test]
fn a_run_that_stops_offline_still_says_the_remote_was_out_of_reach() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();
    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=one\n");
    machine.run_ok("dotsync");

    // A local edit against an incoming change to the same line: the merge a
    // sync is cannot resolve that. The first stop is the online run, and it is
    // what picked the incoming change up — so the machine goes offline already
    // holding both sides of the collision.
    machine.write_file(".bashrc", "export DOTSYNC=edited\n");
    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=theirs\n");
    machine.run_expecting("dotsync", 1);

    harness.disconnect_remote();

    let sync_output = machine.run("dotsync --output json");
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "the conflict still stops the run\n{}",
        render_output(&sync_output)
    );
    assert!(
        String::from_utf8_lossy(&sync_output.stderr).contains("could not reach the remote"),
        "a stop must say which state it stopped against\n{}",
        render_output(&sync_output)
    );
    assert!(
        parse_stdout_json(&sync_output)["remote_unreachable"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "the error JSON must carry why the remote was out of reach\n{}",
        render_output(&sync_output)
    );

    // Same for a command that stops before it does anything at all: the run
    // still happened, and it still could not see the remote.
    let commit_output = machine.run_expecting(
        "dotsync --output json commit nosuchscope -m 'nope' -- .bashrc",
        1,
    );
    assert!(
        String::from_utf8_lossy(&commit_output.stderr).contains("could not reach the remote"),
        "a refused commit must say it too\n{}",
        render_output(&commit_output)
    );
    assert!(
        parse_stdout_json(&commit_output)["remote_unreachable"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "{}",
        render_output(&commit_output)
    );
}

/// DESIGN.md, "Failure model: no dead ends": every state dotsync can produce
/// must be one dotsync commands alone can recover from. An init that cannot
/// reach the remote is the likeliest failure there is — a typo in the URL, no
/// network yet — and it must not leave behind a repo that the retry refuses to
/// touch.
#[test]
fn an_init_that_could_not_reach_the_remote_can_simply_be_retried() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    harness.disconnect_remote();
    let failed_init = machine.init();
    assert_eq!(
        failed_init.status.code(),
        Some(1),
        "init has nothing to work from when it cannot reach the remote\n{}",
        render_output(&failed_init)
    );
    assert!(
        String::from_utf8_lossy(&failed_init.stderr).contains("could not reach the remote"),
        "init must say what actually went wrong\n{}",
        render_output(&failed_init)
    );

    harness.reconnect_remote();
    let retried_init = machine.init();
    assert!(
        retried_init.status.success(),
        "the remedy for a failed init is running it again\n{}",
        render_output(&retried_init)
    );
    assert!(
        machine.file_exists(".config/dotsync/config.toml"),
        "the retried init must have set this machine up properly"
    );
}
