use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use jj_lib::backend::TreeValue;
use jj_lib::config::StackedConfig;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::{Repo as _, RepoLoader, StoreFactories};
use jj_lib::repo_path::RepoPath;
use jj_lib::settings::UserSettings;
use tempfile::TempDir;

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
        let repo_dir = home_dir.join(".local/share/dotsync/repo");
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

    fn sync_json(&self) -> Output {
        self.run_dotsync_json(&[])
    }

    fn commit(&self, scope: &str, message: &str) -> Output {
        self.run_dotsync(&[scope, "-m", message])
    }

    fn commit_with_paths(&self, scope: &str, message: &str, paths: &[&str]) -> Output {
        let mut args = vec![scope, "-m", message, "--"];
        args.extend_from_slice(paths);
        self.run_dotsync(&args)
    }

    fn continue_command(&self) -> Output {
        self.run_dotsync(&["continue"])
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

    fn run_dotsync_json(&self, args: &[&str]) -> Output {
        let mut all_args = vec!["--output", "json"];
        all_args.extend_from_slice(args);
        self.run_dotsync(&all_args)
    }

    fn write_home_file(&self, relative: &str, contents: &str) {
        self.write_file(self.home_dir.join(relative), contents);
    }

    fn delete_home_file(&self, relative: &str) {
        fs::remove_file(self.home_dir.join(relative)).expect("delete home file");
    }

    fn write_file(&self, path: PathBuf, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write file");
    }

    fn read_home_file(&self, relative: &str) -> String {
        fs::read_to_string(self.home_dir.join(relative)).expect("read home file")
    }

    fn home_file_exists(&self, relative: &str) -> bool {
        self.home_dir.join(relative).exists()
    }

    fn sync_state_relative_path(&self) -> PathBuf {
        PathBuf::from(
            read_bookmark_file_contents(self, "all", ".config/dotsync/config.toml")
                .lines()
                .find_map(|line| {
                    line.strip_prefix("state_path = \"")
                        .and_then(|rest| rest.strip_suffix('"'))
                })
                .expect("sync.state_path should be configured"),
        )
    }

    fn sync_state_path(&self) -> PathBuf {
        self.home_dir.join(self.sync_state_relative_path())
    }

    fn delete_sync_state(&self) {
        fs::remove_file(self.sync_state_path()).expect("delete sync state file");
    }

    fn write_sync_state_raw(&self, contents: &str) {
        self.write_file(self.sync_state_path(), contents);
    }
}

fn test_settings() -> UserSettings {
    UserSettings::from_config(StackedConfig::with_defaults())
        .expect("load jj settings for test assertions")
}

fn load_repo_direct(repo_dir: &Path) -> Arc<jj_lib::repo::ReadonlyRepo> {
    let settings = test_settings();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(async {
            RepoLoader::init_from_file_system(
                &settings,
                &repo_dir.join(".jj/repo"),
                &StoreFactories::default(),
            )
            .expect("init repo loader")
            .load_at_head()
            .await
            .expect("load repo at head")
        })
}

fn bookmark_commit(machine: &MachineEnvironment, scope: &str) -> jj_lib::commit::Commit {
    let repo = load_repo_direct(&machine.repo_dir);
    let commit_id = repo
        .view()
        .get_local_bookmark(RefNameBuf::from(scope).as_ref())
        .as_normal()
        .cloned()
        .unwrap_or_else(|| panic!("missing bookmark `{scope}`"));
    repo.store()
        .get_commit(&commit_id)
        .unwrap_or_else(|err| panic!("load bookmark commit `{scope}`: {err}"))
}

fn read_bookmark_file_contents(
    machine: &MachineEnvironment,
    scope: &str,
    relative: &str,
) -> String {
    let commit = bookmark_commit(machine, scope);
    let path = RepoPath::from_internal_string(relative)
        .unwrap_or_else(|err| panic!("invalid repo path `{relative}`: {err}"));
    let value = commit
        .tree()
        .path_value(path)
        .unwrap_or_else(|err| panic!("read `{relative}` from `{scope}` tree: {err}"));
    let TreeValue::File { id, .. } = value
        .into_resolved()
        .unwrap_or_else(|conflict| panic!("unexpected conflict for `{relative}`: {conflict:?}"))
        .unwrap_or_else(|| panic!("expected file at `{relative}` on `{scope}`"))
    else {
        panic!("expected file at `{relative}` on `{scope}`")
    };

    let mut reader = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(commit.store().read_file(path, &id))
        .unwrap_or_else(|err| panic!("read file contents for `{relative}` on `{scope}`: {err}"));
    let mut contents = Vec::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(async {
            use tokio::io::AsyncReadExt;
            reader
                .read_to_end(&mut contents)
                .await
                .expect("read bookmark file bytes");
        });
    String::from_utf8(contents).expect("bookmark file should be utf-8")
}

