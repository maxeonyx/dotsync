// Changing where config lives: promoting it to a shared scope, narrowing it to
// the scope it belongs on, and taking a scope's own version away so it
// inherits again. Each is one run from any machine, whatever scope it targets,
// because the content comes from the repo rather than from this machine's
// home — and each says, per machine, what it changed.

mod harness;
use harness::*;

fn files_json(machine: &MachineEnvironment, args: &str) -> serde_json::Value {
    parse_stdout_json(&machine.run_ok(&format!("dotsync files {args} --output json")))
}

fn standing(payload: &serde_json::Value, scope: &str, path: &str) -> Option<String> {
    payload["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["scope"] == scope && row["path"] == path)
        .map(|row| row["standing"].as_str().unwrap().to_string())
}

/// The machines a write says it changed, and how, keyed `machine:path`.
fn effect(payload: &serde_json::Value) -> Vec<String> {
    let mut found = Vec::new();
    for machine in payload["effect"].as_array().expect("effect") {
        for change in machine["changes"].as_array().unwrap() {
            found.push(format!(
                "{}:{}:{}",
                machine["machine"].as_str().unwrap(),
                change["path"].as_str().unwrap(),
                change["change"].as_str().unwrap()
            ));
        }
    }
    found.sort();
    found
}

/// Config starts on one machine and is shared when a second machine wants it
/// (Max's own procedure). The second machine promotes the first machine's
/// version to the scope they share, in one run: the first machine's config is
/// unchanged, the second gains it, and nothing collides on the first
/// machine's scope.
#[test]
fn move_promotes_another_machines_file_to_the_scope_they_share() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".config/tool.conf", "colour = green\n");
    machine_a.run_ok("dotsync commit goof-a -m 'try it here' -- .config/tool.conf");

    let moved = parse_stdout_json(&machine_b.run_ok(
        "dotsync move .config/tool.conf --from goof-a --to linux -m 'share tool' --output json",
    ));
    assert_eq!(moved["command"], "move", "{moved:#}");
    assert_eq!(
        effect(&moved),
        vec!["goof-b:.config/tool.conf:added".to_string()],
        "goof-a's config is unchanged by construction; only goof-b gains it\n{moved:#}"
    );
    assert_eq!(machine_b.read_file(".config/tool.conf"), "colour = green\n");

    let files = files_json(&machine_b, ".config/tool.conf");
    assert_eq!(
        standing(&files, "linux", ".config/tool.conf").as_deref(),
        Some("added")
    );
    assert_eq!(
        standing(&files, "goof-a", ".config/tool.conf").as_deref(),
        Some("inherited"),
        "goof-a no longer holds its own copy\n{files:#}"
    );

    machine_a.run_ok("dotsync");
    assert_eq!(machine_a.read_file(".config/tool.conf"), "colour = green\n");
    assert_stderr_snapshot(
        &machine_a.run_ok("dotsync status"),
        "dotsync: no changes for goof-a\n",
    );
}

/// Config pushed too broad could be deleted elsewhere but never moved back
/// down: deleting it from the broad scope cascaded the deletion through the
/// narrow one. `move` narrows it in one run — here from a machine that is not
/// even under the target — and the machine that should keep it keeps it.
#[test]
fn move_narrows_config_to_a_scope_this_machine_is_not_under() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".local/bin/underview", "#!/bin/sh\nexec underview-dev\n");
    machine_a.run_ok("dotsync commit linux -m 'underview' -- .local/bin/underview");
    machine_b.run_ok("dotsync");
    assert!(machine_b.file_exists(".local/bin/underview"));

    let moved = parse_stdout_json(&machine_b.run_ok(
        "dotsync move .local/bin/underview --from linux --to goof-a -m 'only goof-a runs underview' --output json",
    ));
    assert_eq!(
        effect(&moved),
        vec!["goof-b:.local/bin/underview:removed".to_string()],
        "{moved:#}"
    );
    assert!(!machine_b.file_exists(".local/bin/underview"));

    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".local/bin/underview"),
        "#!/bin/sh\nexec underview-dev\n"
    );
    let files = files_json(&machine_a, ".local/bin/underview");
    assert_eq!(
        standing(&files, "goof-a", ".local/bin/underview").as_deref(),
        Some("added")
    );
    assert_eq!(
        standing(&files, "linux", ".local/bin/underview"),
        None,
        "{files:#}"
    );
}

