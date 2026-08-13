use std::fs;
use std::path::{Path, PathBuf};

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
fn diff_shows_line_oriented_home_drift_without_syncing() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".config/app.conf",
        "line one\nline two\n",
    );
    machine.run_ok("dotsync");

    machine.write_file(".config/app.conf", "line one\nchanged two\n");

    let diff_output = machine.run_expecting("dotsync diff", 1);

    assert_eq!(
        machine.read_file(".config/app.conf"),
        "line one\nchanged two\n"
    );
    assert_stderr_snapshot(
        &diff_output,
        "\
dotsync: 1 changed managed file(s) for mx-xps-cy
  M .config/app.conf (edited here since the last sync)
--- repo
+++ system
@@ -1,2 +1,2 @@
 line one
-line two
+changed two
",
    );
}

#[test]
fn view_summarizes_checked_in_scopes_and_files() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "all", ".gitconfig", "[user]\nname = Shared\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    machine.run_ok("dotsync");

    let view_output = machine.run_ok("dotsync view");
    assert_stdout_snapshot(
        &view_output,
        "\
Scopes
all
linux <- all
mx-xps-cy <- linux

Files
.config/dotsync/config.toml
.gitconfig
",
    );
}

#[test]
fn view_scope_shows_checked_in_file_tree() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "all", ".gitconfig", "[user]\nname = Shared\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    machine.run_ok("dotsync");

    let view_output = machine.run_ok("dotsync view --scope mx-xps-cy");
    assert_stdout_snapshot(
        &view_output,
        "\
Scope mx-xps-cy
.config/dotsync/config.toml
.gitconfig
",
    );
}

#[test]
fn view_file_shows_scopes_and_scoped_file_content() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "all", ".gitconfig", "[user]\nname = Shared\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    machine.run_ok("dotsync");

    let file_scopes_output = machine.run_ok("dotsync view --file .gitconfig");
    assert_stdout_snapshot(
        &file_scopes_output,
        "\
File .gitconfig
Scopes
all
linux
mx-xps-cy
",
    );

    let file_content_output = machine.run_ok("dotsync view --scope mx-xps-cy --file .gitconfig");
    assert_stdout_snapshot(&file_content_output, "[user]\nname = Shared\n");
}

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
fn commit_with_no_paths_ignores_unmanaged_home_files() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // An unmanaged file in home. `dotsync commit <scope> -m ...` with no paths
    // means "every managed file that changed", and nothing managed changed, so
    // this is an ordinary no-op commit — not a reason to refuse the command.
    machine.write_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let revision_before = bookmark_revision(&machine, "all");

    let commit_output = machine.run("dotsync commit all -m 'nothing changed'");
    assert_eq!(
        commit_output.status.code(),
        Some(0),
        "a no-paths commit with nothing to commit should succeed\n{}",
        render_output(&commit_output)
    );

    assert_eq!(bookmark_revision(&machine, "all"), revision_before);
    assert!(
        !bookmark_has_file(&machine, "all", ".gitconfig"),
        "an unmanaged home file must not be swept into the scope"
    );
    assert_eq!(machine.read_file(".gitconfig"), "[user]\nname = \"Max\"\n");
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
fn explicit_commit_command_adds_file_to_scope_and_syncs() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".gitconfig", "[user]\nname = \"Max\"\n");

    machine.run_ok("dotsync commit all -m 'add gitconfig' -- .gitconfig");

    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
    assert_eq!(machine.read_file(".gitconfig"), "[user]\nname = \"Max\"\n");
}

#[test]
fn unknown_command_is_not_treated_as_scope_commit() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let output = machine.run("dotsync nonesuch");

    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown top-level command should be a usage error\n{}",
        render_output(&output)
    );
    assert_stderr_snapshot(
        &output,
        "dotsync: unknown command `nonesuch`; run `dotsync --help` for supported commands\n",
    );
}

#[test]
fn commit_path_that_matches_nothing_is_an_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".apprc", "ui_theme = dark\n");
    fs::create_dir_all(machine.home_dir.join("empty-dir")).expect("create empty dir");
    let revision_before = bookmark_revision(&machine, "all");

    // A typo, a `~/`-prefixed path, an absolute path, and a directory holding
    // nothing at all. Each of these used to commit nothing and report success,
    // which tells an agent its config was saved when it was not.
    let absolute = machine.home_dir.join(".apprc");
    let absolute = absolute.to_str().expect("home path should be UTF-8");
    for command in [
        "dotsync commit all -m typo -- nonexistent-file".to_string(),
        "dotsync commit all -m tilde -- '~/.apprc'".to_string(),
        format!("dotsync commit all -m absolute -- {absolute}"),
        "dotsync commit all -m empty-dir -- empty-dir".to_string(),
    ] {
        let output = machine.run(&command);
        assert_eq!(
            output.status.code(),
            Some(1),
            "`{command}` should fail rather than report a successful empty commit\n{}",
            render_output(&output)
        );
        assert_eq!(
            bookmark_revision(&machine, "all"),
            revision_before,
            "`{command}` must not move the scope"
        );
    }

    let typo_output = machine.run("dotsync commit all -m typo -- nonexistent-file");
    assert_stderr_snapshot(
        &typo_output,
        &format!(
            "\
dotsync: cannot commit that path

What dotsync does:
Dotsync records the home files you name onto a scope branch, then cascades that scope so every machine sharing it receives the change. Every file on a scope is written back into home on each of those machines.

This flow:
This commit flow resolves each path you name against your home directory, checks that it is a config file dotsync may record, and commits the ones that changed.

Expected:
It expects every path you name to be a config file inside your home directory, named relative to it, and to exist either in home or on the target scope already.

Current state found:
`nonexistent-file` matched nothing: no file exists at or under {}/nonexistent-file, and scope `all` tracks no file at or under `nonexistent-file`.

Why dotsync stopped:
Dotsync stopped before recording anything. A commit records every path you named or none of them, so fixing the paths above and rerunning the same command is safe.

Correct flow:
- name paths relative to your home directory: `dotsync commit all -m \"message\" -- .config/fish/config.fish`.
- do not use `~/`, absolute paths, or `..`; dotsync resolves every path against your home directory already, and records it verbatim.
- run `dotsync status` to see which managed files changed.
",
            machine.home_dir.display()
        ),
    );

    let absolute_output = machine.run(&format!("dotsync commit all -m absolute -- {absolute}"));
    let absolute_stderr = String::from_utf8_lossy(&absolute_output.stderr);
    assert!(
        absolute_stderr.contains(&format!(
            "`{absolute}` is an absolute path, and dotsync resolves every commit path against your home directory."
        )),
        "{}",
        render_output(&absolute_output)
    );
}

#[test]
fn commit_path_that_escapes_home_is_an_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // Dotsync records the path you name verbatim as a repo path, and every
    // machine on that scope writes it back out under its own home. A path that
    // climbs out of home therefore writes outside home everywhere.
    let outside = machine.home_dir.parent().expect("home has a parent");
    write_file_at(&outside.join("outside.conf"), "PWNED=1\n");
    write_file_at(&outside.join("deeper.conf"), "PWNED=2\n");
    let revision_before = bookmark_revision(&machine, "all");

    for command in [
        "dotsync commit all -m escape -- ../outside.conf",
        "dotsync commit all -m escape -- ../../machine-a/home/../deeper.conf",
    ] {
        let output = machine.run(command);
        assert_eq!(
            output.status.code(),
            Some(1),
            "`{command}` should be refused\n{}",
            render_output(&output)
        );
        assert_eq!(
            bookmark_revision(&machine, "all"),
            revision_before,
            "`{command}` must not move the scope"
        );
    }

    assert!(
        !bookmark_has_file(&machine, "all", "../outside.conf"),
        "a path that climbs out of home must never become a repo entry"
    );
}

#[test]
fn committing_the_scope_graph_outside_all_is_an_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let config_path = ".config/dotsync/config.toml";
    let original = machine.read_file(config_path);
    machine.write_file(
        config_path,
        &format!("{original}\n# hyprland: wayland compositor config\n"),
    );
    let linux_before = bookmark_revision(&machine, "linux");

    // Dotsync only ever reads the scope graph from `all`. A copy recorded on
    // another scope configures nothing, but it still syncs into home on that
    // scope's machines, where it overwrites the real one.
    let wrong_scope = machine.run(&format!(
        "dotsync commit linux -m 'describe hyprland' -- {config_path}"
    ));
    assert_eq!(
        wrong_scope.status.code(),
        Some(1),
        "committing the scope graph to a non-all scope should be refused\n{}",
        render_output(&wrong_scope)
    );
    assert_eq!(
        bookmark_revision(&machine, "linux"),
        linux_before,
        "the refused commit must not move the scope"
    );

    let stderr = String::from_utf8_lossy(&wrong_scope.stderr).into_owned();
    assert!(
        stderr.contains("dotsync only reads it from `all`"),
        "the refusal must teach where the scope graph lives\n{}",
        render_output(&wrong_scope)
    );

    // The same change is fine on `all`, which is the only place it is read.
    machine.run_ok(&format!(
        "dotsync commit all -m 'describe hyprland' -- {config_path}"
    ));
    assert!(
        read_bookmark_file_contents(&machine, "all", config_path)
            .contains("# hyprland: wayland compositor config"),
        "the scope graph change should land on all"
    );
}

#[test]
fn commit_reports_every_unusable_path_at_once() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".apprc", "ui_theme = dark\n");

    // Reporting only the first bad path costs the agent one round trip per
    // mistake, and each round trip is a full fetch-and-commit attempt.
    let output = machine.run_expecting(
        "dotsync commit all -m mixed -- nonexistent-file '~/.apprc' .local/share/dotsync/repo",
        1,
    );

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    for expected in [
        "`nonexistent-file` matched nothing",
        "`~/.apprc` matched nothing",
        "`.local/share/dotsync/repo` is dotsync's hidden repo itself",
    ] {
        assert!(
            stderr.contains(expected),
            "one run should report every unusable path; missing {expected:?}\n{}",
            render_output(&output)
        );
    }
    assert!(
        !stderr.contains("is inside dotsync's hidden repo at"),
        "the repo root is the repo, not something inside it\n{}",
        render_output(&output)
    );
}

#[test]
fn commit_path_inside_dotsyncs_own_state_is_an_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let sync_state = machine.sync_state_relative_path();
    let sync_state = sync_state.to_str().expect("sync state path is UTF-8");
    assert!(
        machine.file_exists(sync_state),
        "init should have written the sync state file"
    );
    let revision_before = bookmark_revision(&machine, "all");

    // Both of these are dotsync's own bookkeeping sitting in home: the
    // machine-local sync state, and the hidden repo itself. Naming either used
    // to be filtered out of the selection without a word, so the commit
    // reported success having recorded nothing.
    for command in [
        format!("dotsync commit all -m state -- {sync_state}"),
        "dotsync commit all -m repo -- .local/share/dotsync/repo".to_string(),
    ] {
        let output = machine.run(&command);
        assert_eq!(
            output.status.code(),
            Some(1),
            "`{command}` should fail rather than report a successful empty commit\n{}",
            render_output(&output)
        );
        assert_eq!(
            bookmark_revision(&machine, "all"),
            revision_before,
            "`{command}` must not move the scope"
        );
    }

    let state_output = machine.run(&format!("dotsync commit all -m state -- {sync_state}"));
    assert_stderr_snapshot(
        &state_output,
        &format!(
            "\
dotsync: cannot commit that path

What dotsync does:
Dotsync records the home files you name onto a scope branch, then cascades that scope so every machine sharing it receives the change. Every file on a scope is written back into home on each of those machines.

This flow:
This commit flow resolves each path you name against your home directory, checks that it is a config file dotsync may record, and commits the ones that changed.

Expected:
It expects every path you name to be a config file inside your home directory, named relative to it, and to exist either in home or on the target scope already.

Current state found:
`{sync_state}` is this machine's dotsync sync state; it records which machine scope this home uses, so it has to stay machine-local.

Why dotsync stopped:
Dotsync stopped before recording anything. A commit records every path you named or none of them, so fixing the paths above and rerunning the same command is safe.

Correct flow:
- name paths relative to your home directory: `dotsync commit all -m \"message\" -- .config/fish/config.fish`.
- do not use `~/`, absolute paths, or `..`; dotsync resolves every path against your home directory already, and records it verbatim.
- commit the config files you edited instead; dotsync's own state is not config and cannot travel on a scope.
- to change which scopes exist, edit `.config/dotsync/config.toml` in home and commit that path to `all`.
- run `dotsync status` to see which managed files changed.
"
        ),
    );
}