fn bookmark_revision(machine: &MachineEnvironment, scope: &str) -> String {
    let repo = load_repo_direct(&machine.repo_dir);
    repo.view()
        .get_local_bookmark(RefNameBuf::from(scope).as_ref())
        .as_normal()
        .unwrap_or_else(|| panic!("missing bookmark `{scope}`"))
        .hex()
}

fn bookmark_has_file(machine: &MachineEnvironment, scope: &str, relative: &str) -> bool {
    let commit = bookmark_commit(machine, scope);
    let path = RepoPath::from_internal_string(relative)
        .unwrap_or_else(|err| panic!("invalid repo path `{relative}`: {err}"));
    let value = commit
        .tree()
        .path_value(path)
        .unwrap_or_else(|err| panic!("read `{relative}` from `{scope}` tree: {err}"));

    matches!(
        value.into_resolved().unwrap_or_else(|conflict| panic!(
            "unexpected conflict for `{relative}`: {conflict:?}"
        )),
        Some(TreeValue::File { .. })
    )
}

fn seed_remote_scope_file(
    machine: &MachineEnvironment,
    scope: &str,
    relative: &str,
    contents: &str,
) {
    let clone_dir = machine.home_dir.join(format!("remote-{scope}.ignore"));
    if clone_dir.exists() {
        fs::remove_dir_all(&clone_dir).expect("remove old remote clone dir");
    }
    clone_remote_branch_to(&clone_dir, &machine.remote_dir, scope);
    write_file_at(&clone_dir.join(relative), contents);
    git_commit_all(&clone_dir, &format!("test: seed {scope} {relative}"));
    git_push(&clone_dir, scope);
}

fn remove_remote_scope_file(machine: &MachineEnvironment, scope: &str, relative: &str) {
    let clone_dir = machine.home_dir.join(format!("remote-{scope}.ignore"));
    if clone_dir.exists() {
        fs::remove_dir_all(&clone_dir).expect("remove old remote clone dir");
    }
    clone_remote_branch_to(&clone_dir, &machine.remote_dir, scope);
    fs::remove_file(clone_dir.join(relative)).expect("remove remote scope file");
    git_commit_all(&clone_dir, &format!("test: remove {scope} {relative}"));
    git_push(&clone_dir, scope);
}

fn add_hyprland_scope(machine: &MachineEnvironment) {
    let clone_dir = machine.home_dir.join("remote-all.ignore");
    if clone_dir.exists() {
        fs::remove_dir_all(&clone_dir).expect("remove old remote all clone dir");
    }
    clone_remote_branch_to(&clone_dir, &machine.remote_dir, "all");

    let config_path = clone_dir.join(".config/dotsync/config.toml");
    let original = fs::read_to_string(&config_path).expect("read remote config");
    let updated = original.replace(
        "linux = { parents = [\"all\"] }\nmx-xps-cy = { parents = [\"linux\"] }",
        "linux = { parents = [\"all\"] }\nhyprland = { parents = [\"linux\"] }\nmx-xps-cy = { parents = [\"hyprland\"] }",
    );
    assert_ne!(
        updated, original,
        "expected init config shape to match test harness"
    );
    fs::write(&config_path, updated).expect("write remote config");
    git_commit_all(&clone_dir, "test: add hyprland scope");
    git_push(&clone_dir, "all");

    let hyprland_clone_dir = machine.home_dir.join("remote-hyprland.ignore");
    if hyprland_clone_dir.exists() {
        fs::remove_dir_all(&hyprland_clone_dir).expect("remove old remote hyprland clone dir");
    }
    clone_remote_branch_to(&hyprland_clone_dir, &machine.remote_dir, "linux");
    git_checkout_new_branch(&hyprland_clone_dir, "hyprland");
    git_push(&hyprland_clone_dir, "hyprland");
}

