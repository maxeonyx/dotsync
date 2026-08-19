// Plain `dotsync`: the merge that brings home to this machine's scope, what it
// carries across and what it discards, and what a run says about the files it
// wrote, kept or stopped on.

mod harness;
use harness::*;

#[test]
fn v03_plain_sync_ignores_unrelated_home_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file("untracked-notes.txt", "leave me alone\n");

    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "plain dotsync should ignore unrelated home-directory changes in bare-repo mode\n{}",
        render_output(&sync_output)
    );
    assert_eq!(machine.read_file("untracked-notes.txt"), "leave me alone\n");
}

/// A stop lists the files it stopped on, and used to list them as `- ` bullets
/// printed straight after the `Correct flow:` bullets — so the files read as
/// more instructions. They are also the same files `status` and `diff` report,
/// and were rendered in a third shape.
///
/// The stop a sync can reach is the conflict: an edit dotsync can merge around
/// is carried rather than stopped on, so the run that stops is the one where
/// home and the scope changed the same file.
#[test]
fn the_conflict_stop_lists_files_the_way_status_does_and_apart_from_its_instructions() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();
    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    machine.run_ok("dotsync");
    machine.write_file(".bashrc", "export DOTSYNC=mine\n");
    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=theirs\n");

    let stopped = machine.run_expecting("dotsync", 1);
    let stderr = String::from_utf8_lossy(&stopped.stderr).into_owned();

    let status_output = machine.run("dotsync status");
    let status_stderr = String::from_utf8_lossy(&status_output.stderr).into_owned();
    let status_line = status_stderr
        .lines()
        .find(|line| line.contains(".bashrc"))
        .expect("status names the changed file");
    assert!(
        stderr.contains(status_line),
        "the stop names the file the way `status` does\nstop:\n{stderr}\nstatus line: {status_line:?}"
    );

    let (instructions, files) = stderr
        .split_once("Correct flow:")
        .expect("the stop teaches a correct flow");
    assert!(
        !instructions.contains("- .bashrc"),
        "the file list must not be rendered as instruction bullets\n{stderr}"
    );
    assert!(
        files.contains("Conflicted files:"),
        "the file list needs a heading of its own so it is not read as more instructions\n{stderr}"
    );
}

#[test]
fn sync_does_not_rewrite_files_that_already_match() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".apprc", "ui_theme = dark\n");
    machine.run_ok("dotsync");
    let written_at = machine.modified_time(".apprc");

    std::thread::sleep(std::time::Duration::from_millis(20));
    machine.run_ok("dotsync");

    assert_eq!(
        machine.modified_time(".apprc"),
        written_at,
        "a sync that changes nothing must not rewrite the file"
    );
}

#[test]
fn an_untracked_home_file_is_not_overwritten_by_an_incoming_add() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    // Real content in A's home that dotsync has never seen.
    machine_a.write_file(".newfile", "mine\n");

    machine_b.write_file(".newfile", "theirs\n");
    machine_b.run_ok("dotsync commit all -m 'add newfile' -- .newfile");

    let sync_a = machine_a.run("dotsync");
    assert_eq!(
        sync_a.status.code(),
        Some(1),
        "an incoming add that collides with untracked home content is drift\n{}",
        render_output(&sync_a)
    );
    assert_eq!(
        machine_a.read_file(".newfile"),
        "mine\n",
        "dotsync must not silently overwrite home content it has never seen"
    );

    machine_a.run_ok("dotsync --force");
    assert_eq!(machine_a.read_file(".newfile"), "theirs\n");
}

