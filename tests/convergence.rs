// This machine and the remote each holding commits the other does not:
// unpushed scopes after an interrupted push, diverged bookmarks, lost push
// races, and cascades that were only half published.

mod harness;
use harness::*;

#[test]
fn interrupted_push_reports_that_scope_updates_were_not_pushed() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".config/fish/dev-certs.fish", "set -gx DEV_CERTS 1\n");
    block_remote_pushes(&machine);
    // Given a limit, because item 2 turns the push into a retry loop and this
    // is the state that must not be retried: the remote is refusing the write
    // itself, so trying again cannot change the answer. A run that never
    // returns is the failure this guards, and a plain `output()` reports it as
    // a suite the runner killed minutes later with nothing said about where.
    let commit_output = machine.run_within(
        "dotsync --output json commit all -m 'add dev-certs helper' -- .config/fish/dev-certs.fish",
        std::time::Duration::from_secs(60),
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

/// DESIGN, "The convergence model": bookmark divergence is "a **routine
/// event, not an edge case**", and the convergence pass answers it — "diverged
/// → a real merge commit".
///
/// This is that event in its plainest form. Machine B's push was interrupted,
/// so it holds a commit on `all` the remote has never seen; machine A, knowing
/// nothing about that, published a different file to the same scope. Both
/// machines are now ahead of the other on `all` and on `linux`, from two
/// innocent non-overlapping edits.
///
/// Nothing here is exotic and nothing is lost: the merge is trivial, both
/// files survive, and the remote ends up holding both. The test follows it all
/// the way round to machine A, because a convergence that only heals the
/// machine that diverged has not converged anything.
#[test]
fn two_machines_that_both_moved_a_scope_converge_into_a_merge() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    // Machine B's cascade lands and its push does not — the issue #19 shape,
    // and the commonest way a machine ends up holding history the remote lacks.
    machine_b.write_file(".config/from-b.conf", "from machine b\n");
    block_remote_pushes(&machine_b);
    machine_b.run("dotsync commit all -m 'add b config' -- .config/from-b.conf");
    allow_remote_pushes(&machine_b);
    assert_ne!(
        bookmark_revision(&machine_b, "all"),
        remote_branch_revision(&machine_b, "all"),
        "this test needs a push that really was rejected"
    );

    machine_a.write_file(".config/from-a.conf", "from machine a\n");
    machine_a.run_ok("dotsync commit all -m 'add a config' -- .config/from-a.conf");

    let converge = machine_b.run("dotsync");
    assert_eq!(
        converge.status.code(),
        Some(0),
        "two machines that both moved `all` is the routine event the convergence pass exists for, not a stop\n{}",
        render_output(&converge)
    );

    assert_eq!(
        machine_b.read_file(".config/from-a.conf"),
        "from machine a\n",
        "the other machine's change has to arrive"
    );
    assert_eq!(
        machine_b.read_file(".config/from-b.conf"),
        "from machine b\n",
        "and this machine's own must survive being merged with it"
    );
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/from-b.conf"),
        "from machine b\n",
        "the merge has to reach the remote too, or the divergence is still there next run"
    );
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/from-a.conf"),
        "from machine a\n"
    );

    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".config/from-b.conf"),
        "from machine b\n",
        "and the machine that never diverged picks the merge up as an ordinary sync"
    );
}

