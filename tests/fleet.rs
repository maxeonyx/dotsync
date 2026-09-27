// Reading the fleet: every scope, from any machine, in one vocabulary.
//
// An agent deciding whether config should be shared, specialised, moved or
// left alone has to see every scope's version of it — including scopes this
// machine does not descend from. These are the questions it asks, answered
// through dotsync alone, without the hidden repo or a clone of the remote.

mod harness;
use harness::*;

/// The rows `dotsync files --output json` answered with, for one path.
fn rows_for<'a>(payload: &'a serde_json::Value, path: &str) -> Vec<&'a serde_json::Value> {
    payload["files"]
        .as_array()
        .expect("`files` is an array of rows")
        .iter()
        .filter(|row| row["path"] == path)
        .collect()
}

fn row<'a>(payload: &'a serde_json::Value, scope: &str, path: &str) -> &'a serde_json::Value {
    rows_for(payload, path)
        .into_iter()
        .find(|row| row["scope"] == scope)
        .unwrap_or_else(|| panic!("no row for {path} on {scope}\n{payload:#}"))
}

/// The failure that started this: an agent auditing other machines for config
/// worth sharing could not read a file on a scope this machine is not under,
/// concluded dotsync could not do it, and stopped.
#[test]
fn show_prints_a_file_on_another_machines_scope() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_b.write_file(".config/tool.conf", "colour = green\n");
    machine_b.run_ok("dotsync commit goof-b -m 'this box only' -- .config/tool.conf");

    let shown = machine_a.run_ok("dotsync show goof-b .config/tool.conf");
    assert_stdout_snapshot(&shown, "colour = green\n");

    let json =
        parse_stdout_json(&machine_a.run_ok("dotsync show goof-b .config/tool.conf --output json"));
    assert_eq!(json["command"], "show");
    assert_eq!(json["scope"], "goof-b");
    assert_eq!(json["path"], ".config/tool.conf");
    assert_eq!(json["kind"], "file");
    assert_eq!(json["contents"], "colour = green\n");
}

#[test]
fn show_says_when_the_scope_does_not_hold_the_file() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);

    let output = machine_a.run_expecting("dotsync show goof-b .nothing-here --output json", 1);
    let payload = parse_stdout_json(&output);
    assert_eq!(payload["status"], "error", "{}", render_output(&output));
    assert_eq!(
        payload["error"],
        "file_not_on_scope",
        "{}",
        render_output(&output)
    );
}

/// A symlink's content is its target and an executable is still a file: `show`
/// says which kind it is, because two scopes holding the same bytes as
/// different kinds are different config.
#[test]
fn show_says_what_kind_of_entry_it_printed() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".local/bin/hello", "#!/bin/sh\necho hi\n");
    machine_a.make_executable(".local/bin/hello");
    symlink_at(
        std::path::Path::new("/opt/nvim-config"),
        &machine_a.home_dir.join(".config/nvim"),
    );
    machine_a.run_ok("dotsync commit linux -m 'script and link' -- .local/bin/hello .config/nvim");

    let script =
        parse_stdout_json(&machine_a.run_ok("dotsync show linux .local/bin/hello --output json"));
    assert_eq!(script["kind"], "executable", "{script:#}");

    let link =
        parse_stdout_json(&machine_a.run_ok("dotsync show linux .config/nvim --output json"));
    assert_eq!(link["kind"], "symlink", "{link:#}");
    assert_eq!(link["contents"], "/opt/nvim-config", "{link:#}");
}

/// Where each scope sits, which ones are machines, and which machines a
/// change to a scope would reach — the graph as the questions that need it
/// ask it, rather than as parent lists to walk by hand.
#[test]
fn scopes_lists_the_graph_with_the_machines_each_scope_reaches() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);

    let payload = parse_stdout_json(&machine_a.run_ok("dotsync scopes --output json"));
    assert_eq!(payload["command"], "scopes");
    assert_eq!(payload["machine_scope"], "goof-a");
    let scope = |name: &str| {
        payload["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|scope| scope["name"] == name)
            .unwrap_or_else(|| panic!("no scope {name}\n{payload:#}"))
            .clone()
    };

    let all = scope("all");
    assert_eq!(all["parents"], serde_json::json!([]));
    assert_eq!(all["children"], serde_json::json!(["linux"]));
    assert_eq!(all["machine"], false);
    assert_eq!(all["machines"], serde_json::json!(["goof-a", "goof-b"]));

    let goof_b = scope("goof-b");
    assert_eq!(goof_b["parents"], serde_json::json!(["linux"]));
    assert_eq!(goof_b["machine"], true);
    assert_eq!(goof_b["machines"], serde_json::json!(["goof-b"]));

    let human = machine_a.run_ok("dotsync scopes");
    let stdout = String::from_utf8_lossy(&human.stdout).into_owned();
    assert!(
        stdout.contains("goof-a") && stdout.contains("goof-b") && stdout.contains("linux"),
        "{stdout}"
    );
}