/// Deleting a managed file from home is a local change like any other: `diff`
/// shows what it would discard, an ordinary sync carries it rather than
/// stopping on it or quietly putting the file back, `commit` records it, and
/// `--force` is how you change your mind.
#[test]
fn deleting_a_managed_file_is_a_change_the_sync_carries_and_commit_records() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    machine.run_ok("dotsync");

    machine.delete_file(".bashrc");

    let diff_output = machine.run("dotsync diff");
    assert_eq!(
        diff_output.status.code(),
        Some(1),
        "a deleted managed file is a change, and `diff` must show it\n{}",
        render_output(&diff_output)
    );
    let diff_stderr = String::from_utf8_lossy(&diff_output.stderr).into_owned();
    assert!(
        diff_stderr.contains(".bashrc") && diff_stderr.contains("-export DOTSYNC=repo"),
        "`diff` must render what the deletion would discard\n{diff_stderr}"
    );

    // Nothing in the repo collides with the deletion, so the sync has nothing
    // to decide: it carries the deletion and says it is still a change.
    let sync_output = machine.run_ok("dotsync");
    assert!(
        !machine.file_exists(".bashrc"),
        "a sync that had nothing to merge must not put the file back\n{}",
        render_output(&sync_output)
    );
    assert_eq!(
        parse_stdout_json(&machine.run_ok("dotsync status --output json"))["changes"]
            .as_array()
            .map(Vec::len),
        Some(1),
        "and the deletion is still this machine's to decide about"
    );

    // Changing your mind: `--force` discards the deletion with every other
    // local change.
    machine.run_ok("dotsync --force");
    assert_eq!(machine.read_file(".bashrc"), "export DOTSYNC=repo\n");

    machine.delete_file(".bashrc");
    machine.run_ok("dotsync commit mx-xps-cy -m 'drop bashrc' -- .bashrc");
    assert!(!bookmark_has_file(&machine, "mx-xps-cy", ".bashrc"));
    assert!(!machine.file_exists(".bashrc"));

    machine.run_ok("dotsync");
    assert_stderr_snapshot(
        &machine.run_ok("dotsync status"),
        "dotsync: no changes for mx-xps-cy\n",
    );
}

#[test]
fn a_concurrent_merge_leaves_no_drift_behind() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(
        ".config/app.conf",
        "alpha = 1\nbeta = 2\ngamma = 3\ndelta = 4\nepsilon = 5\n",
    );
    machine_a.run_ok("dotsync commit all -m 'seed app.conf' -- .config/app.conf");
    machine_b.run_ok("dotsync");

    // Line-disjoint edits, so the merge succeeds. B is behind when it commits.
    machine_a.write_file(
        ".config/app.conf",
        "alpha = 100\nbeta = 2\ngamma = 3\ndelta = 4\nepsilon = 5\n",
    );
    machine_a.run_ok("dotsync commit all -m 'a changes alpha' -- .config/app.conf");

    machine_b.write_file(
        ".config/app.conf",
        "alpha = 1\nbeta = 2\ngamma = 3\ndelta = 4\nepsilon = 500\n",
    );
    let commit_b = machine_b.run("dotsync commit all -m 'b changes epsilon' -- .config/app.conf");
    assert!(
        commit_b.status.success(),
        "a commit whose merge succeeded must not then stop on its own result\n{}",
        render_output(&commit_b)
    );

    // Home holds only B's side until the commit's own sync writes the merge
    // down. That sync is the one step that can do it, so it has to run.
    let merged = "alpha = 100\nbeta = 2\ngamma = 3\ndelta = 4\nepsilon = 500\n";
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        merged,
        "the merge dotsync just pushed has to reach the home it came from"
    );
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/app.conf"),
        merged
    );

    let status_b = machine_b.run("dotsync status");
    assert_stderr_snapshot(&status_b, "dotsync: no changes for goof-b\n");
}

#[test]
fn a_remote_advance_is_reported_as_incoming_by_status_diff_and_sync() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    machine_b.write_file(".apprc", "ui_theme = light\nfont = mono\n");
    machine_b.run_ok("dotsync commit all -m 'light theme' -- .apprc");

    // A is merely behind: nothing in its home changed. `status`, `diff` and
    // plain `dotsync` have to agree about that.
    let status_a = machine_a.run_ok("dotsync status");
    assert_stderr_snapshot(
        &status_a,
        "\
dotsync: 1 incoming file(s) for goof-a — plain `dotsync` applies these
  U .apprc (changed on another machine, and not edited here)
",
    );

    let status_json = machine_a.run_ok("dotsync --output json status");
    let json = parse_stdout_json(&status_json);
    assert_eq!(
        json["changes"].as_array().map(Vec::len),
        Some(0),
        "a file this machine has not changed is not a change\n{}",
        render_output(&status_json)
    );
    assert_eq!(json["incoming"].as_array().map(Vec::len), Some(1));

    let diff_a = machine_a.run("dotsync diff");
    assert_eq!(
        diff_a.status.code(),
        Some(0),
        "a routine remote advance is not drift\n{}",
        render_output(&diff_a)
    );
    assert_stderr_snapshot(&diff_a, "dotsync: no changes for goof-a\n");

    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".apprc"),
        "ui_theme = light\nfont = mono\n"
    );
}

