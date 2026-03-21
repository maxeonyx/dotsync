use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json;
use tempfile::TempDir;

const CONFIG_DIR: &str = ".config/dotsync";

struct TestHarness {
    _tempdir: TempDir,
    root_dir: PathBuf,
    remote_dir: PathBuf,
}

impl TestHarness {
    fn new() -> Self {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let root_dir = tempdir.path().to_path_buf();
        let remote_dir = root_dir.join("remote.git");
        init_bare_remote(&remote_dir);

        Self {
            _tempdir: tempdir,
            root_dir,
            remote_dir,
        }
    }

    fn machine(&self, name: &str, os: &str, hostname: &str) -> MachineEnvironment {
        MachineEnvironment::new(
            self.root_dir.join(name),
            self.remote_dir.clone(),
            os,
            hostname,
        )
    }
}

struct MachineEnvironment {
    home_dir: PathBuf,
    repo_dir: PathBuf,
    remote_dir: PathBuf,
    os: String,
    hostname: String,
}

impl MachineEnvironment {
    fn new(root_dir: PathBuf, remote_dir: PathBuf, os: &str, hostname: &str) -> Self {
        let home_dir = root_dir.join("home");
        let repo_dir = home_dir.join("dotfiles");
        fs::create_dir_all(&home_dir).expect("create home dir");
        Self {
            home_dir,
            repo_dir,
            remote_dir,
            os: os.to_string(),
            hostname: hostname.to_string(),
        }
    }

    fn init(&self) -> Output {
        self.run_dotsync(&[
            "init",
            self.remote_dir
                .to_str()
                .expect("remote path should be valid UTF-8"),
        ])
    }

    fn sync(&self) -> Output {
        self.run_dotsync(&[])
    }

    fn commit(&self, scope: &str, message: &str) -> Output {
        self.run_dotsync(&[scope, "-m", message])
    }

    fn force_sync(&self) -> Output {
        self.run_dotsync(&["--force"])
    }

    fn run_dotsync(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dotsync"));
        command.args(args);
        command.current_dir(&self.home_dir);
        command.env("HOME", &self.home_dir);
        command.env("DOTSYNC_OS", &self.os);
        command.env("DOTSYNC_HOSTNAME", &self.hostname);
        command.output().expect("run dotsync")
    }

    fn run_dotsync_json(&self, args: &[&str]) -> (Output, serde_json::Value) {
        let mut full_args = vec!["--output", "json"];
        full_args.extend_from_slice(args);
        let output = self.run_dotsync(&full_args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let json = serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!(
                "failed to parse JSON stdout: {e}\nstdout: {stdout}\n{}",
                render_output(&output)
            )
        });
        (output, json)
    }

    fn write_repo_file(&self, relative: &str, contents: &str) {
        self.write_file(self.repo_dir.join(relative), contents);
    }

    fn write_home_file(&self, relative: &str, contents: &str) {
        self.write_file(self.home_dir.join(relative), contents);
    }

    fn read_home_file(&self, relative: &str) -> String {
        fs::read_to_string(self.home_dir.join(relative)).expect("read home file")
    }

    fn read_repo_file(&self, relative: &str) -> String {
        fs::read_to_string(self.repo_dir.join(relative)).expect("read repo file")
    }

    fn home_file_exists(&self, relative: &str) -> bool {
        self.home_dir.join(relative).exists()
    }

    fn repo_exists(&self) -> bool {
        self.repo_dir.exists()
    }

    fn write_file(&self, path: PathBuf, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write file");
    }
}

fn init_bare_remote(remote_dir: &Path) {
    if let Some(parent) = remote_dir.parent() {
        fs::create_dir_all(parent).expect("create remote parent dir");
    }

    let output = Command::new("git")
        .args(["init", "--bare"])
        .arg(remote_dir)
        .output()
        .expect("run git init --bare");
    assert!(
        output.status.success(),
        "git init --bare failed: {}",
        render_output(&output)
    );
}