/// DESIGN, "The convergence model": "diverged → a real merge commit, pausing
/// on file conflicts exactly like any cascade merge."
///
/// Two machines edited the same line of the same file on the same scope, and
/// only one of them got its push through. That is the divergence that cannot
/// be merged silently, and it is the one where "divergence is a merge, not a
/// wall" earns its keep: the answer is the ordinary conflict, put in front of
/// the agent in home, resolved with the ordinary command.
///
/// Today the run stops with `scope_diverged` before anything is merged, so the
/// agent is shown neither the version it is colliding with nor the version
/// they both came from, and the only way out of the state is repo surgery.
///
/// Where the three versions reach the agent is not asserted: PLAN §2.3 step 6
/// leaves conflict presentation to the agent validation loop, so markers in
/// home and a description that leaves home alone are both live answers. What
/// they have to hold is the same either way.
#[test]
fn a_divergence_over_one_file_is_a_conflict_to_resolve_rather_than_a_wall() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.run_ok("dotsync commit all -m 'add base config' -- .config/app.conf");
    machine_b.run_ok("dotsync");

    // Machine B's edit lands locally and cannot be published.
    machine_b.write_file(".config/app.conf", "setting = \"from b\"\n");
    block_remote_pushes(&machine_b);
    machine_b.run("dotsync commit all -m 'set from b' -- .config/app.conf");
    allow_remote_pushes(&machine_b);
    assert_ne!(
        bookmark_revision(&machine_b, "all"),
        remote_branch_revision(&machine_b, "all"),
        "this test needs a push that really was rejected"
    );

    // Machine A edits the same line and publishes it.
    machine_a.write_file(".config/app.conf", "setting = \"from a\"\n");
    machine_a.run_ok("dotsync commit all -m 'set from a' -- .config/app.conf");

    let converge = machine_b.run("dotsync");
    assert_the_conflict_is_in_front_of_the_agent(
        &machine_b,
        &converge,
        ".config/app.conf",
        [
            "setting = \"base\"",
            "setting = \"from a\"",
            "setting = \"from b\"",
        ],
    );

    // Conflicted heads are never pushed, so the remote still holds exactly
    // what machine A published while this machine works on it.
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/app.conf"),
        "setting = \"from a\"\n",
        "an unresolved conflict must not reach the shared remote"
    );

    machine_b.write_file(".config/app.conf", "setting = \"agreed\"\n");
    machine_b.run_ok("dotsync continue");
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/app.conf"),
        "setting = \"agreed\"\n",
        "and the resolution is published like any other, or the wall just moved"
    );
    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".config/app.conf"),
        "setting = \"agreed\"\n"
    );
}

/// DESIGN, "The convergence model": "**Push is a loop, not a step.** A
/// rejected push isn't an error; it means another machine pushed first. Fetch,
/// converge, push again."
///
/// The race is real and narrow: this machine fetched, built its commit and its
/// cascade on what it found, and in the time that took, another machine
/// published to the same scope. The remote refuses the push because it has
/// moved. Nothing is wrong — the run simply has to notice, converge onto what
/// arrived, and offer it again.
///
/// Today the push is one attempt: the run reports the scope as unpushed and
/// stops trying. Worse than the report, the state it leaves behind is the
/// diverged one, so the rerun an agent would reach for is a dead end too.
#[test]
fn a_push_another_machine_won_the_race_to_is_retried_within_the_run() {
    let harness = TestHarness::new();
    let (_machine_a, machine_b) = two_synced_machines(&harness);

    park_a_racing_commit(&machine_b, "all", ".config/from-a.conf", "from machine a\n");

    machine_b.write_file(".config/from-b.conf", "from machine b\n");
    let commit = machine_b.run_racing_the_first_push(
        "dotsync commit all -m 'add b config' --output json -- .config/from-b.conf",
        "all",
    );
    assert_eq!(
        commit.status.code(),
        Some(0),
        "losing a push race is not a failure of the run that lost it\n{}",
        render_output(&commit)
    );

    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/from-a.conf"),
        "from machine a\n",
        "this test needs a race the other machine really won"
    );
    assert!(
        remote_branch_holds(&machine_b, "all", ".config/from-b.conf"),
        "and the run has to converge onto what it found and publish anyway\n{}",
        render_output(&commit)
    );
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/from-b.conf"),
        "from machine b\n"
    );

    let payload = parse_stdout_json(&commit);
    assert_eq!(
        payload["unpushed_scopes"],
        serde_json::json!([]),
        "a scope that was published on the second attempt is not unpushed\n{}",
        render_output(&commit)
    );
    assert_eq!(
        machine_b.read_file(".config/from-a.conf"),
        "from machine a\n",
        "and what the run converged onto reaches home, like any other incoming change"
    );
}

