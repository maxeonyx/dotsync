// What a `dotsync commit` selects and what it refuses: named paths, named
// directories, paths that are not the caller's to record, and what `--force`
// changes about those answers.

use std::fs;

mod harness;
use harness::*;

#[test]
fn commit_with_no_paths_ignores_unmanaged_home_files() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // An unmanaged file in home. `dotsync commit <scope> -m ...` with no paths
    // means "every managed file that changed", and nothing managed changed, so
    // this is an ordinary no-op commit — not a reason to refuse the command.
    machine.write_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let revision_before = bookmark_revision(&machine, "all");

    let commit_output = machine.run("dotsync commit all -m 'nothing changed'");
    assert_eq!(
        commit_output.status.code(),
        Some(0),
        "a no-paths commit with nothing to commit should succeed\n{}",
        render_output(&commit_output)
    );

    assert_eq!(bookmark_revision(&machine, "all"), revision_before);
    assert!(
        !bookmark_has_file(&machine, "all", ".gitconfig"),
        "an unmanaged home file must not be swept into the scope"
    );
    assert_eq!(machine.read_file(".gitconfig"), "[user]\nname = \"Max\"\n");
}

#[test]
fn commit_without_paths_imports_all_diffs() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".config/app.conf",
        "setting = \"original\"\n",
    );
    machine.run_ok("dotsync");

    machine.write_file(".config/app.conf", "setting = \"updated\"\n");

    machine.run_ok("dotsync commit mx-xps-cy -m update");

    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".config/app.conf"),
        "setting = \"updated\"\n"
    );
}

#[test]
fn commit_path_that_matches_nothing_is_an_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".apprc", "ui_theme = dark\n");
    fs::create_dir_all(machine.home_dir.join("empty-dir")).expect("create empty dir");
    let revision_before = bookmark_revision(&machine, "all");

    // A typo, a `~/`-prefixed path, an absolute path, and a directory holding
    // nothing at all. Each of these used to commit nothing and report success,
    // which tells an agent its config was saved when it was not.
    let absolute = machine.home_dir.join(".apprc");
    let absolute = absolute.to_str().expect("home path should be UTF-8");
    for command in [
        "dotsync commit all -m typo -- nonexistent-file".to_string(),
        "dotsync commit all -m tilde -- '~/.apprc'".to_string(),
        format!("dotsync commit all -m absolute -- {absolute}"),
        "dotsync commit all -m empty-dir -- empty-dir".to_string(),
    ] {
        let output = machine.run(&command);
        assert_eq!(
            output.status.code(),
            Some(1),
            "`{command}` should fail rather than report a successful empty commit\n{}",
            render_output(&output)
        );
        assert_eq!(
            bookmark_revision(&machine, "all"),
            revision_before,
            "`{command}` must not move the scope"
        );
    }

    let typo_output = machine.run("dotsync commit all -m typo -- nonexistent-file");
    assert_stderr_snapshot(
        &typo_output,
        &format!(
            "\
dotsync: cannot commit that path

What dotsync does:
Dotsync records the home files you name onto a scope branch, then cascades that scope so every machine sharing it receives the change. Every file on a scope is written back into home on each of those machines.

This flow:
This commit flow resolves each path you name against your home directory, checks that it is a config file dotsync may record, and commits the ones that changed.

Expected:
It expects every path you name to be a config file inside your home directory, named relative to it, and to exist either in home or on the target scope already.

Current state found:
`nonexistent-file` matched nothing: no file exists at or under {}/nonexistent-file, and scope `all` tracks no file at or under `nonexistent-file`.

Why dotsync stopped:
Dotsync stopped before recording anything. A commit records every path you named or none of them, so fixing the paths above and rerunning the same command is safe.

Correct flow:
- name paths relative to your home directory: `dotsync commit all -m \"message\" -- .config/fish/config.fish`.
- do not use `~/`, absolute paths, or `..`; dotsync resolves every path against your home directory already, and records it verbatim.
- run `dotsync status` to see which managed files changed.
",
            machine.home_dir.display()
        ),
    );

    let absolute_output = machine.run(&format!("dotsync commit all -m absolute -- {absolute}"));
    let absolute_stderr = String::from_utf8_lossy(&absolute_output.stderr);
    assert!(
        absolute_stderr.contains(&format!(
            "`{absolute}` is an absolute path, and dotsync resolves every commit path against your home directory."
        )),
        "{}",
        render_output(&absolute_output)
    );
}

