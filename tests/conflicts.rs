// A paused cascade: how one arises, what home holds while it is paused, how
// `continue` and `abort` end it, and how every other command reports it.

use std::fs;

mod harness;
use harness::*;

#[test]
fn continue_without_pause_returns_clear_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let continue_output = machine.run("dotsync continue");
    assert_eq!(
        continue_output.status.code(),
        Some(1),
        "continue without a paused cascade should return a normal command error\n{}",
        render_output(&continue_output)
    );
    assert_stderr_snapshot(
        &continue_output,
        "dotsync: there is no paused cascade on this machine\n",
    );
}

#[test]
fn concurrent_same_scope_file_edits_require_resolution() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    machine_a.init_ok();
    machine_b.init_ok();
    machine_a.run_ok("dotsync --force");

    // Establish the shared base version first.
    machine_a.write_file(".config/shared.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add shared base' -- .config/shared.conf");

    // Both machines start the conflict scenario from the same synced base.
    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".config/shared.conf"),
        "setting = \"base\"\n"
    );

    machine_b.run_ok("dotsync");
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "setting = \"base\"\n"
    );

    // Divergent local edits start here. B must not sync again before committing.
    machine_a.write_file(".config/shared.conf", "setting = \"all-a\"\n");
    machine_b.write_file(".config/shared.conf", "setting = \"all-b\"\n");
    assert_eq!(
        machine_a.read_file(".config/shared.conf"),
        "setting = \"all-a\"\n"
    );
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "setting = \"all-b\"\n",
        "machine B must make its own local edit before machine A publishes"
    );

    machine_a.run_ok("dotsync commit all -m 'update shared from a' -- .config/shared.conf");
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "setting = \"all-b\"\n",
        "machine B must not sync to machine A's published edit before committing its own edit"
    );

    let conflict =
        machine_b.run("dotsync commit all -m 'update shared from b' -- .config/shared.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "concurrent same-scope edit should require conflict resolution\n{}",
        render_output(&conflict)
    );
    assert_stderr_snapshot(
        &conflict,
        r#"dotsync: cascade paused

What dotsync does:
Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config.

This flow:
This commit flow was merging the scoped change through the scope DAG and reached a branch where the same file had incompatible edits.

Expected:
It expects you to edit the conflicted file in home to the merged contents you want, then run `dotsync continue` to create the merge commit and resume the cascade.

Current state found:
paused scope: all

Why dotsync stopped:
cascade paused at scope `all` with conflicts in .config/shared.conf

Correct flow:
- edit each conflicted file at its real path in home so it holds the merged contents you want; the file has to change, because dotsync reads the resolution back out of it.
- run `dotsync continue` from the same machine to finish cascading and syncing.
- or run `dotsync abort` from the same machine to discard the paused cascade; that reverts the conflicted files in home to this machine's scope state.
- do not run another dotsync commit while the cascade is paused.
"#,
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_b, "all", ".config/shared.conf"),
        "setting = \"all-a\"\n",
        "failed concurrent commit must leave the shared scope at the already-published version"
    );
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "setting = \"all-b\"\n",
        "failed concurrent commit must not overwrite B's unresolved home edit"
    );

    machine_b.write_file(".config/shared.conf", "setting = \"all-a+all-b\"\n");
    machine_b.run_ok("dotsync continue");
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "setting = \"all-a+all-b\"\n"
    );

    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".config/shared.conf"),
        "setting = \"all-a+all-b\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "all", ".config/shared.conf"),
        "setting = \"all-a+all-b\"\n"
    );
}

#[test]
fn concurrent_edits_still_require_resolution_after_an_intervening_status() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".config/shared.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add shared base' -- .config/shared.conf");
    machine_b.run_ok("dotsync");

    // Two genuinely different edits to one file on one scope.
    machine_a.write_file(".config/shared.conf", "setting = \"all-a\"\n");
    machine_b.write_file(".config/shared.conf", "setting = \"all-b\"\n");
    machine_a.run_ok("dotsync commit all -m 'update shared from a' -- .config/shared.conf");

    // The read-only command that the workflow tells B to run first. It must not
    // turn a genuine two-sided conflict into a silent overwrite of A's edit.
    machine_b.run_ok("dotsync status");

    let conflict =
        machine_b.run("dotsync commit all -m 'update shared from b' -- .config/shared.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "a two-sided conflict must still pause after an intervening read-only command\n{}",
        render_output(&conflict)
    );
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/shared.conf"),
        "setting = \"all-a\"\n",
        "machine A's published edit must still be on the remote"
    );
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "setting = \"all-b\"\n",
        "the paused commit must not overwrite B's unresolved home edit"
    );

    machine_b.write_file(".config/shared.conf", "setting = \"all-a+all-b\"\n");
    machine_b.run_ok("dotsync continue");
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/shared.conf"),
        "setting = \"all-a+all-b\"\n"
    );
}

#[test]
fn shared_scope_conflict_pauses_and_continue_applies_resolution_to_machine_homes() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    machine_a.init_ok();
    machine_b.init_ok();
    machine_a.run_ok("dotsync --force");

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add base config' -- .config/app.conf");

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    machine_a.run_ok("dotsync commit linux -m 'customize linux config' -- .config/app.conf");

    machine_b.run_ok("dotsync");
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"linux\"\n"
    );

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "conflicting all-to-linux cascade should pause\n{}",
        render_output(&conflict)
    );
    assert_stderr_snapshot(
        &conflict,
        "\
dotsync: cascade paused

What dotsync does:
Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config.

This flow:
This commit flow was merging the scoped change through the scope DAG and reached a branch where the same file had incompatible edits.

Expected:
It expects you to edit the conflicted file in home to the merged contents you want, then run `dotsync continue` to create the merge commit and resume the cascade.

Current state found:
paused scope: linux

Why dotsync stopped:
cascade paused at scope `linux` with conflicts in .config/app.conf

Correct flow:
- edit each conflicted file at its real path in home so it holds the merged contents you want; the file has to change, because dotsync reads the resolution back out of it.
- run `dotsync continue` from the same machine to finish cascading and syncing.
- or run `dotsync abort` from the same machine to discard the paused cascade; that reverts the conflicted files in home to this machine's scope state.
- do not run another dotsync commit while the cascade is paused.
"
    );

    machine_b.write_file(".config/app.conf", "setting = \"all+linux\"\n");
    machine_b.run_ok("dotsync continue");
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"all+linux\"\n"
    );

    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "all", ".config/app.conf"),
        "setting = \"all\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "linux", ".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "goof-a", ".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "goof-b", ".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
}