#[test]
fn commit_explicit_path_adds_file_to_scope_and_syncs() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".config/existing.txt", "existing\n");
    machine.run_ok("dotsync");

    machine.write_file(".gitconfig", "[user]\nname = \"Max\"\n");

    machine.run_ok("dotsync commit all -m 'add gitconfig' -- .gitconfig");

    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
    assert!(machine.file_exists(".gitconfig"));
    assert_eq!(machine.read_file(".gitconfig"), "[user]\nname = \"Max\"\n");
}

#[test]
fn commit_modifies_existing_file_on_scope() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "linux", ".bashrc", "export PATH=\"$PATH\"\n");
    machine.run_ok("dotsync");

    machine.write_file(".bashrc", "export PATH=\"$HOME/bin:$PATH\"\n");
    machine.write_sync_state_raw(&format!(
        "{{\"machine_scope\":\"all\",\"last_synced_revision\":\"{}\"}}",
        bookmark_revision(&machine, "all")
    ));

    machine.run_ok("dotsync commit linux -m 'update bashrc' -- .bashrc");

    assert_eq!(
        read_bookmark_file_contents(&machine, "linux", ".bashrc"),
        "export PATH=\"$HOME/bin:$PATH\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".bashrc"),
        "export PATH=\"$HOME/bin:$PATH\"\n"
    );
    assert_eq!(
        machine.read_file(".bashrc"),
        "export PATH=\"$HOME/bin:$PATH\"\n"
    );
}

#[test]
fn commit_deletes_file_from_scope() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "all", ".config/remove-me.txt", "delete me\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    machine.run_ok("dotsync");
    assert!(machine.file_exists(".config/remove-me.txt"));

    machine.delete_file(".config/remove-me.txt");

    machine.run_ok("dotsync commit all -m 'remove file' -- .config/remove-me.txt");

    assert!(!bookmark_has_file(&machine, "all", ".config/remove-me.txt"));
    assert!(!bookmark_has_file(
        &machine,
        "mx-xps-cy",
        ".config/remove-me.txt"
    ));
    assert!(!machine.file_exists(".config/remove-me.txt"));
}

#[test]
fn commit_cascades_through_all_descendants() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    add_hyprland_scope(&machine);
    seed_remote_scope_file(&machine, "all", ".config/all-only.txt", "all\n");
    seed_remote_scope_file(&machine, "linux", ".config/linux-only.txt", "linux\n");
    seed_remote_scope_file(
        &machine,
        "hyprland",
        ".config/hyprland-only.txt",
        "hyprland\n",
    );
    machine.run_ok("dotsync");

    machine.write_file(".config/shared.txt", "shared everywhere\n");

    machine.run_ok("dotsync commit all -m 'add shared file' -- .config/shared.txt");

    for scope in ["all", "linux", "hyprland", "mx-xps-cy"] {
        assert_eq!(
            read_bookmark_file_contents(&machine, scope, ".config/shared.txt"),
            "shared everywhere\n",
            "expected `.config/shared.txt` to cascade to `{scope}`"
        );
    }
}

#[test]
fn multiple_machines_can_contribute_to_all_without_losing_changes() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    machine_a.init_ok();
    machine_b.init_ok();
    machine_a.run_ok("dotsync --force");

    machine_a.write_file(".config/shared-a.conf", "from machine a\n");
    machine_a.run_ok("dotsync commit all -m 'add shared a' -- .config/shared-a.conf");

    machine_b.run_ok("dotsync");
    assert_eq!(
        machine_b.read_file(".config/shared-a.conf"),
        "from machine a\n"
    );

    machine_b.write_file(".config/shared-b.conf", "from machine b\n");
    machine_b.run_ok("dotsync commit all -m 'add shared b' -- .config/shared-b.conf");

    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".config/shared-a.conf"),
        "from machine a\n"
    );
    assert_eq!(
        machine_a.read_file(".config/shared-b.conf"),
        "from machine b\n"
    );
    assert_eq!(
        machine_b.read_file(".config/shared-a.conf"),
        "from machine a\n"
    );
    assert_eq!(
        machine_b.read_file(".config/shared-b.conf"),
        "from machine b\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "all", ".config/shared-a.conf"),
        "from machine a\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "all", ".config/shared-b.conf"),
        "from machine b\n"
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
fn commit_to_machine_scope_does_not_cascade() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".config/machine-local.txt", "machine only\n");

    machine.run_ok("dotsync commit mx-xps-cy -m 'add machine file' -- .config/machine-local.txt");

    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".config/machine-local.txt"),
        "machine only\n"
    );
    assert!(!bookmark_has_file(
        &machine,
        "linux",
        ".config/machine-local.txt"
    ));
    assert!(!bookmark_has_file(
        &machine,
        "all",
        ".config/machine-local.txt"
    ));
}

#[test]
fn commit_without_paths_imports_all_diffs() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".config/app.conf",
        "setting = \"original\"\n",
    );
    machine.run_ok("dotsync");

    machine.write_file(".config/app.conf", "setting = \"updated\"\n");

    machine.run_ok("dotsync commit mx-xps-cy -m update");

    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".config/app.conf"),
        "setting = \"updated\"\n"
    );
}

#[test]
fn commit_noop_when_no_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".config/unchanged.txt", "same\n");
    machine.run_ok("dotsync");

    let revision_before = bookmark_revision(&machine, "mx-xps-cy");

    machine.run_expecting("dotsync commit mx-xps-cy -m noop", 0);

    let revision_after = bookmark_revision(&machine, "mx-xps-cy");
    assert_eq!(revision_after, revision_before);
}

#[test]
fn noop_commit_names_the_scope_it_targeted() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // The report for a commit that found nothing was default-constructed, so
    // it did not carry the scope the agent had just named - and the message
    // interpolated the empty string into "committed  and synced". It now names
    // the scope, and says what it did instead of claiming a commit.
    let commit_output = machine.run_expecting("dotsync commit mx-xps-cy -m noop", 0);
    assert_stderr_snapshot(
        &commit_output,
        "dotsync: nothing to record on `mx-xps-cy`; no commit was made and home was not synced\n",
    );
}

#[test]
fn commit_invalid_scope_errors() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let commit_output =
        machine.run_expecting("dotsync commit nonexistent -m test -- .gitconfig", 1);

    assert_stderr_snapshot(
        &commit_output,
        "\
dotsync: invalid scope

What dotsync does:
Dotsync stores dotfiles in a scope DAG so shared config can live on shared ancestor scopes and machine-specific config can stay isolated on leaf scopes.

This flow:
This flow resolves the scope you named against the scope graph, which dotsync reads from `.config/dotsync/config.toml` on the `all` scope.

Expected:
It expects the scope you name to exist in that graph.

Current state found:
scope `nonexistent` does not exist in config

Why dotsync stopped:
Dotsync stopped because there is no such scope: it can neither place a change on one nor show you what one holds.

Correct flow:
- run `dotsync view` to list the scopes that do exist.
- then name one of those. For a commit, pick the root-est appropriate ancestor scope that should own the change.
"
    );
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

    let expected = r#"{"current_state":["expected repo path: {repo}; standard location: ~/.local/share/dotsync/repo"],"drifts":[],"error":"not_initialized","forced_overwrites":[],"message":"Dotsync could not find its hidden repo at {repo}. Run `dotsync init <remote-url>` from this home directory first.","status":"error"}
"#
    .replace("{repo}", &machine.repo_dir.display().to_string());
    let stdout = String::from_utf8_lossy(&status_output.stdout);
    assert_eq!(stdout, expected, "{}", render_output(&status_output));
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
fn status_shows_modified_file() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    machine.run_ok("dotsync");

    machine.write_file(".bashrc", "export DOTSYNC=modified\n");

    let status_output = machine.run_expecting("dotsync status", 0);

    assert_stderr_snapshot(
        &status_output,
        "\
dotsync: 1 changed managed file(s) for mx-xps-cy
  M .bashrc (edited here since the last sync)
",
    );
}

#[test]
fn force_is_refused_with_one_message_wherever_it_is_meaningless() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // `--force` reaches exactly one decision: whether to overwrite drifted
    // home files. On the commands that never write home it means nothing, and
    // it has to say so in whichever position the agent wrote it - `--output`
    // works after the subcommand, so that is the position agents will reach
    // for.
    for command in [
        "dotsync status --force",
        "dotsync --force status",
        "dotsync diff --force",
        "dotsync --force diff",
        "dotsync view --force",
        "dotsync --force view",
        "dotsync init --force",
        "dotsync --force init",
        "dotsync abort --force",
        "dotsync --force abort",
    ] {
        let output = machine.run(command);
        assert_eq!(
            output.status.code(),
            Some(2),
            "`{command}` should be a usage error\n{}",
            render_output(&output)
        );
        let name = command
            .split_whitespace()
            .find(|word| !matches!(*word, "dotsync" | "--force"))
            .expect("command name");
        assert_stderr_snapshot(
            &output,
            &format!(
                "dotsync: `--force` has no meaning for `{name}`; it only decides whether to overwrite drifted files in your home directory, which is a choice made by plain `dotsync`, `commit`, and `continue`\n"
            ),
        );
    }

    // The commands that do write home keep it, in both positions.
    machine.write_file(".apprc", "ui_theme = dark\n");
    machine.run_ok("dotsync commit all -m 'add apprc' --force -- .apprc");
    machine.write_file(".apprc", "ui_theme = light\n");
    machine.run_ok("dotsync --force commit all -m 'light theme' -- .apprc");
    machine.run_ok("dotsync --force");
}

#[test]
fn output_format_is_accepted_after_the_subcommand() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // `--force` is global and `--output` was not, so the two flags on the same
    // struct had opposite positional rules and neither one said so.
    let status_output = machine.run_expecting("dotsync status --output json", 0);

    let payload = parse_stdout_json(&status_output);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["command"], "status");
    assert_eq!(payload["machine_scope"], "mx-xps-cy");
}

#[test]
fn clap_usage_errors_emit_the_json_contract() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    // Clap's own usage errors used to exit 2 with plain text and nothing on
    // stdout, so an agent driving dotsync with `--output json` got no JSON at
    // all for the most common mistake it can make.
    for command in [
        "dotsync commit --output json",
        "dotsync --output json commit",
        "dotsync --output json commit all --nosuchflag",
    ] {
        let output = machine.run(command);
        assert_eq!(
            output.status.code(),
            Some(2),
            "`{command}` should be a usage error\n{}",
            render_output(&output)
        );

        let payload = parse_stdout_json(&output);
        assert_eq!(payload["status"], "error", "`{command}`");
        assert_eq!(payload["error"], "usage", "`{command}`");
        assert!(
            payload["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "`{command}` should explain the usage error in its JSON message\n{}",
            render_output(&output)
        );
        assert!(
            !output.stderr.is_empty(),
            "`{command}` should still explain itself on stderr\n{}",
            render_output(&output)
        );
    }
}

#[test]
fn status_shows_deleted_file() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    machine.run_ok("dotsync");

    machine.delete_file(".bashrc");

    let status_output = machine.run_expecting("dotsync status", 0);

    assert_stderr_snapshot(
        &status_output,
        "\
dotsync: 1 changed managed file(s) for mx-xps-cy
  D .bashrc (deleted here since the last sync)
",
    );
}

#[test]
fn status_clean_shows_no_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    machine.run_ok("dotsync");

    let status_output = machine.run_expecting("dotsync status", 0);

    assert_stderr_snapshot(&status_output, "dotsync: no changes for mx-xps-cy\n");
}