/// A forced sync is the one thing plain `dotsync` does that cannot be undone:
/// it throws away what is in home. The notes on stderr said so and the payload
/// did not, so the machine-readable half of the run was the less honest one.
#[test]
fn a_forced_sync_says_which_home_files_it_overwrote() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    let clean = machine.run_ok("dotsync --output json");
    assert_eq!(
        parse_stdout_json(&clean)["overwritten_files"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "a sync that overwrote nothing says so, rather than saying nothing\n{}",
        render_output(&clean)
    );

    machine.write_file(".bashrc", "export DOTSYNC=mine\n");
    let forced = machine.run_ok("dotsync --force --output json");
    let json = parse_stdout_json(&forced);
    assert_eq!(
        json["overwritten_files"],
        serde_json::json!([".bashrc"]),
        "the file whose contents this run discarded has to be in the payload\n{}",
        render_output(&forced)
    );
    assert_eq!(machine.read_file(".bashrc"), "export DOTSYNC=repo\n");
}

/// One value, one field. `scope` and `machine_scope` used to be byte-identical
/// on every command that only syncs, so a reader could not tell which question
/// `scope` answered without knowing which command produced it.
#[test]
fn every_command_that_only_syncs_names_the_machine_scope_once() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.run_ok(&format!(
        "dotsync --output json init {}",
        machine
            .remote_dir
            .to_str()
            .expect("remote path should be valid UTF-8")
    ));
    let json = parse_stdout_json(&init_output);
    assert_eq!(json["command"], "init");
    assert_eq!(json["machine_scope"], "mx-xps-cy");
    assert!(
        json.get("scope").is_none(),
        "`scope` on init only ever repeated the machine scope\n{}",
        render_output(&init_output)
    );

    let sync_output = machine.run_ok("dotsync --output json");
    let json = parse_stdout_json(&sync_output);
    assert_eq!(json["command"], "sync");
    assert_eq!(json["machine_scope"], "mx-xps-cy");
    assert!(
        json.get("scope").is_none(),
        "`scope` on sync only ever repeated the machine scope\n{}",
        render_output(&sync_output)
    );
}

/// Home is the working copy and its parent is the mark, so a sync is
/// `merge(home, mark, new head)` (PLAN §2.3 step 2; `spike.ignore/README.md`,
/// "Implications for the real step 2": "if the in-memory merge is resolved,
/// create the new wc commit ... carrying non-conflicting local edits across
/// the sync"). An edit to one file and an incoming change to another is that
/// merge in its easiest form — the two sides do not even touch the same path,
/// so there is nothing to decide.
///
/// Today drift is a gate rather than a merge input: one edited file anywhere
/// in home stops the whole sync, so this machine receives nothing until it
/// decides what to do about a file the incoming change has nothing to do with.
/// The edit itself stays a local change either way — it is not committed here,
/// and this run must not publish it.
#[test]
fn a_sync_applies_incoming_changes_while_carrying_an_unrelated_local_edit() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    // Two managed files on a shared scope, both of them synced to both
    // machines, so each side below starts from the same bytes.
    machine_a.write_file(".apprc", "ui_theme = dark\n");
    machine_a.write_file(".editorrc", "tabs = 4\n");
    machine_a.run_ok("dotsync commit all -m 'seed config' -- .apprc .editorrc");
    machine_b.run_ok("dotsync");
    assert_eq!(machine_b.read_file(".apprc"), "ui_theme = dark\n");
    assert_eq!(machine_b.read_file(".editorrc"), "tabs = 4\n");

    // B is mid-edit on one file and has not decided what to do with it yet.
    machine_b.write_file(".editorrc", "tabs = 2\n");

    // A publishes a change to the other one.
    machine_a.write_file(".apprc", "ui_theme = light\n");
    machine_a.run_ok("dotsync commit all -m 'light theme' -- .apprc");

    let sync_b = machine_b.run("dotsync");
    assert_eq!(
        sync_b.status.code(),
        Some(0),
        "an edit to one file and an incoming change to another is a merge with nothing to decide\n{}",
        render_output(&sync_b)
    );
    assert_eq!(
        machine_b.read_file(".apprc"),
        "ui_theme = light\n",
        "the incoming change has to arrive\n{}",
        render_output(&sync_b)
    );
    assert_eq!(
        machine_b.read_file(".editorrc"),
        "tabs = 2\n",
        "and the local edit has to survive the sync that carried it"
    );

    // The edit is still an edit: uncommitted, still this machine's to decide
    // about, and no longer confusable with the file that just arrived.
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
        [".editorrc"],
        "the edit is still a local change, and the file this run applied is not one\n{}",
        render_output(&status)
    );
    let incoming: Vec<&str> = payload["incoming"]
        .as_array()
        .expect("status answers with an incoming array")
        .iter()
        .filter_map(|change| change["path"].as_str())
        .collect();
    assert!(
        !incoming.contains(&".apprc"),
        "and it is not still incoming, because it already arrived\n{}",
        render_output(&status)
    );

    for scope in ["all", "linux", "goof-b"] {
        assert_ne!(
            remote_branch_file_contents(&machine_b, scope, ".editorrc"),
            "tabs = 2\n",
            "an uncommitted edit is nobody else's business: `{scope}` must not hold it"
        );
    }
}

