// The command line itself: argument parsing, usage errors, the JSON error
// envelope every stop shares, and the vocabulary the stops are allowed to use.

mod harness;
use harness::*;

#[test]
fn unknown_command_is_not_treated_as_scope_commit() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let output = machine.run("dotsync nonesuch");

    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown top-level command should be a usage error\n{}",
        render_output(&output)
    );
    assert_stderr_snapshot(
        &output,
        "dotsync: unknown command `nonesuch`; run `dotsync --help` for supported commands\n",
    );
}

/// An unknown command with `--output json` after it emitted nothing at all on
/// stdout: clap's external-subcommand catch-all swallows every trailing
/// argument, so the flag never reached the parsed value the emitter reads. An
/// empty stdout and exit 2 is indistinguishable from a crash, for the one
/// mistake an agent makes most.
#[test]
fn an_unknown_command_honors_the_json_contract_wherever_the_flag_sits() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    for command in ["dotsync --output json bogus", "dotsync bogus --output json"] {
        let output = machine.run(command);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{command}\n{}",
            render_output(&output)
        );
        let json = parse_stdout_json(&output);
        assert_eq!(json["status"], "error", "{command}");
        assert_eq!(json["error"], "usage", "{command}");
        assert!(
            json["message"]
                .as_str()
                .is_some_and(|message| message.contains("bogus")),
            "{command}\n{}",
            render_output(&output)
        );
    }
}

#[test]
fn clap_usage_errors_emit_the_json_contract() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    // Clap's own usage errors used to exit 2 with plain text and nothing on
    // stdout, so an agent driving dotsync with `--output json` got no JSON at
    // all for the most common mistake it can make.
    for command in [
        "dotsync commit --output json",
        "dotsync --output json commit",
        "dotsync --output json commit all --nosuchflag",
    ] {
        let output = machine.run(command);
        assert_eq!(
            output.status.code(),
            Some(2),
            "`{command}` should be a usage error\n{}",
            render_output(&output)
        );

        let payload = parse_stdout_json(&output);
        assert_eq!(payload["status"], "error", "`{command}`");
        assert_eq!(payload["error"], "usage", "`{command}`");
        assert!(
            payload["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "`{command}` should explain the usage error in its JSON message\n{}",
            render_output(&output)
        );
        assert!(
            !output.stderr.is_empty(),
            "`{command}` should still explain itself on stderr\n{}",
            render_output(&output)
        );
    }
}

/// Every error payload carries the same three collections, so error handling
/// has one shape — PLAN says so. Usage errors carried none of them, which is
/// the one error an agent hits before it has learned anything else.
#[test]
fn a_usage_error_has_the_same_shape_as_every_other_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let output = machine.run_expecting("dotsync --output json bogus", 2);

    let json = parse_stdout_json(&output);
    for field in ["current_state", "drifts", "forced_overwrites"] {
        assert_eq!(
            json[field].as_array().map(Vec::len),
            Some(0),
            "a usage error must carry `{field}` like every other error\n{}",
            render_output(&output)
        );
    }
}

#[test]
fn output_format_is_accepted_after_the_subcommand() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // `--force` is global and `--output` was not, so the two flags on the same
    // struct had opposite positional rules and neither one said so.
    let status_output = machine.run_expecting("dotsync status --output json", 0);

    let payload = parse_stdout_json(&status_output);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["command"], "status");
    assert_eq!(payload["machine_scope"], "mx-xps-cy");
}

#[test]
fn force_is_refused_with_one_message_wherever_it_is_meaningless() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // `--force` reaches exactly one decision: whether to overwrite drifted
    // home files. On the commands that never write home it means nothing, and
    // it has to say so in whichever position the agent wrote it - `--output`
    // works after the subcommand, so that is the position agents will reach
    // for.
    for command in [
        "dotsync status --force",
        "dotsync --force status",
        "dotsync diff --force",
        "dotsync --force diff",
        "dotsync view --force",
        "dotsync --force view",
        "dotsync init --force",
        "dotsync --force init",
        "dotsync abort --force",
        "dotsync --force abort",
    ] {
        let output = machine.run(command);
        assert_eq!(
            output.status.code(),
            Some(2),
            "`{command}` should be a usage error\n{}",
            render_output(&output)
        );
        let name = command
            .split_whitespace()
            .find(|word| !matches!(*word, "dotsync" | "--force"))
            .expect("command name");
        assert_stderr_snapshot(
            &output,
            &format!(
                "dotsync: `--force` has no meaning for `{name}`; it only decides whether to overwrite drifted files in your home directory, which is a choice made by plain `dotsync`, `commit`, and `continue`\n"
            ),
        );
    }

    // The commands that do write home keep it, in both positions.
    machine.write_file(".apprc", "ui_theme = dark\n");
    machine.run_ok("dotsync commit all -m 'add apprc' --force -- .apprc");
    machine.write_file(".apprc", "ui_theme = light\n");
    machine.run_ok("dotsync --force commit all -m 'light theme' -- .apprc");
    machine.run_ok("dotsync --force");
}