#[test]
fn status_json_contract() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    machine.run_ok("dotsync");

    machine.write_file(".bashrc", "export DOTSYNC=modified\n");

    let status_output = machine.run_expecting("dotsync --output json status", 0);

    let json = parse_stdout_json(&status_output);
    assert_eq!(json["status"], "ok");
    assert_eq!(json["command"], "status");
    assert_eq!(json["machine_scope"], "mx-xps-cy");

    let changes = json["changes"]
        .as_array()
        .expect("changes should be an array");
    assert!(
        changes.iter().any(|change| {
            change["path"]
                .as_str()
                .is_some_and(|path| path.contains(".bashrc"))
                && change["state"] == "modified"
        }),
        "expected .bashrc modified entry\n{}",
        render_output(&status_output)
    );
}

#[test]
fn status_ignores_unmanaged_files() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".unmanaged-status-test", "this file is unmanaged\n");

    let status_output = machine.run_expecting("dotsync status", 0);

    assert_stderr_snapshot(&status_output, "dotsync: no changes for mx-xps-cy\n");
}

// Issue #19: an interrupted push leaves local scope bookmarks ahead of the
// remote. That is normal VCS state — unpushed commits — and no dotsync command
// may treat it as a fetch conflict.

#[test]
fn interrupted_push_reports_that_scope_updates_were_not_pushed() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".config/fish/dev-certs.fish", "set -gx DEV_CERTS 1\n");
    block_remote_pushes(&machine);
    let commit_output = machine.run(
        "dotsync --output json commit all -m 'add dev-certs helper' -- .config/fish/dev-certs.fish",
    );
    allow_remote_pushes(&machine);

    assert_ne!(
        bookmark_revision(&machine, "all"),
        remote_branch_revision(&machine, "all"),
        "this test needs a push that really was rejected"
    );

    // The exit code is deliberately not asserted: whether a rejected push is an
    // error or just deferred convergence is a work item 2 question. What must
    // be true either way is that the run names the scopes the remote does not
    // have, both to the user and to an agent reading JSON.
    let json = parse_stdout_json(&commit_output);
    assert_eq!(
        json["unpushed_scopes"],
        serde_json::json!(["all", "linux", "mx-xps-cy"]),
        "{}",
        render_output(&commit_output)
    );

    let stderr = String::from_utf8_lossy(&commit_output.stderr);
    for scope in ["all", "linux", "mx-xps-cy"] {
        assert!(
            stderr.contains(scope),
            "the human output must name the unpushed scope `{scope}`: {}",
            render_output(&commit_output)
        );
    }
}

#[test]
fn status_works_while_local_scopes_are_ahead_of_remote() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    interrupt_push_after_cascade(
        &machine,
        ".config/fish/dev-certs.fish",
        "set -gx DEV_CERTS 1\n",
    );

    let status_output = machine.run("dotsync status");
    assert_eq!(
        status_output.status.code(),
        Some(0),
        "`dotsync status` must keep working while local scopes are unpushed: {}",
        render_output(&status_output)
    );
    assert_stderr_snapshot(&status_output, "dotsync: no changes for mx-xps-cy\n");
}

#[test]
fn view_works_while_local_scopes_are_ahead_of_remote() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    interrupt_push_after_cascade(
        &machine,
        ".config/fish/dev-certs.fish",
        "set -gx DEV_CERTS 1\n",
    );

    let view_output = machine.run("dotsync view");
    assert_eq!(
        view_output.status.code(),
        Some(0),
        "`dotsync view` must keep working while local scopes are unpushed: {}",
        render_output(&view_output)
    );
    assert!(
        String::from_utf8_lossy(&view_output.stdout).contains(".config/fish/dev-certs.fish"),
        "`dotsync view` should show the locally committed file: {}",
        render_output(&view_output)
    );
}

#[test]
fn diff_works_while_local_scopes_are_ahead_of_remote() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    interrupt_push_after_cascade(
        &machine,
        ".config/fish/dev-certs.fish",
        "set -gx DEV_CERTS 1\n",
    );

    let diff_output = machine.run("dotsync diff");
    assert_eq!(
        diff_output.status.code(),
        Some(0),
        "`dotsync diff` must keep working while local scopes are unpushed, and home matches the repo here: {}",
        render_output(&diff_output)
    );
}

#[test]
fn plain_sync_pushes_scopes_left_unpushed_by_an_interrupted_commit() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    interrupt_push_after_cascade(
        &machine,
        ".config/fish/dev-certs.fish",
        "set -gx DEV_CERTS 1\n",
    );

    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "running dotsync again is the documented remedy for an interrupted push: {}",
        render_output(&sync_output)
    );

    for scope in ["all", "linux", "mx-xps-cy"] {
        assert_eq!(
            remote_branch_revision(&machine, scope),
            bookmark_revision(&machine, scope),
            "`dotsync` should have pushed the pending `{scope}` bookmark"
        );
    }
    assert_eq!(
        remote_branch_file_contents(&machine, "all", ".config/fish/dev-certs.fish"),
        "set -gx DEV_CERTS 1\n"
    );
}

#[test]
fn commit_with_nothing_to_commit_still_publishes_unpushed_scopes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    interrupt_push_after_cascade(
        &machine,
        ".config/fish/dev-certs.fish",
        "set -gx DEV_CERTS 1\n",
    );

    // Committing a path that already matches the scope adds no history — but
    // the run still has to publish what the interrupted run left behind.
    let commit_output = machine
        .run_ok("dotsync --output json commit all -m 'no change' -- .config/fish/dev-certs.fish");

    for scope in ["all", "linux", "mx-xps-cy"] {
        assert_eq!(
            remote_branch_revision(&machine, scope),
            bookmark_revision(&machine, scope),
            "a commit with nothing to add must still publish the pending `{scope}` bookmark"
        );
    }
    assert_eq!(
        remote_branch_file_contents(&machine, "all", ".config/fish/dev-certs.fish"),
        "set -gx DEV_CERTS 1\n"
    );

    let json = parse_stdout_json(&commit_output);
    assert_eq!(
        json["unpushed_scopes"],
        serde_json::json!([]),
        "{}",
        render_output(&commit_output)
    );
}

#[test]
fn commit_pushes_scopes_left_unpushed_by_an_interrupted_commit() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    interrupt_push_after_cascade(
        &machine,
        ".config/fish/dev-certs.fish",
        "set -gx DEV_CERTS 1\n",
    );

    machine.write_file(".config/fish/aliases.fish", "alias ll 'ls -l'\n");
    let commit_output =
        machine.run("dotsync commit all -m 'add aliases' -- .config/fish/aliases.fish");
    assert!(
        commit_output.status.success(),
        "a later commit must work on a machine with unpushed scopes: {}",
        render_output(&commit_output)
    );

    for scope in ["all", "linux", "mx-xps-cy"] {
        assert_eq!(
            remote_branch_revision(&machine, scope),
            bookmark_revision(&machine, scope),
            "`dotsync commit` should have pushed the pending `{scope}` bookmark"
        );
    }
    assert_eq!(
        remote_branch_file_contents(&machine, "all", ".config/fish/dev-certs.fish"),
        "set -gx DEV_CERTS 1\n",
        "the stranded commit must reach the remote too, not just the new one"
    );
    assert_eq!(
        remote_branch_file_contents(&machine, "all", ".config/fish/aliases.fish"),
        "alias ll 'ls -l'\n"
    );
}

#[test]
fn drift_stop_during_commit_does_not_strand_unpushed_history() {
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

    // Unrelated drift that the commit does not select: the home sync will stop
    // on it after the cascade transaction has already created history.
    machine.write_file(".gitconfig", "[user]\nname = \"Drifted\"\n");
    machine.write_file(".config/fish/dev-certs.fish", "set -gx DEV_CERTS 1\n");

    let commit_output =
        machine.run("dotsync commit all -m 'add dev-certs helper' -- .config/fish/dev-certs.fish");
    assert_eq!(
        commit_output.status.code(),
        Some(1),
        "the unrelated drift should still stop the home sync: {}",
        render_output(&commit_output)
    );
    assert!(
        render_output(&commit_output).contains("drift"),
        "the stop should be the drift stop, not something else: {}",
        render_output(&commit_output)
    );

    for scope in ["all", "linux", "mx-xps-cy"] {
        assert_eq!(
            remote_branch_revision(&machine, scope),
            bookmark_revision(&machine, scope),
            "push must happen before the home sync, so a drift stop cannot strand `{scope}` history"
        );
    }
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
fn diverged_leaf_scope_keeps_the_local_commit_and_the_home_file() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // A leaf scope diverges on its own: nothing else is local-ahead, so
    // reconciliation reaches this scope with no other scope to stop on first.
    machine.write_file(".config/fish/dev-certs.fish", "set -gx DEV_CERTS 1\n");
    block_remote_pushes(&machine);
    machine
        .run("dotsync commit mx-xps-cy -m 'add dev-certs helper' -- .config/fish/dev-certs.fish");
    allow_remote_pushes(&machine);
    assert_ne!(
        bookmark_revision(&machine, "mx-xps-cy"),
        remote_branch_revision(&machine, "mx-xps-cy"),
        "this test needs a push that really was rejected"
    );
    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".config/other-machine.conf",
        "from another machine\n",
    );

    machine.run("dotsync");

    assert!(
        bookmark_has_file(&machine, "mx-xps-cy", ".config/fish/dev-certs.fish"),
        "the unpushed commit must survive a fetch that cannot be reconciled"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".config/fish/dev-certs.fish"),
        "set -gx DEV_CERTS 1\n"
    );
    assert!(
        machine.file_exists(".config/fish/dev-certs.fish"),
        "the home file must survive too — dotsync must never delete a managed file to reconcile bookmarks"
    );
}

#[test]
fn diverged_scope_bookmark_is_reported_as_divergence_not_as_overwrite() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    interrupt_push_after_cascade(
        &machine,
        ".config/fish/dev-certs.fish",
        "set -gx DEV_CERTS 1\n",
    );
    // Another machine pushes to `all` while this machine holds unpushed work on
    // it, so `all` genuinely diverges while `linux` and `mx-xps-cy` are merely
    // local-ahead.
    seed_remote_scope_file(
        &machine,
        "all",
        ".config/other-machine.conf",
        "from another machine\n",
    );

    let sync_output = machine.run("dotsync");
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "true divergence is still an error until the convergence pass lands: {}",
        render_output(&sync_output)
    );

    // The exact wording is the implementer's choice, but the error must name
    // divergence and the diverged scope, and must not reuse the phrasing that
    // misdescribed ordinary unpushed work as a fetch conflict.
    let rendered = render_output(&sync_output);
    assert!(
        rendered.to_lowercase().contains("diverge"),
        "a diverged scope must be described as divergence: {rendered}"
    );
    assert!(
        rendered.contains("`all`"),
        "the diverged scope must be named: {rendered}"
    );
}

#[test]
fn tdd_ratchet_gatekeeper() {
    if std::env::var("TDD_RATCHET").is_err() {
        panic!("Run tdd-ratchet instead of cargo test.");
    }
}

#[test]
fn selected_add_modify_and_delete_are_applied_without_touching_unselected_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(
        &machine,
        "all",
        ".config/fish/config.fish",
        "set -g fish_greeting on\n",
    );
    seed_remote_scope_file(&machine, "all", ".config/fish/removed.fish", "remove me\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    machine.run_ok("dotsync");

    machine.write_file(".config/fish/config.fish", "set -g fish_greeting off\n");
    machine.write_file(".config/fish/completions/git.fish", "complete -c git\n");
    machine.delete_file(".config/fish/removed.fish");

    machine.run_ok("dotsync commit all -m 'update fish dir' -- .config/fish/");

    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".config/fish/config.fish"),
        "set -g fish_greeting off\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".config/fish/completions/git.fish"),
        "complete -c git\n"
    );
    assert!(!bookmark_has_file(
        &machine,
        "all",
        ".config/fish/removed.fish"
    ));
    assert_eq!(
        machine.read_file(".config/fish/config.fish"),
        "set -g fish_greeting off\n"
    );
    assert_eq!(
        machine.read_file(".config/fish/completions/git.fish"),
        "complete -c git\n"
    );
    assert!(!machine.file_exists(".config/fish/removed.fish"));
}

