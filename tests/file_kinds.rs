// Paths that are not ordinary regular files: symlinks on a scope, in home and
// in a selection, and the kinds nothing may read through.

use std::fs;
use std::path::{Path, PathBuf};

mod harness;
use harness::*;

/// A named path that is not a regular file used to hang the process: `fs::read`
/// on a FIFO blocks until somebody writes to the other end, and nothing ever
/// does. A directory selection already steps around one and says so; naming it
/// exactly has to be refused, and every other read of home has to refuse it
/// too, because a tracked path can become one at any time.
#[test]
fn a_named_path_that_is_not_a_regular_file_is_refused_rather_than_read() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    make_fifo(&machine.home_dir.join(".pipe"));

    let named = machine.run_within(
        "dotsync commit all -m 'pipe' -- .pipe",
        std::time::Duration::from_secs(20),
    );
    assert_eq!(
        named.status.code(),
        Some(1),
        "naming a fifo must be refused, not read\n{}",
        render_output(&named)
    );
    let stderr = String::from_utf8_lossy(&named.stderr).into_owned();
    assert!(
        stderr.contains("not a regular file"),
        "and the refusal has to say why\n{stderr}"
    );

    // The same file where dotsync reads home without being asked: a path the
    // scope already tracks, replaced in home by a fifo.
    machine.write_file(".bashrc", "export DOTSYNC=1\n");
    machine.run_ok("dotsync commit all -m 'bashrc' -- .bashrc");
    machine.delete_file(".bashrc");
    make_fifo(&machine.home_dir.join(".bashrc"));

    let status = machine.run_within("dotsync status", std::time::Duration::from_secs(20));
    assert_eq!(
        status.status.code(),
        Some(1),
        "reading home must stop rather than block forever\n{}",
        render_output(&status)
    );
    assert!(
        String::from_utf8_lossy(&status.stderr).contains("not a regular file"),
        "{}",
        render_output(&status)
    );
}

/// Dotsync records what it finds at the path you name, so a link is an entry
/// whose content is its target — but a path that reaches its file *through* a
/// link is a different claim: what dotsync would read is not what was named,
/// and every machine on the scope would write those bytes at a path where it
/// has no such link. Refused in one place, `working_copy::home_disk_path`, so
/// no route into home can forget it.
#[test]
fn a_path_that_reaches_its_file_through_a_symlink_is_refused() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    // Config kept outside home and linked into place, which is a real pattern.
    let outside = machine
        .home_dir
        .parent()
        .expect("home has a parent")
        .join("elsewhere");
    write_file_at(&outside.join("nvim/init.lua"), "vim.opt.number = true\n");
    symlink_at(
        &outside.join("nvim"),
        &machine.home_dir.join(".config/nvim"),
    );

    let through = machine.run("dotsync commit all -m 'link' -- .config/nvim/init.lua");
    assert_eq!(
        through.status.code(),
        Some(1),
        "a path that resolves through a symlink must be refused\n{}",
        render_output(&through)
    );
    assert!(
        String::from_utf8_lossy(&through.stderr).contains("symlink"),
        "and the refusal has to say what it found\n{}",
        render_output(&through)
    );
    assert!(
        !bookmark_has_file(&machine, "all", ".config/nvim/init.lua"),
        "so nothing reached the scope through the link"
    );

    // The link itself is a file dotsync can record, and so is a real file
    // beside it.
    machine.run_ok("dotsync commit all -m 'the link itself' -- .config/nvim");
    assert_eq!(
        remote_branch_entry_mode(&machine, "all", ".config/nvim").as_deref(),
        Some("120000"),
        "naming the link records the link"
    );
    machine.write_file(".bashrc", "export DOTSYNC=1\n");
    machine.run_ok("dotsync commit all -m 'real file' -- .bashrc");
}

/// Naming a symlink records it as a symlink, storing the target. Today it is
/// refused outright — the conservative placeholder that stood in for this
/// decision.
#[test]
fn commit_records_a_symlink_as_a_symlink() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".config/app/real.conf", "theme = dark\n");
    symlink_at(
        Path::new("real.conf"),
        &machine.home_dir.join(".config/app/current.conf"),
    );

    machine.run_ok("dotsync commit all -m 'point current at real' -- .config/app/current.conf");

    assert_eq!(
        remote_branch_entry_mode(&machine, "all", ".config/app/current.conf").as_deref(),
        Some("120000"),
        "the scope has to record it as a link, not as a file"
    );
    assert_eq!(
        remote_branch_file_contents(&machine, "all", ".config/app/current.conf"),
        "real.conf",
        "and what it stores is the target"
    );
    assert!(
        machine.is_symlink(".config/app/current.conf"),
        "committing a link must leave home holding the same link"
    );
}