#[test]
fn conflict_messages_agree_that_resolving_means_editing_home() {
    let harness = TestHarness::new();
    let (machine, pause) = pause_a_conflict_on_linux(&harness);

    // The pause used to say "keep the desired final contents", which reads as
    // "leaving the file alone is a valid resolution". `continue` refuses that,
    // so the pause has to ask for an edit.
    let pause_stderr = String::from_utf8_lossy(&pause.stderr).into_owned();
    assert!(
        pause_stderr.contains(
            "the file has to change, because dotsync reads the resolution back out of it"
        ),
        "the pause must say the conflicted file has to change\n{}",
        render_output(&pause)
    );

    let refusal = machine.run("dotsync continue");
    let refusal_stderr = String::from_utf8_lossy(&refusal.stderr).into_owned();
    // `dotsync abort` syncs home back to the machine scope, so telling an agent
    // to abort and then commit "the contents you want" hands it the contents
    // abort just destroyed.
    assert!(
        refusal_stderr
            .contains("reverts the conflicted files in home to this machine's scope state"),
        "the refusal must say that abort reverts home\n{}",
        render_output(&refusal)
    );
    assert!(
        refusal_stderr.contains("save them outside home"),
        "the refusal must say to save wanted contents outside home before aborting\n{}",
        render_output(&refusal)
    );
}

#[test]
fn continue_refuses_a_pause_that_predates_the_resolution_check() {
    let harness = TestHarness::new();
    let (machine, _pause) = pause_a_conflict_on_linux(&harness);

    // A machine that upgrades while a cascade is paused holds a pause file
    // written by the older binary, which recorded no pre-pause contents. The
    // resolution check has nothing to compare against there, and skipping it
    // silently reopens the data loss it exists to prevent.
    let pause_path = machine.repo_dir.join(".dotsync-paused-cascade.json");
    let mut pause_state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&pause_path).expect("read pause state"))
            .expect("pause state is JSON");
    let pause_object = pause_state
        .as_object_mut()
        .expect("pause state is an object");
    let recorded = pause_object
        .remove("paused_home_contents")
        .expect("this dotsync records pre-pause contents");
    assert!(
        recorded.as_object().is_some_and(|map| !map.is_empty()),
        "the pause should have recorded contents to remove"
    );
    fs::write(
        &pause_path,
        serde_json::to_string_pretty(&pause_state).expect("serialize pause state"),
    )
    .expect("write pause state");

    let continued = machine.run("dotsync continue");
    assert_eq!(
        continued.status.code(),
        Some(3),
        "continue must refuse a pause it cannot verify, and the cascade is still paused\n{}",
        render_output(&continued)
    );
    let stderr = String::from_utf8_lossy(&continued.stderr).into_owned();
    assert!(
        stderr.contains("run `dotsync abort`"),
        "the refusal must point at the way out\n{}",
        render_output(&continued)
    );

    // The way out has to actually work: abort reads nothing the old pause file
    // lacks.
    machine.run_ok("dotsync abort");
    assert_eq!(
        machine.read_file(".config/app.conf"),
        "setting = \"linux\"\n",
        "abort should have reverted home to this machine's scope state"
    );
}

#[test]
fn continue_refuses_a_conflicted_file_that_was_never_resolved() {
    let harness = TestHarness::new();
    let (machine_b, _pause) = pause_a_conflict_on_linux(&harness);

    // The pause tells the agent to resolve the conflicted file in home, but
    // dotsync never wrote the two conflicting versions there, so the file is
    // exactly as the agent left it. Taking that as the resolution silently
    // deletes the `linux` version, and reports success doing it.
    let continued = machine_b.run("dotsync continue");
    assert_eq!(
        continued.status.code(),
        Some(3),
        "continue must refuse an unresolved conflict, and the cascade is still paused\n{}",
        render_output(&continued)
    );
    assert_stderr_snapshot(
        &continued,
        "\
dotsync: conflict not resolved

What dotsync does:
Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config. Where two branches changed one file differently, the cascade pauses and asks you for the merged contents.

This flow:
This continue flow reads each conflicted file back out of your home directory and records what it finds there as the resolution.

Expected:
It expects those files to have changed since the cascade paused, because the resolution is the contents you write into them.

Current state found:
unchanged since the cascade paused at scope `linux`: .config/app.conf

Why dotsync stopped:
Dotsync does not yet write the two conflicting versions into home, so an unchanged file is not a resolution - it is only the version that happened to already be there. Recording it would silently discard the other scope's version.

Correct flow:
- read the version dotsync would discard with `dotsync view --scope linux --file .config/app.conf`, and compare it against the file in home.
- write the merged contents into the file in home, then run `dotsync continue`.
- `dotsync abort` discards the paused cascade, and reverts the conflicted files in home to this machine's scope state - so anything in home you want to keep must be saved outside home first.
- if home already holds exactly the contents you want: save them outside home, run `dotsync abort`, put them back, commit them to `linux` directly, then redo the original commit.
",
    );

    assert_eq!(
        read_bookmark_file_contents(&machine_b, "linux", ".config/app.conf"),
        "setting = \"linux\"\n",
        "the refused continue must not have discarded the linux version"
    );

    // The guard is not a wedge: a real resolution still finishes the cascade.
    machine_b.write_file(".config/app.conf", "setting = \"all+linux\"\n");
    machine_b.run_ok("dotsync continue");
    assert_eq!(
        read_bookmark_file_contents(&machine_b, "linux", ".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
}

#[test]
fn continue_preserves_non_conflicting_parent_changes_from_paused_merge() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    machine_a.init_ok();
    machine_b.init_ok();
    machine_a.run_ok("dotsync --force");

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.write_file(".config/shared.conf", "shared = \"base\"\n");
    machine_a
        .run_ok("dotsync commit all -m 'add base config' -- .config/app.conf .config/shared.conf");

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    machine_a.run_ok("dotsync commit linux -m 'customize linux config' -- .config/app.conf");

    machine_b.run_ok("dotsync");
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"linux\"\n"
    );
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "shared = \"base\"\n"
    );

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    machine_b.write_file(".config/shared.conf", "shared = \"updated\"\n");
    let conflict = machine_b.run(
        "dotsync commit all -m 'update shared config' -- .config/app.conf .config/shared.conf",
    );
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "conflicting all-to-linux cascade should pause\n{}",
        render_output(&conflict)
    );

    machine_b.write_file(".config/app.conf", "setting = \"all+linux\"\n");
    machine_b.run_ok("dotsync continue");
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "shared = \"updated\"\n"
    );

    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
    assert_eq!(
        machine_a.read_file(".config/shared.conf"),
        "shared = \"updated\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "linux", ".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "linux", ".config/shared.conf"),
        "shared = \"updated\"\n"
    );
}

#[test]
fn continue_json_reports_unpushed_scopes() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    machine_a.init_ok();
    machine_b.init_ok();
    machine_a.run_ok("dotsync --force");

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add base config' -- .config/app.conf");
    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    machine_a.run_ok("dotsync commit linux -m 'customize linux config' -- .config/app.conf");

    machine_b.run_ok("dotsync");
    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "this test needs a paused cascade\n{}",
        render_output(&conflict)
    );

    machine_b.write_file(".config/app.conf", "setting = \"all+linux\"\n");
    block_remote_pushes(&machine_b);
    let continued = machine_b.run_ok("dotsync --output json continue");
    allow_remote_pushes(&machine_b);
    assert_ne!(
        bookmark_revision(&machine_b, "all"),
        remote_branch_revision(&machine_b, "all"),
        "this test needs a push that really was rejected"
    );

    let json = parse_stdout_json(&continued);
    let unpushed = json["unpushed_scopes"]
        .as_array()
        .expect("continue should report unpushed_scopes like every other publishing command")
        .iter()
        .map(|scope| {
            scope
                .as_str()
                .expect("scope should be a string")
                .to_string()
        })
        .collect::<Vec<_>>();
    for scope in ["all", "linux"] {
        assert!(
            unpushed.contains(&scope.to_string()),
            "`{scope}` was cascaded and refused, so it must be reported unpushed: {}",
            render_output(&continued)
        );
    }
}