// --- Wave 1: the three-way drift authority -------------------------------
//
// Every test below distinguishes three sides of one managed path: what dotsync
// last synced to this machine, what is in home now, and what the machine
// scope's tip holds now. Before Wave 1 each consumer answered "what differs"
// against a different baseline, which is what made DL-1 (a machine that is
// merely behind silently reverting another machine's published work)
// representable at all.

#[test]
fn a_stale_home_file_cannot_be_committed_over_another_machines_change() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    // B adds a line and publishes it. A has done nothing since the seed: its
    // home `.apprc` is not edited, it is simply behind.
    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\nsize = 14\n");
    machine_b.run_ok("dotsync commit all -m 'add size' -- .apprc");
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".apprc"),
        "ui_theme = dark\nfont = mono\nsize = 14\n"
    );

    // The taught workflow starts with `status`, and every dotsync command
    // fetches on entry. Whatever `status` reports, the commit that follows must
    // not re-record A's older content on top of B's published change.
    machine_a.run_ok("dotsync status");

    let commit_a = machine_a.run("dotsync commit all -m 'commit what status showed' -- .apprc");
    assert_eq!(
        commit_a.status.code(),
        Some(1),
        "committing a file this machine has not edited must be refused\n{}",
        render_output(&commit_a)
    );
    let stderr = String::from_utf8_lossy(&commit_a.stderr).into_owned();
    assert!(
        stderr.contains("has not been edited here"),
        "the refusal must say the file was not edited here\n{stderr}"
    );
    assert!(
        stderr.contains("run `dotsync` to bring this machine up to date"),
        "the refusal must point at plain `dotsync`\n{stderr}"
    );

    assert_eq!(
        remote_branch_file_contents(&machine_a, "all", ".apprc"),
        "ui_theme = dark\nfont = mono\nsize = 14\n",
        "the refused commit must leave the other machine's published change alone"
    );

    // The taught recovery works: sync, then edit, then commit.
    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".apprc"),
        "ui_theme = dark\nfont = mono\nsize = 14\n"
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

#[test]
fn commit_force_applies_to_the_named_paths_and_not_to_unrelated_drift() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".gitconfig", "[user]\nname = Repo\n");
    seed_remote_scope_file(&machine, "mx-xps-cy", ".config/app.conf", "setting = one\n");
    machine.run_ok("dotsync");

    machine.write_file(".gitconfig", "[user]\nname = Drifted\n");
    machine.write_file(".config/app.conf", "setting = two\n");

    let commit_output =
        machine.run("dotsync commit mx-xps-cy -m 'update app' --force -- .config/app.conf");
    assert_eq!(
        commit_output.status.code(),
        Some(1),
        "`--force` on commit covers the paths it names, so unrelated drift still stops the home sync\n{}",
        render_output(&commit_output)
    );
    assert_eq!(
        machine.read_file(".gitconfig"),
        "[user]\nname = Drifted\n",
        "`--force` on a commit must not revert a file the commit never named"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".config/app.conf"),
        "setting = two\n",
        "the named change is still recorded"
    );
}

#[test]
fn forcing_a_stale_commit_records_what_it_overwrote() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\nsize = 14\n");
    machine_b.run_ok("dotsync commit all -m 'add size' -- .apprc");

    machine_a.run_ok("dotsync status");

    let commit_a = machine_a
        .run_ok("dotsync --output json commit all -m 'revert on purpose' --force -- .apprc");

    let json = parse_stdout_json(&commit_a);
    assert_eq!(
        json["forced_overwrites"]
            .as_array()
            .expect("forced_overwrites should be an array"),
        &vec![serde_json::Value::from(".apprc")],
        "a forced overwrite of an incoming change has to be on the record\n{}",
        render_output(&commit_a)
    );
    assert_eq!(
        remote_branch_file_contents(&machine_a, "all", ".apprc"),
        "ui_theme = dark\nfont = mono\n"
    );
}

#[test]
fn diff_reports_an_inserted_line_as_a_single_addition() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".config/app.conf",
        "one\ntwo\nthree\n",
    );
    machine.run_ok("dotsync");

    machine.write_file(".config/app.conf", "one\ninserted\ntwo\nthree\n");

    let diff_output = machine.run_expecting("dotsync diff", 1);
    let stderr = String::from_utf8_lossy(&diff_output.stderr).into_owned();
    assert!(
        stderr.contains("+inserted"),
        "the inserted line should be reported as an addition\n{stderr}"
    );
    assert!(
        !stderr.contains("-two") && !stderr.contains("-three"),
        "a real line diff aligns the unchanged tail instead of rewriting it\n{stderr}"
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
fn committing_a_path_another_machine_deleted_is_refused() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    // B removes the file and publishes the removal. A has not synced since.
    machine_b.delete_file(".apprc");
    machine_b.run_ok("dotsync commit all -m 'drop apprc' -- .apprc");

    machine_a.run_ok("dotsync status");

    let commit_a = machine_a.run("dotsync commit all -m 'keep apprc' -- .apprc");
    assert_eq!(
        commit_a.status.code(),
        Some(1),
        "naming a file another machine deleted must not quietly record nothing\n{}",
        render_output(&commit_a)
    );
    let stderr = String::from_utf8_lossy(&commit_a.stderr).into_owned();
    assert!(
        stderr.contains("deleted on another machine"),
        "the refusal must say what happened to the file\n{stderr}"
    );
    assert!(
        stderr.contains("run `dotsync` to bring this machine up to date"),
        "the refusal must point at plain `dotsync`\n{stderr}"
    );

    // Applying the deletion is one way out.
    machine_a.run_ok("dotsync");
    assert!(!machine_a.file_exists(".apprc"));

    // Putting it back on purpose is the other, and it says so in the JSON.
    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\n");
    machine_b.run_ok("dotsync --output json commit all -m 'put it back' -- .apprc");
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".apprc"),
        "ui_theme = dark\nfont = mono\n"
    );
}

#[test]
fn a_forced_overwrite_is_reported_even_when_the_run_then_fails() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    machine_a.write_file(".config/other.conf", "other = base\n");
    machine_a.run_ok("dotsync commit all -m 'add other' -- .config/other.conf");
    machine_b.run_ok("dotsync");

    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\nsize = 14\n");
    machine_b.run_ok("dotsync commit all -m 'add size' -- .apprc");

    // A forces the revert of `.apprc`, and separately has drift on a file the
    // commit does not name — so the commit's own home sync stops after the
    // forced history has already been written and pushed.
    machine_a.run_ok("dotsync status");
    machine_a.write_file(".config/other.conf", "other = drifted\n");

    let commit_a =
        machine_a.run("dotsync --output json commit all -m 'revert apprc' --force -- .apprc");
    assert_eq!(
        commit_a.status.code(),
        Some(1),
        "unrelated drift still stops the home sync\n{}",
        render_output(&commit_a)
    );
    assert_eq!(
        remote_branch_file_contents(&machine_a, "all", ".apprc"),
        "ui_theme = dark\nfont = mono\n",
        "the forced overwrite really did happen before the run stopped"
    );

    let json = parse_stdout_json(&commit_a);
    assert_eq!(
        json["forced_overwrites"]
            .as_array()
            .expect("forced_overwrites should be an array on the error path too"),
        &vec![serde_json::Value::from(".apprc")],
        "a run that overwrote someone else's change must say so whether or not it then finished\n{}",
        render_output(&commit_a)
    );
}

#[test]
fn a_successful_forced_commit_says_what_it_overwrote() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\nsize = 14\n");
    machine_b.run_ok("dotsync commit all -m 'add size' -- .apprc");

    machine_a.run_ok("dotsync status");

    // Succeeding is not a reason to stay quiet. A run that reverted another
    // machine's published change has to say so on the way past, exactly as it
    // does when it goes on to fail — the successful one is the commoner case.
    let commit_a = machine_a.run_ok("dotsync commit all -m 'revert on purpose' --force -- .apprc");
    assert_stderr_snapshot(
        &commit_a,
        "\
dotsync: recorded 1 file(s) over an incoming change, because you passed `--force`
- .apprc
dotsync: committed all and synced 2 file(s)
",
    );
}

// DESIGN.md, "The convergence model": "Offline is just deferred convergence.
// If fetch fails due to network, dotsync skips it and proceeds against
// last-known remote state." A machine that cannot reach the remote is in an
// ordinary state, not a broken one — so no command refuses to run because of
// it, and every command says which state it is reporting against.

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

/// Naming a directory says "commit what changed under here", which is what a
/// bare `dotsync commit <scope>` says about the whole machine — so it filters
/// like one. Naming a path exactly is a claim about that path, and a claim is
/// what deserves an argument.
#[test]
fn a_directory_selection_records_what_this_machine_changed_and_says_what_it_skipped() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".config/fish/config.fish", "set -g theme dark\n");
    machine_a.write_file(".config/fish/aliases.fish", "alias ll 'ls -l'\n");
    machine_a.run_ok("dotsync commit all -m 'seed fish config' -- .config/fish/");
    machine_b.run_ok("dotsync");

    // B publishes a change to one file under that directory. A has not synced
    // it, and has an edit of its own to a different file under there, plus a
    // brand new file it wants to add.
    machine_b.write_file(".config/fish/aliases.fish", "alias ll 'ls -lah'\n");
    machine_b.run_ok("dotsync commit all -m 'better ll' -- .config/fish/aliases.fish");

    machine_a.write_file(".config/fish/config.fish", "set -g theme light\n");
    machine_a.write_file(
        ".config/fish/functions.fish",
        "function gs; git status; end\n",
    );

    // Named exactly, B's file is still refused: that is a claim that home's
    // copy should win, and it would revert what B published.
    let named_exactly =
        machine_a.run("dotsync commit all -m 'take mine' -- .config/fish/aliases.fish");
    assert_eq!(
        named_exactly.status.code(),
        Some(1),
        "naming a path another machine changed must still be refused\n{}",
        render_output(&named_exactly)
    );

    let directory_commit =
        machine_a.run("dotsync commit all -m 'light theme and functions' -- .config/fish/");
    assert_eq!(
        directory_commit.status.code(),
        Some(0),
        "a directory selection must commit what changed under it rather than refuse\n{}",
        render_output(&directory_commit)
    );
    let stderr = String::from_utf8_lossy(&directory_commit.stderr).into_owned();
    assert!(
        stderr.contains(".config/fish/aliases.fish"),
        "the run must say which file under the directory it left alone\n{stderr}"
    );

    assert_eq!(
        remote_branch_file_contents(&machine_a, "all", ".config/fish/config.fish"),
        "set -g theme light\n",
        "the edit this machine made must be recorded"
    );
    assert_eq!(
        remote_branch_file_contents(&machine_a, "all", ".config/fish/functions.fish"),
        "function gs; git status; end\n",
        "a new file under a named directory must still be added"
    );
    assert_eq!(
        remote_branch_file_contents(&machine_a, "all", ".config/fish/aliases.fish"),
        "alias ll 'ls -lah'\n",
        "the other machine's published change must survive the directory commit"
    );
    assert_eq!(
        machine_a.read_file(".config/fish/aliases.fish"),
        "alias ll 'ls -lah'\n",
        "and the sync that follows must bring this machine up to it"
    );
}

/// A run that stops still has to say which state it stopped against. Drift is
/// the commonest stop there is, and its advice is `--force` — overwrite home
/// with the repo — so a reader who is not told the repo snapshot is however
/// old this machine's last fetch was cannot judge that advice.
#[test]
fn a_run_that_stops_offline_still_says_the_remote_was_out_of_reach() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();
    machine.write_file(".bashrc", "export DOTSYNC=one\n");
    machine.run_ok("dotsync commit all -m 'add bashrc' -- .bashrc");

    machine.write_file(".bashrc", "export DOTSYNC=edited\n");
    harness.disconnect_remote();

    let sync_output = machine.run("dotsync --output json");
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "drift still stops the run\n{}",
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