#[test]
fn init_creates_repo_and_syncs_config() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let output = machine.init();

    assert!(output.status.success(), "{}", render_output(&output));
    assert!(machine.repo_exists(), "repo was not created");
    assert!(
        machine.home_file_exists(&format!("{CONFIG_DIR}/config.toml")),
        "config was not synced to home"
    );
    let config = machine.read_home_file(&format!("{CONFIG_DIR}/config.toml"));
    assert!(config.contains("all = {}"), "config was: {config}");
    assert!(
        config.contains("linux = { parents = [\"all\"] }"),
        "config was: {config}"
    );
    assert!(
        config.contains("mx-xps-cy = { parents = [\"linux\"] }"),
        "config was: {config}"
    );
}

#[test]
fn basic_sync_after_init_copies_repo_file_to_home() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    assert!(machine.init().status.success(), "init failed");
    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let commit_output = machine.commit("all", "add gitconfig");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert_eq!(
        machine.read_home_file(".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
}

#[test]
fn multi_machine_shared_config_syncs_to_joining_machine() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "mx-xps-cy");
    assert!(machine_a.init().status.success(), "init failed");
    machine_a.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");
    assert!(
        machine_a.commit("all", "add gitconfig").status.success(),
        "machine A commit failed"
    );

    let machine_b = harness.machine("machine-b", "windows", "mx-pc-win");
    let init_output = machine_b.init();

    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );
    assert_eq!(
        machine_b.read_home_file(".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
    let config = machine_b.read_home_file(&format!("{CONFIG_DIR}/config.toml"));
    assert!(
        config.contains("windows = { parents = [\"all\"] }"),
        "config was: {config}"
    );
    assert!(
        config.contains("mx-pc-win = { parents = [\"windows\"] }"),
        "config was: {config}"
    );
}

#[test]
fn scope_specific_config_stays_within_scope_descendants() {
    let harness = TestHarness::new();
    let linux_machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    assert!(linux_machine.init().status.success(), "init failed");
    let windows_machine = harness.machine("machine-b", "windows", "mx-pc-win");
    let second_init = windows_machine.init();
    assert!(
        second_init.status.success(),
        "{}",
        render_output(&second_init)
    );
    let config = windows_machine.read_repo_file(&format!("{CONFIG_DIR}/config.toml"));
    assert!(
        config.contains("windows = { parents = [\"all\"] }"),
        "config was: {config}"
    );

    linux_machine.write_repo_file(".config/hypr/linux-only.conf", "enabled = true\n");
    let commit_output = linux_machine.commit("linux", "linux only");

    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );
    assert_eq!(
        linux_machine.read_home_file(".config/hypr/linux-only.conf"),
        "enabled = true\n"
    );

    let windows_sync = windows_machine.sync();
    assert!(
        windows_sync.status.success(),
        "{}",
        render_output(&windows_sync)
    );
    assert!(
        !windows_machine.home_file_exists(".config/hypr/linux-only.conf"),
        "windows machine unexpectedly received linux file"
    );

    let linux_clone = harness.machine("machine-c", "linux", "mx-linux-box");
    let linux_init = linux_clone.init();
    assert!(
        linux_init.status.success(),
        "{}",
        render_output(&linux_init)
    );
    assert_eq!(
        linux_clone.read_home_file(".config/hypr/linux-only.conf"),
        "enabled = true\n"
    );
}

#[test]
fn drift_detection_preserves_home_file() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    assert!(machine.init().status.success(), "init failed");
    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");
    assert!(
        machine.commit("all", "add gitconfig").status.success(),
        "commit failed"
    );
    machine.write_home_file(".gitconfig", "[user]\nname = \"Drift\"\n");

    let output = machine.sync();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{}", render_output(&output));
    assert!(stderr.contains("drift"), "stderr was: {stderr}");
    assert!(stderr.contains(".gitconfig"), "stderr was: {stderr}");
    assert_eq!(
        machine.read_home_file(".gitconfig"),
        "[user]\nname = \"Drift\"\n"
    );
}