#[test]
fn commit_while_cascade_paused_is_blocked_without_mutating_scope() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    machine_a.init_ok();
    machine_b.init_ok();
    machine_a.run_ok("dotsync --force");

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add base config' -- .config/app.conf");

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    machine_a.run_ok("dotsync commit linux -m 'customize linux config' -- .config/app.conf");

    machine_b.run_ok("dotsync");

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "conflicting all-to-linux cascade should pause\n{}",
        render_output(&conflict)
    );

    let machine_scope_revision_before = bookmark_revision(&machine_b, "goof-b");
    machine_b.write_file(".config/other.conf", "other = true\n");

    let blocked =
        machine_b.run("dotsync commit goof-b -m 'try commit while paused' -- .config/other.conf");

    assert_eq!(
        blocked.status.code(),
        Some(3),
        "commit while a cascade is paused should be blocked, with the code that means a pause is waiting\n{}",
        render_output(&blocked)
    );
    assert_stderr_snapshot(
        &blocked,
        "\
dotsync: paused cascade in progress

What dotsync does:
Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config.

This flow:
This commit flow was about to start a new scoped commit, but a previous cascade is still paused for conflict resolution.

Expected:
It expects exactly one cascade to be active at a time so commit history, conflict resolution, and home sync state stay aligned.

Current state found:
paused scope: linux

Why dotsync stopped:
Dotsync stopped before fetching, committing, or syncing because starting another commit would hide the real paused-cascade task and may mutate unrelated scope state.

Correct flow:
- edit each conflicted file at its real path in home so it holds the merged contents you want; the file has to change, because dotsync reads the resolution back out of it.
- run `dotsync continue` to finish the paused cascade.
- or run `dotsync abort` to discard the paused cascade; that reverts the conflicted files in home to this machine's scope state.
- after `dotsync continue` succeeds, rerun the new commit if it is still needed.
"
    );
    assert_eq!(
        bookmark_revision(&machine_b, "goof-b"),
        machine_scope_revision_before,
        "blocked commit must not mutate the target scope"
    );
}

#[test]
fn paused_cascade_withholds_publishing_until_it_is_resolved() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    machine_a.init_ok();
    machine_b.init_ok();
    machine_a.run_ok("dotsync --force");

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add base config' -- .config/app.conf");
    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    machine_a.run_ok("dotsync commit linux -m 'customize linux config' -- .config/app.conf");

    machine_b.run_ok("dotsync");
    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "this test needs a paused cascade\n{}",
        render_output(&conflict)
    );

    // Put home back to the machine scope's content so the sync below has no
    // drift to stop on: this test is about publishing, not about drift.
    machine_b.write_file(".config/app.conf", "setting = \"linux\"\n");
    let remote_before =
        ["all", "linux", "goof-a", "goof-b"].map(|scope| remote_branch_revision(&machine_b, scope));

    let sync_output = machine_b.run("dotsync --output json");
    assert!(
        sync_output.status.success(),
        "a paused cascade must not stop dotsync from running: {}",
        render_output(&sync_output)
    );

    for (scope, before) in ["all", "linux", "goof-a", "goof-b"]
        .iter()
        .zip(remote_before)
    {
        assert_eq!(
            remote_branch_revision(&machine_b, scope),
            before,
            "a half-cascaded `{scope}` must not be published while the cascade is paused — `dotsync abort` could not take it back"
        );
    }

    let json = parse_stdout_json(&sync_output);
    let unpushed = json["unpushed_scopes"]
        .as_array()
        .expect("unpushed_scopes should be an array")
        .iter()
        .map(|scope| {
            scope
                .as_str()
                .expect("scope should be a string")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert!(
        unpushed.contains(&"all".to_string()),
        "the withheld scopes must be reported: {}",
        render_output(&sync_output)
    );

    let stderr = String::from_utf8_lossy(&sync_output.stderr);
    assert!(
        stderr.to_lowercase().contains("paused"),
        "the run must say why it did not publish: {}",
        render_output(&sync_output)
    );
    assert!(
        stderr.contains("dotsync continue") && stderr.contains("dotsync abort"),
        "the run must say how to unblock publishing: {}",
        render_output(&sync_output)
    );
}

#[test]
fn abort_paused_cascade_restores_pre_pause_state_and_clears_pause() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    machine_a.init_ok();
    machine_b.init_ok();
    machine_a.run_ok("dotsync --force");

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add base config' -- .config/app.conf");

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    machine_a.run_ok("dotsync commit linux -m 'customize linux config' -- .config/app.conf");

    machine_b.run_ok("dotsync");
    let all_before_pause = bookmark_revision(&machine_b, "all");
    let linux_before_pause = bookmark_revision(&machine_b, "linux");
    let machine_before_pause = bookmark_revision(&machine_b, "goof-b");

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "conflicting all-to-linux cascade should pause\n{}",
        render_output(&conflict)
    );

    let aborted = machine_b.run_ok("dotsync abort");
    // Abort reverts home, so it says what it reverted: the edit that started
    // the cascade is gone, and that is the point of the command.
    assert_stderr_snapshot(
        &aborted,
        "\
dotsync: overwrote 1 drifted file(s)
  M .config/app.conf (edited here since the last sync)
--- repo
+++ system
@@ -1 +1 @@
-setting = \"linux\"
+setting = \"all\"
dotsync: aborted the cascade paused at linux and synced 2 file(s)
",
    );

    assert_eq!(bookmark_revision(&machine_b, "all"), all_before_pause);
    assert_eq!(bookmark_revision(&machine_b, "linux"), linux_before_pause);
    assert_eq!(
        bookmark_revision(&machine_b, "goof-b"),
        machine_before_pause
    );
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"linux\"\n"
    );

    let status = machine_b.run_ok("dotsync status");
    assert_stderr_snapshot(&status, "dotsync: no changes for goof-b\n");

    machine_b.write_file(".config/other.conf", "other = true\n");
    machine_b.run_ok("dotsync commit goof-b -m 'commit after abort' -- .config/other.conf");
}

#[test]
fn abort_paused_cascade_restores_non_conflicting_selected_paths() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    machine_a.init_ok();
    machine_b.init_ok();
    machine_a.run_ok("dotsync --force");

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.write_file(".config/other.conf", "other = false\n");
    machine_a
        .run_ok("dotsync commit all -m 'add base config' -- .config/app.conf .config/other.conf");

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    machine_a.run_ok("dotsync commit linux -m 'customize linux config' -- .config/app.conf");

    machine_b.run_ok("dotsync");

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    machine_b.write_file(".config/other.conf", "other = true\n");
    let conflict = machine_b
        .run("dotsync commit all -m 'update shared config' -- .config/app.conf .config/other.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "conflicting all-to-linux cascade should pause\n{}",
        render_output(&conflict)
    );

    machine_b.run_ok("dotsync abort");

    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"linux\"\n"
    );
    assert_eq!(machine_b.read_file(".config/other.conf"), "other = false\n");

    let status = machine_b.run_ok("dotsync status");
    assert_stderr_snapshot(&status, "dotsync: no changes for goof-b\n");
}

