// Plain `dotsync`: the drift model that decides whether a home file may be
// overwritten, the sync-state record that model reads from, and what a run
// says about the files it wrote, skipped or stopped on.

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

#[test]
fn drift_detected_human_error_stands_alone() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".gitconfig",
        "[user]\nname = \"Repo\"\n",
    );
    machine.run_ok("dotsync");

    machine.write_file(".gitconfig", "[user]\nname = \"Drifted\"\n");

    let sync_output = machine.run_expecting("dotsync", 1);

    assert_stderr_snapshot(
        &sync_output,
        r#"dotsync: drift detected

What dotsync does:
Dotsync keeps its hidden repo as the source of truth for your home-directory config: the repo is the source of truth, and dotsync syncs committed repo state into the live system.

This flow:
This sync flow compares managed files in your home directory against the repo version for this machine scope before copying anything.

Expected:
This flow expects managed files in your home directory to already match the repo, unless you intentionally choose to overwrite drift.

Current state found:
The files that differ are listed under `Changed files:` below, each with what it would be replaced by.

Why dotsync stopped:
Dotsync stopped before overwriting local drift so you can inspect what would be replaced.

Correct flow:
- If the repo is correct, rerun with `dotsync --force` to overwrite the drift after reviewing the diffs.
- If the live file is the change you wanted, run `dotsync status`, then commit the intended path with `dotsync commit <scope> -m "message" -- <path>`.

Changed files:
  M .gitconfig (edited here since the last sync)
--- repo
+++ system
@@ -1,2 +1,2 @@
 [user]
-name = "Repo"
+name = "Drifted"
"#,
    );
}

#[test]
fn drift_detected_json_contract_stays_compatible() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".gitconfig",
        "[user]\nname = \"Repo\"\n",
    );
    machine.run_ok("dotsync");

    machine.write_file(".gitconfig", "[user]\nname = \"Drifted\"\n");

    let sync_output = machine.run_expecting("dotsync --output json", 1);

    let json = parse_stdout_json(&sync_output);
    assert_eq!(json["status"], "error");
    assert_eq!(json["error"], "drift_detected");
    assert!(json["message"].as_str().is_some());
    assert!(json["current_state"].is_array());

    let drifts = json["drifts"]
        .as_array()
        .expect("drifts should be an array");
    assert_eq!(drifts.len(), 1);
    assert_eq!(drifts[0]["path"], ".gitconfig");
    assert!(drifts[0]["state"].as_str().is_some());
    assert!(drifts[0]["reason"].as_str().is_some());
    assert!(drifts[0]["diff"].as_str().is_some());
}

/// The drift stop lists the files it stopped on, and used to list them as `- `
/// bullets printed straight after the `Correct flow:` bullets — so the files
/// read as more instructions. They are also the same files `status` and `diff`
/// report, and were rendered in a third shape.
#[test]
fn the_drift_stop_lists_files_the_way_status_does_and_apart_from_its_instructions() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();
    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    machine.run_ok("dotsync");
    machine.write_file(".bashrc", "export DOTSYNC=mine\n");

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
        files.contains("Changed files:"),
        "the file list needs a heading of its own so it is not read as more instructions\n{stderr}"
    );
}

#[test]
fn missing_state_file_disables_deletion() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".gitconfig",
        "[user]\nname = \"Max\"\n",
    );
    machine.run_ok("dotsync");
    assert!(machine.file_exists(".gitconfig"));

    machine.delete_sync_state();
    remove_remote_scope_file(&machine, "mx-xps-cy", ".gitconfig");

    machine.run_ok("dotsync");
    assert!(
        machine.file_exists(".gitconfig"),
        "without sync state, dotsync should fail safe and leave the previously managed file in home"
    );
}

#[test]
fn invalid_state_file_returns_clear_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_sync_state_raw("not valid json\n");

    let sync_output = machine.run("dotsync");
    assert!(
        !sync_output.status.success(),
        "sync should fail when the sync state file is corrupt\n{}",
        render_output(&sync_output)
    );
    let expected = format!(
        "\
dotsync: invalid sync state

What dotsync does:
Dotsync keeps the repo as the source of truth and uses a local sync-state file to remember which machine scope was last synced here and which revision that sync used.

This flow:
This sync flow reads that local state to know which prior managed files may need removal and which machine scope should be treated as authoritative for this home.

Expected:
It expects that state file, if present, to be valid and readable; it expects that state file, if present, to be valid.

Current state found:
sync state error at {}: failed to parse sync state: expected ident at line 1 column 2

Why dotsync stopped:
Dotsync stopped because it cannot safely decide what prior sync state to trust.

Correct flow:
- fix or delete the bad sync-state file and rerun the command.
- After that, let dotsync recreate valid sync state from a successful sync.
",
        machine.sync_state_path().display()
    );
    assert_stderr_snapshot(&sync_output, &expected);
}

#[test]
fn invalid_sync_state_human_error_stands_alone() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_sync_state_raw("not valid json\n");

    let sync_output = machine.run_expecting("dotsync", 1);

    let expected = format!(
        "\
dotsync: invalid sync state

What dotsync does:
Dotsync keeps the repo as the source of truth and uses a local sync-state file to remember which machine scope was last synced here and which revision that sync used.

This flow:
This sync flow reads that local state to know which prior managed files may need removal and which machine scope should be treated as authoritative for this home.

Expected:
It expects that state file, if present, to be valid and readable; it expects that state file, if present, to be valid.

Current state found:
sync state error at {}: failed to parse sync state: expected ident at line 1 column 2

Why dotsync stopped:
Dotsync stopped because it cannot safely decide what prior sync state to trust.

Correct flow:
- fix or delete the bad sync-state file and rerun the command.
- After that, let dotsync recreate valid sync state from a successful sync.
",
        machine.sync_state_path().display()
    );
    assert_stderr_snapshot(&sync_output, &expected);
}