#[test]
fn force_sync_overwrites_drift() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    assert!(machine.init().status.success(), "init failed");
    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");
    assert!(
        machine.commit("all", "add gitconfig").status.success(),
        "commit failed"
    );
    machine.write_home_file(".gitconfig", "[user]\nname = \"Drift\"\n");

    let output = machine.force_sync();

    assert!(output.status.success(), "{}", render_output(&output));
    assert_eq!(
        machine.read_home_file(".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
}

#[test]
fn commit_rejects_non_ancestor_scope() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    assert!(machine.init().status.success(), "init failed");
    let windows_machine = harness.machine("machine-b", "windows", "mx-pc-win");
    let second_init = windows_machine.init();
    assert!(
        second_init.status.success(),
        "{}",
        render_output(&second_init)
    );
    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let output = machine.commit("windows", "wrong scope");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{}", render_output(&output));
    assert!(stderr.contains("windows"), "stderr was: {stderr}");
    assert!(
        stderr.contains("ancestor") || stderr.contains("does not exist"),
        "stderr was: {stderr}"
    );
}

#[test]
fn invalid_scope_is_rejected() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    assert!(machine.init().status.success(), "init failed");

    let output = machine.commit("nonexistent", "bad");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{}", render_output(&output));
    assert!(stderr.contains("nonexistent"), "stderr was: {stderr}");
    assert!(stderr.contains("does not exist"), "stderr was: {stderr}");
}

#[test]
fn config_validation_rejects_cycle() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    assert!(machine.init().status.success(), "init failed");
    machine.write_repo_file(
        &format!("{CONFIG_DIR}/config.toml"),
        "[scopes]\nall = { parents = [\"linux\"] }\nlinux = { parents = [\"all\"] }\nmx-xps-cy = { parents = [\"linux\"] }\n",
    );

    let output = machine.sync();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{}", render_output(&output));
    assert!(stderr.contains("cycle"), "stderr was: {stderr}");
}

#[test]
fn config_validation_rejects_missing_parent() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    assert!(machine.init().status.success(), "init failed");
    machine.write_repo_file(
        &format!("{CONFIG_DIR}/config.toml"),
        "[scopes]\nall = {}\nlinux = { parents = [\"missing\"] }\nmx-xps-cy = { parents = [\"linux\"] }\n",
    );

    let output = machine.sync();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{}", render_output(&output));
    assert!(stderr.contains("missing parent") || stderr.contains("references missing parent"));
}

// ── Merge cascade tests ──────────────────────────────────────────────