/// The leak this whole area was found through, and the reason the blanket
/// refusal existed: `selflink -> $HOME` used to walk all of home under an
/// aliased prefix and publish 72 files, including `.ssh/id_ed25519`, `.netrc`
/// and the whole hidden `.jj` store, exit 0.
///
/// Under "treat links as files and do not follow them" the sweep stays closed
/// for a structural reason rather than a guard: `selflink` is one symlink
/// entry, so there is no directory to walk. That is the claim here — recording
/// the link is allowed, and home's contents still do not reach the remote.
#[test]
fn a_symlink_to_home_records_one_entry_rather_than_sweeping_home() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    machine.write_file(".ssh/id_ed25519", "PRIVATE KEY\n");
    machine.write_file(".netrc", "machine example.com login me password hunter2\n");
    symlink_at(&machine.home_dir, &machine.home_dir.join("selflink"));

    machine.run_ok("dotsync commit all -m 'link to home' -- selflink/");

    assert_eq!(
        remote_branch_entry_mode(&machine, "all", "selflink").as_deref(),
        Some("120000"),
        "a link to home is one link entry"
    );

    let published = remote_branch_entries(&machine, "all")
        .into_iter()
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    for forbidden in [".ssh/id_ed25519", ".netrc", ".jj", "sync-state"] {
        assert!(
            !published.iter().any(|path| path.contains(forbidden)),
            "`{forbidden}` must never reach the remote\npublished: {published:?}"
        );
    }
}

/// A symlink that is already on a scope has to reach home as a symlink. Today
/// it does not: the repo read turns `TreeValue::Symlink` into the target
/// string as bytes, and the home write puts those bytes in a regular file, so
/// `~/.config/app/current.conf` ends up being a file whose entire contents are
/// the nine characters `real.conf`.
///
/// Nothing here needs dotsync to be able to *commit* a link — a plain git
/// client puts it on `all` and an ordinary cascade carries it down — which is
/// why this is reachable today and is the sharpest statement of the decision.
#[test]
fn a_symlink_on_a_scope_materialises_in_home_as_a_symlink() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "all", ".config/app/real.conf", "theme = dark\n");
    seed_remote_scope_symlink(&machine, "all", ".config/app/current.conf", "real.conf");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");

    machine.run_ok("dotsync");

    assert!(
        machine.is_symlink(".config/app/current.conf"),
        "a symlink on the scope must arrive in home as a symlink, not as a regular file holding the target text (home holds: {:?})",
        fs::read_to_string(machine.home_dir.join(".config/app/current.conf")).ok()
    );
    assert_eq!(
        machine.read_link(".config/app/current.conf"),
        PathBuf::from("real.conf"),
        "and it must point where the scope says it points"
    );
}

/// Home holds a link where the scope holds a regular file. Sync must replace
/// the link, not write through it — writing through it edits a file dotsync
/// was never asked to manage, under a name it does not know.
///
/// Reproduced with two managed files: `toolA` recorded as a regular file, then
/// replaced in home by a link to the managed `toolB`, and `dotsync --force`
/// wrote toolA's recorded content into toolB. The link target here is outside
/// home so the loss is unambiguous, and its content is asserted explicitly:
/// that byte comparison is the data loss.
#[test]
fn a_sync_replaces_a_home_symlink_instead_of_writing_through_it() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".apprc", "ui = dark\n");
    machine.run_ok("dotsync");
    assert_eq!(machine.read_file(".apprc"), "ui = dark\n");

    // A file dotsync does not manage, at a path dotsync has never heard of.
    let outside = harness.root_dir.join("outside/notes.txt");
    write_file_at(&outside, "notes nobody asked dotsync to touch\n");
    machine.replace_with_symlink(".apprc", &outside);

    seed_remote_scope_file(&machine, "mx-xps-cy", ".apprc", "ui = light\n");

    // A plain sync stops: home holds something that is not what was synced.
    machine.run_expecting("dotsync", 1);
    assert_eq!(
        fs::read_to_string(&outside).expect("read the link target"),
        "notes nobody asked dotsync to touch\n",
        "a run that stopped must not have written anything"
    );

    machine.run_ok("dotsync --force");
    assert_eq!(
        fs::read_to_string(&outside).expect("read the link target"),
        "notes nobody asked dotsync to touch\n",
        "the link's target is not a managed file and must survive byte for byte"
    );
    assert!(
        !machine.is_symlink(".apprc"),
        "the link itself is what the scope disagrees with, so the link is what gets replaced"
    );
    assert_eq!(
        machine.read_file(".apprc"),
        "ui = light\n",
        "and home ends up holding the scope's regular file"
    );
}

