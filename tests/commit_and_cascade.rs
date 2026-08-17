// What a commit records on a scope and how the cascade carries it to the
// descendant scopes and into home.

mod harness;
use harness::*;

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