#[test]
fn diamond_cascade_resolves_conflicts_across_multi_parent_merge() {
    // Scope graph:
    //       all
    //      / | \
    //   linux a  b
    //      \ | /
    //     machine
    //
    // Commit conflicting changes to `a` and `b`, cascade to `machine` —
    // machine merges from both parents and hits a conflict.
    // Resolve, continue, verify.

    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    assert!(machine.init().status.success(), "init failed");

    // Add scopes `a` and `b`, make machine inherit from all three
    let diamond_config = "\
[scopes]
all = {}
linux = { parents = [\"all\"] }
a = { parents = [\"all\"] }
b = { parents = [\"all\"] }
mx-xps-cy = { parents = [\"linux\", \"a\", \"b\"] }
";
    machine.write_repo_file(&format!("{CONFIG_DIR}/config.toml"), diamond_config);
    let config_commit = machine.commit("all", "add diamond scopes");
    assert!(
        config_commit.status.success(),
        "{}",
        render_output(&config_commit)
    );

    // Commit to `a` — creates branch, adds a line to .shellrc
    machine.write_repo_file(".shellrc", "# shared\nexport FROM_A=1\n");
    let a_commit = machine.commit("a", "add shell config from a");
    assert!(a_commit.status.success(), "{}", render_output(&a_commit));

    // Commit to `b` — conflicting change to same file
    machine.write_repo_file(".shellrc", "# shared\nexport FROM_B=1\n");
    let (b_output, b_json) = machine.run_dotsync_json(&["b", "-m", "add shell config from b"]);

    // Cascade should pause with conflict on `machine`
    assert_eq!(
        b_output.status.code(),
        Some(3),
        "expected conflict exit code: {}",
        render_output(&b_output)
    );
    assert_eq!(b_json["status"], "conflict");
    assert_eq!(b_json["scope"], "mx-xps-cy");
    let conflicted_files: Vec<&str> = b_json["conflicted_files"]
        .as_array()
        .expect("conflicted_files array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        conflicted_files.contains(&".shellrc"),
        "expected .shellrc in conflicted files: {conflicted_files:?}"
    );

    // Resolve: write the merged version in the repo
    machine.write_repo_file(".shellrc", "# shared\nexport FROM_A=1\nexport FROM_B=1\n");

    // Continue the cascade
    let cont = machine.run_dotsync(&["continue"]);
    assert!(cont.status.success(), "{}", render_output(&cont));

    // Verify the resolved file made it to home
    assert_eq!(
        machine.read_home_file(".shellrc"),
        "# shared\nexport FROM_A=1\nexport FROM_B=1\n"
    );
}

#[test]
fn recorded_conflict_resolution_survives_subsequent_cascade() {
    // Scope graph: all → linux → machine (default from init)
    //
    // 1. Commit to machine (file change)
    // 2. Commit conflicting change to linux → cascade to machine conflicts
    //    → resolve → continue
    // 3. Commit conflicting change to all → cascade to linux conflicts
    //    → resolve → continue → cascade to machine should be CLEAN
    //    (resolution from step 2 is in merge history)

    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    assert!(machine.init().status.success(), "init failed");

    // Step 1: commit a file on the machine scope
    machine.write_repo_file(".shellrc", "# machine version\nexport MACHINE=1\n");
    let machine_commit = machine.commit("mx-xps-cy", "machine shell config");
    assert!(
        machine_commit.status.success(),
        "{}",
        render_output(&machine_commit)
    );

    // Step 2: commit conflicting change to linux — cascade to machine conflicts
    machine.write_repo_file(".shellrc", "# linux version\nexport LINUX=1\n");
    let (linux_output, linux_json) =
        machine.run_dotsync_json(&["linux", "-m", "linux shell config"]);
    assert_eq!(
        linux_output.status.code(),
        Some(3),
        "expected conflict exit code: {}",
        render_output(&linux_output)
    );
    assert_eq!(linux_json["status"], "conflict");
    assert_eq!(linux_json["scope"], "mx-xps-cy");

    // Resolve: combine both
    machine.write_repo_file(".shellrc", "# resolved\nexport LINUX=1\nexport MACHINE=1\n");
    let cont1 = machine.run_dotsync(&["continue"]);
    assert!(cont1.status.success(), "{}", render_output(&cont1));

    // Step 3: commit conflicting change to all — cascade should conflict at linux
    machine.write_repo_file(".shellrc", "# all version\nexport ALL=1\n");
    let (all_output, all_json) = machine.run_dotsync_json(&["all", "-m", "all shell config"]);
    assert_eq!(
        all_output.status.code(),
        Some(3),
        "expected conflict at linux: {}",
        render_output(&all_output)
    );
    assert_eq!(all_json["status"], "conflict");
    assert_eq!(all_json["scope"], "linux");

    // Resolve: combine all + linux
    machine.write_repo_file(
        ".shellrc",
        "# final\nexport ALL=1\nexport LINUX=1\nexport MACHINE=1\n",
    );
    let cont2 = machine.run_dotsync(&["continue"]);
    assert!(
        cont2.status.success(),
        "second continue failed (machine cascade should be clean): {}",
        render_output(&cont2)
    );

    // The key assertion: machine has the fully resolved file,
    // meaning the resolution from step 2 was preserved in merge history
    assert_eq!(
        machine.read_home_file(".shellrc"),
        "# final\nexport ALL=1\nexport LINUX=1\nexport MACHINE=1\n",
    );
}

#[test]
fn multi_machine_cascade_resolves_other_machines_conflicts_and_returns_home() {
    // Scope graph:
    //       all
    //      /   \
    //   linux  windows
    //     |       |
    //   machA   machB
    //
    // machA commits to linux, machB commits to windows (same file, different content).
    // machA commits to all with a conflicting change — cascade walks the
    // whole DAG and conflicts on the other branches.
    // machA resolves, continues until done.
    // Verify: machA ends up on its own branch. machB syncs and gets the resolved file.

    let harness = TestHarness::new();
    let mach_a = harness.machine("machine-a", "linux", "mx-xps-cy");
    assert!(mach_a.init().status.success(), "machA init failed");
    let mach_b = harness.machine("machine-b", "windows", "mx-pc-win");
    assert!(mach_b.init().status.success(), "machB init failed");

    // machA commits a file to linux scope
    mach_a.write_repo_file(".shellrc", "# linux\nexport LINUX=1\n");
    let linux_commit = mach_a.commit("linux", "linux shell config");
    assert!(
        linux_commit.status.success(),
        "{}",
        render_output(&linux_commit)
    );

    // machB syncs to pick up latest, then commits a conflicting file to windows scope
    assert!(mach_b.sync().status.success(), "machB sync failed");
    mach_b.write_repo_file(".shellrc", "# windows\nexport WINDOWS=1\n");
    let windows_commit = mach_b.commit("windows", "windows shell config");
    assert!(
        windows_commit.status.success(),
        "{}",
        render_output(&windows_commit)
    );

    // machA syncs to pick up machB's changes
    assert!(mach_a.sync().status.success(), "machA sync failed");

    // machA commits a conflicting change to `all` — cascade will walk:
    // all → linux (may conflict) → machA, all → windows (may conflict) → machB
    mach_a.write_repo_file(".shellrc", "# all\nexport ALL=1\n");
    let (all_output, all_json) = mach_a.run_dotsync_json(&["all", "-m", "shared shell config"]);

    // Should pause with a conflict somewhere in the cascade
    assert_eq!(
        all_output.status.code(),
        Some(3),
        "expected conflict exit code: {}",
        render_output(&all_output)
    );
    assert_eq!(all_json["status"], "conflict");
    let first_scope = all_json["scope"].as_str().expect("scope string");

    // Resolve the first conflict — write a combined version
    write_resolution(&mach_a, first_scope);
    let (cont1_output, cont1_json) = mach_a.run_dotsync_json(&["continue"]);

    // There may be a second conflict (the other OS branch)
    if cont1_output.status.code() == Some(3) {
        assert_eq!(cont1_json["status"], "conflict");
        let second_scope = cont1_json["scope"].as_str().expect("scope string");
        write_resolution(&mach_a, second_scope);
        let cont2 = mach_a.run_dotsync(&["continue"]);
        assert!(
            cont2.status.success(),
            "second continue failed: {}",
            render_output(&cont2)
        );
    } else {
        assert!(
            cont1_output.status.success(),
            "continue failed: {}",
            render_output(&cont1_output)
        );
    }

    // machA should be back on its own branch — syncing should work
    let mach_a_sync = mach_a.sync();
    assert!(
        mach_a_sync.status.success(),
        "machA sync after cascade failed: {}",
        render_output(&mach_a_sync)
    );
    let mach_a_file = mach_a.read_home_file(".shellrc");
    assert!(
        mach_a_file.contains("ALL=1"),
        "machA should have ALL: {mach_a_file}"
    );
    assert!(
        mach_a_file.contains("LINUX=1"),
        "machA should have LINUX: {mach_a_file}"
    );

    // machB syncs — should get the resolved file without conflicts
    let mach_b_sync = mach_b.sync();
    assert!(
        mach_b_sync.status.success(),
        "machB sync after cascade failed: {}",
        render_output(&mach_b_sync)
    );
    let mach_b_file = mach_b.read_home_file(".shellrc");
    assert!(
        mach_b_file.contains("ALL=1"),
        "machB should have ALL: {mach_b_file}"
    );
    assert!(
        mach_b_file.contains("WINDOWS=1"),
        "machB should have WINDOWS: {mach_b_file}"
    );
}

/// Helper: write a resolved .shellrc based on which scope is conflicted.
fn write_resolution(machine: &MachineEnvironment, scope: &str) {
    let mut lines = vec!["# resolved"];
    // Include the scope-specific export plus the shared one
    match scope {
        s if s.contains("linux") || s == "mx-xps-cy" => {
            lines.push("export ALL=1");
            lines.push("export LINUX=1");
        }
        s if s.contains("windows") || s == "mx-pc-win" => {
            lines.push("export ALL=1");
            lines.push("export WINDOWS=1");
        }
        _ => {
            lines.push("export ALL=1");
        }
    }
    let resolved = lines.join("\n") + "\n";
    machine.write_repo_file(".shellrc", &resolved);
}

fn render_output(output: &Output) -> String {
    format!(
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn tdd_ratchet_gatekeeper() {
    if std::env::var("TDD_RATCHET").is_err() {
        panic!("Run tdd-ratchet instead of cargo test.");
    }
}