#[test]
fn abort_restores_a_drifted_file_outside_the_paused_selection() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.write_file(".config/unrelated.conf", "unrelated = \"base\"\n");
    machine_a.run_ok(
        "dotsync commit all -m 'add base config' -- .config/app.conf .config/unrelated.conf",
    );

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    machine_a.run_ok("dotsync commit linux -m 'customize linux config' -- .config/app.conf");

    machine_b.run_ok("dotsync");

    // Drift on a file the paused commit never named.
    machine_b.write_file(".config/unrelated.conf", "unrelated = \"drifted\"\n");
    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "conflicting all-to-linux cascade should pause\n{}",
        render_output(&conflict)
    );

    let aborted = machine_b.run("dotsync abort");
    assert!(
        aborted.status.success(),
        "abort reverts all the config files, so unrelated drift cannot block it\n{}",
        render_output(&aborted)
    );
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"linux\"\n"
    );
    assert_eq!(
        machine_b.read_file(".config/unrelated.conf"),
        "unrelated = \"base\"\n",
        "abort is a full sync of home, not a selective restore"
    );
}

/// A paused cascade is one state with one remedy, so it is one exit code
/// wherever a command meets it. It used to be 3 for the run that created the
/// pause and 1 for the next run that ran into it, which teaches an agent that
/// its own next command is a different kind of failure.
#[test]
fn a_paused_cascade_is_the_same_answer_whichever_command_meets_it() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add base config' -- .config/app.conf");
    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    machine_a.run_ok("dotsync commit linux -m 'customize linux config' -- .config/app.conf");

    machine_b.run_ok("dotsync");
    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "the run that pauses reports the pause\n{}",
        render_output(&conflict)
    );

    machine_b.write_file(".config/other.conf", "other = true\n");
    let blocked = machine_b.run("dotsync commit goof-b -m 'try another' -- .config/other.conf");
    assert_eq!(
        blocked.status.code(),
        Some(3),
        "and so does the next run that meets it\n{}",
        render_output(&blocked)
    );

    let unresolved = machine_b.run("dotsync continue");
    assert_eq!(
        unresolved.status.code(),
        Some(3),
        "a continue that finds nothing resolved is still the same paused cascade\n{}",
        render_output(&unresolved)
    );

    let aborted = machine_b.run_ok("dotsync --output json abort");
    let json = parse_stdout_json(&aborted);
    assert_eq!(
        json["paused_scope"],
        "linux",
        "abort names the scope the cascade was paused at, and says which scope that is\n{}",
        render_output(&aborted)
    );
    assert!(
        json.get("aborted_scope").is_none(),
        "the scope was paused, not aborted; the cascade was aborted\n{}",
        render_output(&aborted)
    );
    assert!(
        json.get("scope").is_none(),
        "one value, one field\n{}",
        render_output(&aborted)
    );
}

/// `dotsync status` is the reflex diagnostic. On a machine where a cascade is
/// paused — which is a machine that cannot commit anything at all — it
/// reported "no changes", and nothing anywhere else said otherwise once the
/// original pause message had scrolled away.
#[test]
fn status_and_diff_say_a_cascade_is_paused() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add base' -- .config/app.conf");
    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    machine_a.run_ok("dotsync commit linux -m 'linux flavour' -- .config/app.conf");

    machine_b.run_ok("dotsync");
    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    machine_b.run_expecting(
        "dotsync commit all -m 'shared change' -- .config/app.conf",
        3,
    );

    // Home now holds exactly what the pause left there, so both commands would
    // otherwise report a clean machine.
    machine_b.write_file(".config/app.conf", "setting = \"linux\"\n");

    for command in ["dotsync status", "dotsync diff"] {
        let output = machine_b.run(command);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("paused") && stderr.contains("linux"),
            "`{command}` must say a cascade is paused, and where\n{stderr}"
        );
        assert!(
            stderr.contains("dotsync continue") || stderr.contains("dotsync abort"),
            "`{command}` must point at the way out\n{stderr}"
        );
    }

    for command in ["dotsync --output json status", "dotsync --output json diff"] {
        let output = machine_b.run(command);
        assert_eq!(
            parse_stdout_json(&output)["paused_cascade"],
            "linux",
            "`{command}` must report the pause in the payload too\n{}",
            render_output(&output)
        );
    }
}

/// `view` is the third read-only diagnostic, and the only one that stayed
/// silent about a paused cascade — so an agent that reached for the command
/// whose whole job is orientation was the one agent not told that this machine
/// cannot commit anything.
#[test]
fn view_says_a_cascade_is_paused() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add base' -- .config/app.conf");
    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    machine_a.run_ok("dotsync commit linux -m 'linux flavour' -- .config/app.conf");

    machine_b.run_ok("dotsync");
    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    machine_b.run_expecting(
        "dotsync commit all -m 'shared change' -- .config/app.conf",
        3,
    );

    // Every shape `view` answers in, because the pause is true of the machine
    // rather than of the question asked.
    for command in [
        "dotsync view",
        "dotsync view --scope all",
        "dotsync view --file .config/app.conf",
    ] {
        let output = machine_b.run_ok(command);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("paused") && stderr.contains("linux"),
            "`{command}` must say a cascade is paused, and where\n{stderr}"
        );

        let json_output = machine_b.run(&format!("{command} --output json"));
        assert_eq!(
            parse_stdout_json(&json_output)["paused_cascade"],
            "linux",
            "`{command}` must report the pause in the payload too\n{}",
            render_output(&json_output)
        );
    }
}

