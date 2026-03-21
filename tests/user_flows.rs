use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