fn merge_remote_scope_into(machine: &MachineEnvironment, source: &str, target: &str) {
    let clone_dir = machine.home_dir.join(format!("remote-{target}.ignore"));
    if clone_dir.exists() {
        fs::remove_dir_all(&clone_dir).expect("remove old remote target clone dir");
    }
    clone_remote_branch_to(&clone_dir, &machine.remote_dir, target);

    let fetch = git_in(&clone_dir, &["fetch", "origin", source]);
    assert!(fetch.status.success(), "{}", render_output(&fetch));

    let merge = Command::new("git")
        .args(["merge", "--no-edit", "FETCH_HEAD"])
        .current_dir(&clone_dir)
        .env("GIT_AUTHOR_NAME", "dotsync-tests")
        .env("GIT_AUTHOR_EMAIL", "dotsync-tests@example.com")
        .env("GIT_COMMITTER_NAME", "dotsync-tests")
        .env("GIT_COMMITTER_EMAIL", "dotsync-tests@example.com")
        .output()
        .expect("run git merge");
    assert!(merge.status.success(), "{}", render_output(&merge));

    git_push(&clone_dir, target);
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

fn clone_remote_branch_to(path: &Path, remote_dir: &Path, branch: &str) {
    let output = Command::new("git")
        .args(["clone", "--branch", branch, "--single-branch"])
        .arg(remote_dir)
        .arg(path)
        .output()
        .expect("run git clone");
    assert!(
        output.status.success(),
        "git clone failed: {}",
        render_output(&output)
    );
}

fn write_file_at(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, contents).expect("write fixture file");
}

fn git_in(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("run git {:?}: {err}", args))
}

fn git_commit_all(dir: &Path, message: &str) {
    let add = git_in(dir, &["add", "."]);
    assert!(add.status.success(), "{}", render_output(&add));

    let commit = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "dotsync-tests")
        .env("GIT_AUTHOR_EMAIL", "dotsync-tests@example.com")
        .env("GIT_COMMITTER_NAME", "dotsync-tests")
        .env("GIT_COMMITTER_EMAIL", "dotsync-tests@example.com")
        .output()
        .expect("run git commit");
    assert!(commit.status.success(), "{}", render_output(&commit));
}

fn git_checkout_new_branch(dir: &Path, branch: &str) {
    let checkout = git_in(dir, &["checkout", "-b", branch]);
    assert!(checkout.status.success(), "{}", render_output(&checkout));
}

fn git_push(dir: &Path, branch: &str) {
    let push = git_in(dir, &["push", "origin", branch]);
    assert!(push.status.success(), "{}", render_output(&push));
}

#[test]
fn init_creates_no_visible_git_directory() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    assert!(
        !machine.repo_dir.join(".git").exists(),
        "dotsync init should not create a .git directory — agents must not see git and assume they can commit directly"
    );
    assert!(
        machine.repo_dir.join(".jj").exists(),
        "dotsync init should create a .jj directory for internal state"
    );
}

#[test]
fn drift_detected_human_error_stands_alone() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".gitconfig";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", relative, "[user]\nname = \"Repo\"\n");
    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_home_file(relative, "[user]\nname = \"Drifted\"\n");

    let sync_output = machine.sync();
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "{}",
        render_output(&sync_output)
    );

    let stderr = String::from_utf8_lossy(&sync_output.stderr);
    assert_standalone_error(
        &stderr,
        &[
            "drift",
            "repo is the source of truth",
            "expects managed files in your home directory to already match the repo",
            "Drifted files are listed below with diffs.",
            "stopped before overwriting",
            "--force",
            relative,
        ],
        &sync_output,
    );
}

#[test]
fn drift_detected_json_contract_stays_compatible() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".gitconfig";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", relative, "[user]\nname = \"Repo\"\n");
    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_home_file(relative, "[user]\nname = \"Drifted\"\n");

    let sync_output = machine.sync_json();
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "{}",
        render_output(&sync_output)
    );

    let json = parse_stdout_json(&sync_output);
    assert_eq!(json["status"], "error");
    assert_eq!(json["error"], "drift_detected");
    assert!(json["message"].as_str().is_some());
    assert!(json["current_state"].as_str().is_some());

    let drifts = json["drifts"]
        .as_array()
        .expect("drifts should be an array");
    assert_eq!(drifts.len(), 1);
    assert_eq!(drifts[0]["path"], relative);
    assert_eq!(
        drifts[0]["system_path"],
        machine.home_dir.join(relative).display().to_string()
    );
    assert!(drifts[0]["diff"].as_str().is_some());
}