/// Naming a directory is how new files get onto a scope in bulk, and adding a
/// file to a shared scope is the one thing in a commit that every other
/// machine will then have written into its home. A run that does it silently
/// reads exactly like a run that changed one line.
#[test]
fn a_commit_says_which_files_it_started_tracking() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".config/fish/config.fish", "set -g fish_greeting off\n");
    machine.write_file(".config/fish/aliases.fish", "alias ll 'ls -l'\n");
    let commit_output = machine.run_expecting(
        "dotsync --output json commit all -m 'add fish config' -- .config/fish/",
        0,
    );

    let newly_tracked = parse_stdout_json(&commit_output)["newly_tracked"]
        .as_array()
        .expect("newly_tracked should be an array")
        .iter()
        .filter_map(|path| path.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    assert_eq!(
        newly_tracked,
        vec![
            ".config/fish/aliases.fish".to_string(),
            ".config/fish/config.fish".to_string()
        ],
        "a commit must report the files it put on the scope for the first time\n{}",
        render_output(&commit_output)
    );
    let stderr = String::from_utf8_lossy(&commit_output.stderr).into_owned();
    assert!(
        stderr.contains(".config/fish/aliases.fish"),
        "and say so in words too\n{stderr}"
    );

    // Editing a file that is already on the scope is not starting to track it.
    machine.write_file(".config/fish/config.fish", "set -g fish_greeting on\n");
    let edit_output = machine.run_expecting(
        "dotsync --output json commit all -m 'flip greeting' -- .config/fish/",
        0,
    );
    assert_eq!(
        parse_stdout_json(&edit_output)["newly_tracked"]
            .as_array()
            .expect("newly_tracked should be an array")
            .len(),
        0,
        "an edit to a tracked file is not a new file\n{}",
        render_output(&edit_output)
    );
}

/// `dotsync commit all -m msg -- .` reads like "commit everything", and what
/// it actually does is walk the whole home directory and publish it: ssh keys,
/// `.netrc`, browser profiles, anything. Nothing about the run says so, and
/// once it is on the remote it is on every machine that shares the scope.
#[test]
fn a_selection_that_names_the_whole_home_directory_is_refused() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".ssh/id_ed25519", "PRIVATE KEY\n");
    machine.write_file(".netrc", "machine example.com login me password hunter2\n");
    machine.write_file(".bashrc", "export DOTSYNC=1\n");

    for selection in [".", "./"] {
        let output = machine.run(&format!(
            "dotsync commit all -m 'everything' -- {selection}"
        ));
        assert_eq!(
            output.status.code(),
            Some(1),
            "`{selection}` names the whole home directory and must be refused\n{}",
            render_output(&output)
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("home directory"),
            "the refusal must say what was named\n{stderr}"
        );
    }

    let absolute = machine.run(&format!(
        "dotsync commit all -m 'everything' -- {}",
        machine.home_dir.display()
    ));
    assert_eq!(
        absolute.status.code(),
        Some(1),
        "naming home by its absolute path must be refused too\n{}",
        render_output(&absolute)
    );

    assert!(
        !bookmark_has_file(&machine, "all", ".ssh/id_ed25519"),
        "no refused sweep may have recorded a private key"
    );
    assert!(!bookmark_has_file(&machine, "all", ".netrc"));

    // Naming a real directory still works, and still only reaches under it.
    machine.write_file(".config/app/settings.toml", "theme = \"dark\"\n");
    machine.run_ok("dotsync commit all -m 'app settings' -- .config/app/");
    assert!(bookmark_has_file(
        &machine,
        "all",
        ".config/app/settings.toml"
    ));
    assert!(!bookmark_has_file(&machine, "all", ".netrc"));
}

/// Shells complete directories with a trailing separator, so agents and people
/// both type them. `.config/fish/` already worked; `.bashrc/` matched the
/// tracked file by path components and then failed inside the commit, leaking
/// jj's own vocabulary at a point where nothing is left to teach.
#[test]
fn a_trailing_separator_on_a_named_file_is_just_a_trailing_separator() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".bashrc", "export DOTSYNC=one\n");
    machine.run_ok("dotsync commit all -m 'add bashrc' -- .bashrc");

    machine.write_file(".bashrc", "export DOTSYNC=two\n");
    let with_slash = machine.run("dotsync commit all -m 'edit bashrc' -- .bashrc/");
    assert!(
        with_slash.status.success(),
        "a trailing separator must not stop the commit\n{}",
        render_output(&with_slash)
    );
    assert!(
        !String::from_utf8_lossy(&with_slash.stderr).contains("jj"),
        "and must never leak jj's vocabulary\n{}",
        render_output(&with_slash)
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".bashrc"),
        "export DOTSYNC=two\n",
        "the file it named is the file it should record"
    );

    // `./` in front says nothing either.
    machine.write_file(".bashrc", "export DOTSYNC=three\n");
    machine.run_ok("dotsync commit all -m 'edit again' -- ./.bashrc");
    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".bashrc"),
        "export DOTSYNC=three\n"
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

/// The home-root refusal is a check on the path you typed, and a symlink is a
/// way of typing a different path. `selflink -> $HOME` walks all of home under
/// an aliased prefix, which also slips past the repo-root and sync-state
/// guards, because those are prefix tests on the path as written.
#[test]
fn a_symlink_pointing_at_home_cannot_be_used_to_sweep_it() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".ssh/id_ed25519", "PRIVATE KEY\n");
    machine.write_file(".netrc", "machine example.com login me password hunter2\n");
    symlink_at(&machine.home_dir, &machine.home_dir.join("selflink"));

    let output = machine.run("dotsync commit all -m 'sweep' -- selflink/");
    assert_eq!(
        output.status.code(),
        Some(1),
        "a symlink to home must not be a way to commit all of home\n{}",
        render_output(&output)
    );

    let tracked = machine.run_ok("dotsync view --scope all");
    let tracked = String::from_utf8_lossy(&tracked.stdout).into_owned();
    for forbidden in ["selflink", "id_ed25519", ".netrc", ".jj", "sync-state"] {
        assert!(
            !tracked.contains(forbidden),
            "`{forbidden}` must never reach a scope\n{tracked}"
        );
    }
}

/// Dotsync records the content at the path you name, and every machine on the
/// scope writes that content back to the same path. A symlink names something
/// else — which may live outside home entirely — so until dotsync has an
/// answer for what that should mean, naming one is refused rather than
/// guessed at. See PLAN.md §1.5.
#[test]
fn a_symlinked_selection_path_is_refused_whether_it_is_a_file_or_a_directory() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // Config kept outside home and linked into place, which is a real pattern.
    let outside = machine
        .home_dir
        .parent()
        .expect("home has a parent")
        .join("elsewhere");
    write_file_at(&outside.join("nvim/init.lua"), "vim.opt.number = true\n");
    write_file_at(&outside.join("vimrc"), "set number\n");
    symlink_at(
        &outside.join("nvim"),
        &machine.home_dir.join(".config/nvim"),
    );
    symlink_at(&outside.join("vimrc"), &machine.home_dir.join(".vimrc"));

    for selection in [".config/nvim/", ".vimrc"] {
        let output = machine.run(&format!("dotsync commit all -m 'link' -- {selection}"));
        assert_eq!(
            output.status.code(),
            Some(1),
            "`{selection}` is a symlink and must be refused\n{}",
            render_output(&output)
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("symlink"),
            "the refusal must say what it found\n{stderr}"
        );
    }

    // A path that reaches a real file through a symlinked parent is the same
    // claim wearing a different hat.
    let through = machine.run("dotsync commit all -m 'link' -- .config/nvim/init.lua");
    assert_eq!(
        through.status.code(),
        Some(1),
        "a path that resolves through a symlink must be refused too\n{}",
        render_output(&through)
    );

    // Real files next to them are unaffected.
    machine.write_file(".bashrc", "export DOTSYNC=1\n");
    machine.run_ok("dotsync commit all -m 'real file' -- .bashrc");
}

/// A commit that records nothing ran no sync, so it has no sync to report. The
/// empty `machine_scope` it used to print came from a default-constructed sync
/// report, and the headline claimed a commit that never happened.
#[test]
fn a_commit_that_records_nothing_says_so_instead_of_reporting_an_empty_sync() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".apprc", "ui_theme = dark\n");
    let recorded = machine.run_ok("dotsync --output json commit all -m 'add apprc' -- .apprc");
    let json = parse_stdout_json(&recorded);
    assert_eq!(
        json["outcome"],
        "committed",
        "a commit that recorded something must say which of the two things it did\n{}",
        render_output(&recorded)
    );
    assert_eq!(json["machine_scope"], "mx-xps-cy");

    let again = machine.run_expecting(
        "dotsync --output json commit all -m 'add apprc again' -- .apprc",
        0,
    );
    let json = parse_stdout_json(&again);
    assert_eq!(json["status"], "ok");
    assert_eq!(json["command"], "commit");
    assert_eq!(
        json["outcome"],
        "nothing_to_commit",
        "a commit that recorded nothing must be distinguishable from one that did\n{}",
        render_output(&again)
    );
    assert_eq!(json["scope"], "all");
    assert_eq!(
        json["machine_scope"],
        "mx-xps-cy",
        "the machine scope is known whether or not there was anything to commit\n{}",
        render_output(&again)
    );
    assert!(
        json.get("synced_files").is_none(),
        "a commit that never ran a sync must not report an empty one\n{}",
        render_output(&again)
    );

    let human = machine.run("dotsync commit all -m 'add apprc again' -- .apprc");
    assert_stderr_snapshot(
        &human,
        "dotsync: nothing to record on `all`; no commit was made and home was not synced\n",
    );
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

/// `diff` is `status`'s changes with the diffs shown, so the two commands have
/// to use one vocabulary for one file. They used to disagree on the field name
/// (`status` vs `state`), on the word for the population, and `status` wrapped
/// its files in a `groups` array that never held more than one group.
#[test]
fn status_and_diff_describe_the_same_change_with_the_same_words() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    machine.run_ok("dotsync");
    machine.write_file(".bashrc", "export DOTSYNC=modified\n");

    let status_output = machine.run_ok("dotsync --output json status");
    let status_json = parse_stdout_json(&status_output);
    assert!(
        status_json.get("groups").is_none(),
        "the group that never grouped anything is gone\n{}",
        render_output(&status_output)
    );
    let status_change = &status_json["changes"][0];
    assert_eq!(status_change["path"], ".bashrc");
    assert!(status_change["state"].as_str().is_some());
    assert!(status_change["reason"].as_str().is_some());

    let diff_output = machine.run_expecting("dotsync --output json diff", 1);
    let diff_json = parse_stdout_json(&diff_output);
    assert!(
        diff_json.get("drifts").is_none(),
        "`diff` reports the same changes `status` does, under the same name\n{}",
        render_output(&diff_output)
    );
    let diff_change = &diff_json["changes"][0];
    assert_eq!(diff_change["path"], status_change["path"]);
    assert_eq!(diff_change["state"], status_change["state"]);
    assert_eq!(diff_change["reason"], status_change["reason"]);
    assert!(
        diff_change["diff"].as_str().is_some(),
        "the diff is what `diff` adds to the same answer\n{}",
        render_output(&diff_output)
    );

    let status_stderr = String::from_utf8_lossy(&status_output.stderr).into_owned();
    let diff_stderr = String::from_utf8_lossy(&diff_output.stderr).into_owned();
    assert_eq!(
        status_stderr.lines().next(),
        diff_stderr.lines().next(),
        "one machine, one moment, one file: both commands say it the same way\nstatus:\n{status_stderr}\ndiff:\n{diff_stderr}"
    );
    assert_eq!(
        status_stderr.lines().nth(1),
        diff_stderr.lines().nth(1),
        "and they name the file the same way too\nstatus:\n{status_stderr}\ndiff:\n{diff_stderr}"
    );
}

/// A run that found three problems found three things, not one paragraph. The
/// human rendering joins them; the machine-readable one must not have to be
/// split back apart on a newline.
#[test]
fn a_run_that_stops_lists_each_thing_it_found_separately() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let output = machine.run_expecting(
        "dotsync --output json commit all -m x -- /etc/passwd ../outside typo.conf",
        1,
    );

    let json = parse_stdout_json(&output);
    let current_state = json["current_state"].as_array().unwrap_or_else(|| {
        panic!(
            "current_state should be an array\n{}",
            render_output(&output)
        )
    });
    assert_eq!(
        current_state.len(),
        3,
        "one entry per path the run refused\n{}",
        render_output(&output)
    );
    assert!(
        current_state
            .iter()
            .all(|entry| entry.as_str().is_some_and(|text| !text.contains('\n'))),
        "each entry is one fact, not a joined paragraph\n{}",
        render_output(&output)
    );

    let not_initialized = harness
        .machine("machine-b", "linux", "mx-vps-fd")
        .run("dotsync --output json status");
    let json = parse_stdout_json(&not_initialized);
    assert!(
        json["current_state"].is_array(),
        "every error carries the same shape of state, whether it has one fact or three\n{}",
        render_output(&not_initialized)
    );
}