#[test]
fn commit_path_that_escapes_home_is_an_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // Dotsync records the path you name verbatim as a repo path, and every
    // machine on that scope writes it back out under its own home. A path that
    // climbs out of home therefore writes outside home everywhere.
    let outside = machine.home_dir.parent().expect("home has a parent");
    write_file_at(&outside.join("outside.conf"), "PWNED=1\n");
    write_file_at(&outside.join("deeper.conf"), "PWNED=2\n");
    let revision_before = bookmark_revision(&machine, "all");

    for command in [
        "dotsync commit all -m escape -- ../outside.conf",
        "dotsync commit all -m escape -- ../../machine-a/home/../deeper.conf",
    ] {
        let output = machine.run(command);
        assert_eq!(
            output.status.code(),
            Some(1),
            "`{command}` should be refused\n{}",
            render_output(&output)
        );
        assert_eq!(
            bookmark_revision(&machine, "all"),
            revision_before,
            "`{command}` must not move the scope"
        );
    }

    assert!(
        !bookmark_has_file(&machine, "all", "../outside.conf"),
        "a path that climbs out of home must never become a repo entry"
    );
}

#[test]
fn committing_the_scope_graph_outside_all_is_an_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let config_path = ".config/dotsync/config.toml";
    let original = machine.read_file(config_path);
    machine.write_file(
        config_path,
        &format!("{original}\n# hyprland: wayland compositor config\n"),
    );
    let linux_before = bookmark_revision(&machine, "linux");

    // Dotsync only ever reads the scope graph from `all`. A copy recorded on
    // another scope configures nothing, but it still syncs into home on that
    // scope's machines, where it overwrites the real one.
    let wrong_scope = machine.run(&format!(
        "dotsync commit linux -m 'describe hyprland' -- {config_path}"
    ));
    assert_eq!(
        wrong_scope.status.code(),
        Some(1),
        "committing the scope graph to a non-all scope should be refused\n{}",
        render_output(&wrong_scope)
    );
    assert_eq!(
        bookmark_revision(&machine, "linux"),
        linux_before,
        "the refused commit must not move the scope"
    );

    let stderr = String::from_utf8_lossy(&wrong_scope.stderr).into_owned();
    assert!(
        stderr.contains("dotsync only reads it from `all`"),
        "the refusal must teach where the scope graph lives\n{}",
        render_output(&wrong_scope)
    );

    // The same change is fine on `all`, which is the only place it is read.
    machine.run_ok(&format!(
        "dotsync commit all -m 'describe hyprland' -- {config_path}"
    ));
    assert!(
        read_bookmark_file_contents(&machine, "all", config_path)
            .contains("# hyprland: wayland compositor config"),
        "the scope graph change should land on all"
    );
}

#[test]
fn commit_path_inside_dotsyncs_own_state_is_an_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let revision_before = bookmark_revision(&machine, "all");
    let repo_relative = machine
        .repo_dir
        .strip_prefix(&machine.home_dir)
        .expect("the hidden repo lives in home")
        .to_str()
        .expect("the repo path is UTF-8")
        .to_string();

    // The hidden repo is dotsync's own bookkeeping sitting in home. Naming it,
    // or anything under it, used to be filtered out of the selection without a
    // word, so the commit reported success having recorded nothing.
    for command in [
        format!("dotsync commit all -m repo -- {repo_relative}"),
        format!("dotsync commit all -m repo-file -- {repo_relative}/.jj"),
    ] {
        let output = machine.run(&command);
        assert_eq!(
            output.status.code(),
            Some(1),
            "`{command}` should fail rather than report a successful empty commit\n{}",
            render_output(&output)
        );
        assert_eq!(
            bookmark_revision(&machine, "all"),
            revision_before,
            "`{command}` must not move the scope"
        );
    }

    let repo_output = machine.run(&format!("dotsync commit all -m repo -- {repo_relative}"));
    assert_stderr_snapshot(
        &repo_output,
        &format!(
            "\
dotsync: cannot commit that path

What dotsync does:
Dotsync records the home files you name onto a scope branch, then cascades that scope so every machine sharing it receives the change. Every file on a scope is written back into home on each of those machines.

This flow:
This commit flow resolves each path you name against your home directory, checks that it is a config file dotsync may record, and commits the ones that changed.

Expected:
It expects every path you name to be a config file inside your home directory, named relative to it, and to exist either in home or on the target scope already.

Current state found:
`{repo_relative}` is dotsync's hidden repo itself, at {}, which is where dotsync stores every scope.

Why dotsync stopped:
Dotsync stopped before recording anything. A commit records every path you named or none of them, so fixing the paths above and rerunning the same command is safe.

Correct flow:
- name paths relative to your home directory: `dotsync commit all -m \"message\" -- .config/fish/config.fish`.
- do not use `~/`, absolute paths, or `..`; dotsync resolves every path against your home directory already, and records it verbatim.
- commit the config files you edited instead; dotsync's hidden repo is not config and cannot travel on a scope.
- to change which scopes exist, edit `.config/dotsync/config.toml` in home and commit that path to `all`.
- run `dotsync status` to see which managed files changed.
",
            machine.repo_dir.display()
        ),
    );
}