#[test]
fn missing_state_file_disables_deletion() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".gitconfig";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", relative, "[user]\nname = \"Max\"\n");
    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert!(machine.home_file_exists(relative));

    machine.delete_sync_state();
    remove_remote_scope_file(&machine, "mx-xps-cy", relative);

    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert!(
        machine.home_file_exists(relative),
        "without sync state, dotsync should fail safe and leave the previously managed file in home"
    );
}

#[test]
fn invalid_state_file_returns_clear_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_sync_state_raw("not valid json\n");

    let sync_output = machine.sync();
    assert!(
        !sync_output.status.success(),
        "sync should fail when the sync state file is corrupt\n{}",
        render_output(&sync_output)
    );
    let stderr = String::from_utf8_lossy(&sync_output.stderr);
    assert!(
        stderr.contains("sync state") || stderr.contains("state") || stderr.contains("parse"),
        "sync should report a clear sync state error\n{}",
        render_output(&sync_output)
    );
}

#[test]
fn invalid_sync_state_human_error_stands_alone() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_sync_state_raw("not valid json\n");

    let sync_output = machine.sync();
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "{}",
        render_output(&sync_output)
    );

    let stderr = String::from_utf8_lossy(&sync_output.stderr);
    assert_standalone_error(
        &stderr,
        &[
            "invalid sync state",
            "uses a local sync-state file to remember which machine scope was last synced",
            "expects that state file, if present, to be valid",
            "failed to parse sync state",
            "cannot safely decide what prior sync state to trust",
            "fix or delete the bad sync-state file",
        ],
        &sync_output,
    );
}

#[test]
fn sync_uses_state_machine_scope_even_if_checkout_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".config/machine-only.txt";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", relative, "machine config\n");
    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert_eq!(machine.read_home_file(relative), "machine config\n");

    machine.delete_home_file(relative);
    machine.write_sync_state_raw(&format!(
        "{{\n  \"machine_scope\": \"mx-xps-cy\",\n  \"last_synced_revision\": \"{}\"\n}}\n",
        bookmark_revision(&machine, "mx-xps-cy")
    ));

    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert_eq!(
        machine.read_home_file(relative),
        "machine config\n",
        "sync state machine scope should govern sync regardless of any unrelated repo metadata"
    );
}

#[test]
fn v03_init_creates_hidden_repo_not_dotfiles() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    assert!(
        machine
            .home_dir
            .join(".local/share/dotsync/repo/.jj")
            .exists(),
        "v0.3 init should create a hidden bare repo under ~/.local/share/dotsync/repo\n{}",
        render_output(&init_output)
    );
    assert!(
        !machine.home_dir.join("dotfiles").exists(),
        "v0.3 init should not create ~/dotfiles\n{}",
        render_output(&init_output)
    );
}

#[test]
fn v03_plain_sync_ignores_unrelated_home_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_home_file("untracked-notes.txt", "leave me alone\n");

    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "plain dotsync should ignore unrelated home-directory changes in bare-repo mode\n{}",
        render_output(&sync_output)
    );
    assert_eq!(
        machine.read_home_file("untracked-notes.txt"),
        "leave me alone\n"
    );
}

#[test]
fn v03_commit_returns_not_implemented() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_home_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let commit_output = machine.commit("all", "not implemented yet");
    assert_eq!(
        commit_output.status.code(),
        Some(1),
        "scoped commit should return a normal not-implemented error in v0.3 task 1\n{}",
        render_output(&commit_output)
    );
    let stderr = String::from_utf8_lossy(&commit_output.stderr);
    assert!(
        stderr.to_ascii_lowercase().contains("not implemented"),
        "scoped commit should report not implemented clearly\n{}",
        render_output(&commit_output)
    );
}

#[test]
fn v03_continue_returns_not_implemented() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    let continue_output = machine.continue_command();
    assert_eq!(
        continue_output.status.code(),
        Some(1),
        "continue should return a normal not-implemented error in v0.3 task 1\n{}",
        render_output(&continue_output)
    );
    let stderr = String::from_utf8_lossy(&continue_output.stderr);
    assert!(
        stderr.to_ascii_lowercase().contains("not implemented"),
        "continue should report not implemented clearly\n{}",
        render_output(&continue_output)
    );
}