/// One path is one path. The JSON message counted with the plural wording
/// whatever the count was, so an agent read "1 of the paths you named" for a
/// command that named exactly one.
#[test]
fn naming_one_unusable_path_reads_as_one_path() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let unusable = machine.run_expecting("dotsync --output json commit all -m x -- typo.conf", 1);
    assert_eq!(
        parse_stdout_json(&unusable)["message"],
        "cannot commit the path you named",
        "{}",
        render_output(&unusable)
    );

    let two = machine.run_expecting(
        "dotsync --output json commit all -m x -- typo.conf other-typo.conf",
        1,
    );
    assert_eq!(
        parse_stdout_json(&two)["message"],
        "cannot commit 2 of the paths you named",
        "{}",
        render_output(&two)
    );
}

/// A named directory that holds a symlink used to record everything else and
/// say nothing about the link, so an agent reading `newly_tracked` would
/// believe the whole directory reached the scope.
#[test]
fn a_symlink_under_a_named_directory_is_reported_rather_than_silently_skipped() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let outside = harness.root_dir.join("outside/nvim-init.lua");
    write_file_at(&outside, "vim.o.number = true\n");
    machine.write_file(".config/app/settings.conf", "theme = dark\n");
    symlink_at(&outside, &machine.home_dir.join(".config/app/linked.lua"));

    let commit = machine.run_expecting(
        "dotsync --output json commit all -m 'app config' -- .config/app/",
        0,
    );

    let json = parse_stdout_json(&commit);
    let skipped = json["skipped_paths"].as_array().unwrap_or_else(|| {
        panic!(
            "skipped_paths should be an array\n{}",
            render_output(&commit)
        )
    });
    let linked = skipped
        .iter()
        .find(|entry| entry["path"] == ".config/app/linked.lua")
        .unwrap_or_else(|| {
            panic!(
                "a symlink the directory matched must be reported, not dropped\n{}",
                render_output(&commit)
            )
        });
    assert_eq!(linked["state"], "symlink");
    assert!(
        linked["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("symlink")),
        "the reason has to say what dotsync would have had to do with it\n{}",
        render_output(&commit)
    );

    let stderr = String::from_utf8_lossy(&commit.stderr).into_owned();
    assert!(
        stderr.contains(".config/app/linked.lua"),
        "and a human reader has to be told too\n{stderr}"
    );

    assert_eq!(
        remote_branch_file_contents(&machine, "all", ".config/app/settings.conf"),
        "theme = dark\n",
        "the real file under the directory is still recorded"
    );
    assert!(
        !bookmark_has_file(&machine, "all", ".config/app/linked.lua"),
        "and the link itself is not"
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

/// dotsync abstracts jj away entirely, which has to include the words it uses
/// when it stops. `bookmark` is a jj concept an agent has never been told
/// about, and a file that is simply not on a scope is an ordinary answer rather
/// than a backend failure.
#[test]
fn a_user_never_meets_the_backends_vocabulary() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let bad_scope = machine.run_expecting("dotsync --output json view --scope nosuchscope", 1);
    let stderr = String::from_utf8_lossy(&bad_scope.stderr).into_owned();
    assert!(
        !stderr.contains("bookmark"),
        "a scope that does not exist is a scope that does not exist\n{stderr}"
    );
    assert_eq!(
        parse_stdout_json(&bad_scope)["error"],
        "invalid_scope",
        "and it is the same mistake `commit` already teaches about\n{}",
        render_output(&bad_scope)
    );

    let missing_file = machine.run_expecting(
        "dotsync --output json view --scope all --file .nosuchfile",
        1,
    );
    let stderr = String::from_utf8_lossy(&missing_file.stderr).into_owned();
    assert!(
        !stderr.contains("jj operation failed"),
        "a file that is not on a scope is not an internal failure\n{stderr}"
    );
    let json = parse_stdout_json(&missing_file);
    assert_eq!(
        json["error"],
        "file_not_on_scope",
        "and an agent branching on the code can tell it from a broken repo\n{}",
        render_output(&missing_file)
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

/// An unknown command with `--output json` after it emitted nothing at all on
/// stdout: clap's external-subcommand catch-all swallows every trailing
/// argument, so the flag never reached the parsed value the emitter reads. An
/// empty stdout and exit 2 is indistinguishable from a crash, for the one
/// mistake an agent makes most.
#[test]
fn an_unknown_command_honors_the_json_contract_wherever_the_flag_sits() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    for command in ["dotsync --output json bogus", "dotsync bogus --output json"] {
        let output = machine.run(command);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{command}\n{}",
            render_output(&output)
        );
        let json = parse_stdout_json(&output);
        assert_eq!(json["status"], "error", "{command}");
        assert_eq!(json["error"], "usage", "{command}");
        assert!(
            json["message"]
                .as_str()
                .is_some_and(|message| message.contains("bogus")),
            "{command}\n{}",
            render_output(&output)
        );
    }
}

/// Every error payload carries the same three collections, so error handling
/// has one shape — PLAN says so. Usage errors carried none of them, which is
/// the one error an agent hits before it has learned anything else.
#[test]
fn a_usage_error_has_the_same_shape_as_every_other_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let output = machine.run_expecting("dotsync --output json bogus", 2);

    let json = parse_stdout_json(&output);
    for field in ["current_state", "drifts", "forced_overwrites"] {
        assert_eq!(
            json[field].as_array().map(Vec::len),
            Some(0),
            "a usage error must carry `{field}` like every other error\n{}",
            render_output(&output)
        );
    }
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

/// A named path that is not a regular file used to hang the process: `fs::read`
/// on a FIFO blocks until somebody writes to the other end, and nothing ever
/// does. A directory selection already steps around one and says so; naming it
/// exactly has to be refused, and every other read of home has to refuse it
/// too, because a tracked path can become one at any time.
#[test]
fn a_named_path_that_is_not_a_regular_file_is_refused_rather_than_read() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    make_fifo(&machine.home_dir.join(".pipe"));

    let named = machine.run_within(
        "dotsync commit all -m 'pipe' -- .pipe",
        std::time::Duration::from_secs(20),
    );
    assert_eq!(
        named.status.code(),
        Some(1),
        "naming a fifo must be refused, not read\n{}",
        render_output(&named)
    );
    let stderr = String::from_utf8_lossy(&named.stderr).into_owned();
    assert!(
        stderr.contains("not a regular file"),
        "and the refusal has to say why\n{stderr}"
    );

    // The same file where dotsync reads home without being asked: a path the
    // scope already tracks, replaced in home by a fifo.
    machine.write_file(".bashrc", "export DOTSYNC=1\n");
    machine.run_ok("dotsync commit all -m 'bashrc' -- .bashrc");
    machine.delete_file(".bashrc");
    make_fifo(&machine.home_dir.join(".bashrc"));

    let status = machine.run_within("dotsync status", std::time::Duration::from_secs(20));
    assert_eq!(
        status.status.code(),
        Some(1),
        "reading home must stop rather than block forever\n{}",
        render_output(&status)
    );
    assert!(
        String::from_utf8_lossy(&status.stderr).contains("not a regular file"),
        "{}",
        render_output(&status)
    );
}

/// `dotsync view --file` on a path no scope holds printed the two headings and
/// nothing between them, which reads exactly like a bug — and is the answer to
/// the commonest reason for asking: a typo.
#[test]
fn view_says_when_no_scope_holds_a_file() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let output = machine.run_expecting("dotsync view --file .nosuchfile", 0);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.contains("No scope holds .nosuchfile"),
        "an empty answer has to say it is an answer\n{stdout}"
    );
}

/// Every command a stop tells you to rerun has to be a command dotsync knows.
///
/// The advice was built from the identifier the JSON payload uses for the
/// command, and for plain sync those are not the same string: the payload says
/// `"sync"`, but bare `dotsync` *is* the sync command and `dotsync sync` is not
/// a subcommand at all. So a fresh machine's very first interaction ended by
/// naming an invocation that exits 2 — the same defect this advice exists to
/// fix, inverted, on the reflex command.
#[test]
fn every_command_the_advice_names_is_a_command_dotsync_knows() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let sync = machine.run_expecting("dotsync", 1);
    let sync_stderr = String::from_utf8_lossy(&sync.stderr).into_owned();
    assert!(
        sync_stderr.contains("then rerun `dotsync`."),
        "bare `dotsync` is how you run a sync; there is nothing to add to it\n{sync_stderr}"
    );

    let commit = machine.run("dotsync commit all -m 'add bashrc' -- .bashrc");
    let commit_stderr = String::from_utf8_lossy(&commit.stderr).into_owned();

    for stderr in [&sync_stderr, &commit_stderr] {
        let advice = stderr
            .split_once("Correct flow:")
            .expect("the stop teaches a correct flow")
            .1;
        let invocations = quoted_dotsync_invocations(advice);
        assert!(
            !invocations.is_empty(),
            "the advice has to name something to run\n{stderr}"
        );
        for invocation in invocations {
            let attempt = machine.run(&invocation);
            let attempt_stderr = String::from_utf8_lossy(&attempt.stderr).into_owned();
            assert!(
                !attempt_stderr.contains("unknown command"),
                "the advice named `{invocation}`, which dotsync does not recognise\n{attempt_stderr}"
            );
        }
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

// Symlink handling — six tests for the 2026-08-13 decision that symlinks are
// treated as files and never followed (PLAN.md §1.5). Every one of them
// describes behaviour dotsync does not have yet.

/// A symlink that is already on a scope has to reach home as a symlink. Today
/// it does not: the repo read turns `TreeValue::Symlink` into the target
/// string as bytes, and the home write puts those bytes in a regular file, so
/// `~/.config/app/current.conf` ends up being a file whose entire contents are
/// the nine characters `real.conf`.
///
/// Nothing here needs dotsync to be able to *commit* a link — a plain git
/// client puts it on `all` and an ordinary cascade carries it down — which is
/// why this is reachable today and is the sharpest statement of the decision.
#[test]
fn a_symlink_on_a_scope_materialises_in_home_as_a_symlink() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "all", ".config/app/real.conf", "theme = dark\n");
    seed_remote_scope_symlink(&machine, "all", ".config/app/current.conf", "real.conf");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");

    machine.run_ok("dotsync");

    assert!(
        machine.is_symlink(".config/app/current.conf"),
        "a symlink on the scope must arrive in home as a symlink, not as a regular file holding the target text (home holds: {:?})",
        fs::read_to_string(machine.home_dir.join(".config/app/current.conf")).ok()
    );
    assert_eq!(
        machine.read_link(".config/app/current.conf"),
        PathBuf::from("real.conf"),
        "and it must point where the scope says it points"
    );
}

/// Home holds a link where the scope holds a regular file. Sync must replace
/// the link, not write through it — writing through it edits a file dotsync
/// was never asked to manage, under a name it does not know.
///
/// Reproduced with two managed files: `toolA` recorded as a regular file, then
/// replaced in home by a link to the managed `toolB`, and `dotsync --force`
/// wrote toolA's recorded content into toolB. The link target here is outside
/// home so the loss is unambiguous, and its content is asserted explicitly:
/// that byte comparison is the data loss.
#[test]
fn a_sync_replaces_a_home_symlink_instead_of_writing_through_it() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".apprc", "ui = dark\n");
    machine.run_ok("dotsync");
    assert_eq!(machine.read_file(".apprc"), "ui = dark\n");

    // A file dotsync does not manage, at a path dotsync has never heard of.
    let outside = harness.root_dir.join("outside/notes.txt");
    write_file_at(&outside, "notes nobody asked dotsync to touch\n");
    machine.replace_with_symlink(".apprc", &outside);

    seed_remote_scope_file(&machine, "mx-xps-cy", ".apprc", "ui = light\n");

    // A plain sync stops: home holds something that is not what was synced.
    machine.run_expecting("dotsync", 1);
    assert_eq!(
        fs::read_to_string(&outside).expect("read the link target"),
        "notes nobody asked dotsync to touch\n",
        "a run that stopped must not have written anything"
    );

    machine.run_ok("dotsync --force");
    assert_eq!(
        fs::read_to_string(&outside).expect("read the link target"),
        "notes nobody asked dotsync to touch\n",
        "the link's target is not a managed file and must survive byte for byte"
    );
    assert!(
        !machine.is_symlink(".apprc"),
        "the link itself is what the scope disagrees with, so the link is what gets replaced"
    );
    assert_eq!(
        machine.read_file(".apprc"),
        "ui = light\n",
        "and home ends up holding the scope's regular file"
    );
}