/// PLAN item 2: "Kill-in-the-middle black-box tests enforce that every
/// interruption point converges on rerun."
///
/// This is that matrix's `continue` row, at the one interruption point that
/// does not already converge. `continue` finishes the cascade in one
/// transaction and then pushes, so a push that fails leaves a machine holding
/// a resolved conflict the remote has never seen — the issue #19 state, one
/// flow along. On its own that heals: the next run publishes it. It stops
/// healing the moment another machine publishes anything at all, because then
/// the scope has diverged and every command on this machine answers
/// `scope_diverged` instead.
///
/// So the run that lost its push is told "The next run will try again", and
/// the next run cannot.
#[test]
fn an_interrupted_continue_converges_when_another_machine_moved_the_scope() {
    let harness = TestHarness::new();
    let (machine_a, machine_b, _pause) = pause_a_conflict_on(&harness, "linux");

    machine_b.write_file(".config/app.conf", "setting = \"resolved\"\n");
    block_remote_pushes(&machine_b);
    machine_b.run("dotsync continue");
    allow_remote_pushes(&machine_b);
    assert_ne!(
        bookmark_revision(&machine_b, "linux"),
        remote_branch_revision(&machine_b, "linux"),
        "this test needs a `continue` whose push really was rejected"
    );

    machine_a.write_file(".config/from-a.conf", "from machine a\n");
    machine_a.run_ok("dotsync commit all -m 'add a config' -- .config/from-a.conf");

    let rerun = machine_b.run("dotsync --output json");
    assert_eq!(
        rerun.status.code(),
        Some(0),
        "rerunning dotsync is the whole remedy for an interrupted run, so it cannot be the thing that stops\n{}",
        render_output(&rerun)
    );
    assert_eq!(
        remote_branch_file_contents(&machine_b, "linux", ".config/app.conf"),
        "setting = \"resolved\"\n",
        "the resolution the agent already did has to reach the remote"
    );
    assert_eq!(
        parse_stdout_json(&rerun)["unpushed_scopes"],
        serde_json::json!([]),
        "and nothing may be left behind\n{}",
        render_output(&rerun)
    );
    assert_eq!(
        machine_b.read_file(".config/from-a.conf"),
        "from machine a\n",
        "while the change that arrived meanwhile is applied like any other"
    );
}

/// DESIGN, "The convergence model": "for each scope in topological order, the
/// new head is the merge of {local head, remote head, **updated parent-scope
/// heads**}".
///
/// That third input is the one nothing today computes, and this is what it
/// costs. Machine A's push was accepted for `all` and refused for `linux` —
/// half-published, which is what a push interrupted in the middle looks like
/// from the outside. Machine A is then gone: the laptop is shut, the agent
/// moved on. Every other machine now fetches an `all` that has moved and a
/// `linux` that has not, and no cascade will ever carry the change down,
/// because the cascade lived in the run that died.
///
/// Machine B says `no changes`, reports `synced 1 file(s)` and applies
/// nothing, run after run, silently, for ever. Under convergence there is
/// nothing special about the state at all: `linux`'s new head merges the `all`
/// head that just moved, machine B's own scope merges that, the file arrives,
/// and publishing the result heals machine A's half-finished push for
/// everybody.
#[test]
fn a_scope_published_without_its_cascade_still_reaches_the_other_machines() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".gitconfig", "[user]\n\tname = Shared\n");
    block_remote_pushes_except(&machine_a, "all");
    machine_a.run("dotsync commit all -m 'share git identity' -- .gitconfig");
    allow_every_branch_to_be_pushed(&machine_a);
    assert!(
        remote_branch_holds(&machine_a, "all", ".gitconfig"),
        "this test needs a push the remote accepted for `all`"
    );
    for scope in ["linux", "goof-a", "goof-b"] {
        assert!(
            !remote_branch_holds(&machine_a, scope, ".gitconfig"),
            "and did not accept for `{scope}`, which is the half that never happened"
        );
    }

    let sync = machine_b.run_ok("dotsync");
    assert!(
        machine_b.file_exists(".gitconfig"),
        "a change published to a scope this machine descends from has to arrive, whatever run put it there\n{}",
        render_output(&sync)
    );
    assert_eq!(
        machine_b.read_file(".gitconfig"),
        "[user]\n\tname = Shared\n"
    );
    assert!(
        bookmark_has_file(&machine_b, "goof-b", ".gitconfig"),
        "which means this machine's own scope merged the parent that moved\n{}",
        render_output(&sync)
    );
    assert!(
        remote_branch_holds(&machine_b, "linux", ".gitconfig"),
        "and publishing that merge finishes what the interrupted run started, for every other machine too\n{}",
        render_output(&sync)
    );
}