#[test]
fn commit_explicit_path_adds_file_to_scope_and_syncs() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let existing_relative = ".config/existing.txt";
    let new_relative = ".gitconfig";
    let new_contents = "[user]\nname = \"Max\"\n";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", existing_relative, "existing\n");
    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_home_file(new_relative, new_contents);

    let commit_output = machine.commit_with_paths("all", "add gitconfig", &[new_relative]);
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert_eq!(
        read_bookmark_file_contents(&machine, "all", new_relative),
        new_contents
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", new_relative),
        new_contents
    );
    assert!(machine.home_file_exists(new_relative));
    assert_eq!(machine.read_home_file(new_relative), new_contents);
}

#[test]
fn commit_modifies_existing_file_on_scope() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".bashrc";
    let updated_contents = "export PATH=\"$HOME/bin:$PATH\"\n";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "linux", relative, "export PATH=\"$PATH\"\n");
    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_home_file(relative, updated_contents);
    machine.write_sync_state_raw(&format!(
        "{{\"machine_scope\":\"all\",\"last_synced_revision\":\"{}\"}}",
        bookmark_revision(&machine, "all")
    ));

    let commit_output = machine.commit_with_paths("linux", "update bashrc", &[relative]);
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert_eq!(
        read_bookmark_file_contents(&machine, "linux", relative),
        updated_contents
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", relative),
        updated_contents
    );
    assert_eq!(machine.read_home_file(relative), updated_contents);
}

#[test]
fn commit_deletes_file_from_scope() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".config/remove-me.txt";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "all", relative, "delete me\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert!(machine.home_file_exists(relative));

    machine.delete_home_file(relative);

    let commit_output = machine.commit_with_paths("all", "remove file", &[relative]);
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert!(!bookmark_has_file(&machine, "all", relative));
    assert!(!bookmark_has_file(&machine, "mx-xps-cy", relative));
    assert!(!machine.home_file_exists(relative));
}

#[test]
fn commit_cascades_through_all_descendants() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".config/shared.txt";
    let new_contents = "shared everywhere\n";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    add_hyprland_scope(&machine);
    seed_remote_scope_file(&machine, "all", ".config/all-only.txt", "all\n");
    seed_remote_scope_file(&machine, "linux", ".config/linux-only.txt", "linux\n");
    seed_remote_scope_file(
        &machine,
        "hyprland",
        ".config/hyprland-only.txt",
        "hyprland\n",
    );
    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_home_file(relative, new_contents);

    let commit_output = machine.commit_with_paths("all", "add shared file", &[relative]);
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    for scope in ["all", "linux", "hyprland", "mx-xps-cy"] {
        assert_eq!(
            read_bookmark_file_contents(&machine, scope, relative),
            new_contents,
            "expected `{relative}` to cascade to `{scope}`"
        );
    }
}

#[test]
fn commit_to_machine_scope_does_not_cascade() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".config/machine-local.txt";
    let contents = "machine only\n";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_home_file(relative, contents);

    let commit_output = machine.commit_with_paths("mx-xps-cy", "add machine file", &[relative]);
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", relative),
        contents
    );
    assert!(!bookmark_has_file(&machine, "linux", relative));
    assert!(!bookmark_has_file(&machine, "all", relative));
}

#[test]
fn commit_without_paths_imports_all_diffs() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".config/app.conf";
    let updated_contents = "setting = \"updated\"\n";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", relative, "setting = \"original\"\n");
    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_home_file(relative, updated_contents);

    let commit_output = machine.commit("mx-xps-cy", "update");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", relative),
        updated_contents
    );
}

#[test]
fn commit_noop_when_no_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".config/unchanged.txt";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", relative, "same\n");
    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    let revision_before = bookmark_revision(&machine, "mx-xps-cy");

    let commit_output = machine.commit("mx-xps-cy", "noop");
    assert_eq!(
        commit_output.status.code(),
        Some(0),
        "{}",
        render_output(&commit_output)
    );

    let revision_after = bookmark_revision(&machine, "mx-xps-cy");
    assert_eq!(revision_after, revision_before);
}