#[test]
fn commit_reports_every_unusable_path_at_once() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".apprc", "ui_theme = dark\n");

    // Reporting only the first bad path costs the agent one round trip per
    // mistake, and each round trip is a full fetch-and-commit attempt.
    let output = machine.run_expecting(
        "dotsync commit all -m mixed -- nonexistent-file '~/.apprc' .local/share/dotsync/repo",
        1,
    );

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    for expected in [
        "`nonexistent-file` matched nothing",
        "`~/.apprc` matched nothing",
        "`.local/share/dotsync/repo` is dotsync's hidden repo itself",
    ] {
        assert!(
            stderr.contains(expected),
            "one run should report every unusable path; missing {expected:?}\n{}",
            render_output(&output)
        );
    }
    assert!(
        !stderr.contains("is inside dotsync's hidden repo at"),
        "the repo root is the repo, not something inside it\n{}",
        render_output(&output)
    );
}

/// One path is one path. The JSON message counted with the plural wording
/// whatever the count was, so an agent read "1 of the paths you named" for a
/// command that named exactly one.
#[test]
fn naming_one_unusable_path_reads_as_one_path() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    let unusable = machine.run_expecting("dotsync --output json commit all -m x -- typo.conf", 1);
    assert_eq!(
        parse_stdout_json(&unusable)["message"],
        "cannot commit the path you named",
        "{}",
        render_output(&unusable)
    );

    let two = machine.run_expecting(
        "dotsync --output json commit all -m x -- typo.conf other-typo.conf",
        1,
    );
    assert_eq!(
        parse_stdout_json(&two)["message"],
        "cannot commit 2 of the paths you named",
        "{}",
        render_output(&two)
    );
}

/// Shells complete directories with a trailing separator, so agents and people
/// both type them. `.config/fish/` already worked; `.bashrc/` matched the
/// tracked file by path components and then failed inside the commit, leaking
/// jj's own vocabulary at a point where nothing is left to teach.
#[test]
fn a_trailing_separator_on_a_named_file_is_just_a_trailing_separator() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".bashrc", "export DOTSYNC=one\n");
    machine.run_ok("dotsync commit all -m 'add bashrc' -- .bashrc");

    machine.write_file(".bashrc", "export DOTSYNC=two\n");
    let with_slash = machine.run("dotsync commit all -m 'edit bashrc' -- .bashrc/");
    assert!(
        with_slash.status.success(),
        "a trailing separator must not stop the commit\n{}",
        render_output(&with_slash)
    );
    assert!(
        !String::from_utf8_lossy(&with_slash.stderr).contains("jj"),
        "and must never leak jj's vocabulary\n{}",
        render_output(&with_slash)
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".bashrc"),
        "export DOTSYNC=two\n",
        "the file it named is the file it should record"
    );

    // `./` in front says nothing either.
    machine.write_file(".bashrc", "export DOTSYNC=three\n");
    machine.run_ok("dotsync commit all -m 'edit again' -- ./.bashrc");
    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".bashrc"),
        "export DOTSYNC=three\n"
    );
}

