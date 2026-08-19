// Dotsync's inputs rather than its workflows: the hand-edited scope graph and
// the environment a machine identifies itself from.
//
// PLAN §2.2: "the defects were found by driving states dotsync knows about;
// the disasters were found by attacking things dotsync accepts." Every other
// scenario file drives a workflow. Everything in here hands dotsync something
// and asks what it does with it.
//
// Two of these arrive through `config.toml` today, and PLAN §2.3 cuts that
// file at step 4 and brings it back with a reconciler at step 8. So none of
// these tests asserts anything about how a scope comes to exist: they assert
// what has to be true of dotsync whichever way it does. The `config.toml`
// editing is confined to the two helpers at the bottom of this file, which are
// the setup and not the subject — when the file goes, they change and the
// assertions do not.

mod harness;
use harness::*;

/// One machine renames another machine's scope. From where it is standing the
/// new graph is perfectly valid — every scope it belongs to is still there —
/// so the change is published, and the machine that was renamed is the one
/// that pays.
///
/// Reproduced by hand on v0.3.25 and recorded in PLAN §2.2: the renamed
/// machine gets exit 1 out of every command. `dotsync`, `status` and `diff`
/// say `unable to determine current machine scope`, one line, no teaching
/// block and nothing to do next; `view` says the scope has no history;
/// `continue` and `abort` say there is no paused cascade; `init` says already
/// initialized. Its branch is sitting on the remote untouched. There is simply
/// no route back to it.
///
/// What this pins is the route back, not the rename. A future where publishing
/// a rearrangement that breaks somebody else is refused outright passes this
/// too, because then there is nothing to recover from — which is why the run
/// that publishes it is deliberately not required to succeed.
#[test]
fn a_machine_whose_scope_the_fleet_renamed_has_a_route_back() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    machine_b.run_ok("dotsync");

    rename_a_scope(&machine_a, "goof-b", "goof-b-renamed");
    machine_a.run("dotsync commit all -m 'rename goof-b' -- .config/dotsync/config.toml");

    assert_dotsync_can_get_this_machine_working(&machine_b);
}

/// A new scope joins the fleet's graph, and `dotsync view` stops working on
/// every machine in it — including the machine that made the change, whose run
/// reported success a moment earlier.
///
/// Reproduced by hand on v0.3.25 and recorded in PLAN §2.2: declaring a scope
/// the way `docs/SKILL.md` instructs creates no bookmark, at commit or at
/// sync, and from then on `view` exits 1 with "scope `hyprland` is configured,
/// but this machine's repo has no history for it" wherever the config reaches.
///
/// PLAN §2.3 step 3 is what this pins: `status`, `diff` and `view` "must work
/// on any repo state". A read-only command that refuses to describe the state
/// it is in is refusing to do the one thing it is for — and a machine that had
/// nothing to do with the change has lost the command it would use to find out
/// what happened.
///
/// **Not pinned here, and reported as an open question:** that a run reporting
/// success for a scope creation means the scope actually exists. Every way of
/// writing that pins the creation mechanism, which steps 4 and 8 remove and
/// reinstate.
#[test]
fn view_still_answers_on_every_machine_after_a_scope_joins_the_graph() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    machine_b.run_ok("dotsync");

    declare_a_scope(&machine_a, "hyprland", "linux");
    machine_a.run("dotsync commit all -m 'add the hyprland scope' -- .config/dotsync/config.toml");
    machine_b.run("dotsync");

    for (whose, machine) in [
        ("the machine that had nothing to do with it", &machine_b),
        ("the machine that made the change", &machine_a),
    ] {
        let view = machine.run("dotsync view --output json");
        assert_eq!(
            view.status.code(),
            Some(0),
            "`view` is how a machine finds out what the scopes hold, and {whose} no longer has it\n{}",
            render_output(&view)
        );
        assert_eq!(
            parse_stdout_json(&view)["scopes"][0],
            "all",
            "and it has to answer with the scopes, not just exit quietly\n{}",
            render_output(&view)
        );
    }
}

/// A machine that calls itself by the name of a scope other machines share.
/// `DOTSYNC_HOSTNAME=linux` on a linux machine is the easy way to do it by
/// accident, and dotsync takes the name: `dotsync init` says "initialized
/// linux", `dotsync status` answers `"machine_scope":"linux"`, and from then
/// on the scope this machine is told is its own is the shared OS scope.
///
/// Reproduced by hand on v0.3.25 and recorded in PLAN §2.2. A file committed
/// to it — which the agent has every reason to believe reaches this machine
/// and no other, because that is what a machine scope is — lands in the other
/// linux machine's home on its next ordinary sync, exit 0 at both ends and
/// nothing said anywhere.
///
/// The scope committed to is whatever dotsync says this machine's own is,
/// rather than `linux` spelled out, so this asks the question in every future:
/// refuse the name, hand out a different one, or keep matching hostnames and
/// make the collision impossible some other way — all of them pass, and the
/// only thing that fails is a machine scope that is not this machine's alone.
/// If dotsync will not name one at all, nothing was ever published under it
/// and there is nothing left to check.
#[test]
fn a_machine_named_after_a_shared_scope_does_not_publish_its_private_config() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let named_after_a_shared_scope = harness.machine("machine-b", "linux", "linux");

    machine_a.init_ok();
    named_after_a_shared_scope.init();
    machine_a.run("dotsync --force");

    if let Some(own_scope) = machine_scope_reported_by(&named_after_a_shared_scope) {
        named_after_a_shared_scope.write_file(
            ".config/machine-only.conf",
            "identity = \"this machine only\"\n",
        );
        named_after_a_shared_scope.run(&format!(
            "dotsync commit {own_scope} -m 'config for this machine only' -- .config/machine-only.conf"
        ));
    }

    let sync_a = machine_a.run("dotsync");
    assert!(
        !machine_a.file_exists(".config/machine-only.conf"),
        "the scope dotsync calls a machine's own has to be that machine's alone, and this one is shared with every linux machine\n--- what the other machine holds ---\n{}\n--- its sync ---\n{}",
        machine_a.read_file(".config/machine-only.conf"),
        render_output(&sync_a)
    );
}

/// Renames a scope in this machine's home `config.toml`, the way an agent
/// editing the file would. Setup, not subject: PLAN §2.3 step 4 cuts this file
/// and step 8 brings it back with a reconciler behind it, so this helper is
/// expected to be rewritten and the tests using it are not.
fn rename_a_scope(machine: &MachineEnvironment, from: &str, to: &str) {
    let path = ".config/dotsync/config.toml";
    let config = machine.read_file(path);
    let renamed = config.replace(&format!("\n{from} = "), &format!("\n{to} = "));
    assert_ne!(config, renamed, "the fixture found no `{from}` to rename");
    machine.write_file(path, &renamed);
}

/// Declares a new scope in this machine's home `config.toml` — `docs/SKILL.md`
/// tells an agent to edit this file and commit it to `all`, and this is that
/// edit. Setup, not subject, for the same reason as `rename_a_scope`.
fn declare_a_scope(machine: &MachineEnvironment, name: &str, parent: &str) {
    let path = ".config/dotsync/config.toml";
    let config = machine.read_file(path);
    let declared = config.replace(
        "\n[scopes]\n",
        &format!("\n[scopes]\n\n# `{name}` — every machine running {name}.\n{name} = {{ parents = [\"{parent}\"] }}\n"),
    );
    assert_ne!(
        config, declared,
        "the fixture found no `[scopes]` table to add to"
    );
    machine.write_file(path, &declared);
}