/// Naming a symlink records it as a symlink, storing the target. Today it is
/// refused outright — the conservative placeholder that stood in for this
/// decision.
#[test]
fn commit_records_a_symlink_as_a_symlink() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".config/app/real.conf", "theme = dark\n");
    symlink_at(
        Path::new("real.conf"),
        &machine.home_dir.join(".config/app/current.conf"),
    );

    machine.run_ok("dotsync commit all -m 'point current at real' -- .config/app/current.conf");

    assert_eq!(
        remote_branch_entry_mode(&machine, "all", ".config/app/current.conf").as_deref(),
        Some("120000"),
        "the scope has to record it as a link, not as a file"
    );
    assert_eq!(
        remote_branch_file_contents(&machine, "all", ".config/app/current.conf"),
        "real.conf",
        "and what it stores is the target"
    );
    assert!(
        machine.is_symlink(".config/app/current.conf"),
        "committing a link must leave home holding the same link"
    );
}

/// The whole story, in the shape a person meets it: a versioned script and a
/// `tool -> tool-1.2.0` link beside it. The link has to still be a link on the
/// second machine, and must not have become a second copy of the script.
///
/// Named as a directory selection rather than naming the link exactly, so this
/// fails on the round trip rather than on the refusal that
/// `commit_records_a_symlink_as_a_symlink` covers: today the walk silently
/// skips the link, machine A publishes only the script, and machine B has no
/// link at all.
#[test]
fn a_symlink_to_a_sibling_script_survives_the_round_trip_to_another_machine() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "box1");
    let machine_b = harness.machine("machine-b", "linux", "box2");

    machine_a.init_ok();

    machine_a.write_file(".local/bin/tool-1.2.0", "#!/bin/sh\necho tool\n");
    symlink_at(
        Path::new("tool-1.2.0"),
        &machine_a.home_dir.join(".local/bin/tool"),
    );

    machine_a.run_ok("dotsync commit all -m 'ship tool and the current link' -- .local/bin/");

    machine_b.init_ok();

    assert_eq!(
        machine_b.read_file(".local/bin/tool-1.2.0"),
        "#!/bin/sh\necho tool\n"
    );
    assert!(
        machine_b.is_symlink(".local/bin/tool"),
        "the link has to arrive as a link (machine b holds: {:?})",
        fs::read_to_string(machine_b.home_dir.join(".local/bin/tool")).ok()
    );
    assert_eq!(
        machine_b.read_link(".local/bin/tool"),
        PathBuf::from("tool-1.2.0"),
        "pointing at its sibling, not carrying a second copy of it"
    );
}

/// A difference in kind is a difference. Both fixtures below hold exactly the
/// bytes the scope holds — the link's target text is the file's content, and
/// vice versa — so the only thing that differs is whether the path is a link.
/// Today both read as clean, because the drift read follows the link and then
/// compares content.
///
/// Exit codes are the 2026-08-13 decision: `status` is concise and always
/// exits 0, `diff` exits 1 when it finds anything.
#[test]
fn status_and_diff_report_a_kind_difference_between_a_link_and_a_file() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".apprc", "ui = dark\n");
    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".config/app/real.conf",
        "theme = dark\n",
    );
    seed_remote_scope_symlink(
        &machine,
        "mx-xps-cy",
        ".config/app/current.conf",
        "real.conf",
    );
    machine.run_ok("dotsync");

    // A link where the scope holds a file, over content identical to the
    // scope's.
    let outside = harness.root_dir.join("outside/apprc.txt");
    write_file_at(&outside, "ui = dark\n");
    machine.replace_with_symlink(".apprc", &outside);

    // A file where the scope holds a link, holding exactly the target text.
    machine.replace_with_regular_file(".config/app/current.conf", "real.conf");

    let status = machine.run_expecting("dotsync --output json status", 0);
    let changed = parse_stdout_json(&status);
    let changed = changed["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("changes should be an array\n{}", render_output(&status)));
    for path in [".apprc", ".config/app/current.conf"] {
        let entry = changed
            .iter()
            .find(|entry| entry["path"] == path)
            .unwrap_or_else(|| {
                panic!(
                    "`{path}` differs from the scope in kind and must be reported\n{}",
                    render_output(&status)
                )
            });
        assert!(
            entry["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("symlink")),
            "the reason must name the kind difference rather than describing content\n{}",
            render_output(&status)
        );
    }

    let diff = machine.run("dotsync --output json diff");
    assert_eq!(
        diff.status.code(),
        Some(1),
        "diff exits 1 when it finds changes\n{}",
        render_output(&diff)
    );
    let diffed = parse_stdout_json(&diff);
    let diffed = diffed["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("changes should be an array\n{}", render_output(&diff)))
        .iter()
        .map(|entry| entry["path"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    for path in [".apprc", ".config/app/current.conf"] {
        assert!(
            diffed.iter().any(|reported| reported == path),
            "`{path}` must be in diff's population too\n{}",
            render_output(&diff)
        );
    }
}

/// The leak this whole area was found through, and the reason the blanket
/// refusal existed: `selflink -> $HOME` used to walk all of home under an
/// aliased prefix and publish 72 files, including `.ssh/id_ed25519`, `.netrc`
/// and the whole hidden `.jj` store, exit 0.
///
/// Under "treat links as files and do not follow them" the sweep stays closed
/// for a structural reason rather than a guard: `selflink` is one symlink
/// entry, so there is no directory to walk. That is the claim here — recording
/// the link is allowed, and home's contents still do not reach the remote.
#[test]
fn a_symlink_to_home_records_one_entry_rather_than_sweeping_home() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".ssh/id_ed25519", "PRIVATE KEY\n");
    machine.write_file(".netrc", "machine example.com login me password hunter2\n");
    symlink_at(&machine.home_dir, &machine.home_dir.join("selflink"));

    machine.run_ok("dotsync commit all -m 'link to home' -- selflink/");

    assert_eq!(
        remote_branch_entry_mode(&machine, "all", "selflink").as_deref(),
        Some("120000"),
        "a link to home is one link entry"
    );

    let published = remote_branch_entries(&machine, "all")
        .into_iter()
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    for forbidden in [".ssh/id_ed25519", ".netrc", ".jj", "sync-state"] {
        assert!(
            !published.iter().any(|path| path.contains(forbidden)),
            "`{forbidden}` must never reach the remote\npublished: {published:?}"
        );
    }
}

// Three decided behaviours dotsync does not have yet: the commit target has to
// be a scope this machine is on (PLAN §6, DESIGN "Commands"), the drift stop
// has to know a cascade is paused (PLAN, item 3), and `view` has to answer the
// two orientation questions it is asked (PLAN, item 4).

/// The scope a commit names has to be this machine's own scope or an ancestor
/// of it (Max, 2026-08-13): home only ever moves forward and the config it
/// holds is supposed to stay valid, so there is no version of another
/// machine's branch that this machine can claim to have started from. Today
/// this is silently accepted, exit 0, and the change reaches the other
/// machine's branch on the remote.
///
/// The refusal has to teach the replacement pattern, because "you cannot do
/// that" on its own leaves an agent with a real need and no way to meet it:
/// put the shared material — and the pattern for writing it — on the common
/// ancestor, and leave an agent running on that machine family to add its own
/// drop-ins on its own scope. The assertions below pin that the refusal names
/// the scope that was asked for and the shared ancestor it should have used;
/// the prose is the implementer's.
///
/// This is only about *choosing* a commit target. A cascade from a shared
/// ancestor still merges into descendant scopes this machine is not on, so
/// conflicts outside this machine's ancestry stay a normal event.
#[test]
fn committing_to_a_scope_this_machine_is_not_on_is_refused() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".apprc", "ui = dark\n");
    let refused = machine_a.run("dotsync commit goof-b -m 'set the other box up' -- .apprc");
    assert_eq!(
        refused.status.code(),
        Some(1),
        "committing to a scope this machine is not on must be refused\n{}",
        render_output(&refused)
    );

    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        stderr.contains("goof-b"),
        "the refusal has to name the scope that was asked for\n{stderr}"
    );
    assert!(
        stderr.contains("linux"),
        "and it has to point at the shared ancestor, which is where the material and the pattern for it belong\n{stderr}"
    );

    let json =
        machine_a.run("dotsync --output json commit goof-b -m 'set the other box up' -- .apprc");
    let payload = parse_stdout_json(&json);
    assert_eq!(payload["status"], "error", "{}", render_output(&json));
    assert!(
        payload["current_state"]
            .as_array()
            .is_some_and(|facts| facts
                .iter()
                .any(|fact| fact.as_str().is_some_and(|fact| fact.contains("goof-b")))),
        "the payload has to carry the same fact the rendering does\n{}",
        render_output(&json)
    );

    assert!(
        remote_branch_entry_mode(&machine_a, "goof-b", ".apprc").is_none(),
        "nothing may reach the other machine's branch\nit holds: {:?}",
        remote_branch_entries(&machine_a, "goof-b")
    );
    assert_eq!(
        machine_a.read_file(".apprc"),
        "ui = dark\n",
        "a refusal destroys nothing in home"
    );

    // The refusal is not a wall: the pattern it teaches has to work. The
    // shared ancestor is a scope this machine is on, so committing there is
    // how the same material reaches the other machine.
    machine_a.run_ok("dotsync commit linux -m 'set every linux box up' -- .apprc");
    assert_eq!(
        remote_branch_file_contents(&machine_a, "goof-b", ".apprc"),
        "ui = dark\n",
        "and the cascade is what carries it to the other machine"
    );
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

/// The overview lists every scope, and nothing in it says which one is this
/// machine — so the command whose whole job is orientation leaves an agent
/// unable to answer "where am I?". `status` knows: the machine scope is in
/// sync state. `view` just does not compute it.
#[test]
fn the_overview_says_which_scope_is_this_machine() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);

    let json = machine_a.run_ok("dotsync view --output json");
    assert_eq!(
        parse_stdout_json(&json)["machine_scope"],
        "goof-a",
        "the overview has to name this machine's scope, under the name every other payload uses for it\n{}",
        render_output(&json)
    );

    let human = machine_a.run_ok("dotsync view");
    let stdout = String::from_utf8_lossy(&human.stdout).into_owned();
    let this_machine = the_line_naming(&stdout, "goof-a");
    let another_machine = the_line_naming(&stdout, "goof-b");
    assert_ne!(
        this_machine.replace("goof-a", ""),
        another_machine.replace("goof-b", ""),
        "the two machine scopes render identically apart from their names, so the rendering says nothing about where you are\n{stdout}"
    );
}