/// `dotsync commit all -m msg -- .` reads like "commit everything", and what
/// it actually does is walk the whole home directory and publish it: ssh keys,
/// `.netrc`, browser profiles, anything. Nothing about the run says so, and
/// once it is on the remote it is on every machine that shares the scope.
#[test]
fn a_selection_that_names_the_whole_home_directory_is_refused() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".ssh/id_ed25519", "PRIVATE KEY\n");
    machine.write_file(".netrc", "machine example.com login me password hunter2\n");
    machine.write_file(".bashrc", "export DOTSYNC=1\n");

    for selection in [".", "./"] {
        let output = machine.run(&format!(
            "dotsync commit all -m 'everything' -- {selection}"
        ));
        assert_eq!(
            output.status.code(),
            Some(1),
            "`{selection}` names the whole home directory and must be refused\n{}",
            render_output(&output)
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("home directory"),
            "the refusal must say what was named\n{stderr}"
        );
    }

    let absolute = machine.run(&format!(
        "dotsync commit all -m 'everything' -- {}",
        machine.home_dir.display()
    ));
    assert_eq!(
        absolute.status.code(),
        Some(1),
        "naming home by its absolute path must be refused too\n{}",
        render_output(&absolute)
    );

    assert!(
        !bookmark_has_file(&machine, "all", ".ssh/id_ed25519"),
        "no refused sweep may have recorded a private key"
    );
    assert!(!bookmark_has_file(&machine, "all", ".netrc"));

    // Naming a real directory still works, and still only reaches under it.
    machine.write_file(".config/app/settings.toml", "theme = \"dark\"\n");
    machine.run_ok("dotsync commit all -m 'app settings' -- .config/app/");
    assert!(bookmark_has_file(
        &machine,
        "all",
        ".config/app/settings.toml"
    ));
    assert!(!bookmark_has_file(&machine, "all", ".netrc"));
}

/// Naming a directory says "commit what changed under here", which is what a
/// bare `dotsync commit <scope>` says about the whole machine — so it filters
/// like one. Naming a path exactly is a claim about that path, and a claim is
/// what deserves an argument.
#[test]
fn a_directory_selection_records_what_this_machine_changed_and_says_what_it_skipped() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".config/fish/config.fish", "set -g theme dark\n");
    machine_a.write_file(".config/fish/aliases.fish", "alias ll 'ls -l'\n");
    machine_a.run_ok("dotsync commit all -m 'seed fish config' -- .config/fish/");
    machine_b.run_ok("dotsync");

    // B publishes a change to one file under that directory. A has not synced
    // it, and has an edit of its own to a different file under there, plus a
    // brand new file it wants to add.
    machine_b.write_file(".config/fish/aliases.fish", "alias ll 'ls -lah'\n");
    machine_b.run_ok("dotsync commit all -m 'better ll' -- .config/fish/aliases.fish");

    machine_a.write_file(".config/fish/config.fish", "set -g theme light\n");
    machine_a.write_file(
        ".config/fish/functions.fish",
        "function gs; git status; end\n",
    );

    // Named exactly, B's file is still refused: that is a claim that home's
    // copy should win, and it would revert what B published.
    let named_exactly =
        machine_a.run("dotsync commit all -m 'take mine' -- .config/fish/aliases.fish");
    assert_eq!(
        named_exactly.status.code(),
        Some(1),
        "naming a path another machine changed must still be refused\n{}",
        render_output(&named_exactly)
    );

    let directory_commit =
        machine_a.run("dotsync commit all -m 'light theme and functions' -- .config/fish/");
    assert_eq!(
        directory_commit.status.code(),
        Some(0),
        "a directory selection must commit what changed under it rather than refuse\n{}",
        render_output(&directory_commit)
    );
    let stderr = String::from_utf8_lossy(&directory_commit.stderr).into_owned();
    assert!(
        stderr.contains(".config/fish/aliases.fish"),
        "the run must say which file under the directory it left alone\n{stderr}"
    );

    assert_eq!(
        remote_branch_file_contents(&machine_a, "all", ".config/fish/config.fish"),
        "set -g theme light\n",
        "the edit this machine made must be recorded"
    );
    assert_eq!(
        remote_branch_file_contents(&machine_a, "all", ".config/fish/functions.fish"),
        "function gs; git status; end\n",
        "a new file under a named directory must still be added"
    );
    assert_eq!(
        remote_branch_file_contents(&machine_a, "all", ".config/fish/aliases.fish"),
        "alias ll 'ls -lah'\n",
        "the other machine's published change must survive the directory commit"
    );
    assert_eq!(
        machine_a.read_file(".config/fish/aliases.fish"),
        "alias ll 'ls -lah'\n",
        "and the sync that follows must bring this machine up to it"
    );
}

