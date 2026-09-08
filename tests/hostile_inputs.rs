// Dotsync's inputs rather than its workflows: the hand-edited scope graph and
// the environment a machine identifies itself from.
//
// PLAN §2.2: "the defects were found by driving states dotsync knows about;
// the disasters were found by attacking things dotsync accepts." Every other
// scenario file drives a workflow. Everything in here hands dotsync something
// and asks what it does with it.
//
// None of these tests asserts anything about how a scope comes to exist: they
// assert what has to be true of dotsync whichever way it does. The setup is
// confined to the helpers at the bottom of this file — a plain git client
// doing to the remote what a plain git client can do.

mod harness;
use harness::*;

/// A machine's scope, renamed out from under it. Nothing in dotsync renames a
/// scope — the graph is append-only — so the way this happens is a plain git
/// client on the shared remote, which is the same way it happened through
/// `config.toml`: from where the renamer is standing everything is fine, and
/// the machine that was renamed is the one that pays.
///
/// Reproduced by hand on v0.3.25 and recorded in PLAN §2.2: the renamed
/// machine got exit 1 out of every command, one line each, no teaching block
/// and nothing to do next — `view` said the scope had no history, `abort` and
/// `continue` said there was no paused cascade, `init` said already
/// initialized. Its branch was sitting on the remote untouched. There was
/// simply no route back to it.
///
/// What this pins is the route back, not the rename.
#[test]
fn a_machine_whose_scope_the_fleet_renamed_has_a_route_back() {
    let harness = TestHarness::new();
    let (_machine_a, machine_b) = two_synced_machines(&harness);
    machine_b.run_ok("dotsync");

    rename_a_branch_with_a_plain_git_client(&machine_b, "goof-b", "goof-b-renamed");

    assert_dotsync_can_get_this_machine_working(&machine_b);
}

/// A new scope joins the fleet's graph, and `dotsync view` stops working on
/// every machine in it — including the machine that made the change, whose run
/// reported success a moment earlier.
///
/// Reproduced by hand on v0.3.25 and recorded in PLAN §2.2: declaring a scope
/// in `config.toml` the way `docs/SKILL.md` instructed created no bookmark, at
/// commit or at sync, and from then on `view` exited 1 with "scope `hyprland`
/// is configured, but this machine's repo has no history for it" wherever the
/// config reached.
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

    machine_a.run("dotsync create-scope hyprland --parent linux");
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
        // Named by field rather than as a bare string: the overview's
        // `scopes` carries each scope's parents beside its name, which
        // `view_summarizes_checked_in_scopes_and_files` pins and the human
        // rendering of the DAG needs. This asserts that the answer is there,
        // which is what the test is about.
        assert_eq!(
            parse_stdout_json(&view)["scopes"][0]["name"],
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
    machine_a.run("dotsync");

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

/// The remote is a git remote, so anything with git can push to it, and a
/// branch nobody's scope is named after is the commonest thing it will push.
/// Dotsync follows every ref the remote has while modelling only scopes
/// (PLAN §2.2), and the branch's own owner deleting it is what that costs:
/// jj abandons the commits nothing reaches any more, and the fetch
/// transaction that abandoned them is committed without rebasing the
/// descendants jj recorded — every command that fetches panics from then on,
/// `status` included, and the machine stays that way until somebody puts the
/// branch back.
#[test]
fn a_branch_deleted_from_the_remote_does_not_stop_this_machine() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    machine.init_ok();

    push_a_branch_with_a_plain_git_client(&machine, "someones-experiment", "NOTES.md", "wip\n");
    machine.run_ok("dotsync");
    delete_a_branch_with_a_plain_git_client(&machine, "someones-experiment");

    machine.run_ok("dotsync status");
    machine.run_ok("dotsync");
    assert!(
        !remote_branches(&machine).contains(&"someones-experiment".to_string()),
        "and dotsync must not put a branch back that its owner deleted: {:?}",
        remote_branches(&machine)
    );
}

/// The same ref, moved rather than removed. Dotsync offers every bookmark it
/// holds to the remote, so a branch that is nobody's scope is a branch dotsync
/// will happily push its own idea of — undoing a rewind the branch's owner
/// meant.
#[test]
fn a_branch_rewound_on_the_remote_is_not_pushed_back() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    machine.init_ok();

    let ahead =
        push_a_branch_with_a_plain_git_client(&machine, "someones-experiment", "NOTES.md", "wip\n");
    machine.run_ok("dotsync");
    let rewound = rewind_a_branch_with_a_plain_git_client(&machine, "someones-experiment");
    assert_ne!(ahead, rewound, "the fixture did not rewind anything");

    machine.run_ok("dotsync");
    assert_eq!(
        remote_branch_revision(&machine, "someones-experiment"),
        rewound,
        "dotsync moved a branch that is not one of its scopes"
    );
}

/// Renames a branch on the shared remote with a plain git client: the scope's
/// history is still there, under a name nothing is looking for. Setup, not
/// subject.
fn rename_a_branch_with_a_plain_git_client(machine: &MachineEnvironment, from: &str, to: &str) {
    let at = remote_branch_revision(machine, from);
    let create = git_in(&machine.remote_dir, &["branch", to, &at]);
    assert!(create.status.success(), "{}", render_output(&create));
    delete_a_branch_with_a_plain_git_client(machine, from);
}
