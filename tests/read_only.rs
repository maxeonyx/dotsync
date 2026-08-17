// `status`, `diff` and `view`: what they report, in the same words as each
// other, without mutating anything and without refusing to answer.

mod harness;
use harness::*;

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