/// Naming a directory is how new files get onto a scope in bulk, and adding a
/// file to a shared scope is the one thing in a commit that every other
/// machine will then have written into its home. A run that does it silently
/// reads exactly like a run that changed one line.
#[test]
fn a_commit_says_which_files_it_started_tracking() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".config/fish/config.fish", "set -g fish_greeting off\n");
    machine.write_file(".config/fish/aliases.fish", "alias ll 'ls -l'\n");
    let commit_output = machine.run_expecting(
        "dotsync --output json commit all -m 'add fish config' -- .config/fish/",
        0,
    );

    let newly_tracked = parse_stdout_json(&commit_output)["newly_tracked"]
        .as_array()
        .expect("newly_tracked should be an array")
        .iter()
        .filter_map(|path| path.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    assert_eq!(
        newly_tracked,
        vec![
            ".config/fish/aliases.fish".to_string(),
            ".config/fish/config.fish".to_string()
        ],
        "a commit must report the files it put on the scope for the first time\n{}",
        render_output(&commit_output)
    );
    let stderr = String::from_utf8_lossy(&commit_output.stderr).into_owned();
    assert!(
        stderr.contains(".config/fish/aliases.fish"),
        "and say so in words too\n{stderr}"
    );

    // Editing a file that is already on the scope is not starting to track it.
    machine.write_file(".config/fish/config.fish", "set -g fish_greeting on\n");
    let edit_output = machine.run_expecting(
        "dotsync --output json commit all -m 'flip greeting' -- .config/fish/",
        0,
    );
    assert_eq!(
        parse_stdout_json(&edit_output)["newly_tracked"]
            .as_array()
            .expect("newly_tracked should be an array")
            .len(),
        0,
        "an edit to a tracked file is not a new file\n{}",
        render_output(&edit_output)
    );
}

#[test]
fn selected_add_modify_and_delete_are_applied_without_touching_unselected_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(
        &machine,
        "all",
        ".config/fish/config.fish",
        "set -g fish_greeting on\n",
    );
    seed_remote_scope_file(&machine, "all", ".config/fish/removed.fish", "remove me\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    machine.run_ok("dotsync");

    machine.write_file(".config/fish/config.fish", "set -g fish_greeting off\n");
    machine.write_file(".config/fish/completions/git.fish", "complete -c git\n");
    machine.delete_file(".config/fish/removed.fish");

    machine.run_ok("dotsync commit all -m 'update fish dir' -- .config/fish/");

    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".config/fish/config.fish"),
        "set -g fish_greeting off\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".config/fish/completions/git.fish"),
        "complete -c git\n"
    );
    assert!(!bookmark_has_file(
        &machine,
        "all",
        ".config/fish/removed.fish"
    ));
    assert_eq!(
        machine.read_file(".config/fish/config.fish"),
        "set -g fish_greeting off\n"
    );
    assert_eq!(
        machine.read_file(".config/fish/completions/git.fish"),
        "complete -c git\n"
    );
    assert!(!machine.file_exists(".config/fish/removed.fish"));
}

#[test]
fn a_stale_home_file_cannot_be_committed_over_another_machines_change() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    // B adds a line and publishes it. A has done nothing since the seed: its
    // home `.apprc` is not edited, it is simply behind.
    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\nsize = 14\n");
    machine_b.run_ok("dotsync commit all -m 'add size' -- .apprc");
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".apprc"),
        "ui_theme = dark\nfont = mono\nsize = 14\n"
    );

    // The taught workflow starts with `status`, and every dotsync command
    // fetches on entry. Whatever `status` reports, the commit that follows must
    // not re-record A's older content on top of B's published change.
    machine_a.run_ok("dotsync status");

    let commit_a = machine_a.run("dotsync commit all -m 'commit what status showed' -- .apprc");
    assert_eq!(
        commit_a.status.code(),
        Some(1),
        "committing a file this machine has not edited must be refused\n{}",
        render_output(&commit_a)
    );
    let stderr = String::from_utf8_lossy(&commit_a.stderr).into_owned();
    assert!(
        stderr.contains("has not been edited here"),
        "the refusal must say the file was not edited here\n{stderr}"
    );
    assert!(
        stderr.contains("run `dotsync` to bring this machine up to date"),
        "the refusal must point at plain `dotsync`\n{stderr}"
    );

    assert_eq!(
        remote_branch_file_contents(&machine_a, "all", ".apprc"),
        "ui_theme = dark\nfont = mono\nsize = 14\n",
        "the refused commit must leave the other machine's published change alone"
    );

    // The taught recovery works: sync, then edit, then commit.
    machine_a.run_ok("dotsync");
    assert_eq!(
        machine_a.read_file(".apprc"),
        "ui_theme = dark\nfont = mono\nsize = 14\n"
    );
}