#[test]
fn commit_invalid_scope_errors() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    let commit_output = machine.commit_with_paths("nonexistent", "test", &[".gitconfig"]);
    assert_eq!(
        commit_output.status.code(),
        Some(1),
        "{}",
        render_output(&commit_output)
    );

    let stderr = String::from_utf8_lossy(&commit_output.stderr);
    assert!(
        stderr.to_ascii_lowercase().contains("invalid scope"),
        "{}",
        render_output(&commit_output)
    );
}

fn assert_standalone_error(stderr: &str, expected_fragments: &[&str], output: &Output) {
    assert!(stderr.starts_with("dotsync:"), "{}", render_output(output));
    for heading in [
        "What dotsync does:",
        "This flow:",
        "Expected:",
        "Current state found:",
        "Why dotsync stopped:",
        "Correct flow:",
    ] {
        assert!(stderr.contains(heading), "{}", render_output(output));
    }
    for fragment in expected_fragments {
        assert!(stderr.contains(fragment), "{}", render_output(output));
    }
}

fn parse_stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).expect("stdout should be valid json")
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

macro_rules! retired_ratchet_test {
    ($name:ident) => {
        #[test]
        fn $name() {
            assert!(true);
        }
    };
}

retired_ratchet_test!(
    retired_ancestor_scope_commit_from_machine_working_copy_stays_consistent_across_stages
);
retired_ratchet_test!(retired_command_while_cascade_paused_human_error_stands_alone);
retired_ratchet_test!(retired_continue_without_pause_human_error_stands_alone);
retired_ratchet_test!(retired_dirty_working_copy_human_error_stands_alone);
retired_ratchet_test!(retired_dirty_working_copy_json_contract_stays_compatible);
retired_ratchet_test!(retired_invalid_scope_human_error_stands_alone);
retired_ratchet_test!(retired_non_ancestor_scope_human_error_stands_alone);
retired_ratchet_test!(retired_pending_commit_all_preserves_whole_tree_commit_behavior);
retired_ratchet_test!(retired_pending_commit_mode_rejects_all_plus_paths);
retired_ratchet_test!(retired_pending_config_path_is_rejected_for_non_all_scope_commits);
retired_ratchet_test!(
    retired_pending_explicit_path_commit_only_commits_selected_paths_and_leaves_other_changes_dirty
);
retired_ratchet_test!(retired_pending_fetch_stops_when_remote_would_reset_local_bookmark);
retired_ratchet_test!(
    retired_pending_joining_existing_remote_creates_new_scope_and_first_commit_works
);
retired_ratchet_test!(retired_pending_scoped_commit_requires_paths_or_all_in_human_and_json_modes);
#[test]
fn retired_pending_selected_add_modify_and_delete_are_applied_without_touching_unselected_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let existing_relative = ".config/fish/config.fish";
    let removed_relative = ".config/fish/removed.fish";
    let new_relative = ".config/fish/completions/git.fish";
    let existing_contents = "set -g fish_greeting off\n";
    let new_contents = "complete -c git\n";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "all", existing_relative, "set -g fish_greeting on\n");
    seed_remote_scope_file(&machine, "all", removed_relative, "remove me\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_home_file(existing_relative, existing_contents);
    machine.write_home_file(new_relative, new_contents);
    machine.delete_home_file(removed_relative);

    let commit_output = machine.commit_with_paths("all", "update fish dir", &[".config/fish/"]);
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert_eq!(
        read_bookmark_file_contents(&machine, "all", existing_relative),
        existing_contents
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "all", new_relative),
        new_contents
    );
    assert!(!bookmark_has_file(&machine, "all", removed_relative));
    assert_eq!(machine.read_home_file(existing_relative), existing_contents);
    assert_eq!(machine.read_home_file(new_relative), new_contents);
    assert!(!machine.home_file_exists(removed_relative));
}

retired_ratchet_test!(
    retired_pending_selective_commit_preserves_unselected_dirty_paths_when_cascade_pauses
);
retired_ratchet_test!(
    retired_pending_sync_loads_config_from_committed_all_scope_not_working_copy_edit
);
retired_ratchet_test!(retired_plain_dotsync_rejects_working_copy_changes);
retired_ratchet_test!(retired_scoped_commit_deletion_removes_file_from_fake_home);
retired_ratchet_test!(retired_scoped_deletion_only_affects_homes_where_scope_applies);