/// Plain `dotsync` is the command an agent runs by reflex, and on a machine
/// with a paused cascade it is the one still handing out two answers that are
/// both wrong. Reproduced on v0.3.18 and again while writing this: with the
/// cascade paused at `linux` and the conflicted file sitting in home as the
/// resolution buffer, the run exits 1 with `drift_detected` and offers
/// `dotsync --force` — which overwrites the in-flight resolution, verified by
/// hand — and "run `dotsync status`, then commit the intended path", which is
/// refused with exit 3 precisely because a cascade is paused.
///
/// Neither channel carries `paused_cascade`, though `status`, `diff` and
/// `view` all gained it in the Wave 3 review round.
///
/// The exit code is deliberately not pinned to a number. Whether a sync that
/// meets a pause stops with 1 or with the state's own 3 is a question item 3
/// answers; that it stops, says why, and stops advising a refused remedy is
/// decided now.
#[test]
fn the_drift_stop_says_a_cascade_is_paused_rather_than_advising_a_refused_commit() {
    let harness = TestHarness::new();
    let (machine, _pause) = pause_a_conflict_on_linux(&harness);

    let sync = machine.run("dotsync");
    assert_ne!(
        sync.status.code(),
        Some(0),
        "the conflicted file in home is not something to sync over\n{}",
        render_output(&sync)
    );

    let stderr = String::from_utf8_lossy(&sync.stderr).into_owned();
    assert!(
        stderr
            .lines()
            .any(|line| line.contains("paused") && line.contains("linux")),
        "the stop has to say a cascade is paused, and where\n{stderr}"
    );

    let json = machine.run("dotsync --output json");
    assert_eq!(
        parse_stdout_json(&json)["paused_cascade"],
        "linux",
        "and the payload has to carry it too, like `status`, `diff` and `view` do\n{}",
        render_output(&json)
    );

    let advice = stderr
        .split_once("Correct flow:")
        .expect("the stop teaches a correct flow")
        .1;
    assert!(
        advice.contains("dotsync continue") && advice.contains("dotsync abort"),
        "the only two remedies this state has are the ones it must name\n{advice}"
    );
    assert!(
        !advice.contains("dotsync commit"),
        "a commit is refused outright while a cascade is paused, so advising one is a dead end\n{advice}"
    );
    for invocation in quoted_dotsync_invocations(advice) {
        assert!(
            !invocation.contains("--force"),
            "`{invocation}` would overwrite the conflicted file in home, which is where the resolution is being written\n{advice}"
        );
    }
}

/// While a cascade is paused, the conflicted file in home *is* the resolution
/// buffer — DESIGN: "While markers are materialized, drift detection treats
/// them as the expected home content." So there is nothing there for `--force`
/// to overwrite, and the half-written resolution is the one thing in home that
/// cannot be reconstructed from anywhere else.
///
/// Today it is overwritten. Confirmed by hand at the byte level: with the
/// cascade paused and a resolution part-written, `dotsync --force` reported
/// `overwrote 1 drifted file(s)`, replaced the buffer's contents with the
/// machine scope's version, and exited 0 — while the same run correctly
/// withheld publishing because a cascade was paused. Data loss with a
/// contradictory message.
///
/// The exit code is deliberately not pinned. Whether this run stops with the
/// state's own 3, or syncs everything else and exits 0, is item 3's question;
/// that it does not destroy the resolution is decided now.
#[test]
fn forcing_a_sync_does_not_overwrite_a_conflict_resolution_in_progress() {
    let harness = TestHarness::new();
    let (machine, _pause) = pause_a_conflict_on_linux(&harness);

    // The agent is part-way through resolving: the markers are gone and the
    // resolution is written, but `dotsync continue` has not run yet.
    machine.write_file(".config/app.conf", "setting = \"all+linux\"\n");

    let forced = machine.run("dotsync --force --output json");
    assert_eq!(
        machine.read_file(".config/app.conf"),
        "setting = \"all+linux\"\n",
        "`--force` overwrote the resolution being written into the conflicted file, which exists nowhere else\n{}",
        render_output(&forced)
    );
    assert!(
        !parse_stdout_json(&forced)["overwritten_files"]
            .as_array()
            .is_some_and(|files| files.iter().any(|file| file == ".config/app.conf")),
        "and it must not claim to have\n{}",
        render_output(&forced)
    );

    // The resolution is still there to be finished with, which is the point:
    // a run that meets a pause leaves the machine able to get out of it.
    machine.run_ok("dotsync continue");
    assert_eq!(
        remote_branch_file_contents(&machine, "linux", ".config/app.conf"),
        "setting = \"all+linux\"\n",
        "the resolution the agent wrote is the one that got published"
    );
}

/// K2, and the reason PLAN item 3 opens with "read this first": resolving a
/// conflict on a scope this machine is not on is broken end to end, and the
/// run reports two contradictory things at once.
///
/// Driven by hand against this build before the test was written. Machine B
/// pauses cascading into `goof-a` — machine A's leaf scope, which B does not
/// descend from — writes the resolution into `.config/app.conf` in home, and
/// runs `dotsync continue`. The continue *works*: it records the merge,
/// finishes the cascade, and pushes, so the remote `goof-a` really does hold
/// `setting = "goof-a+all"` afterwards. Then it exits 1 with `drift_detected`,
/// because the resolution sitting in home is a change against `goof-b`, which
/// is not a scope that holds it. The headline says the run failed; the outcome
/// is that it worked and published.
///
/// And the machine stays that way. `status` reports `.config/app.conf` as
/// changed for ever, because no scope this machine syncs from holds the
/// resolution — the only way out is `dotsync --force`, which is a run that
/// destroys something being offered as the remedy for a run that succeeded.
///
/// DESIGN answers it in "Conflict resolution in home": for a conflict outside
/// this machine's ancestry "the affected home path serves as a temporary
/// resolution buffer — with the mode-switch stated loudly: 'this file
/// temporarily contains the conflicted merge for scope `windows`; it is not
/// your machine's config; after `continue` or `abort` it will be restored to
/// your machine's version.'" So `continue` owes home the restoration, and the
/// run that does its whole job owes an exit code that says so.
#[test]
fn continue_after_a_pause_on_another_machines_scope_restores_this_machines_config() {
    let harness = TestHarness::new();
    let (machine_a, machine_b, _pause) = pause_a_conflict_on(&harness, "goof-a");

    // The resolution machine B is being asked for is machine A's config: the
    // merge of what `all` now says with what `goof-a` said.
    machine_b.write_file(".config/app.conf", "setting = \"goof-a+all\"\n");
    let continued = machine_b.run("dotsync continue");
    assert_eq!(
        continued.status.code(),
        Some(0),
        "the continue did its whole job — merge, cascade, push — and then reported failure\n{}",
        render_output(&continued)
    );

    // It really did publish, so the exit code above is the only thing that was
    // wrong about the run.
    assert_eq!(
        remote_branch_file_contents(&machine_b, "goof-a", ".config/app.conf"),
        "setting = \"goof-a+all\"\n",
        "the resolution has to reach the scope it was for"
    );

    // Home is machine B's config again. It was borrowed as a resolution buffer
    // for another machine's branch, and the borrowing ends here.
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"all\"\n",
        "home still holds another machine's config after the cascade finished\n{}",
        render_output(&continued)
    );

    // The wedge: with the resolution left in home there is no scope this
    // machine syncs from that holds it, so it reads as a local change no
    // amount of syncing can settle.
    let status = machine_b.run_expecting("dotsync status --output json", 0);
    let payload = parse_stdout_json(&status);
    assert_eq!(
        payload["changes"].as_array().map(Vec::len),
        Some(0),
        "a finished cascade must not leave the machine permanently changed\n{}",
        render_output(&status)
    );
    assert!(
        payload.get("paused_cascade").is_none(),
        "and it must not still be paused\n{}",
        render_output(&status)
    );
    let diff = machine_b.run("dotsync diff");
    assert_eq!(
        diff.status.code(),
        Some(0),
        "`diff` exits 1 on drift, and there is no drift here\n{}",
        render_output(&diff)
    );

    // The machine the resolution was for picks it up as an ordinary sync.
    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".config/app.conf"),
        "setting = \"goof-a+all\"\n"
    );
}

