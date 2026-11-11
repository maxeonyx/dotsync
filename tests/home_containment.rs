// Every command that writes home changes only what it is meant to change.
// Files no scope holds — a binary in `.local/bin`, a folder of agent
// definitions, scheduled tasks — are not dotsync's, and no read, write or
// abort may change them, however dotsync has come to know they exist.
//
// Checked over the whole home tree, because the files that go missing are the
// ones nobody thought to list.

mod harness;
use harness::*;

const UNTRACKED: [&str; 5] = [
    ".local/bin/dotsync",
    ".local/bin/helper",
    ".claude/agents/planner.md",
    ".claude/agents/reviewer.md",
    ".claude/scheduled-tasks/prioritize-morning/SKILL.md",
];

/// Two machines on `all -> linux`, with `.claude/settings.json` and
/// `.claude/cc-writes` on `all`, and goof-b's home also holding files no
/// scope has ever held, beside the tracked ones and in folders of their own.
fn a_home_with_untracked_files(harness: &TestHarness) -> (MachineEnvironment, MachineEnvironment) {
    let (machine_a, machine_b) = two_synced_machines(harness);
    machine_a.write_file(".claude/settings.json", "{ \"model\": \"base\" }\n");
    machine_a.write_file(".claude/cc-writes", "line one\n");
    machine_a.write_file(".config/app.conf", "app = 1\n");
    machine_a.run_ok(
        "dotsync commit all -m 'shared config' -- .claude/settings.json .claude/cc-writes .config/app.conf",
    );
    machine_b.run_ok("dotsync");
    for relative in UNTRACKED {
        machine_b.write_file(relative, &format!("{relative}\n"));
    }
    machine_b.make_executable(".local/bin/dotsync");
    (machine_a, machine_b)
}

/// What an agent planning a config change runs first: which files under
/// these folders does no scope track, and what would a commit of one record.
fn read_the_untracked_folders(machine: &MachineEnvironment) {
    machine.run_ok("dotsync status .claude .local/bin");
    machine.run_ok("dotsync commit goof-b --dry-run -m 'preview' -- .claude/agents");
}

/// Runs `command` and asserts it changed home at exactly `allowed` paths, or
/// a subset of them, and nowhere else.
fn assert_changes_home_only_at(machine: &MachineEnvironment, command: &str, allowed: &[&str]) {
    let before = home_tree(machine);
    let output = machine.run(command);
    assert!(output.status.success(), "{}", render_output(&output));
    let after = home_tree(machine);
    let outside: Vec<String> = changed_between(&before, &after)
        .into_iter()
        .filter(|path| !allowed.contains(&path.as_str()))
        .collect();
    assert!(
        outside.is_empty(),
        "`{command}` changed home outside {allowed:?}:\n{}",
        outside
            .iter()
            .map(|path| format!(
                "  {path}\n    before: {:?}\n    after: {:?}",
                before.get(path),
                after.get(path)
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Discard, a sync and an abort after the reads, each changing home only
/// where it is meant to.
fn every_write_changes_only_its_own_paths(
    machine_a: &MachineEnvironment,
    machine_b: &MachineEnvironment,
) {
    machine_b.write_file(".claude/cc-writes", "line one\nmine\n");
    machine_b.write_file(".config/app.conf", "app = 2\n");
    assert_changes_home_only_at(
        machine_b,
        "dotsync discard .claude/cc-writes",
        &[".claude/cc-writes"],
    );

    machine_a.write_file(".claude/settings.json", "{ \"model\": \"newer\" }\n");
    machine_a.run_ok("dotsync commit all -m 'newer model' -- .claude/settings.json");
    assert_changes_home_only_at(machine_b, "dotsync", &[".claude/settings.json"]);

    // The paused cascade of the incident: settings committed to the wrong
    // scope, colliding with an override below it.
    machine_a.write_file(".claude/settings.json", "{ \"model\": \"linux\" }\n");
    machine_a.run_ok("dotsync commit linux -m 'linux model' -- .claude/settings.json");
    machine_b.write_file(".claude/settings.json", "{ \"model\": \"wrong scope\" }\n");
    let paused = machine_b.run("dotsync commit all -m 'settings' -- .claude/settings.json");
    assert_eq!(paused.status.code(), Some(1), "{}", render_output(&paused));
    assert_changes_home_only_at(machine_b, "dotsync abort", &[]);
}

#[test]
fn writes_after_reading_untracked_folders_change_only_their_own_paths() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = a_home_with_untracked_files(&harness);
    read_the_untracked_folders(&machine_b);
    every_write_changes_only_its_own_paths(&machine_a, &machine_b);
}

/// What a machine records as home content it put there is what its scopes
/// hold. A read is not a reason to add to it.
#[test]
fn reads_leave_the_recorded_home_content_as_they_found_it() {
    let harness = TestHarness::new();
    let (_machine_a, machine_b) = a_home_with_untracked_files(&harness);
    let before = recorded_as_materialized(&machine_b);
    read_the_untracked_folders(&machine_b);
    assert_eq!(recorded_as_materialized(&machine_b), before);
}

/// The state 0.13.2's reads left on machines that ran them: the untracked
/// files recorded as home content dotsync had materialized. The build that
/// reads them must not then treat them as its own.
#[test]
fn a_home_polluted_by_0_13_2_reads_keeps_its_untracked_files() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = a_home_with_untracked_files(&harness);
    record_as_materialized_like_0_13_2(&machine_b, &UNTRACKED);
    every_write_changes_only_its_own_paths(&machine_a, &machine_b);
}