/// What a scope holds that it does not inherit is the audit question: what is
/// local to each machine, what each one overrides, what each one suppresses.
/// A scope's full file list cannot answer it — an override is the same path
/// with different bytes.
#[test]
fn files_own_lists_what_each_scope_adds_overrides_and_removes() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".apprc", "shared = yes\n");
    machine_a.write_file(".gone", "everyone but b\n");
    machine_a.run_ok("dotsync commit all -m 'shared' -- .apprc .gone");
    machine_b.run_ok("dotsync");

    machine_b.write_file(".apprc", "shared = no\n");
    machine_b.delete_file(".gone");
    machine_b.write_file(".local-only", "b\n");
    machine_b.run_ok("dotsync commit goof-b -m 'b differs' -- .apprc .gone .local-only");

    let payload = parse_stdout_json(&machine_a.run_ok("dotsync files --own --output json"));
    assert_eq!(payload["command"], "files");
    assert_eq!(row(&payload, "all", ".apprc")["standing"], "added");
    assert_eq!(row(&payload, "goof-b", ".apprc")["standing"], "overridden");
    assert_eq!(row(&payload, "goof-b", ".gone")["standing"], "removed");
    assert_eq!(row(&payload, "goof-b", ".local-only")["standing"], "added");
    assert!(
        rows_for(&payload, ".apprc")
            .iter()
            .all(|row| row["standing"] != "inherited"),
        "`--own` leaves out what a scope merely inherits\n{payload:#}"
    );
    assert!(
        rows_for(&payload, ".apprc")
            .iter()
            .all(|row| row["scope"] != "linux" && row["scope"] != "goof-a"),
        "{payload:#}"
    );
}

/// Where a machine's version of a file comes from, answered per scope — the
/// question `view --file`'s "owner" answered wrongly whenever two scopes held
/// their own versions.
#[test]
fn files_says_where_each_scopes_version_comes_from() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".apprc", "shared = yes\n");
    machine_a.run_ok("dotsync commit linux -m 'shared' -- .apprc");
    machine_b.run_ok("dotsync");
    machine_b.write_file(".apprc", "shared = no\n");
    machine_b.run_ok("dotsync commit goof-b -m 'b differs' -- .apprc");

    let payload = parse_stdout_json(&machine_a.run_ok("dotsync files .apprc --output json"));
    let on_a = row(&payload, "goof-a", ".apprc");
    assert_eq!(on_a["standing"], "inherited", "{payload:#}");
    assert_eq!(on_a["origin"], serde_json::json!(["linux"]), "{payload:#}");
    let on_b = row(&payload, "goof-b", ".apprc");
    assert_eq!(on_b["origin"], serde_json::json!(["goof-b"]), "{payload:#}");
    assert!(
        rows_for(&payload, ".apprc")
            .iter()
            .all(|row| row["scope"] != "all"),
        "a scope that holds nothing at the path has no row\n{payload:#}"
    );
    assert_ne!(on_a["content"], on_b["content"], "{payload:#}");
}

/// Two machines that each added the same file with the same bytes are the
/// commonest promotion candidate there is, and a content id on every row is
/// what makes them visible without reading every version.
#[test]
fn files_gives_identical_versions_the_same_content_id() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(
        ".config/fish/functions/ll.fish",
        "function ll; ls -l; end\n",
    );
    machine_a.run_ok("dotsync commit goof-a -m 'll' -- .config/fish/functions/ll.fish");
    machine_b.write_file(
        ".config/fish/functions/ll.fish",
        "function ll; ls -l; end\n",
    );
    machine_b.run_ok("dotsync commit goof-b -m 'll' -- .config/fish/functions/ll.fish");

    let payload =
        parse_stdout_json(&machine_b.run_ok("dotsync files --own .config/fish --output json"));
    let a = row(&payload, "goof-a", ".config/fish/functions/ll.fish");
    let b = row(&payload, "goof-b", ".config/fish/functions/ll.fish");
    assert_eq!(a["content"], b["content"], "{payload:#}");
    assert!(a["content"].as_str().is_some_and(|id| !id.is_empty()));
}