/// The other half of the mode switch: the pause has to say whose config is
/// now sitting in the file, before the agent starts editing it.
///
/// DESIGN wants it "stated loudly": "this file temporarily contains the
/// conflicted merge for scope `windows`; it is not your machine's config;
/// after `continue` or `abort` it will be restored to your machine's version."
/// Today the pause for `goof-a` is word-for-word the pause for `linux` apart
/// from the scope name, so an agent reading it has no way to tell that the
/// contents it is about to write are not its own machine's.
///
/// The wording is the implementer's. What is asserted is the one fact that
/// cannot be stated without saying it: the message distinguishes the scope
/// being resolved from this machine's own scope, which means naming both.
/// There is no in-ancestry counterpart because there is no mode to switch:
/// when the conflict is on this machine's own path, the resolution *is* its
/// config, which is what every existing conflict test already exercises.
#[test]
fn a_pause_on_another_machines_scope_says_whose_config_is_in_the_file() {
    let harness = TestHarness::new();
    let (_machine_a, _machine_b, pause) = pause_a_conflict_on(&harness, "goof-a");

    let stderr = String::from_utf8_lossy(&pause.stderr).into_owned();
    assert!(
        stderr.contains("goof-a"),
        "the pause has to name the scope being resolved\n{}",
        render_output(&pause)
    );
    assert!(
        stderr.contains("goof-b"),
        "and it has to name this machine's own scope, because the whole point is that the file no longer holds it\n{}",
        render_output(&pause)
    );
}

/// DL-2's root cause. "Resolved" has to be a property of the *content* — no
/// conflict markers left in the file — and not of having run `continue`.
///
/// Today's interim guard asks a different question: did the file change since
/// the pause? A file the agent edited and left markers in answers yes, so
/// `continue` takes it as the resolution. Driven by hand against this build:
/// the marker text below was recorded on `linux`, cascaded into `goof-a` and
/// `goof-b`, pushed to the remote, and reported as `resumed cascade and synced
/// 3 file(s)` with exit 0. Every other machine then syncs a file full of
/// `<<<<<<<` into its live config.
///
/// DESIGN: "`continue` verifies the markers are gone", and "Refuses if markers
/// remain". Exit 3 because the cascade is still paused afterwards — 3 is a
/// property of the state, not of the command that met it.
#[test]
fn continue_refuses_a_resolution_that_still_holds_conflict_markers() {
    let harness = TestHarness::new();
    let (machine, _pause) = pause_a_conflict_on_linux(&harness);

    // A half-done resolution: the agent picked one side of the file and never
    // took the markers out. The trailing line is there so the file is a change
    // against whatever the pause materialized into it, in both the world where
    // that is markers and the world where it is not.
    machine.write_file(
        ".config/app.conf",
        "<<<<<<< all\nsetting = \"all\"\n||||||| base\nsetting = \"base\"\n=======\nsetting = \"linux\"\n>>>>>>> linux\n# half-done\n",
    );
    let continued = machine.run("dotsync continue");
    assert!(
        !read_bookmark_file_contents(&machine, "linux", ".config/app.conf").contains("<<<<<<<"),
        "conflict markers were recorded as the merged contents\n{}",
        render_output(&continued)
    );
    assert!(
        !remote_branch_file_contents(&machine, "linux", ".config/app.conf").contains("<<<<<<<"),
        "and they reached the remote, where every other machine syncs from"
    );
    assert_eq!(
        continued.status.code(),
        Some(3),
        "a file that still holds conflict markers is not a resolution, and the cascade is still paused after refusing it\n{}",
        render_output(&continued)
    );

    // Refusing is not a wedge: a real resolution still finishes the cascade.
    machine.write_file(".config/app.conf", "setting = \"all+linux\"\n");
    machine.run_ok("dotsync continue");
    assert_eq!(
        remote_branch_file_contents(&machine, "linux", ".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
}

/// The same requirement on the other shape, where getting it wrong is worse:
/// the markers are recorded on *another machine's* branch, so the machine that
/// pays for it is not the one that made the mistake and cannot see what
/// happened. Driven by hand against this build: `goof-a` on the remote ended
/// up holding the marker text, and machine A's next plain `dotsync` writes it
/// into its live config.
#[test]
fn continue_refuses_conflict_markers_left_in_another_machines_scope() {
    let harness = TestHarness::new();
    let (machine_a, machine_b, _pause) = pause_a_conflict_on(&harness, "goof-a");

    machine_b.write_file(
        ".config/app.conf",
        "<<<<<<< all\nsetting = \"all\"\n||||||| base\nsetting = \"base\"\n=======\nsetting = \"goof-a\"\n>>>>>>> goof-a\n# half-done\n",
    );
    let continued = machine_b.run("dotsync continue");
    assert!(
        !remote_branch_file_contents(&machine_b, "goof-a", ".config/app.conf").contains("<<<<<<<"),
        "conflict markers were published onto another machine's branch\n{}",
        render_output(&continued)
    );
    // Asserted after the contents, because the exit code alone cannot tell
    // this apart from K2: today's continue exits 1 here whatever it recorded.
    assert_eq!(
        continued.status.code(),
        Some(3),
        "a file that still holds conflict markers is not a resolution, and the cascade is still paused after refusing it\n{}",
        render_output(&continued)
    );

    // And the machine that would have received them is untouched.
    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".config/app.conf"),
        "setting = \"goof-a\"\n",
        "the other machine's config must not have changed at all"
    );

    // Refusing is not a wedge here either. The exit code of this continue is
    // not asserted: that a continue outside this machine's ancestry exits 1
    // after succeeding is K2, pinned separately above.
    machine_b.write_file(".config/app.conf", "setting = \"goof-a+all\"\n");
    machine_b.run("dotsync continue");
    assert_eq!(
        remote_branch_file_contents(&machine_b, "goof-a", ".config/app.conf"),
        "setting = \"goof-a+all\"\n",
        "a real resolution still gets published"
    );
}