/// The whole story, in the shape a person meets it: a versioned script and a
/// `tool -> tool-1.2.0` link beside it. The link has to still be a link on the
/// second machine, and must not have become a second copy of the script.
///
/// Named as a directory selection rather than naming the link exactly, so this
/// fails on the round trip rather than on the refusal that
/// `commit_records_a_symlink_as_a_symlink` covers: today the walk silently
/// skips the link, machine A publishes only the script, and machine B has no
/// link at all.
#[test]
fn a_symlink_to_a_sibling_script_survives_the_round_trip_to_another_machine() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "box1");
    let machine_b = harness.machine("machine-b", "linux", "box2");

    machine_a.init_ok();

    machine_a.write_file(".local/bin/tool-1.2.0", "#!/bin/sh\necho tool\n");
    symlink_at(
        Path::new("tool-1.2.0"),
        &machine_a.home_dir.join(".local/bin/tool"),
    );

    machine_a.run_ok("dotsync commit all -m 'ship tool and the current link' -- .local/bin/");

    machine_b.init_ok();

    assert_eq!(
        machine_b.read_file(".local/bin/tool-1.2.0"),
        "#!/bin/sh\necho tool\n"
    );
    assert!(
        machine_b.is_symlink(".local/bin/tool"),
        "the link has to arrive as a link (machine b holds: {:?})",
        fs::read_to_string(machine_b.home_dir.join(".local/bin/tool")).ok()
    );
    assert_eq!(
        machine_b.read_link(".local/bin/tool"),
        PathBuf::from("tool-1.2.0"),
        "pointing at its sibling, not carrying a second copy of it"
    );
}

/// A difference in kind is a difference. Both fixtures below hold exactly the
/// bytes the scope holds — the link's target text is the file's content, and
/// vice versa — so the only thing that differs is whether the path is a link.
/// Today both read as clean, because the drift read follows the link and then
/// compares content.
///
/// Exit codes are the 2026-08-13 decision: `status` is concise and always
/// exits 0, `diff` exits 1 when it finds anything.
#[test]
fn status_and_diff_report_a_kind_difference_between_a_link_and_a_file() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    machine.init_ok();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".apprc", "ui = dark\n");
    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".config/app/real.conf",
        "theme = dark\n",
    );
    seed_remote_scope_symlink(
        &machine,
        "mx-xps-cy",
        ".config/app/current.conf",
        "real.conf",
    );
    machine.run_ok("dotsync");

    // A link where the scope holds a file, over content identical to the
    // scope's.
    let outside = harness.root_dir.join("outside/apprc.txt");
    write_file_at(&outside, "ui = dark\n");
    machine.replace_with_symlink(".apprc", &outside);

    // A file where the scope holds a link, holding exactly the target text.
    machine.replace_with_regular_file(".config/app/current.conf", "real.conf");

    let status = machine.run_expecting("dotsync --output json status", 0);
    let changed = parse_stdout_json(&status);
    let changed = changed["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("changes should be an array\n{}", render_output(&status)));
    for path in [".apprc", ".config/app/current.conf"] {
        let entry = changed
            .iter()
            .find(|entry| entry["path"] == path)
            .unwrap_or_else(|| {
                panic!(
                    "`{path}` differs from the scope in kind and must be reported\n{}",
                    render_output(&status)
                )
            });
        assert!(
            entry["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("symlink")),
            "the reason must name the kind difference rather than describing content\n{}",
            render_output(&status)
        );
    }

    let diff = machine.run("dotsync --output json diff");
    assert_eq!(
        diff.status.code(),
        Some(1),
        "diff exits 1 when it finds changes\n{}",
        render_output(&diff)
    );
    let diffed = parse_stdout_json(&diff);
    let diffed = diffed["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("changes should be an array\n{}", render_output(&diff)))
        .iter()
        .map(|entry| entry["path"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    for path in [".apprc", ".config/app/current.conf"] {
        assert!(
            diffed.iter().any(|reported| reported == path),
            "`{path}` must be in diff's population too\n{}",
            render_output(&diff)
        );
    }
}