#[test]
fn committing_a_path_another_machine_deleted_is_refused() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    // B removes the file and publishes the removal. A has not synced since.
    machine_b.delete_file(".apprc");
    machine_b.run_ok("dotsync commit all -m 'drop apprc' -- .apprc");

    machine_a.run_ok("dotsync status");

    let commit_a = machine_a.run("dotsync commit all -m 'keep apprc' -- .apprc");
    assert_eq!(
        commit_a.status.code(),
        Some(1),
        "naming a file another machine deleted must not quietly record nothing\n{}",
        render_output(&commit_a)
    );
    let stderr = String::from_utf8_lossy(&commit_a.stderr).into_owned();
    assert!(
        stderr.contains("deleted on another machine"),
        "the refusal must say what happened to the file\n{stderr}"
    );
    assert!(
        stderr.contains("run `dotsync` to bring this machine up to date"),
        "the refusal must point at plain `dotsync`\n{stderr}"
    );

    // Applying the deletion is one way out.
    machine_a.run_ok("dotsync");
    assert!(!machine_a.file_exists(".apprc"));

    // Putting it back on purpose is the other, and it says so in the JSON.
    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\n");
    machine_b.run_ok("dotsync --output json commit all -m 'put it back' -- .apprc");
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".apprc"),
        "ui_theme = dark\nfont = mono\n"
    );
}

#[test]
fn commit_force_applies_to_the_named_paths_and_not_to_unrelated_drift() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".gitconfig", "[user]\nname = Repo\n");
    seed_remote_scope_file(&machine, "mx-xps-cy", ".config/app.conf", "setting = one\n");
    machine.run_ok("dotsync");

    machine.write_file(".gitconfig", "[user]\nname = Drifted\n");
    machine.write_file(".config/app.conf", "setting = two\n");

    let commit_output =
        machine.run("dotsync commit mx-xps-cy -m 'update app' --force -- .config/app.conf");
    assert_eq!(
        commit_output.status.code(),
        Some(0),
        "a change the commit did not name is an input to its home sync rather than a wall in front of it\n{}",
        render_output(&commit_output)
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".config/app.conf"),
        "setting = two\n",
        "the named change is still recorded"
    );

    // The unnamed change went neither way: not reverted, and not recorded on
    // the authority of a `--force` that named something else.
    assert_eq!(
        machine.read_file(".gitconfig"),
        "[user]\nname = Drifted\n",
        "`--force` on a commit must not revert a file the commit never named"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".gitconfig"),
        "[user]\nname = Repo\n",
        "and must not record it either"
    );
    let status = machine.run_ok("dotsync status --output json");
    let payload = parse_stdout_json(&status);
    let changed: Vec<&str> = payload["changes"]
        .as_array()
        .expect("status answers with a changes array")
        .iter()
        .filter_map(|change| change["path"].as_str())
        .collect();
    assert_eq!(
        changed,
        [".gitconfig"],
        "so it is still this machine's to decide about, and still reported as such\n{}",
        render_output(&status)
    );
}

#[test]
fn forcing_a_stale_commit_records_what_it_overwrote() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\nsize = 14\n");
    machine_b.run_ok("dotsync commit all -m 'add size' -- .apprc");

    machine_a.run_ok("dotsync status");

    let commit_a = machine_a
        .run_ok("dotsync --output json commit all -m 'revert on purpose' --force -- .apprc");

    let json = parse_stdout_json(&commit_a);
    assert_eq!(
        json["forced_overwrites"]
            .as_array()
            .expect("forced_overwrites should be an array"),
        &vec![serde_json::Value::from(".apprc")],
        "a forced overwrite of an incoming change has to be on the record\n{}",
        render_output(&commit_a)
    );
    assert_eq!(
        remote_branch_file_contents(&machine_a, "all", ".apprc"),
        "ui_theme = dark\nfont = mono\n"
    );
}