/// DESIGN: "The cascade never pauses structurally — every convergence pass
/// completes in one atomic transaction, writing every merge commit, conflicted
/// or not", and "Conflicted heads are not pushed. They stay local-ahead ...
/// until resolved; everything non-conflicted still pushes."
///
/// Today neither half happens: the cascade stops at the conflict, and the
/// `WithheldPausedCascade` guard withholds *everything*, including `all`,
/// which is not conflicted and whose commit is the entire reason the run
/// existed. So a pause strands committed history unpushed — the exact shape of
/// the 2026-07-27 wedge, and what design principle 5 ("a drift stop must not
/// strand unpushed commits") exists to prevent.
///
/// PLAN item 3 wants the eligibility check inside `push_scope_updates` "so
/// every call site is covered by construction". A black-box test cannot see
/// where the check lives, so it asks the next best thing: a second, separate
/// run — a plain `dotsync`, a different call site — has to give the same
/// answer.
#[test]
fn a_pause_publishes_the_scopes_it_did_not_conflict_on() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    machine_a.init_ok();
    machine_b.init_ok();
    machine_a.run_ok("dotsync --force");

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add base config' -- .config/app.conf");
    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    machine_a.run_ok("dotsync commit linux -m 'customize linux config' -- .config/app.conf");

    machine_b.run_ok("dotsync");

    // Recorded before the pause so the assertions below are about what this
    // run published, not about what the remote happened to hold.
    let conflicted_scopes = ["linux", "goof-a", "goof-b"];
    let before = conflicted_scopes.map(|scope| remote_branch_revision(&machine_b, scope));

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let pause = machine_b.run_expecting(
        "dotsync commit all -m 'update shared config' -- .config/app.conf",
        3,
    );

    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/app.conf"),
        "setting = \"all\"\n",
        "`all` is not conflicted, and its commit is the whole reason this run existed: a pause must not strand it\n{}",
        render_output(&pause)
    );
    for (scope, was) in conflicted_scopes.iter().zip(&before) {
        assert_eq!(
            &remote_branch_revision(&machine_b, scope),
            was,
            "`{scope}` inherited the conflict, and a conflicted head is never pushed"
        );
    }

    // A different command, and therefore a different push call site, has to
    // give the same answer. Its exit code is not asserted: what plain
    // `dotsync` does when it meets a pause is a separate open question.
    machine_b.run("dotsync");
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/app.conf"),
        "setting = \"all\"\n"
    );
    for (scope, was) in conflicted_scopes.iter().zip(&before) {
        assert_eq!(
            &remote_branch_revision(&machine_b, scope),
            was,
            "a later run pushed the conflicted head of `{scope}`"
        );
    }
}

/// The same rule where it separates one scope from the rest instead of
/// stopping everything: the conflict is on `goof-a`, so `all`, `linux` and
/// this machine's own `goof-b` are all clean merges with nothing wrong with
/// them, and `goof-a` alone stays local until it is resolved.
///
/// This is the shape that matters for the machine sitting in front of you.
/// With the conflict on another machine's branch, withholding the whole push
/// means this machine's own scope — its own finished config — sits unpublished
/// behind someone else's unresolved merge.
#[test]
fn a_pause_on_another_machines_scope_still_publishes_this_machines_own() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    machine_a.init_ok();
    machine_b.init_ok();
    machine_a.run_ok("dotsync --force");

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add base config' -- .config/app.conf");
    machine_a.write_file(".config/app.conf", "setting = \"goof-a\"\n");
    machine_a.run_ok("dotsync commit goof-a -m 'customize goof-a config' -- .config/app.conf");

    machine_b.run_ok("dotsync");
    let goof_a_before = remote_branch_revision(&machine_b, "goof-a");

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let pause = machine_b.run_expecting(
        "dotsync commit all -m 'update shared config' -- .config/app.conf",
        3,
    );

    for scope in ["all", "linux", "goof-b"] {
        assert_eq!(
            remote_branch_file_contents(&machine_b, scope, ".config/app.conf"),
            "setting = \"all\"\n",
            "`{scope}` merged cleanly, so it publishes: another machine's unresolved merge is not a reason to hold this machine's own config back\n{}",
            render_output(&pause)
        );
    }
    assert_eq!(
        remote_branch_revision(&machine_b, "goof-a"),
        goof_a_before,
        "and the one conflicted head stays local until it is resolved"
    );
}

/// DESIGN: "'Paused' is not a stored mode; it is a derived observation: one or
/// more local scope heads have conflicted trees", and "Anything derivable from
/// the repo must be derived, never cached in a side file. Derived state is
/// automatically correct after a crash; stored state is a fresh opportunity to
/// be wrong."
///
/// This asks that as a behaviour rather than as the absence of a file: remove
/// whatever machine-local record of the pause exists, and the answer must not
/// change. Under the design there is nothing to remove and this is a no-op.
/// Today `.dotsync-paused-cascade.json` is the only copy of the intent, so
/// losing it loses the pause entirely — driven by hand against this build,
/// `status` reports an ordinary modified file with no `paused_cascade`,
/// `continue` says "there is no paused cascade on this machine", and the repo
/// is left half-cascaded with nothing saying so.
///
/// One shape is enough here, and it is the only test in this batch where that
/// is true: where the pause state is read from is the same question whichever
/// scope conflicted, and both shapes write the same one file.
#[test]
fn a_pause_survives_losing_every_machine_local_record_of_it() {
    let harness = TestHarness::new();
    let (_machine_a, machine_b, _pause) = pause_a_conflict_on(&harness, "linux");

    remove_machine_local_records(&machine_b);

    let status = machine_b.run_expecting("dotsync status --output json", 0);
    assert_eq!(
        parse_stdout_json(&status)["paused_cascade"],
        "linux",
        "the pause is in the repo's conflicted heads, so nothing outside the repo can take it away\n{}",
        render_output(&status)
    );

    let commit = machine_b.run("dotsync commit goof-b -m 'unrelated' -- .config/app.conf");
    assert_eq!(
        commit.status.code(),
        Some(3),
        "and a commit is still refused, because the machine is still in the state that refuses it\n{}",
        render_output(&commit)
    );

    machine_b.write_file(".config/app.conf", "setting = \"all+linux\"\n");
    machine_b.run_ok("dotsync continue");
    assert_eq!(
        remote_branch_file_contents(&machine_b, "linux", ".config/app.conf"),
        "setting = \"all+linux\"\n",
        "and the cascade can still be finished"
    );
}

/// The pause tells the agent to "edit the conflicted file in home to the
/// merged contents you want", and gives it nothing to merge *from*: home holds
/// exactly the bytes the agent last typed there, and neither the version this
/// change is colliding with nor the version they both came from is anywhere in
/// what the run said. The `linux` side is reachable, but only by knowing to
/// run `dotsync view --scope linux --file .config/app.conf`; the base is not
/// reachable at all.
///
/// That is also why the interim DL-2 guard has to ask "did this file change"
/// rather than "are the markers gone" — there are no markers, because there is
/// nothing of the conflict in the file.
///
/// Whether the answer is markers written into home or a description that
/// leaves home alone is PLAN §2.3 step 6's open question, so this asserts only
/// that all three versions reach the agent.
#[test]
fn a_pause_puts_the_conflict_in_front_of_the_agent() {
    let harness = TestHarness::new();
    let (machine, pause) = pause_a_conflict_on_linux(&harness);

    assert_the_conflict_is_in_front_of_the_agent(
        &machine,
        &pause,
        ".config/app.conf",
        [
            "setting = \"base\"",
            "setting = \"all\"",
            "setting = \"linux\"",
        ],
    );
}

/// The same, for a conflict this machine is not on — where it has to be put in
/// front of the agent deliberately rather than by propagation. `goof-b`'s tree
/// is a clean merge, so nothing about syncing this machine's own scope would
/// ever show it the `goof-a` collision it is being asked to resolve.
///
/// DESIGN calls the home path "a temporary resolution buffer" for this case,
/// which is one answer to step 6's question and not the settled one. What is
/// settled either way: the agent is asked to resolve a merge, so it has to be
/// able to see both sides of it and the base. Today it can see neither the
/// other side nor the base, which is what makes leaving the file alone — or
/// writing anything at all into it — so easy to get wrong.
#[test]
fn a_pause_on_another_machines_scope_puts_that_conflict_in_front_of_the_agent() {
    let harness = TestHarness::new();
    let (_machine_a, machine_b, pause) = pause_a_conflict_on(&harness, "goof-a");

    assert_the_conflict_is_in_front_of_the_agent(
        &machine_b,
        &pause,
        ".config/app.conf",
        [
            "setting = \"base\"",
            "setting = \"all\"",
            "setting = \"goof-a\"",
        ],
    );
}