/// Unifying config means taking a machine's own version away so it takes the
/// shared one again — on whichever machine holds it, not only this one.
#[test]
fn drop_returns_an_override_to_what_the_scope_inherits() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".apprc", "theme = dark\n");
    machine_a.run_ok("dotsync commit linux -m 'shared' -- .apprc");
    machine_b.run_ok("dotsync");
    machine_b.write_file(".apprc", "theme = light\n");
    machine_b.run_ok("dotsync commit goof-b -m 'b differs' -- .apprc");

    let dropped = parse_stdout_json(
        &machine_a
            .run_ok("dotsync drop .apprc --from goof-b -m 'one theme everywhere' --output json"),
    );
    assert_eq!(dropped["command"], "drop", "{dropped:#}");
    assert_eq!(
        effect(&dropped),
        vec!["goof-b:.apprc:modified".to_string()],
        "{dropped:#}"
    );

    machine_b.run_ok("dotsync");
    assert_eq!(machine_b.read_file(".apprc"), "theme = dark\n");
    let files = files_json(&machine_b, "--own .apprc");
    assert_eq!(standing(&files, "goof-b", ".apprc"), None, "{files:#}");
}

/// A scope's own file with nothing beneath it to fall back to is simply gone
/// from the machines that had it through that scope.
#[test]
fn drop_of_a_file_a_scope_owns_removes_it_from_the_machines_that_had_it() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".stale", "nothing uses this\n");
    machine_a.run_ok("dotsync commit linux -m 'stale' -- .stale");
    machine_b.run_ok("dotsync");

    let dropped = parse_stdout_json(
        &machine_b.run_ok("dotsync drop .stale --from linux -m 'nothing uses it' --output json"),
    );
    assert_eq!(
        effect(&dropped),
        vec![
            "goof-a:.stale:removed".to_string(),
            "goof-b:.stale:removed".to_string()
        ],
        "{dropped:#}"
    );
    assert!(!machine_b.file_exists(".stale"));
    machine_a.run_ok("dotsync");
    assert!(!machine_a.file_exists(".stale"));
}

/// Moving or dropping a scope's version changes who owns the file; it does not
/// decide for other scopes that hold their own. Those keep what they have, so
/// the run never lands a conflict on another machine's scope for this machine
/// to resolve through its own home.
#[test]
fn move_and_drop_leave_other_scopes_own_versions_alone() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".apprc", "theme = dark\n");
    machine_a.run_ok("dotsync commit goof-a -m 'a' -- .apprc");
    machine_b.write_file(".apprc", "theme = light\n");
    machine_b.run_ok("dotsync commit goof-b -m 'b' -- .apprc");

    let moved = parse_stdout_json(
        &machine_b
            .run_ok("dotsync move .apprc --from goof-a --to linux -m 'share a' --output json"),
    );
    assert_eq!(effect(&moved), Vec::<String>::new(), "{moved:#}");
    assert_eq!(machine_b.read_file(".apprc"), "theme = light\n");
    let files = files_json(&machine_b, ".apprc");
    assert_eq!(
        standing(&files, "goof-b", ".apprc").as_deref(),
        Some("overridden")
    );

    let dropped = parse_stdout_json(
        &machine_b.run_ok("dotsync drop .apprc --from linux -m 'no shared apprc' --output json"),
    );
    assert_eq!(
        effect(&dropped),
        vec!["goof-a:.apprc:removed".to_string()],
        "goof-b's own version survives its parent's going\n{dropped:#}"
    );
    assert_eq!(machine_b.read_file(".apprc"), "theme = light\n");
}

#[test]
fn move_from_a_scope_that_only_inherits_the_file_names_where_it_comes_from() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".apprc", "theme = dark\n");
    machine_a.run_ok("dotsync commit all -m 'shared' -- .apprc");

    let output = machine_a.run_expecting(
        "dotsync move .apprc --from linux --to goof-a -m 'x' --output json",
        1,
    );
    let payload = parse_stdout_json(&output);
    assert_eq!(payload["status"], "error");
    assert_eq!(payload["error"], "not_own_on_scope", "{payload:#}");
    assert!(
        render_output(&output).contains("all"),
        "the stop names the scope the file comes from\n{}",
        render_output(&output)
    );
}