/// Two edits to one file that do not touch the same lines. One of them is an
/// uncommitted edit sitting in this machine's home; the other has already been
/// published from somewhere else. They combine, and a plain `dotsync` is what
/// combines them — which is exactly what happens today.
///
/// What does not happen today is dotsync telling the truth about it beforehand.
/// Drift classification decides "conflict" by comparing bytes, so home and the
/// tip differing anywhere at all reads as a three-way conflict: `status` and
/// `diff` call the file conflicted, and then plain `dotsync` merges it without
/// complaint. Two answers to one question, from two different merge engines.
///
/// So the claim here is that the report and the sync agree: this is a local
/// edit *plus* an incoming change, it is not a conflict, and the run that
/// carries it leaves home holding both changes.
#[test]
fn an_edit_here_and_a_change_elsewhere_in_the_same_file_combine_instead_of_conflicting() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    let base: String = (1..=90).map(|line| format!("line {line}\n")).collect();
    machine_a.write_file(".config/app.conf", &base);
    machine_a.run_ok("dotsync commit all -m 'seed app.conf' -- .config/app.conf");
    machine_b.run_ok("dotsync");
    assert_eq!(machine_b.read_file(".config/app.conf"), base);

    // B is mid-edit at the top of the file and has not committed it.
    let edited_here = base.replace("line 1\n", "line 1 edited on b\n");
    machine_b.write_file(".config/app.conf", &edited_here);

    // A publishes a change at the bottom of the same file.
    let edited_elsewhere = base.replace("line 90\n", "line 90 edited on a\n");
    machine_a.write_file(".config/app.conf", &edited_elsewhere);
    machine_a.run_ok("dotsync commit all -m 'a edits the bottom' -- .config/app.conf");

    let status = machine_b.run_ok("dotsync status --output json");
    let payload = parse_stdout_json(&status);
    let change = payload["changes"]
        .as_array()
        .expect("status answers with a changes array")
        .iter()
        .find(|change| change["path"] == ".config/app.conf")
        .unwrap_or_else(|| {
            panic!(
                "home holds an edit of this machine's own, so the file is a change\n{}",
                render_output(&status)
            )
        });
    assert_ne!(
        change["state"], "conflicted",
        "nothing here conflicts: the sync below merges these two edges without being asked anything\n{}",
        render_output(&status)
    );

    let both = base
        .replace("line 1\n", "line 1 edited on b\n")
        .replace("line 90\n", "line 90 edited on a\n");
    machine_b.run_ok("dotsync");
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        both,
        "the sync combines the two edits rather than choosing between them"
    );

    // Having merged, this is an ordinary uncommitted local edit and says so —
    // both sides of the file are in home now, so there is only one change left
    // at this path.
    let after = machine_b.run_ok("dotsync status --output json");
    let payload = parse_stdout_json(&after);
    let change = payload["changes"]
        .as_array()
        .expect("status answers with a changes array")
        .iter()
        .find(|change| change["path"] == ".config/app.conf")
        .unwrap_or_else(|| {
            panic!(
                "the edit is still uncommitted, so it is still a change\n{}",
                render_output(&after)
            )
        });
    assert_eq!(
        change["state"],
        "modified",
        "nothing is arriving any more, so this is a plain local edit\n{}",
        render_output(&after)
    );

    machine_b.run_ok("dotsync commit all -m 'b edits the top' -- .config/app.conf");
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/app.conf"),
        both,
        "and neither machine's change is lost on the way to the remote"
    );
}