/// A conflict does not need a `commit` to produce it. Home is the working copy
/// and its parent is the mark, so an ordinary sync is `merge(home, mark, new
/// head)` (PLAN §2.3 step 2), and two edits to the same line of the same file
/// are the case that merge cannot resolve on its own.
///
/// What is decided about it: the sync stops *whole*. Home is derived from one
/// commit, so a home built partly from the old head and partly from the new
/// one makes any single parent a lie — `spike.ignore/README.md`: "Partial
/// materialization is forbidden because the wc commit has one parent, and a
/// home derived partly from `P` and partly from `H` makes any single parent a
/// lie ... which is the silent-revert path." So the second, entirely
/// unconflicted incoming file does not arrive either. And no marker is written
/// into home: the conflict is presented instead (PLAN §2.3 step 6, settled
/// 2026-08-19), because a config file full of `<<<<<<<` is broken config for
/// exactly as long as the conflict takes to fix.
///
/// Today the run stops with `drift_detected` before merging anything, so the
/// agent is shown neither the version it collided with nor the version they
/// both came from.
///
/// The exit code is deliberately not pinned — PLAN has left what a stopping
/// sync exits with open through batches A, B and C, and this does not close
/// it. Nor are the payload's key names: the assertion is that the payload
/// carries the conflicted path at all.
#[test]
fn a_local_edit_conflicting_with_an_incoming_change_stops_the_sync_whole() {
    let harness = TestHarness::new();
    let (_machine_a, machine_b) = a_sync_conflict_over_one_line(&harness);

    let stop = machine_b.run("dotsync");
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"b\"\n",
        "the bytes in home are the only copy of this machine's side, and a stop must not touch them\n{}",
        render_output(&stop)
    );
    assert_eq!(
        machine_b.read_file(".apprc"),
        "ui_theme = dark\n",
        "the stop is whole: home is derived from one commit, so an unconflicted file cannot arrive on its own\n{}",
        render_output(&stop)
    );

    assert_the_conflict_is_in_front_of_the_agent(
        &machine_b,
        &stop,
        ".config/app.conf",
        ["setting = \"base\"", "setting = \"a\"", "setting = \"b\""],
    );

    let stopped_json = machine_b.run("dotsync --output json");
    assert!(
        payload_says(&parse_stdout_json(&stopped_json), ".config/app.conf"),
        "an agent reading the payload has to be told which path stopped the run\n{}",
        render_output(&stopped_json)
    );

    for scope in ["all", "linux", "goof-a", "goof-b"] {
        assert_ne!(
            remote_branch_file_contents(&machine_b, scope, ".config/app.conf"),
            "setting = \"b\"\n",
            "nothing was resolved, so this machine's side is nobody else's business yet: `{scope}` must not hold it"
        );
    }
}

/// The other half of the same decision: `continue` is how the agent says
/// "resolved", and what it resolves to is whatever is in home.
///
/// Without markers in home, home reads identically before the agent starts and
/// after it decides to keep its own side, so "I'm done" is the one thing
/// dotsync cannot find out for itself — which is why `continue` survives the
/// rewrite (PLAN §2.3 step 6, "The preference carries an implication").
///
/// Finishing means finishing the whole sync: the unconflicted file that could
/// not arrive on its own arrives now, with the resolution. And the resolution
/// is still just an edit in home — nobody asked for it to be published, so it
/// reaches no scope until it is committed.
///
/// Today `dotsync continue` answers "there is no paused cascade on this
/// machine": a pause is a thing only `commit` can create.
#[test]
fn continue_completes_a_sync_conflict_with_homes_bytes() {
    let harness = TestHarness::new();
    let (_machine_a, machine_b) = a_sync_conflict_over_one_line(&harness);

    machine_b.run("dotsync");
    machine_b.write_file(".config/app.conf", "setting = \"a+b\"\n");

    let continued = machine_b.run("dotsync continue");
    assert_eq!(
        continued.status.code(),
        Some(0),
        "the agent has done the one thing only it could do, and saying so is the whole of `continue`\n{}",
        render_output(&continued)
    );
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"a+b\"\n",
        "the resolution is what the agent wrote, and finishing the sync must not rewrite it\n{}",
        render_output(&continued)
    );
    assert_eq!(
        machine_b.read_file(".apprc"),
        "ui_theme = light\n",
        "and the file the stop withheld arrives with it\n{}",
        render_output(&continued)
    );

    let status = machine_b.run("dotsync status --output json");
    let payload = parse_stdout_json(&status);
    let changed: Vec<&str> = payload["changes"]
        .as_array()
        .expect("status answers with a changes array")
        .iter()
        .filter_map(|change| change["path"].as_str())
        .collect();
    assert_eq!(
        changed,
        [".config/app.conf"],
        "a resolution nobody committed is an ordinary local change, and it is the only one\n{}",
        render_output(&status)
    );

    for scope in ["all", "linux", "goof-a", "goof-b"] {
        assert_ne!(
            remote_branch_file_contents(&machine_b, scope, ".config/app.conf"),
            "setting = \"a+b\"\n",
            "resolving a sync is not publishing: `{scope}` must not hold the resolution until it is committed"
        );
    }
}

/// Two machines, one shared scope, and the collision an ordinary sync has to
/// meet: this machine has an uncommitted edit to one line of `.config/app.conf`
/// and the other machine published a different edit to the same line — plus a
/// change to `.apprc`, which nothing collides with and which is therefore how
/// each test can tell a stop that is whole from one that applied what it could.
///
/// Deliberately not `pause_a_conflict_on`: that fixture's conflict is between
/// two *commits* and arrives during a cascade. This one is between home and
/// the head, which is the conflict the mark makes possible and no test in this
/// file reaches today.
fn a_sync_conflict_over_one_line(
    harness: &TestHarness,
) -> (MachineEnvironment, MachineEnvironment) {
    let (machine_a, machine_b) = two_synced_machines(harness);

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.write_file(".apprc", "ui_theme = dark\n");
    machine_a.run_ok("dotsync commit all -m 'seed config' -- .config/app.conf .apprc");
    machine_b.run_ok("dotsync");
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"base\"\n"
    );
    assert_eq!(machine_b.read_file(".apprc"), "ui_theme = dark\n");

    // This machine's side, edited in home and not committed anywhere.
    machine_b.write_file(".config/app.conf", "setting = \"b\"\n");

    // The other machine's side, published: the same line, and one more file
    // that has nothing to do with it.
    machine_a.write_file(".config/app.conf", "setting = \"a\"\n");
    machine_a.write_file(".apprc", "ui_theme = light\n");
    machine_a.run_ok("dotsync commit all -m 'update config' -- .config/app.conf .apprc");

    (machine_a, machine_b)
}