#[test]
fn files_for_one_scope_lists_everything_it_holds() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".apprc", "shared = yes\n");
    machine_a.run_ok("dotsync commit all -m 'shared' -- .apprc");
    machine_b.write_file(".local-only", "b\n");
    machine_b.run_ok("dotsync commit goof-b -m 'b' -- .local-only");

    let payload =
        parse_stdout_json(&machine_a.run_ok("dotsync files --scope goof-b --output json"));
    let paths: Vec<&str> = payload["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            assert_eq!(row["scope"], "goof-b", "{payload:#}");
            row["path"].as_str().unwrap()
        })
        .collect();
    assert_eq!(paths, vec![".apprc", ".local-only"], "{payload:#}");
}

/// What a scope changes, as content: the diff between what it inherits and
/// what it holds. This is the evidence for "should this override exist?".
#[test]
fn diff_of_one_scope_shows_what_it_changes_against_what_it_inherits() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".apprc", "theme = dark\nfont = mono\n");
    machine_a.run_ok("dotsync commit all -m 'shared' -- .apprc");
    machine_b.run_ok("dotsync");
    machine_b.write_file(".apprc", "theme = light\nfont = mono\n");
    machine_b.run_ok("dotsync commit goof-b -m 'b likes light' -- .apprc");

    let output = machine_a.run_expecting("dotsync diff goof-b", 1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.contains("-theme = dark") && stdout.contains("+theme = light"),
        "{}",
        render_output(&output)
    );

    let unchanged = machine_a.run_expecting("dotsync diff goof-a", 0);
    assert!(
        String::from_utf8_lossy(&unchanged.stdout).trim().is_empty()
            || String::from_utf8_lossy(&unchanged.stderr).contains("no differences"),
        "{}",
        render_output(&unchanged)
    );
}

#[test]
fn diff_of_two_scopes_compares_them() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".apprc", "theme = dark\n");
    machine_a.run_ok("dotsync commit goof-a -m 'a' -- .apprc");
    machine_b.write_file(".apprc", "theme = light\n");
    machine_b.write_file(".only-b", "b\n");
    machine_b.run_ok("dotsync commit goof-b -m 'b' -- .apprc .only-b");

    let payload =
        parse_stdout_json(&machine_a.run_expecting("dotsync diff goof-a goof-b --output json", 1));
    let changes = payload["changes"].as_array().expect("changes");
    let paths: Vec<&str> = changes
        .iter()
        .map(|change| change["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec![".apprc", ".only-b"], "{payload:#}");
    assert!(
        changes[0]["diff"]
            .as_str()
            .is_some_and(|diff| diff.contains("-theme = dark") && diff.contains("+theme = light")),
        "{payload:#}"
    );

    let narrowed = parse_stdout_json(
        &machine_a.run_expecting("dotsync diff goof-a goof-b --output json -- .only-b", 1),
    );
    assert_eq!(
        narrowed["changes"].as_array().unwrap().len(),
        1,
        "{narrowed:#}"
    );
}

/// Choosing where a new machine hangs, and seeing what that would put in its
/// home, used to mean cloning the remote with git. `init` without a parent
/// now keeps the clone it made and stops, so the fleet can be read first.
#[test]
fn the_fleet_can_be_read_before_this_machine_joins_it() {
    let harness = TestHarness::new();
    let (machine_a, _machine_b) = two_synced_machines(&harness);
    machine_a.write_file(".apprc", "shared = yes\n");
    machine_a.run_ok("dotsync commit linux -m 'shared' -- .apprc");

    let newcomer = harness.machine("machine-c", "linux", "goof-c");
    let stopped = newcomer.init();
    assert_eq!(
        stopped.status.code(),
        Some(1),
        "{}",
        render_output(&stopped)
    );

    let scopes = parse_stdout_json(&newcomer.run_ok("dotsync scopes --output json"));
    assert!(
        scopes["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|scope| scope["name"] == "linux"),
        "{scopes:#}"
    );
    assert_stdout_snapshot(
        &newcomer.run_ok("dotsync show linux .apprc"),
        "shared = yes\n",
    );
    assert!(
        !newcomer.file_exists(".apprc"),
        "reading the fleet writes nothing into home"
    );

    newcomer.run_ok("dotsync init --parent linux");
    assert_eq!(newcomer.read_file(".apprc"), "shared = yes\n");
}

/// A waiting merge's versions exist nowhere but dotsync's output, so the
/// command an agent runs by reflex prints them again.
#[test]
fn status_prints_every_version_of_a_waiting_conflict() {
    let harness = TestHarness::new();
    let (machine, _stop) = pause_a_conflict_on_linux(&harness);

    let payload = parse_stdout_json(&machine.run_ok("dotsync status --output json"));
    let conflicts = payload["conflicts"].as_array().expect("conflicts");
    assert!(!conflicts.is_empty(), "{payload:#}");
    assert!(
        conflicts[0]["versions"]
            .as_array()
            .is_some_and(|v| v.len() == 3),
        "base and both sides\n{payload:#}"
    );
}