/// The live fleet migrates by upgrading the binary and running `dotsync`
/// (PLAN §2.6, "The live fleet"), and every machine in it is carrying a
/// `sync-state.json` written by the release before. That file holds exactly
/// `machine_scope` and `last_synced_revision` — a hand-rolled record of what
/// jj's own view already says — so step 2 dissolves it: `spike.ignore/README.md`,
/// "Migration for the live fleet: first run of the new binary creates the wc
/// commit ..., snapshots home ..., deletes `sync-state.json`." PLAN §2.6,
/// "On-disk, per machine": "Two things: the home files, and the hidden repo.
/// Nothing else — no sync state file, no pause file, no `config.toml`."
///
/// So the upgrade run has to do two things at once: pick up whatever was
/// waiting for it, and leave the old file behind. Today it does the first and
/// rewrites the second.
#[test]
fn an_upgraded_machine_sheds_sync_state_json_and_keeps_working() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    let shed = ensure_the_previous_releases_sync_state(&machine_b);

    machine_a.write_file(".apprc", "ui_theme = dark\n");
    machine_a.run_ok("dotsync commit all -m 'seed apprc' -- .apprc");

    let upgrade_run = machine_b.run("dotsync");
    assert_eq!(
        upgrade_run.status.code(),
        Some(0),
        "upgrading is running the new binary, and the first thing it runs is an ordinary sync\n{}",
        render_output(&upgrade_run)
    );
    assert_eq!(
        machine_b.read_file(".apprc"),
        "ui_theme = dark\n",
        "the change that was waiting has to arrive\n{}",
        render_output(&upgrade_run)
    );
    assert!(
        !shed.exists(),
        "the machine-local sync record is jj's now, and a second copy of it is a second authority: {} outlived the run\n{}",
        shed.display(),
        render_output(&upgrade_run)
    );

    let status = machine_b.run("dotsync status");
    assert_eq!(
        status.status.code(),
        Some(0),
        "and the machine is an ordinary machine afterwards\n{}",
        render_output(&status)
    );
    assert_eq!(
        parse_stdout_json(&machine_b.run_ok("dotsync status --output json"))["changes"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "shedding the record must not make home look changed\n{}",
        render_output(&status)
    );
}

/// The machine-local state file the release being upgraded from keeps, made
/// sure to exist, and where to look for it afterwards.
///
/// Ensured rather than asserted: the machine this fixture describes has one
/// because the *old* binary wrote it, which is true whether or not the binary
/// under test still writes one — so a run that has stopped writing it gets the
/// old release's file fabricated in the old release's format.
///
/// The path is the one the previous release wrote it to.
fn ensure_the_previous_releases_sync_state(machine: &MachineEnvironment) -> std::path::PathBuf {
    machine.run_ok("dotsync");

    let path = machine.home_dir.join(".config/dotsync/sync-state.json");

    if !path.exists() {
        let scope = machine_scope_reported_by(machine)
            .expect("a machine being upgraded knows which scope it is");
        write_file_at(
            &path,
            &format!(
                "{{\n  \"machine_scope\": \"{scope}\",\n  \"last_synced_revision\": \"{}\"\n}}\n",
                bookmark_revision(machine, &scope)
            ),
        );
    }
    assert!(
        path.exists(),
        "this fixture is a machine upgrading from a release that kept {}",
        path.display()
    );
    path
}
