// `dotsync abort` undoes what the paused cascade wrote to home, and nothing
// else: every other path in home — tracked, untracked, edited by someone else
// while the merge waited — is byte-for-byte what it was.
//
// The rule is checked over the whole home tree rather than a list of known
// victims, because the incident that prompted it lost files nobody would have
// thought to list.

mod harness;
use harness::*;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Every file and link in home, by path relative to home: its kind, bytes and
/// executable bit. Dotsync's own state and the harness's scratch clones are
/// left out, because they are not home's content.
fn home_tree(machine: &MachineEnvironment) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).expect("list home") {
            let entry = entry.expect("list home");
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("under home")
                .to_string_lossy()
                .replace('\\', "/");
            if relative == ".local/share/dotsync" || relative.starts_with("remote-") {
                continue;
            }
            let kind = entry.file_type().expect("file type");
            if kind.is_dir() {
                walk(root, &path, out);
            } else if kind.is_symlink() {
                let target = fs::read_link(&path).expect("read link");
                out.insert(relative, format!("link -> {}", target.display()));
            } else {
                let bytes = fs::read(&path).expect("read file");
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    entry.metadata().expect("metadata").permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let executable = false;
                out.insert(
                    relative,
                    format!(
                        "{}{}",
                        if executable { "(x) " } else { "" },
                        String::from_utf8_lossy(&bytes)
                    ),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(&machine.home_dir, &machine.home_dir, &mut out);
    out
}

/// The paths whose content differs between two home trees.
fn changed_between(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut paths: Vec<String> = before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// The rule: home after abort is home as the abort found it, except at the
/// paths the paused run wrote, which hold what they held before it ran.
fn assert_abort_restored_only_what_the_cascade_wrote(
    before_pause: &BTreeMap<String, String>,
    after_pause: &BTreeMap<String, String>,
    before_abort: &BTreeMap<String, String>,
    after_abort: &BTreeMap<String, String>,
) {
    let cascade_wrote = changed_between(before_pause, after_pause);
    let mut expected = before_abort.clone();
    for path in &cascade_wrote {
        match before_pause.get(path) {
            Some(content) => expected.insert(path.clone(), content.clone()),
            None => expected.remove(path),
        };
    }
    let wrong = changed_between(&expected, after_abort);
    let report: Vec<String> = wrong
        .iter()
        .map(|path| {
            format!(
                "  {path}\n    expected: {:?}\n    after abort: {:?}",
                expected.get(path),
                after_abort.get(path)
            )
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "abort changed home outside what the paused run wrote (which was {cascade_wrote:?}):\n{}",
        report.join("\n")
    );
}

/// A fleet of `all -> linux -> {goof-a, goof-b}` with `.claude/settings.json`
/// on `all` and overridden on `linux`, and goof-b's home holding what a real
/// home holds besides config: dotsync's own binary, folders of files nobody
/// tracks beside ones that are tracked, and uncommitted edits.
fn incident_fleet(harness: &TestHarness) -> (MachineEnvironment, MachineEnvironment) {
    let (machine_a, machine_b) = two_synced_machines(harness);

    machine_a.write_file(".claude/settings.json", "{ \"model\": \"base\" }\n");
    machine_a.write_file(".claude/CLAUDE.md", "# Shared\n");
    machine_a.write_file(".claude/cc-writes", "line one\n");
    machine_a.write_file(".config/.gitignore", "secret.env\n");
    machine_a.run_ok(
        "dotsync commit all -m 'shared config' -- .claude/settings.json .claude/CLAUDE.md .claude/cc-writes .config/.gitignore",
    );
    machine_b.run_ok("dotsync");
    machine_a.write_file(".claude/settings.json", "{ \"model\": \"linux\" }\n");
    machine_a.run_ok("dotsync commit linux -m 'linux settings' -- .claude/settings.json");

    machine_b.write_file(".local/bin/dotsync", "\x7fELF pretend binary\n");
    machine_b.make_executable(".local/bin/dotsync");
    for task in [
        "prioritize-morning",
        "prioritize-afternoon",
        "ticker-release-friday",
        "weekly-review",
        "inbox-sweep",
    ] {
        machine_b.write_file(
            &format!(".claude/scheduled-tasks/{task}/SKILL.md"),
            &format!("# {task}\n"),
        );
    }
    machine_b.write_file(".config/secret.env", "TOKEN=ignored\n");
    machine_b.write_file(".cache/thing.log", "cache\n");

    (machine_a, machine_b)
}

/// Commits goof-b's settings to `all`, which collides with `linux`'s override
/// and pauses, then aborts — with `between` run while the merge waits.
fn pause_and_abort(machine_b: &MachineEnvironment, between: impl FnOnce()) {
    // Somebody else's work in progress, never committed.
    machine_b.write_file(".config/.gitignore", "secret.env\nanother.env\n");
    machine_b.write_file(".claude/cc-writes", "line one\nline two\n");
    machine_b.write_file(".claude/settings.json", "{ \"model\": \"wrong scope\" }\n");

    let before_pause = home_tree(machine_b);
    let paused = machine_b.run("dotsync commit all -m 'settings' -- .claude/settings.json");
    assert_eq!(
        paused.status.code(),
        Some(1),
        "the commit should pause on linux's override\n{}",
        render_output(&paused)
    );
    let after_pause = home_tree(machine_b);
    between();
    let before_abort = home_tree(machine_b);
    let aborted = machine_b.run("dotsync abort");
    assert!(aborted.status.success(), "{}", render_output(&aborted));
    let after_abort = home_tree(machine_b);

    assert_abort_restored_only_what_the_cascade_wrote(
        &before_pause,
        &after_pause,
        &before_abort,
        &after_abort,
    );
}

#[test]
fn abort_leaves_home_untouched_outside_what_the_cascade_wrote() {
    let harness = TestHarness::new();
    let (_machine_a, machine_b) = incident_fleet(&harness);
    pause_and_abort(&machine_b, || {});
}

#[test]
fn abort_leaves_alone_untracked_files_a_status_listed() {
    let harness = TestHarness::new();
    let (_machine_a, machine_b) = incident_fleet(&harness);
    // Read-only: asks which files under these folders no scope tracks. The
    // folders that appear afterwards are siblings nothing ever listed.
    machine_b.run_ok("dotsync status .claude .local/bin");
    for task in ["standup-notes", "release-checklist"] {
        machine_b.write_file(
            &format!(".claude/scheduled-tasks/{task}/SKILL.md"),
            &format!("# {task}\n"),
        );
    }
    pause_and_abort(&machine_b, || {});
}

#[test]
fn abort_leaves_alone_files_written_while_the_merge_waited() {
    let harness = TestHarness::new();
    let (_machine_a, machine_b) = incident_fleet(&harness);
    pause_and_abort(&machine_b, || {
        machine_b.write_file(".claude/scheduled-tasks/new-task/SKILL.md", "# new\n");
        machine_b.write_file(".claude/CLAUDE.md", "# Shared\nedited while paused\n");
        machine_b.write_file(".local/bin/dotsync", "\x7fELF upgraded binary\n");
    });
}