/// A run that found three problems found three things, not one paragraph. The
/// human rendering joins them; the machine-readable one must not have to be
/// split back apart on a newline.
#[test]
fn a_run_that_stops_lists_each_thing_it_found_separately() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let output = machine.run_expecting(
        "dotsync --output json commit all -m x -- /etc/passwd ../outside typo.conf",
        1,
    );

    let json = parse_stdout_json(&output);
    let current_state = json["current_state"].as_array().unwrap_or_else(|| {
        panic!(
            "current_state should be an array\n{}",
            render_output(&output)
        )
    });
    assert_eq!(
        current_state.len(),
        3,
        "one entry per path the run refused\n{}",
        render_output(&output)
    );
    assert!(
        current_state
            .iter()
            .all(|entry| entry.as_str().is_some_and(|text| !text.contains('\n'))),
        "each entry is one fact, not a joined paragraph\n{}",
        render_output(&output)
    );

    let not_initialized = harness
        .machine("machine-b", "linux", "mx-vps-fd")
        .run("dotsync --output json status");
    let json = parse_stdout_json(&not_initialized);
    assert!(
        json["current_state"].is_array(),
        "every error carries the same shape of state, whether it has one fact or three\n{}",
        render_output(&not_initialized)
    );
}

/// dotsync abstracts jj away entirely, which has to include the words it uses
/// when it stops. `bookmark` is a jj concept an agent has never been told
/// about, and a file that is simply not on a scope is an ordinary answer rather
/// than a backend failure.
#[test]
fn a_user_never_meets_the_backends_vocabulary() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let bad_scope = machine.run_expecting("dotsync --output json view --scope nosuchscope", 1);
    let stderr = String::from_utf8_lossy(&bad_scope.stderr).into_owned();
    assert!(
        !stderr.contains("bookmark"),
        "a scope that does not exist is a scope that does not exist\n{stderr}"
    );
    assert_eq!(
        parse_stdout_json(&bad_scope)["error"],
        "invalid_scope",
        "and it is the same mistake `commit` already teaches about\n{}",
        render_output(&bad_scope)
    );

    let missing_file = machine.run_expecting(
        "dotsync --output json view --scope all --file .nosuchfile",
        1,
    );
    let stderr = String::from_utf8_lossy(&missing_file.stderr).into_owned();
    assert!(
        !stderr.contains("jj operation failed"),
        "a file that is not on a scope is not an internal failure\n{stderr}"
    );
    let json = parse_stdout_json(&missing_file);
    assert_eq!(
        json["error"],
        "file_not_on_scope",
        "and an agent branching on the code can tell it from a broken repo\n{}",
        render_output(&missing_file)
    );
}

/// Every command a stop tells you to rerun has to be a command dotsync knows.
///
/// The advice was built from the identifier the JSON payload uses for the
/// command, and for plain sync those are not the same string: the payload says
/// `"sync"`, but bare `dotsync` *is* the sync command and `dotsync sync` is not
/// a subcommand at all. So a fresh machine's very first interaction ended by
/// naming an invocation that exits 2 — the same defect this advice exists to
/// fix, inverted, on the reflex command.
#[test]
fn every_command_the_advice_names_is_a_command_dotsync_knows() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let sync = machine.run_expecting("dotsync", 1);
    let sync_stderr = String::from_utf8_lossy(&sync.stderr).into_owned();
    assert!(
        sync_stderr.contains("then rerun `dotsync`."),
        "bare `dotsync` is how you run a sync; there is nothing to add to it\n{sync_stderr}"
    );

    let commit = machine.run("dotsync commit all -m 'add bashrc' -- .bashrc");
    let commit_stderr = String::from_utf8_lossy(&commit.stderr).into_owned();

    for stderr in [&sync_stderr, &commit_stderr] {
        let advice = stderr
            .split_once("Correct flow:")
            .expect("the stop teaches a correct flow")
            .1;
        let invocations = quoted_dotsync_invocations(advice);
        assert!(
            !invocations.is_empty(),
            "the advice has to name something to run\n{stderr}"
        );
        for invocation in invocations {
            let attempt = machine.run(&invocation);
            let attempt_stderr = String::from_utf8_lossy(&attempt.stderr).into_owned();
            assert!(
                !attempt_stderr.contains("unknown command"),
                "the advice named `{invocation}`, which dotsync does not recognise\n{attempt_stderr}"
            );
        }
    }
}
