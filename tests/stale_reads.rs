//! A read that reaches the remote (`files`, `show`, `scopes`) records a jj
//! operation. When one lands between two runs that read home, the next run
//! must still leave every file it only read exactly as home has it now.

mod harness;
use harness::*;

fn machine_with_untracked(harness: &TestHarness) -> (MachineEnvironment, MachineEnvironment) {
    let (machine_a, machine_b) = two_synced_machines(harness);
    machine_a.write_file(".claude/settings.json", "{ \"model\": \"base\" }\n");
    machine_a.run_ok("dotsync commit all -m 'shared' -- .claude/settings.json");
    machine_b.run_ok("dotsync");
    machine_b.write_file(".claude/projects/session.jsonl", "line 1\n");
    machine_b.write_file(".claude/agents/planner.md", "planner v1\n");
    machine_b.write_file(".claude/cache/gone.json", "gone\n");
    (machine_a, machine_b)
}

/// Edits the observed files the way a live home does: a transcript grows, an
/// agent file is rewritten, a cache file is deleted.
fn home_moves_on(machine: &MachineEnvironment) {
    machine.write_file(".claude/projects/session.jsonl", "line 1\nline 2\n");
    machine.write_file(".claude/agents/planner.md", "planner v2\n");
    std::fs::remove_file(machine.home_dir.join(".claude/cache/gone.json")).unwrap();
}

fn assert_run_changes_nothing(machine: &MachineEnvironment, command: &str) {
    let before = home_tree(machine);
    let output = machine.run(command);
    let after = home_tree(machine);
    let changed = changed_between(&before, &after);
    let detail: Vec<String> = changed
        .iter()
        .map(|p| {
            format!(
                "  {p}\n    before: {:?}\n    after:  {:?}",
                before.get(p),
                after.get(p)
            )
        })
        .collect();
    assert!(
        changed.is_empty(),
        "`{command}` changed home:\n{}\n{}",
        detail.join("\n"),
        render_output(&output)
    );
}

#[test]
fn status_after_files_leaves_edited_unmanaged_files_alone() {
    let harness = TestHarness::new();
    let (_a, b) = machine_with_untracked(&harness);
    b.run_ok("dotsync status .claude");
    b.run_ok("dotsync files");
    home_moves_on(&b);
    assert_run_changes_nothing(&b, "dotsync status");
}

#[test]
fn sync_after_scopes_leaves_edited_unmanaged_files_alone() {
    let harness = TestHarness::new();
    let (_a, b) = machine_with_untracked(&harness);
    b.run_ok("dotsync status .claude");
    b.run_ok("dotsync scopes");
    home_moves_on(&b);
    assert_run_changes_nothing(&b, "dotsync");
}

#[test]
fn status_after_a_dry_run_commit_and_files_leaves_edits_alone() {
    let harness = TestHarness::new();
    let (_a, b) = machine_with_untracked(&harness);
    b.run_ok("dotsync commit goof-b --dry-run -m 'preview' -- .claude/agents");
    b.run_ok("dotsync files");
    home_moves_on(&b);
    assert_run_changes_nothing(&b, "dotsync status");
}

/// The workflow ~/.config/AGENTS.md teaches with one file and no folder:
/// preview a new file's commit, check it with `dotsync files <path>`, edit it,
/// then commit it for real.
#[test]
fn a_new_file_edited_after_its_preview_commits_its_edited_content() {
    let harness = TestHarness::new();
    let (_a, b) = two_synced_machines(&harness);
    b.write_file(".config/new.conf", "v1\n");
    b.run_ok("dotsync commit goof-b --dry-run -m 'preview' -- .config/new.conf");
    b.run_ok("dotsync files .config/new.conf");
    b.write_file(".config/new.conf", "v2\n");
    let out = b.run("dotsync commit goof-b -m 'real' -- .config/new.conf");
    let shown = b.run("dotsync show goof-b .config/new.conf");
    assert_eq!(
        b.read_file(".config/new.conf"),
        "v2\n",
        "home reverted\ncommit: {}\nshow: {}",
        render_output(&out),
        render_output(&shown)
    );
}

/// A commit that pauses, a look around with `status <folder>` and
/// `dotsync files`, then abort.
#[test]
fn abort_after_files_during_a_pause_leaves_edited_unmanaged_files_alone() {
    let harness = TestHarness::new();
    let (a, b) = machine_with_untracked(&harness);
    a.write_file(".claude/settings.json", "{ \"model\": \"linux\" }\n");
    a.run_ok("dotsync commit linux -m 'linux model' -- .claude/settings.json");
    b.write_file(".claude/settings.json", "{ \"model\": \"wrong scope\" }\n");
    let paused = b.run("dotsync commit all -m 'settings' -- .claude/settings.json");
    assert_eq!(paused.status.code(), Some(1), "{}", render_output(&paused));
    // While paused: what else is under .claude, and what do the scopes hold?
    b.run("dotsync status .claude");
    b.run_ok("dotsync files");
    home_moves_on(&b);
    assert_run_changes_nothing(&b, "dotsync abort");
}