/// `view --file` lists every scope holding the file — the owner plus every
/// descendant, because files propagate down the DAG. Reading that list
/// correctly needs exactly the propagation knowledge an agent was using `view`
/// to acquire, and the useful answer is the one fact the list does not state:
/// which scope owns the file. It is derivable — the owner is the rootmost
/// scope holding it — and it is not computed.
///
/// Both cases are here because the rootmost scope is only interesting when it
/// is not the only one: a file on `all` reaches all four scopes, and a file on
/// a machine scope reaches one.
#[test]
fn view_file_says_which_scope_owns_the_file() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".apprc", "ui = dark\n");
    machine_a.run_ok("dotsync commit all -m 'shared apprc' -- .apprc");

    machine_b.run_ok("dotsync");
    machine_b.write_file(".config/local.conf", "monitor = DP-1\n");
    machine_b.run_ok("dotsync commit goof-b -m 'this box only' -- .config/local.conf");

    let on_the_root = machine_a.run_ok("dotsync view --file .apprc --output json");
    assert_eq!(
        parse_stdout_json(&on_the_root)["owner"],
        "all",
        "a file every scope inherits is owned by the one it was committed to\n{}",
        render_output(&on_the_root)
    );

    let on_a_leaf = machine_a.run_ok("dotsync view --file .config/local.conf --output json");
    assert_eq!(
        parse_stdout_json(&on_a_leaf)["owner"],
        "goof-b",
        "and a file only one machine holds is owned by that machine's scope\n{}",
        render_output(&on_a_leaf)
    );

    let human = machine_a.run_ok("dotsync view --file .apprc");
    let stdout = String::from_utf8_lossy(&human.stdout).into_owned();
    assert!(
        stdout.to_lowercase().contains("own"),
        "the rendering has to say which scope owns it, not leave the reader to derive it from the list\n{stdout}"
    );
}

// Read-only robustness (PLAN item 4), plus the one `--force` case that
// belongs with the conflict work (PLAN item 3). The principle underneath all
// four: a read-only command describes unusual state, it never declines to
// describe anything because of it, and it never mutates. An agent that has
// walked into a weird state must always have a command left that tells it what
// is going on — and today the diagnostics disappear at exactly the moment they
// are most needed.

/// A diverged scope — this machine and the remote each holding commits the
/// other does not — takes every read-only command away today: `status`, `diff`
/// and `view` all exit 1 with `scope_diverged` and answer nothing. That is a
/// dead end, and Max's decision that `dotsync status` "should be concise
/// (doesn't print a diff) always exits 0" (2026-08-13) leaves no exit code for
/// `status` to stop with even if it wanted to.
///
/// So the answer is item 4's in-memory convergence reporting: work out what
/// convergence would do, say the scope diverged, and still answer the question
/// that was asked.
///
/// The divergence here is deliberately non-conflicting — the two sides added
/// different files to `all` — so nothing in this test depends on how a
/// *conflicting* merge should be reported, which is item 2's and item 3's
/// question.
///
/// `diff` keeps its own contract: it exits 1 when it finds drift and 0 when it
/// does not, and divergence is not drift — it is a fact about the repo and the
/// remote, not about home. That is the same answer `diff` already gives a
/// machine that is merely behind. Both halves are pinned below.
///
/// The JSON *key* that carries the divergence is deliberately not pinned: no
/// name for it has been decided, and inventing one here would make the
/// decision rather than record it. What is pinned is that the payload carries
/// the fact and the scope somewhere, and that the envelope says `ok` — because
/// `status` is `"error"` only when the run stopped, and this run did not.
#[test]
fn read_only_commands_describe_a_diverged_scope_instead_of_refusing_to_run() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // This machine holds unpushed commits on every scope, and then another
    // machine publishes a different file to `all`: `all` genuinely diverges
    // while `linux` and `mx-xps-cy` are merely local-ahead.
    interrupt_push_after_cascade(
        &machine,
        ".config/fish/dev-certs.fish",
        "set -gx DEV_CERTS 1\n",
    );
    seed_remote_scope_file(
        &machine,
        "all",
        ".config/other-machine.conf",
        "from another machine\n",
    );

    let status = machine.run("dotsync status --output json");
    assert_eq!(
        status.status.code(),
        Some(0),
        "`dotsync status` always exits 0: a diverged scope is something it found in the world, not a reason to stop\n{}",
        render_output(&status)
    );
    let payload = parse_stdout_json(&status);
    assert_eq!(payload["status"], "ok", "{}", render_output(&status));
    assert!(
        payload["changes"].is_array() && payload["incoming"].is_array(),
        "the question that was asked still has to be answered\n{}",
        render_output(&status)
    );
    assert!(
        payload_says(&payload, "diverg"),
        "and the payload has to carry what the run found\n{}",
        render_output(&status)
    );
    assert!(
        payload_says(&payload, "all"),
        "naming the scope it found it on\n{}",
        render_output(&status)
    );

    // Asked again in the shape a person runs it, because the two channels are
    // populated separately and it is the human one Max reads.
    let human = machine.run_expecting("dotsync status", 0);
    assert_reports_divergence(&human, "all");

    let view = machine.run("dotsync view");
    assert_eq!(
        view.status.code(),
        Some(0),
        "`dotsync view` has no clean/dirty contract to spend an exit code on, so it either answers or it is a dead end\n{}",
        render_output(&view)
    );
    let listed = String::from_utf8_lossy(&view.stdout).into_owned();
    assert!(
        listed.contains(".config/fish/dev-certs.fish"),
        "and it still has to list what the scopes hold\n{listed}"
    );
    assert_reports_divergence(&view, "all");

    // Home holds exactly what the machine scope holds, so there is no drift
    // for `diff` to find. Divergence is not drift.
    let clean = machine.run("dotsync diff");
    assert_eq!(
        clean.status.code(),
        Some(0),
        "`diff` exits 1 for drift, and a diverged scope is not drift — it is the same answer `diff` gives a machine that is merely behind\n{}",
        render_output(&clean)
    );
    assert_reports_divergence(&clean, "all");

    // ...and a diverged scope does not take away the answer `diff` exists to
    // give, either.
    machine.write_file(".config/fish/dev-certs.fish", "set -gx DEV_CERTS 2\n");
    let drifted = machine.run("dotsync diff");
    assert_eq!(
        drifted.status.code(),
        Some(1),
        "`diff` still exits 1 on real drift\n{}",
        render_output(&drifted)
    );
    assert!(
        String::from_utf8_lossy(&drifted.stderr).contains("+set -gx DEV_CERTS 2"),
        "and it still shows the drift it found\n{}",
        render_output(&drifted)
    );
    assert_reports_divergence(&drifted, "all");
}

/// DESIGN: "Read-only commands never mutate. `status`, `diff`, and `view`
/// don't move bookmarks, create commits, or touch home. They fetch (when
/// online) and *report* what convergence would do."
///
/// Today the fetch commits a transaction that fast-forwards every local scope
/// bookmark the remote has moved on, so `status` changes the repo — the
/// command an agent runs to *look* at the machine is one of the commands that
/// changes it. Wave 2 left exactly one site doing this, which is why the
/// before/after of the bookmarks is enough to see it.
///
/// The revisions are read once, before anything runs, and re-read after each
/// command: the first command to move a bookmark would otherwise hide the
/// second.
#[test]
fn read_only_commands_leave_the_scope_bookmarks_where_they_found_them() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // Another machine publishes down the whole graph, so every scope this
    // machine has a bookmark for is behind the remote — the ordinary case, and
    // the one where a fetch that reconciles has something to move.
    seed_remote_scope_file(&machine, "all", ".gitconfig", "[user]\nname = Shared\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");

    let scopes = ["all", "linux", "mx-xps-cy"];
    let before = scopes.map(|scope| bookmark_revision(&machine, scope));

    for command in ["dotsync status", "dotsync diff", "dotsync view"] {
        let output = machine.run(command);
        assert_eq!(
            output.status.code(),
            Some(0),
            "`{command}` on a machine that is merely behind\n{}",
            render_output(&output)
        );

        for (scope, was) in scopes.iter().zip(&before) {
            assert_eq!(
                &bookmark_revision(&machine, scope),
                was,
                "`{command}` moved `{scope}`: a read-only command reports what convergence would do, it does not do it"
            );
        }
        assert!(
            !machine.file_exists(".gitconfig"),
            "`{command}` wrote the incoming file into home; only plain `dotsync` applies it"
        );
    }

    // The change really was there to be applied, so the assertions above were
    // about a run that had something to move, not about an idle fetch.
    machine.run_ok("dotsync");
    assert_eq!(machine.read_file(".gitconfig"), "[user]\nname = Shared\n");
}

/// A refused push is reported by the run that hit it and nowhere else. Once
/// that output has scrolled away, a machine holding unpublished commits looks
/// completely clean: `status` says "no changes" and nothing anywhere says the
/// remote has never seen the work. That is how the 2026-07-27 machine sat
/// wedged for sixteen days.
///
/// `unpushed_scopes` is the name plain `dotsync` and `commit` already use for
/// this list, so `status` — the command an agent and Max both run by reflex —
/// reports it under the same name.
#[test]
fn status_names_the_scopes_this_machine_has_not_published() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // A commit whose cascade landed and whose push did not: every scope holds
    // a commit the remote has never seen, and home is in sync, so there is
    // nothing else for `status` to report.
    interrupt_push_after_cascade(
        &machine,
        ".config/fish/dev-certs.fish",
        "set -gx DEV_CERTS 1\n",
    );

    let json = machine.run_expecting("dotsync status --output json", 0);
    let payload = parse_stdout_json(&json);
    let unpushed = payload["unpushed_scopes"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "`status` has to report the scopes this machine holds and the remote does not\n{}",
                render_output(&json)
            )
        })
        .iter()
        .map(|scope| {
            scope
                .as_str()
                .expect("scope should be a string")
                .to_string()
        })
        .collect::<Vec<_>>();
    for scope in ["all", "linux", "mx-xps-cy"] {
        assert!(
            unpushed.contains(&scope.to_string()),
            "`{scope}` is unpublished and has to be named: {unpushed:?}\n{}",
            render_output(&json)
        );
    }

    let human = machine.run_expecting("dotsync status", 0);
    let stderr = String::from_utf8_lossy(&human.stderr).into_owned();
    for scope in ["all", "linux", "mx-xps-cy"] {
        assert!(
            stderr.contains(scope),
            "a machine with unpublished commits must not read as clean: `{scope}` is missing\n{stderr}"
        );
    }

    // And the report has to stop once the work is published, or it is noise
    // rather than a signal.
    machine.run_ok("dotsync --output json");
    let after = machine.run("dotsync status --output json");
    assert_eq!(
        parse_stdout_json(&after)["unpushed_scopes"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "the remote has everything now\n{}",
        render_output(&after)
    );
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

/// DESIGN: "A conflict anywhere in this machine's scope ancestry propagates
/// down into the machine scope's tree, so syncing writes standard conflict
/// markers ... into the affected home files — using jj's own
/// conflict-materialization code."
///
/// Nothing materializes today: home holds exactly the bytes the agent last
/// typed there, which is why the interim DL-2 guard has to ask "did this file
/// change" instead of "are the markers gone", and why the pause's instruction
/// to "edit the conflicted file to the merged contents you want" gives the
/// agent nothing to merge *from*. The version being merged away is only
/// reachable through `dotsync view --scope linux --file .config/app.conf`, and
/// only if the agent reads far enough into the refusal to find that out.
#[test]
fn a_pause_materializes_the_conflict_into_home() {
    let harness = TestHarness::new();
    let (machine, pause) = pause_a_conflict_on_linux(&harness);

    let home = machine.read_file(".config/app.conf");
    assert_materialized_conflict(
        &home,
        ["all", "linux"],
        [
            "setting = \"all\"",
            "setting = \"base\"",
            "setting = \"linux\"",
        ],
        &render_output(&pause),
    );
}

/// The same materialization for a conflict this machine is not on, where it
/// has to be done deliberately rather than by propagation: `goof-b`'s tree is
/// a clean merge, so nothing about syncing this machine's own scope would ever
/// put the `goof-a` conflict in front of the agent.
///
/// DESIGN: conflicts outside this machine's ancestry "don't appear in home
/// naturally ... the affected home path serves as a temporary resolution
/// buffer". Without the buffer, the agent is asked to resolve a merge it
/// cannot see either side of, which is what makes today's answer — leave the
/// file alone, or write anything at all, and `continue` records it — so easy
/// to get wrong.
#[test]
fn a_pause_on_another_machines_scope_leaves_the_conflict_in_home_as_a_buffer() {
    let harness = TestHarness::new();
    let (_machine_a, machine_b, pause) = pause_a_conflict_on(&harness, "goof-a");

    let home = machine_b.read_file(".config/app.conf");
    assert_materialized_conflict(
        &home,
        ["all", "goof-a"],
        [
            "setting = \"all\"",
            "setting = \"base\"",
            "setting = \"goof-a\"",
        ],
        &render_output(&pause),
    );
}