#[test]
fn a_successful_forced_commit_says_what_it_overwrote() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\nsize = 14\n");
    machine_b.run_ok("dotsync commit all -m 'add size' -- .apprc");

    machine_a.run_ok("dotsync status");

    // Succeeding is not a reason to stay quiet. A run that reverted another
    // machine's published change has to say so on the way past, exactly as it
    // does when it goes on to fail — the successful one is the commoner case.
    let commit_a = machine_a.run_ok("dotsync commit all -m 'revert on purpose' --force -- .apprc");
    assert_stderr_snapshot(
        &commit_a,
        "\
dotsync: recorded 1 file(s) over an incoming change, because you passed `--force`
- .apprc
dotsync: committed all and synced 2 file(s)
",
    );
}

#[test]
fn a_forced_overwrite_is_reported_even_when_the_run_then_fails() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    machine_a.write_file(".config/other.conf", "other = base\n");
    machine_a.run_ok("dotsync commit all -m 'add other' -- .config/other.conf");
    machine_b.run_ok("dotsync");

    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\nsize = 14\n");
    machine_b.write_file(".config/other.conf", "other = from b\n");
    machine_b.run_ok("dotsync commit all -m 'add size, change other' -- .apprc .config/other.conf");

    // A forces the revert of `.apprc`, and separately holds its own edit to a
    // file B changed differently — so the commit's own home sync meets a
    // conflict it cannot resolve, after the forced history has been written and
    // pushed.
    machine_a.run_ok("dotsync status");
    machine_a.write_file(".config/other.conf", "other = from a\n");

    let commit_a =
        machine_a.run("dotsync --output json commit all -m 'revert apprc' --force -- .apprc");
    assert_eq!(
        commit_a.status.code(),
        Some(1),
        "a home file that conflicts with what the scope holds still stops the home sync\n{}",
        render_output(&commit_a)
    );
    assert_eq!(
        remote_branch_file_contents(&machine_a, "all", ".apprc"),
        "ui_theme = dark\nfont = mono\n",
        "the forced overwrite really did happen before the run stopped"
    );

    let json = parse_stdout_json(&commit_a);
    assert_eq!(
        json["forced_overwrites"]
            .as_array()
            .expect("forced_overwrites should be an array on the error path too"),
        &vec![serde_json::Value::from(".apprc")],
        "a run that overwrote someone else's change must say so whether or not it then finished\n{}",
        render_output(&commit_a)
    );
}

/// A commit that records nothing ran no sync, so it has no sync to report. The
/// empty `machine_scope` it used to print came from a default-constructed sync
/// report, and the headline claimed a commit that never happened.
#[test]
fn a_commit_that_records_nothing_says_so_instead_of_reporting_an_empty_sync() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".apprc", "ui_theme = dark\n");
    let recorded = machine.run_ok("dotsync --output json commit all -m 'add apprc' -- .apprc");
    let json = parse_stdout_json(&recorded);
    assert_eq!(
        json["outcome"],
        "committed",
        "a commit that recorded something must say which of the two things it did\n{}",
        render_output(&recorded)
    );
    assert_eq!(json["machine_scope"], "mx-xps-cy");

    let again = machine.run_expecting(
        "dotsync --output json commit all -m 'add apprc again' -- .apprc",
        0,
    );
    let json = parse_stdout_json(&again);
    assert_eq!(json["status"], "ok");
    assert_eq!(json["command"], "commit");
    assert_eq!(
        json["outcome"],
        "nothing_to_commit",
        "a commit that recorded nothing must be distinguishable from one that did\n{}",
        render_output(&again)
    );
    assert_eq!(json["scope"], "all");
    assert_eq!(
        json["machine_scope"],
        "mx-xps-cy",
        "the machine scope is known whether or not there was anything to commit\n{}",
        render_output(&again)
    );
    assert!(
        json.get("synced_files").is_none(),
        "a commit that never ran a sync must not report an empty one\n{}",
        render_output(&again)
    );

    let human = machine.run("dotsync commit all -m 'add apprc again' -- .apprc");
    assert_stderr_snapshot(
        &human,
        "dotsync: nothing to record on `all`; no commit was made and home was not synced\n",
    );
}