#[test]
fn sync_uses_state_machine_scope_even_if_checkout_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".config/machine-only.txt",
        "machine config\n",
    );
    machine.run_ok("dotsync");
    assert_eq!(
        machine.read_file(".config/machine-only.txt"),
        "machine config\n"
    );

    // Deleting a managed file from home is deletion drift, so this restore has
    // to say it means to discard it. What the test is pinning is which machine
    // scope the sync used, not whether the deletion blocked.
    machine.delete_file(".config/machine-only.txt");
    machine.write_sync_state_raw(&format!(
        "{{\n  \"machine_scope\": \"mx-xps-cy\",\n  \"last_synced_revision\": \"{}\"\n}}\n",
        bookmark_revision(&machine, "mx-xps-cy")
    ));

    machine.run_ok("dotsync --force");
    assert_eq!(
        machine.read_file(".config/machine-only.txt"),
        "machine config\n",
        "sync state machine scope should govern sync regardless of any unrelated repo metadata"
    );
}

#[test]
fn a_machine_that_lost_its_sync_state_still_syncs() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    machine.run_ok("dotsync");

    machine.delete_sync_state();
    machine.delete_file(".bashrc");

    // Deleting a managed file from home is drift only when dotsync can show it
    // put the file there. Without sync state it cannot, so this is an ordinary
    // incoming file and the machine converges rather than needing `--force`.
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "a machine with no sync state must still be able to sync\n{}",
        render_output(&sync_output)
    );
    assert_eq!(machine.read_file(".bashrc"), "export DOTSYNC=repo\n");
}

#[test]
fn a_machine_with_no_sync_record_says_so_instead_of_guessing() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=v1\n");
    machine.run_ok("dotsync");

    // Home holds exactly what dotsync wrote there. Losing the state file does
    // not change that — it only means dotsync can no longer prove it.
    machine.delete_sync_state();
    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=v2\n");

    let sync_output = machine.run("dotsync");
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "without a record dotsync cannot tell an edit here from an incoming change, so it must stop\n{}",
        render_output(&sync_output)
    );

    let stderr = String::from_utf8_lossy(&sync_output.stderr).into_owned();
    assert!(
        !stderr.contains("never synced here"),
        "the file was synced here; the diagnosis must not claim otherwise\n{stderr}"
    );
    assert!(
        !stderr.contains("just added"),
        "the repo changed this file rather than adding it\n{stderr}"
    );
    assert!(
        stderr.contains("no sync record"),
        "the diagnosis must name the actual problem: the record is gone\n{stderr}"
    );

    // Both ways out still work.
    machine.run_ok("dotsync --force");
    assert_eq!(machine.read_file(".bashrc"), "export DOTSYNC=v2\n");
}

#[test]
fn a_sync_interrupted_before_saving_state_is_not_drift_on_the_next_run() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".apprc", "version = 1\n");
    machine.run_ok("dotsync");
    let state_before = machine.read_sync_state_raw();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".apprc", "version = 2\n");
    machine.run_ok("dotsync");
    assert_eq!(machine.read_file(".apprc"), "version = 2\n");

    // A run that dies between finishing the home writes and saving sync state
    // leaves exactly this: home already holds the new bytes, and the state file
    // still points at the revision before them. The rerun must converge, not
    // report the bytes it just wrote as local drift.
    machine.write_sync_state_raw(&state_before);

    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "an interrupted sync must converge on rerun\n{}",
        render_output(&sync_output)
    );
    assert_eq!(machine.read_file(".apprc"), "version = 2\n");
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

#[test]
fn deleting_a_managed_file_blocks_sync_and_is_committable() {
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
        "a deleted managed file is drift, and `diff` must show it\n{}",
        render_output(&diff_output)
    );
    let diff_stderr = String::from_utf8_lossy(&diff_output.stderr).into_owned();
    assert!(
        diff_stderr.contains(".bashrc") && diff_stderr.contains("-export DOTSYNC=repo"),
        "`diff` must render what the deletion would discard\n{diff_stderr}"
    );

    let sync_output = machine.run("dotsync");
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "deletion drift blocks sync like an edit\n{}",
        render_output(&sync_output)
    );
    assert!(
        !machine.file_exists(".bashrc"),
        "the blocked sync must not quietly restore the deleted file"
    );

    machine.run_ok("dotsync commit mx-xps-cy -m 'drop bashrc' -- .bashrc");
    assert!(!bookmark_has_file(&machine, "mx-xps-cy", ".bashrc"));
    assert!(!machine.file_exists(".bashrc"));

    machine.run_ok("dotsync");
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
/// The path is read out of `config.toml` when there is one, because that is
/// where the current release configures it, and falls back to the location it
/// puts there by default.
fn ensure_the_previous_releases_sync_state(machine: &MachineEnvironment) -> std::path::PathBuf {
    machine.run_ok("dotsync");

    let configured = std::fs::read_to_string(machine.home_dir.join(".config/dotsync/config.toml"))
        .ok()
        .and_then(|config| {
            config.lines().find_map(|line| {
                line.strip_prefix("state_path = \"")
                    .and_then(|rest| rest.strip_suffix('"'))
                    .map(str::to_string)
            })
        })
        .unwrap_or_else(|| String::from(".config/dotsync/sync-state.json"));
    let path = machine.home_dir.join(configured);

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