/// Every write can be asked what it would do. The answer is the same effect a
/// real run reports, and asking changes nothing anywhere.
#[test]
fn dry_run_reports_the_effect_and_changes_nothing() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".config/tool.conf", "colour = green\n");
    machine_a.run_ok("dotsync commit goof-a -m 'here' -- .config/tool.conf");
    machine_b.run_ok("dotsync");
    let before: Vec<String> = ["all", "linux", "goof-a", "goof-b"]
        .iter()
        .map(|scope| remote_branch_revision(&machine_b, scope))
        .collect();

    let planned = parse_stdout_json(&machine_b.run_ok(
        "dotsync move .config/tool.conf --from goof-a --to linux -m 'share' --dry-run --output json",
    ));
    assert_eq!(planned["dry_run"], true, "{planned:#}");
    assert_eq!(
        effect(&planned),
        vec!["goof-b:.config/tool.conf:added".to_string()],
        "{planned:#}"
    );
    assert!(!machine_b.file_exists(".config/tool.conf"));
    let files = files_json(&machine_b, ".config/tool.conf");
    assert_eq!(
        standing(&files, "linux", ".config/tool.conf"),
        None,
        "{files:#}"
    );

    machine_b.write_file(".bashrc", "alias ll='ls -l'\n");
    let committed = parse_stdout_json(
        &machine_b.run_ok("dotsync commit linux -m 'aliases' --dry-run --output json -- .bashrc"),
    );
    assert_eq!(
        effect(&committed),
        vec!["goof-a:.bashrc:added".to_string()],
        "this machine already has its own edit; the other machine is the one that changes\n{committed:#}"
    );
    assert_eq!(
        standing(&files_json(&machine_b, ".bashrc"), "linux", ".bashrc"),
        None
    );

    let after: Vec<String> = ["all", "linux", "goof-a", "goof-b"]
        .iter()
        .map(|scope| remote_branch_revision(&machine_b, scope))
        .collect();
    assert_eq!(before, after, "a dry run publishes nothing");
}

/// "Tell Max which machines get it untested" is a question a commit to a
/// shared scope can answer about itself.
#[test]
fn a_commit_says_which_machines_it_changed() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".apprc", "theme = dark\n");
    let committed = parse_stdout_json(
        &machine_a.run_ok("dotsync commit linux -m 'shared' --output json -- .apprc"),
    );
    assert_eq!(
        effect(&committed),
        vec!["goof-b:.apprc:added".to_string()],
        "{committed:#}"
    );
}

/// A write from home still has to target a scope home was built from: there is
/// no version of another machine's scope that this machine's edit started
/// from. The repo-sourced verbs are how config reaches those scopes.
#[test]
fn committing_home_to_another_machines_scope_points_at_move() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".apprc", "theme = dark\n");
    let output = machine_a.run_expecting("dotsync commit goof-b -m 'x' -- .apprc", 1);
    assert!(
        render_output(&output).contains("dotsync move"),
        "{}",
        render_output(&output)
    );
}

/// A scope under two parents that hold different versions of a file resolves
/// them with a version of its own. Dropping that version would bring back a
/// merge nobody can resolve from here — so the drop is refused, names the
/// disagreement, and records nothing, rather than stopping on another
/// machine's scope with a pause no later run can find.
#[test]
fn dropping_a_version_that_settles_disagreeing_parents_is_refused() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);
    machine_a.run_ok("dotsync create-scope work --parent all");
    let machine_c = harness.machine("machine-c", "linux", "goof-c");
    machine_c.init_ok_under("linux --parent work");

    machine_c.write_file(".x", "linux\n");
    machine_c.run_ok("dotsync commit linux -m 'x on linux' -- .x");
    machine_c.write_file(".x", "work\n");
    machine_c.run_expecting("dotsync commit work -m 'x on work' -- .x", 1);
    machine_c.write_file(".x", "settled\n");
    machine_c.run_ok("dotsync continue");
    machine_a.run_ok("dotsync");

    for flags in ["--dry-run", ""] {
        let output = machine_a.run_expecting(
            &format!("dotsync drop .x --from goof-c -m 'unify' {flags} --output json"),
            1,
        );
        let payload = parse_stdout_json(&output);
        assert_eq!(
            payload["error"],
            "parents_disagree",
            "{}",
            render_output(&output)
        );
    }
    let status = parse_stdout_json(&machine_a.run_ok("dotsync status --output json"));
    assert!(status.get("paused_cascade").is_none(), "{status:#}");
}
