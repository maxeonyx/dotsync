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
        self.run(&format!(
            "dotsync init {}",
            self.remote_dir
                .to_str()
                .expect("remote path should be valid UTF-8")
        ))
    }

    fn run(&self, command: &str) -> Output {
        let args = dotsync_args(command);
        let mut command = Command::new(env!("CARGO_BIN_EXE_dotsync"));
        command.args(args);
        command.current_dir(&self.home_dir);
        command.env("HOME", &self.home_dir);
        command.env("DOTSYNC_OS", &self.os);
        command.env("DOTSYNC_HOSTNAME", &self.hostname);
        command.output().expect("run dotsync")
    }

    /// Runs dotsync with no `HOME` in the environment, which is how dotsync
    /// finds both the home directory it manages and its hidden repo.
    fn run_without_home(&self, command: &str) -> Output {
        let args = dotsync_args(command);
        let mut command = Command::new(env!("CARGO_BIN_EXE_dotsync"));
        command.args(args);
        command.current_dir(&self.home_dir);
        command.env_remove("HOME");
        command.env("DOTSYNC_OS", &self.os);
        command.env("DOTSYNC_HOSTNAME", &self.hostname);
        command.output().expect("run dotsync")
    }

    fn delete_file(&self, relative: &str) {
        fs::remove_file(self.home_dir.join(relative)).expect("delete file");
    }

    fn write_file(&self, relative: &str, contents: &str) {
        write_file_at(&self.home_dir.join(relative), contents);
    }

    fn read_file(&self, relative: &str) -> String {
        fs::read_to_string(self.home_dir.join(relative)).expect("read file")
    }

    fn file_exists(&self, relative: &str) -> bool {
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
        write_file_at(&self.sync_state_path(), contents);
    }

    fn read_sync_state_raw(&self) -> String {
        fs::read_to_string(self.sync_state_path()).expect("read sync state file")
    }

    fn modified_time(&self, relative: &str) -> std::time::SystemTime {
        fs::metadata(self.home_dir.join(relative))
            .expect("stat home file")
            .modified()
            .expect("read home file mtime")
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

// Scope-branch fixture helpers: these set up checked-in remote state that a
// user could have produced earlier with dotsync, then the tests exercise the
// public CLI against that state.
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

// Push-failure fixture helpers: a `pre-receive` hook that always rejects makes
// the shared fake remote refuse pushes while still serving fetches, which is
// how these tests reproduce the issue #19 incident (cascade lands locally, the
// push never happens).
//
// This leans on git running a `/bin/sh` hook, so every test that blocks pushes
// must assert that the push really was rejected (local revision != remote
// revision). Without that guard a platform where the hook does not run would
// turn these tests silently green.
fn block_remote_pushes(machine: &MachineEnvironment) {
    let hook_path = machine.remote_dir.join("hooks/pre-receive");
    write_file_at(&hook_path, "#!/bin/sh\nexit 1\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))
            .expect("make pre-receive hook executable");
    }
}

fn allow_remote_pushes(machine: &MachineEnvironment) {
    fs::remove_file(machine.remote_dir.join("hooks/pre-receive")).expect("remove pre-receive hook");
}

fn remote_branch_revision(machine: &MachineEnvironment, branch: &str) -> String {
    let output = git_in(&machine.remote_dir, &["rev-parse", branch]);
    assert!(output.status.success(), "{}", render_output(&output));
    String::from_utf8(output.stdout)
        .expect("git rev-parse output should be utf-8")
        .trim()
        .to_string()
}

fn remote_branch_file_contents(
    machine: &MachineEnvironment,
    branch: &str,
    relative: &str,
) -> String {
    let output = git_in(
        &machine.remote_dir,
        &["show", &format!("{branch}:{relative}")],
    );
    assert!(output.status.success(), "{}", render_output(&output));
    String::from_utf8(output.stdout).expect("remote file contents should be utf-8")
}

/// Reproduces the issue #19 wedge: `dotsync commit` writes its commit and
/// cascade transaction, then the push fails, leaving every local scope
/// bookmark ahead of the remote. Pushes are allowed again before returning, so
/// the machine is in the exact state a user finds it in the morning after: a
/// perfectly coherent local repo with unpushed scope commits.
///
/// The exit code of the interrupted run is deliberately not asserted — how
/// dotsync reports a rejected push is a separate question from how it recovers
/// from one, and the wedged state is the same either way.
fn interrupt_push_after_cascade(machine: &MachineEnvironment, relative: &str, contents: &str) {
    machine.write_file(relative, contents);
    block_remote_pushes(machine);

    machine.run(&format!(
        "dotsync commit all -m 'add dev-certs helper' -- {relative}"
    ));

    allow_remote_pushes(machine);

    for scope in ["all", "linux", "mx-xps-cy"] {
        assert_eq!(
            read_bookmark_file_contents(machine, scope, relative),
            contents,
            "expected the cascade to have landed locally on `{scope}` before the push failed"
        );
        assert_ne!(
            bookmark_revision(machine, scope),
            remote_branch_revision(machine, scope),
            "expected local `{scope}` to be ahead of the remote after the failed push"
        );
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

fn assert_stdout_snapshot(output: &Output, expected: &str) {
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        expected,
        "{}",
        render_output(output)
    );
}

fn assert_stderr_snapshot(output: &Output, expected: &str) {
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        expected,
        "{}",
        render_output(output)
    );
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

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".gitconfig",
        "[user]\nname = \"Repo\"\n",
    );
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_file(".gitconfig", "[user]\nname = \"Drifted\"\n");

    let sync_output = machine.run("dotsync");
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "{}",
        render_output(&sync_output)
    );

    assert_stderr_snapshot(
        &sync_output,
        r#"dotsync: drift detected

What dotsync does:
Dotsync keeps its hidden repo as the source of truth for your home-directory config: the repo is the source of truth, and dotsync syncs committed repo state into the live system.

This flow:
This sync flow compares managed files in your home directory against the repo version for this machine scope before copying anything.

Expected:
This flow expects managed files in your home directory to already match the repo, unless you intentionally choose to overwrite drift.

Current state found:
Drifted files are listed below with diffs.

Why dotsync stopped:
Dotsync stopped before overwriting local drift so you can inspect what would be replaced.

Correct flow:
- If the repo is correct, rerun with `dotsync --force` to overwrite the drift after reviewing the diffs.
- If the live file is the change you wanted, run `dotsync status`, then commit the intended path with `dotsync commit <scope> -m "message" -- <path>`.
- .gitconfig (edited here since the last sync)
--- repo
+++ system
@@ -1,2 +1,2 @@
 [user]
-name = "Repo"
+name = "Drifted"
"#,
    );
}

#[test]
fn diff_shows_line_oriented_home_drift_without_syncing() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".config/app.conf",
        "line one\nline two\n",
    );
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_file(".config/app.conf", "line one\nchanged two\n");

    let diff_output = machine.run("dotsync diff");
    assert_eq!(
        diff_output.status.code(),
        Some(1),
        "{}",
        render_output(&diff_output)
    );

    assert_eq!(
        machine.read_file(".config/app.conf"),
        "line one\nchanged two\n"
    );
    assert_stderr_snapshot(
        &diff_output,
        "\
dotsync: 1 drifted managed file(s) for mx-xps-cy
- .config/app.conf (edited here since the last sync)
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
fn view_summarizes_checked_in_scopes_and_files() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "all", ".gitconfig", "[user]\nname = Shared\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    let view_output = machine.run("dotsync view");
    assert!(
        view_output.status.success(),
        "{}",
        render_output(&view_output)
    );
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

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "all", ".gitconfig", "[user]\nname = Shared\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    let view_output = machine.run("dotsync view --scope mx-xps-cy");
    assert!(
        view_output.status.success(),
        "{}",
        render_output(&view_output)
    );
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

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "all", ".gitconfig", "[user]\nname = Shared\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    let file_scopes_output = machine.run("dotsync view --file .gitconfig");
    assert!(
        file_scopes_output.status.success(),
        "{}",
        render_output(&file_scopes_output)
    );
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

    let file_content_output = machine.run("dotsync view --scope mx-xps-cy --file .gitconfig");
    assert!(
        file_content_output.status.success(),
        "{}",
        render_output(&file_content_output)
    );
    assert_stdout_snapshot(&file_content_output, "[user]\nname = Shared\n");
}

#[test]
fn drift_detected_json_contract_stays_compatible() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".gitconfig",
        "[user]\nname = \"Repo\"\n",
    );
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_file(".gitconfig", "[user]\nname = \"Drifted\"\n");

    let sync_output = machine.run("dotsync --output json");
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
    assert_eq!(drifts[0]["path"], ".gitconfig");
    assert_eq!(
        drifts[0]["system_path"],
        machine.home_dir.join(".gitconfig").display().to_string()
    );
    assert!(drifts[0]["diff"].as_str().is_some());
}

#[test]
fn missing_state_file_disables_deletion() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".gitconfig",
        "[user]\nname = \"Max\"\n",
    );
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert!(machine.file_exists(".gitconfig"));

    machine.delete_sync_state();
    remove_remote_scope_file(&machine, "mx-xps-cy", ".gitconfig");

    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert!(
        machine.file_exists(".gitconfig"),
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

    let sync_output = machine.run("dotsync");
    assert!(
        !sync_output.status.success(),
        "sync should fail when the sync state file is corrupt\n{}",
        render_output(&sync_output)
    );
    let expected = format!(
        "\
dotsync: invalid sync state

What dotsync does:
Dotsync keeps the repo as the source of truth and uses a local sync-state file to remember which machine scope was last synced here and which revision that sync used.

This flow:
This sync flow reads that local state to know which prior managed files may need removal and which machine scope should be treated as authoritative for this home.

Expected:
It expects that state file, if present, to be valid and readable; it expects that state file, if present, to be valid.

Current state found:
sync state error at {}: failed to parse sync state: expected ident at line 1 column 2

Why dotsync stopped:
Dotsync stopped because it cannot safely decide what prior sync state to trust.

Correct flow:
- fix or delete the bad sync-state file and rerun the command.
- After that, let dotsync recreate valid sync state from a successful sync.
",
        machine.sync_state_path().display()
    );
    assert_stderr_snapshot(&sync_output, &expected);
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

    let sync_output = machine.run("dotsync");
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "{}",
        render_output(&sync_output)
    );

    let expected = format!(
        "\
dotsync: invalid sync state

What dotsync does:
Dotsync keeps the repo as the source of truth and uses a local sync-state file to remember which machine scope was last synced here and which revision that sync used.

This flow:
This sync flow reads that local state to know which prior managed files may need removal and which machine scope should be treated as authoritative for this home.

Expected:
It expects that state file, if present, to be valid and readable; it expects that state file, if present, to be valid.

Current state found:
sync state error at {}: failed to parse sync state: expected ident at line 1 column 2

Why dotsync stopped:
Dotsync stopped because it cannot safely decide what prior sync state to trust.

Correct flow:
- fix or delete the bad sync-state file and rerun the command.
- After that, let dotsync recreate valid sync state from a successful sync.
",
        machine.sync_state_path().display()
    );
    assert_stderr_snapshot(&sync_output, &expected);
}

#[test]
fn sync_uses_state_machine_scope_even_if_checkout_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".config/machine-only.txt",
        "machine config\n",
    );
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert_eq!(
        machine.read_file(".config/machine-only.txt"),
        "machine config\n"
    );

    // Deleting a managed file from home is deletion drift, so this restore has
    // to say it means to discard it. What the test is pinning is which machine
    // scope the sync used, not whether the deletion blocked.
    machine.delete_file(".config/machine-only.txt");
    machine.write_sync_state_raw(&format!(
        "{{\n  \"machine_scope\": \"mx-xps-cy\",\n  \"last_synced_revision\": \"{}\"\n}}\n",
        bookmark_revision(&machine, "mx-xps-cy")
    ));

    let sync_output = machine.run("dotsync --force");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert_eq!(
        machine.read_file(".config/machine-only.txt"),
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

    machine.write_file("untracked-notes.txt", "leave me alone\n");

    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "plain dotsync should ignore unrelated home-directory changes in bare-repo mode\n{}",
        render_output(&sync_output)
    );
    assert_eq!(machine.read_file("untracked-notes.txt"), "leave me alone\n");
}

#[test]
fn commit_with_no_paths_ignores_unmanaged_home_files() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

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
fn missing_home_is_reported_as_an_environment_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let status_output = machine.run_without_home("dotsync status");
    assert_eq!(
        status_output.status.code(),
        Some(1),
        "{}",
        render_output(&status_output)
    );
    assert_stderr_snapshot(
        &status_output,
        "dotsync: HOME is not set, so dotsync cannot find your home directory. Set HOME to the home directory dotsync should manage, then rerun.\n",
    );
}

#[test]
fn continue_without_pause_returns_clear_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    let continue_output = machine.run("dotsync continue");
    assert_eq!(
        continue_output.status.code(),
        Some(1),
        "continue without a paused cascade should return a normal command error\n{}",
        render_output(&continue_output)
    );
    assert_stderr_snapshot(&continue_output, "dotsync: no paused cascade to continue\n");
}

#[test]
fn abort_paused_cascade_restores_pre_pause_state_and_clears_pause() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    let init_a = machine_a.init();
    assert!(init_a.status.success(), "{}", render_output(&init_a));
    let init_b = machine_b.init();
    assert!(init_b.status.success(), "{}", render_output(&init_b));
    let sync_a_after_join = machine_a.run("dotsync --force");
    assert!(
        sync_a_after_join.status.success(),
        "{}",
        render_output(&sync_a_after_join)
    );

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    let commit_base = machine_a.run("dotsync commit all -m 'add base config' -- .config/app.conf");
    assert!(
        commit_base.status.success(),
        "{}",
        render_output(&commit_base)
    );

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    let commit_linux =
        machine_a.run("dotsync commit linux -m 'customize linux config' -- .config/app.conf");
    assert!(
        commit_linux.status.success(),
        "{}",
        render_output(&commit_linux)
    );

    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));
    let all_before_pause = bookmark_revision(&machine_b, "all");
    let linux_before_pause = bookmark_revision(&machine_b, "linux");
    let machine_before_pause = bookmark_revision(&machine_b, "goof-b");

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "conflicting all-to-linux cascade should pause\n{}",
        render_output(&conflict)
    );

    let aborted = machine_b.run("dotsync abort");
    assert!(aborted.status.success(), "{}", render_output(&aborted));
    // Abort reverts home, so it says what it reverted: the edit that started
    // the cascade is gone, and that is the point of the command.
    assert_stderr_snapshot(
        &aborted,
        "\
dotsync: overwrote 1 drifted file(s)
- .config/app.conf (edited here since the last sync)
--- repo
+++ system
@@ -1 +1 @@
-setting = \"linux\"
+setting = \"all\"
dotsync: aborted cascade at linux and synced 2 file(s)
",
    );

    assert_eq!(bookmark_revision(&machine_b, "all"), all_before_pause);
    assert_eq!(bookmark_revision(&machine_b, "linux"), linux_before_pause);
    assert_eq!(
        bookmark_revision(&machine_b, "goof-b"),
        machine_before_pause
    );
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"linux\"\n"
    );

    let status = machine_b.run("dotsync status");
    assert!(status.status.success(), "{}", render_output(&status));
    assert_stderr_snapshot(&status, "dotsync: no changes for goof-b\n");

    machine_b.write_file(".config/other.conf", "other = true\n");
    let commit_after_abort =
        machine_b.run("dotsync commit goof-b -m 'commit after abort' -- .config/other.conf");
    assert!(
        commit_after_abort.status.success(),
        "{}",
        render_output(&commit_after_abort)
    );
}

#[test]
fn abort_paused_cascade_restores_non_conflicting_selected_paths() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    let init_a = machine_a.init();
    assert!(init_a.status.success(), "{}", render_output(&init_a));
    let init_b = machine_b.init();
    assert!(init_b.status.success(), "{}", render_output(&init_b));
    let sync_a_after_join = machine_a.run("dotsync --force");
    assert!(
        sync_a_after_join.status.success(),
        "{}",
        render_output(&sync_a_after_join)
    );

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.write_file(".config/other.conf", "other = false\n");
    let commit_base = machine_a
        .run("dotsync commit all -m 'add base config' -- .config/app.conf .config/other.conf");
    assert!(
        commit_base.status.success(),
        "{}",
        render_output(&commit_base)
    );

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    let commit_linux =
        machine_a.run("dotsync commit linux -m 'customize linux config' -- .config/app.conf");
    assert!(
        commit_linux.status.success(),
        "{}",
        render_output(&commit_linux)
    );

    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    machine_b.write_file(".config/other.conf", "other = true\n");
    let conflict = machine_b
        .run("dotsync commit all -m 'update shared config' -- .config/app.conf .config/other.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "conflicting all-to-linux cascade should pause\n{}",
        render_output(&conflict)
    );

    let aborted = machine_b.run("dotsync abort");
    assert!(aborted.status.success(), "{}", render_output(&aborted));

    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"linux\"\n"
    );
    assert_eq!(machine_b.read_file(".config/other.conf"), "other = false\n");

    let status = machine_b.run("dotsync status");
    assert!(status.status.success(), "{}", render_output(&status));
    assert_stderr_snapshot(&status, "dotsync: no changes for goof-b\n");
}

#[test]
fn explicit_commit_command_adds_file_to_scope_and_syncs() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let commit_output = machine.run("dotsync commit all -m 'add gitconfig' -- .gitconfig");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
    assert_eq!(machine.read_file(".gitconfig"), "[user]\nname = \"Max\"\n");
}

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

#[test]
fn commit_path_that_matches_nothing_is_an_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

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

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

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

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

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
    let right_scope = machine.run(&format!(
        "dotsync commit all -m 'describe hyprland' -- {config_path}"
    ));
    assert!(
        right_scope.status.success(),
        "{}",
        render_output(&right_scope)
    );
    assert!(
        read_bookmark_file_contents(&machine, "all", config_path)
            .contains("# hyprland: wayland compositor config"),
        "the scope graph change should land on all"
    );
}

#[test]
fn commit_reports_every_unusable_path_at_once() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_file(".apprc", "ui_theme = dark\n");

    // Reporting only the first bad path costs the agent one round trip per
    // mistake, and each round trip is a full fetch-and-commit attempt.
    let output = machine.run(
        "dotsync commit all -m mixed -- nonexistent-file '~/.apprc' .local/share/dotsync/repo",
    );
    assert_eq!(output.status.code(), Some(1), "{}", render_output(&output));

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

#[test]
fn commit_path_inside_dotsyncs_own_state_is_an_error() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    let sync_state = machine.sync_state_relative_path();
    let sync_state = sync_state.to_str().expect("sync state path is UTF-8");
    assert!(
        machine.file_exists(sync_state),
        "init should have written the sync state file"
    );
    let revision_before = bookmark_revision(&machine, "all");

    // Both of these are dotsync's own bookkeeping sitting in home: the
    // machine-local sync state, and the hidden repo itself. Naming either used
    // to be filtered out of the selection without a word, so the commit
    // reported success having recorded nothing.
    for command in [
        format!("dotsync commit all -m state -- {sync_state}"),
        "dotsync commit all -m repo -- .local/share/dotsync/repo".to_string(),
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

    let state_output = machine.run(&format!("dotsync commit all -m state -- {sync_state}"));
    assert_stderr_snapshot(
        &state_output,
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
`{sync_state}` is this machine's dotsync sync state; it records which machine scope this home uses, so it has to stay machine-local.

Why dotsync stopped:
Dotsync stopped before recording anything. A commit records every path you named or none of them, so fixing the paths above and rerunning the same command is safe.

Correct flow:
- name paths relative to your home directory: `dotsync commit all -m \"message\" -- .config/fish/config.fish`.
- do not use `~/`, absolute paths, or `..`; dotsync resolves every path against your home directory already, and records it verbatim.
- commit the config files you edited instead; dotsync's own state is not config and cannot travel on a scope.
- to change which scopes exist, edit `.config/dotsync/config.toml` in home and commit that path to `all`.
- run `dotsync status` to see which managed files changed.
"
        ),
    );
}

#[test]
fn commit_explicit_path_adds_file_to_scope_and_syncs() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", ".config/existing.txt", "existing\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let commit_output = machine.run("dotsync commit all -m 'add gitconfig' -- .gitconfig");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
    assert!(machine.file_exists(".gitconfig"));
    assert_eq!(machine.read_file(".gitconfig"), "[user]\nname = \"Max\"\n");
}

#[test]
fn commit_modifies_existing_file_on_scope() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "linux", ".bashrc", "export PATH=\"$PATH\"\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_file(".bashrc", "export PATH=\"$HOME/bin:$PATH\"\n");
    machine.write_sync_state_raw(&format!(
        "{{\"machine_scope\":\"all\",\"last_synced_revision\":\"{}\"}}",
        bookmark_revision(&machine, "all")
    ));

    let commit_output = machine.run("dotsync commit linux -m 'update bashrc' -- .bashrc");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert_eq!(
        read_bookmark_file_contents(&machine, "linux", ".bashrc"),
        "export PATH=\"$HOME/bin:$PATH\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".bashrc"),
        "export PATH=\"$HOME/bin:$PATH\"\n"
    );
    assert_eq!(
        machine.read_file(".bashrc"),
        "export PATH=\"$HOME/bin:$PATH\"\n"
    );
}

#[test]
fn commit_deletes_file_from_scope() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "all", ".config/remove-me.txt", "delete me\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert!(machine.file_exists(".config/remove-me.txt"));

    machine.delete_file(".config/remove-me.txt");

    let commit_output = machine.run("dotsync commit all -m 'remove file' -- .config/remove-me.txt");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert!(!bookmark_has_file(&machine, "all", ".config/remove-me.txt"));
    assert!(!bookmark_has_file(
        &machine,
        "mx-xps-cy",
        ".config/remove-me.txt"
    ));
    assert!(!machine.file_exists(".config/remove-me.txt"));
}

#[test]
fn commit_cascades_through_all_descendants() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

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
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_file(".config/shared.txt", "shared everywhere\n");

    let commit_output =
        machine.run("dotsync commit all -m 'add shared file' -- .config/shared.txt");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    for scope in ["all", "linux", "hyprland", "mx-xps-cy"] {
        assert_eq!(
            read_bookmark_file_contents(&machine, scope, ".config/shared.txt"),
            "shared everywhere\n",
            "expected `.config/shared.txt` to cascade to `{scope}`"
        );
    }
}

#[test]
fn multiple_machines_can_contribute_to_all_without_losing_changes() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    let init_a = machine_a.init();
    assert!(init_a.status.success(), "{}", render_output(&init_a));
    let init_b = machine_b.init();
    assert!(init_b.status.success(), "{}", render_output(&init_b));
    let sync_a_after_join = machine_a.run("dotsync --force");
    assert!(
        sync_a_after_join.status.success(),
        "{}",
        render_output(&sync_a_after_join)
    );

    machine_a.write_file(".config/shared-a.conf", "from machine a\n");
    let commit_a = machine_a.run("dotsync commit all -m 'add shared a' -- .config/shared-a.conf");
    assert!(commit_a.status.success(), "{}", render_output(&commit_a));

    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));
    assert_eq!(
        machine_b.read_file(".config/shared-a.conf"),
        "from machine a\n"
    );

    machine_b.write_file(".config/shared-b.conf", "from machine b\n");
    let commit_b = machine_b.run("dotsync commit all -m 'add shared b' -- .config/shared-b.conf");
    assert!(commit_b.status.success(), "{}", render_output(&commit_b));

    let sync_a = machine_a.run("dotsync");
    assert!(sync_a.status.success(), "{}", render_output(&sync_a));
    assert_eq!(
        machine_a.read_file(".config/shared-a.conf"),
        "from machine a\n"
    );
    assert_eq!(
        machine_a.read_file(".config/shared-b.conf"),
        "from machine b\n"
    );
    assert_eq!(
        machine_b.read_file(".config/shared-a.conf"),
        "from machine a\n"
    );
    assert_eq!(
        machine_b.read_file(".config/shared-b.conf"),
        "from machine b\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "all", ".config/shared-a.conf"),
        "from machine a\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "all", ".config/shared-b.conf"),
        "from machine b\n"
    );
}

#[test]
fn concurrent_same_scope_file_edits_require_resolution() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    let init_a = machine_a.init();
    assert!(init_a.status.success(), "{}", render_output(&init_a));
    let init_b = machine_b.init();
    assert!(init_b.status.success(), "{}", render_output(&init_b));
    let sync_a_after_join = machine_a.run("dotsync --force");
    assert!(
        sync_a_after_join.status.success(),
        "{}",
        render_output(&sync_a_after_join)
    );

    // Establish the shared base version first.
    machine_a.write_file(".config/shared.conf", "setting = \"base\"\n");
    let commit_base =
        machine_a.run("dotsync commit all -m 'add shared base' -- .config/shared.conf");
    assert!(
        commit_base.status.success(),
        "{}",
        render_output(&commit_base)
    );

    // Both machines start the conflict scenario from the same synced base.
    let sync_a_to_base = machine_a.run("dotsync");
    assert!(
        sync_a_to_base.status.success(),
        "{}",
        render_output(&sync_a_to_base)
    );
    assert_eq!(
        machine_a.read_file(".config/shared.conf"),
        "setting = \"base\"\n"
    );

    let sync_b_to_base = machine_b.run("dotsync");
    assert!(
        sync_b_to_base.status.success(),
        "{}",
        render_output(&sync_b_to_base)
    );
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "setting = \"base\"\n"
    );

    // Divergent local edits start here. B must not sync again before committing.
    machine_a.write_file(".config/shared.conf", "setting = \"all-a\"\n");
    machine_b.write_file(".config/shared.conf", "setting = \"all-b\"\n");
    assert_eq!(
        machine_a.read_file(".config/shared.conf"),
        "setting = \"all-a\"\n"
    );
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "setting = \"all-b\"\n",
        "machine B must make its own local edit before machine A publishes"
    );

    let commit_a =
        machine_a.run("dotsync commit all -m 'update shared from a' -- .config/shared.conf");
    assert!(commit_a.status.success(), "{}", render_output(&commit_a));
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "setting = \"all-b\"\n",
        "machine B must not sync to machine A's published edit before committing its own edit"
    );

    let conflict =
        machine_b.run("dotsync commit all -m 'update shared from b' -- .config/shared.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "concurrent same-scope edit should require conflict resolution\n{}",
        render_output(&conflict)
    );
    assert_stderr_snapshot(
        &conflict,
        r#"dotsync: cascade paused

What dotsync does:
Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config.

This flow:
This commit flow was merging the scoped change through the scope DAG and reached a branch where the same file had incompatible edits.

Expected:
It expects you to edit the conflicted file in home to the merged contents you want, then run `dotsync continue` to create the merge commit and resume the cascade.

Current state found:
paused scope: all

Why dotsync stopped:
cascade paused at scope `all` with conflicts in .config/shared.conf

Correct flow:
- edit each conflicted file at its real path in home so it holds the merged contents you want; the file has to change, because dotsync reads the resolution back out of it.
- run `dotsync continue` from the same machine to finish cascading and syncing.
- or run `dotsync abort` from the same machine to discard the paused cascade; that reverts the conflicted files in home to this machine's scope state.
- do not run another dotsync commit while the cascade is paused.
"#,
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_b, "all", ".config/shared.conf"),
        "setting = \"all-a\"\n",
        "failed concurrent commit must leave the shared scope at the already-published version"
    );
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "setting = \"all-b\"\n",
        "failed concurrent commit must not overwrite B's unresolved home edit"
    );

    machine_b.write_file(".config/shared.conf", "setting = \"all-a+all-b\"\n");
    let continued = machine_b.run("dotsync continue");
    assert!(continued.status.success(), "{}", render_output(&continued));
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "setting = \"all-a+all-b\"\n"
    );

    let sync_a = machine_a.run("dotsync");
    assert!(sync_a.status.success(), "{}", render_output(&sync_a));
    assert_eq!(
        machine_a.read_file(".config/shared.conf"),
        "setting = \"all-a+all-b\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "all", ".config/shared.conf"),
        "setting = \"all-a+all-b\"\n"
    );
}

#[test]
fn shared_scope_conflict_pauses_and_continue_applies_resolution_to_machine_homes() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    let init_a = machine_a.init();
    assert!(init_a.status.success(), "{}", render_output(&init_a));
    let init_b = machine_b.init();
    assert!(init_b.status.success(), "{}", render_output(&init_b));
    let sync_a_after_join = machine_a.run("dotsync --force");
    assert!(
        sync_a_after_join.status.success(),
        "{}",
        render_output(&sync_a_after_join)
    );

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    let commit_base = machine_a.run("dotsync commit all -m 'add base config' -- .config/app.conf");
    assert!(
        commit_base.status.success(),
        "{}",
        render_output(&commit_base)
    );

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    let commit_linux =
        machine_a.run("dotsync commit linux -m 'customize linux config' -- .config/app.conf");
    assert!(
        commit_linux.status.success(),
        "{}",
        render_output(&commit_linux)
    );

    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"linux\"\n"
    );

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "conflicting all-to-linux cascade should pause\n{}",
        render_output(&conflict)
    );
    assert_stderr_snapshot(
        &conflict,
        "\
dotsync: cascade paused

What dotsync does:
Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config.

This flow:
This commit flow was merging the scoped change through the scope DAG and reached a branch where the same file had incompatible edits.

Expected:
It expects you to edit the conflicted file in home to the merged contents you want, then run `dotsync continue` to create the merge commit and resume the cascade.

Current state found:
paused scope: linux

Why dotsync stopped:
cascade paused at scope `linux` with conflicts in .config/app.conf

Correct flow:
- edit each conflicted file at its real path in home so it holds the merged contents you want; the file has to change, because dotsync reads the resolution back out of it.
- run `dotsync continue` from the same machine to finish cascading and syncing.
- or run `dotsync abort` from the same machine to discard the paused cascade; that reverts the conflicted files in home to this machine's scope state.
- do not run another dotsync commit while the cascade is paused.
"
    );

    machine_b.write_file(".config/app.conf", "setting = \"all+linux\"\n");
    let continued = machine_b.run("dotsync continue");
    assert!(continued.status.success(), "{}", render_output(&continued));
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"all+linux\"\n"
    );

    let sync_a = machine_a.run("dotsync");
    assert!(sync_a.status.success(), "{}", render_output(&sync_a));
    assert_eq!(
        machine_a.read_file(".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "all", ".config/app.conf"),
        "setting = \"all\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "linux", ".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "goof-a", ".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "goof-b", ".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
}

/// Two machines, a base file on `all`, a `linux` override of it, then a
/// conflicting edit committed to `all` from the second machine — which leaves
/// that machine with a cascade paused at `linux` over `.config/app.conf`.
/// Returns the paused machine and the output of the run that paused.
fn pause_a_conflict_on_linux(harness: &TestHarness) -> (MachineEnvironment, Output) {
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    let init_a = machine_a.init();
    assert!(init_a.status.success(), "{}", render_output(&init_a));
    let init_b = machine_b.init();
    assert!(init_b.status.success(), "{}", render_output(&init_b));
    let sync_a_after_join = machine_a.run("dotsync --force");
    assert!(
        sync_a_after_join.status.success(),
        "{}",
        render_output(&sync_a_after_join)
    );

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    let commit_base = machine_a.run("dotsync commit all -m 'add base config' -- .config/app.conf");
    assert!(
        commit_base.status.success(),
        "{}",
        render_output(&commit_base)
    );

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    let commit_linux =
        machine_a.run("dotsync commit linux -m 'customize linux config' -- .config/app.conf");
    assert!(
        commit_linux.status.success(),
        "{}",
        render_output(&commit_linux)
    );

    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "{}",
        render_output(&conflict)
    );

    (machine_b, conflict)
}

#[test]
fn conflict_messages_agree_that_resolving_means_editing_home() {
    let harness = TestHarness::new();
    let (machine, pause) = pause_a_conflict_on_linux(&harness);

    // The pause used to say "keep the desired final contents", which reads as
    // "leaving the file alone is a valid resolution". `continue` refuses that,
    // so the pause has to ask for an edit.
    let pause_stderr = String::from_utf8_lossy(&pause.stderr).into_owned();
    assert!(
        pause_stderr.contains(
            "the file has to change, because dotsync reads the resolution back out of it"
        ),
        "the pause must say the conflicted file has to change\n{}",
        render_output(&pause)
    );

    let refusal = machine.run("dotsync continue");
    let refusal_stderr = String::from_utf8_lossy(&refusal.stderr).into_owned();
    // `dotsync abort` syncs home back to the machine scope, so telling an agent
    // to abort and then commit "the contents you want" hands it the contents
    // abort just destroyed.
    assert!(
        refusal_stderr
            .contains("reverts the conflicted files in home to this machine's scope state"),
        "the refusal must say that abort reverts home\n{}",
        render_output(&refusal)
    );
    assert!(
        refusal_stderr.contains("save them outside home"),
        "the refusal must say to save wanted contents outside home before aborting\n{}",
        render_output(&refusal)
    );
}

#[test]
fn continue_refuses_a_pause_that_predates_the_resolution_check() {
    let harness = TestHarness::new();
    let (machine, _pause) = pause_a_conflict_on_linux(&harness);

    // A machine that upgrades while a cascade is paused holds a pause file
    // written by the older binary, which recorded no pre-pause contents. The
    // resolution check has nothing to compare against there, and skipping it
    // silently reopens the data loss it exists to prevent.
    let pause_path = machine.repo_dir.join(".dotsync-paused-cascade.json");
    let mut pause_state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&pause_path).expect("read pause state"))
            .expect("pause state is JSON");
    let pause_object = pause_state
        .as_object_mut()
        .expect("pause state is an object");
    let recorded = pause_object
        .remove("paused_home_contents")
        .expect("this dotsync records pre-pause contents");
    assert!(
        recorded.as_object().is_some_and(|map| !map.is_empty()),
        "the pause should have recorded contents to remove"
    );
    fs::write(
        &pause_path,
        serde_json::to_string_pretty(&pause_state).expect("serialize pause state"),
    )
    .expect("write pause state");

    let continued = machine.run("dotsync continue");
    assert_eq!(
        continued.status.code(),
        Some(1),
        "continue must refuse a pause it cannot verify\n{}",
        render_output(&continued)
    );
    let stderr = String::from_utf8_lossy(&continued.stderr).into_owned();
    assert!(
        stderr.contains("run `dotsync abort`"),
        "the refusal must point at the way out\n{}",
        render_output(&continued)
    );

    // The way out has to actually work: abort reads nothing the old pause file
    // lacks.
    let aborted = machine.run("dotsync abort");
    assert!(aborted.status.success(), "{}", render_output(&aborted));
    assert_eq!(
        machine.read_file(".config/app.conf"),
        "setting = \"linux\"\n",
        "abort should have reverted home to this machine's scope state"
    );
}

#[test]
fn continue_refuses_a_conflicted_file_that_was_never_resolved() {
    let harness = TestHarness::new();
    let (machine_b, _pause) = pause_a_conflict_on_linux(&harness);

    // The pause tells the agent to resolve the conflicted file in home, but
    // dotsync never wrote the two conflicting versions there, so the file is
    // exactly as the agent left it. Taking that as the resolution silently
    // deletes the `linux` version, and reports success doing it.
    let continued = machine_b.run("dotsync continue");
    assert_eq!(
        continued.status.code(),
        Some(1),
        "continue must refuse an unresolved conflict\n{}",
        render_output(&continued)
    );
    assert_stderr_snapshot(
        &continued,
        "\
dotsync: conflict not resolved

What dotsync does:
Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config. Where two branches changed one file differently, the cascade pauses and asks you for the merged contents.

This flow:
This continue flow reads each conflicted file back out of your home directory and records what it finds there as the resolution.

Expected:
It expects those files to have changed since the cascade paused, because the resolution is the contents you write into them.

Current state found:
unchanged since the cascade paused at scope `linux`: .config/app.conf

Why dotsync stopped:
Dotsync does not yet write the two conflicting versions into home, so an unchanged file is not a resolution - it is only the version that happened to already be there. Recording it would silently discard the other scope's version.

Correct flow:
- read the version dotsync would discard with `dotsync view --scope linux --file .config/app.conf`, and compare it against the file in home.
- write the merged contents into the file in home, then run `dotsync continue`.
- `dotsync abort` discards the paused cascade, and reverts the conflicted files in home to this machine's scope state - so anything in home you want to keep must be saved outside home first.
- if home already holds exactly the contents you want: save them outside home, run `dotsync abort`, put them back, commit them to `linux` directly, then redo the original commit.
",
    );

    assert_eq!(
        read_bookmark_file_contents(&machine_b, "linux", ".config/app.conf"),
        "setting = \"linux\"\n",
        "the refused continue must not have discarded the linux version"
    );

    // The guard is not a wedge: a real resolution still finishes the cascade.
    machine_b.write_file(".config/app.conf", "setting = \"all+linux\"\n");
    let resolved = machine_b.run("dotsync continue");
    assert!(resolved.status.success(), "{}", render_output(&resolved));
    assert_eq!(
        read_bookmark_file_contents(&machine_b, "linux", ".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
}

#[test]
fn continue_preserves_non_conflicting_parent_changes_from_paused_merge() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    let init_a = machine_a.init();
    assert!(init_a.status.success(), "{}", render_output(&init_a));
    let init_b = machine_b.init();
    assert!(init_b.status.success(), "{}", render_output(&init_b));
    let sync_a_after_join = machine_a.run("dotsync --force");
    assert!(
        sync_a_after_join.status.success(),
        "{}",
        render_output(&sync_a_after_join)
    );

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.write_file(".config/shared.conf", "shared = \"base\"\n");
    let commit_base = machine_a
        .run("dotsync commit all -m 'add base config' -- .config/app.conf .config/shared.conf");
    assert!(
        commit_base.status.success(),
        "{}",
        render_output(&commit_base)
    );

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    let commit_linux =
        machine_a.run("dotsync commit linux -m 'customize linux config' -- .config/app.conf");
    assert!(
        commit_linux.status.success(),
        "{}",
        render_output(&commit_linux)
    );

    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"linux\"\n"
    );
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "shared = \"base\"\n"
    );

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    machine_b.write_file(".config/shared.conf", "shared = \"updated\"\n");
    let conflict = machine_b.run(
        "dotsync commit all -m 'update shared config' -- .config/app.conf .config/shared.conf",
    );
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "conflicting all-to-linux cascade should pause\n{}",
        render_output(&conflict)
    );

    machine_b.write_file(".config/app.conf", "setting = \"all+linux\"\n");
    let continued = machine_b.run("dotsync continue");
    assert!(continued.status.success(), "{}", render_output(&continued));
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "shared = \"updated\"\n"
    );

    let sync_a = machine_a.run("dotsync");
    assert!(sync_a.status.success(), "{}", render_output(&sync_a));
    assert_eq!(
        machine_a.read_file(".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
    assert_eq!(
        machine_a.read_file(".config/shared.conf"),
        "shared = \"updated\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "linux", ".config/app.conf"),
        "setting = \"all+linux\"\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine_a, "linux", ".config/shared.conf"),
        "shared = \"updated\"\n"
    );
}

#[test]
fn commit_while_cascade_paused_is_blocked_without_mutating_scope() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    let init_a = machine_a.init();
    assert!(init_a.status.success(), "{}", render_output(&init_a));
    let init_b = machine_b.init();
    assert!(init_b.status.success(), "{}", render_output(&init_b));
    let sync_a_after_join = machine_a.run("dotsync --force");
    assert!(
        sync_a_after_join.status.success(),
        "{}",
        render_output(&sync_a_after_join)
    );

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    let commit_base = machine_a.run("dotsync commit all -m 'add base config' -- .config/app.conf");
    assert!(
        commit_base.status.success(),
        "{}",
        render_output(&commit_base)
    );

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    let commit_linux =
        machine_a.run("dotsync commit linux -m 'customize linux config' -- .config/app.conf");
    assert!(
        commit_linux.status.success(),
        "{}",
        render_output(&commit_linux)
    );

    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));

    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "conflicting all-to-linux cascade should pause\n{}",
        render_output(&conflict)
    );

    let machine_scope_revision_before = bookmark_revision(&machine_b, "goof-b");
    machine_b.write_file(".config/other.conf", "other = true\n");

    let blocked =
        machine_b.run("dotsync commit goof-b -m 'try commit while paused' -- .config/other.conf");

    assert_eq!(
        blocked.status.code(),
        Some(1),
        "commit while a cascade is paused should be blocked\n{}",
        render_output(&blocked)
    );
    assert_stderr_snapshot(
        &blocked,
        "\
dotsync: paused cascade in progress

What dotsync does:
Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config.

This flow:
This commit flow was about to start a new scoped commit, but a previous cascade is still paused for conflict resolution.

Expected:
It expects exactly one cascade to be active at a time so commit history, conflict resolution, and home sync state stay aligned.

Current state found:
paused scope: linux

Why dotsync stopped:
Dotsync stopped before fetching, committing, or syncing because starting another commit would hide the real paused-cascade task and may mutate unrelated scope state.

Correct flow:
- edit each conflicted file at its real path in home so it holds the merged contents you want; the file has to change, because dotsync reads the resolution back out of it.
- run `dotsync continue` to finish the paused cascade.
- or run `dotsync abort` to discard the paused cascade; that reverts the conflicted files in home to this machine's scope state.
- after `dotsync continue` succeeds, rerun the new commit if it is still needed.
"
    );
    assert_eq!(
        bookmark_revision(&machine_b, "goof-b"),
        machine_scope_revision_before,
        "blocked commit must not mutate the target scope"
    );
}

#[test]
fn commit_to_machine_scope_does_not_cascade() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_file(".config/machine-local.txt", "machine only\n");

    let commit_output =
        machine.run("dotsync commit mx-xps-cy -m 'add machine file' -- .config/machine-local.txt");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".config/machine-local.txt"),
        "machine only\n"
    );
    assert!(!bookmark_has_file(
        &machine,
        "linux",
        ".config/machine-local.txt"
    ));
    assert!(!bookmark_has_file(
        &machine,
        "all",
        ".config/machine-local.txt"
    ));
}

#[test]
fn commit_without_paths_imports_all_diffs() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".config/app.conf",
        "setting = \"original\"\n",
    );
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_file(".config/app.conf", "setting = \"updated\"\n");

    let commit_output = machine.run("dotsync commit mx-xps-cy -m update");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".config/app.conf"),
        "setting = \"updated\"\n"
    );
}

#[test]
fn commit_noop_when_no_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", ".config/unchanged.txt", "same\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    let revision_before = bookmark_revision(&machine, "mx-xps-cy");

    let commit_output = machine.run("dotsync commit mx-xps-cy -m noop");
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
fn noop_commit_names_the_scope_it_targeted() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    // The report for a commit that found nothing was default-constructed, so
    // it did not carry the scope the agent had just named - and the message
    // interpolated the empty string into "committed  and synced".
    let commit_output = machine.run("dotsync commit mx-xps-cy -m noop");
    assert_eq!(
        commit_output.status.code(),
        Some(0),
        "{}",
        render_output(&commit_output)
    );
    assert_stderr_snapshot(
        &commit_output,
        "dotsync: committed mx-xps-cy and synced 0 file(s)\n",
    );
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

    let commit_output = machine.run("dotsync commit nonexistent -m test -- .gitconfig");
    assert_eq!(
        commit_output.status.code(),
        Some(1),
        "{}",
        render_output(&commit_output)
    );

    assert_stderr_snapshot(
        &commit_output,
        "\
dotsync: invalid scope

What dotsync does:
Dotsync stores dotfiles in a scope DAG so shared config can live on shared ancestor scopes and machine-specific config can stay isolated on leaf scopes.

This flow:
This commit flow records your repo change on the scope you name and then cascades it through descendant scopes.

Expected:
It expects the scope you name to exist in the configured scope DAG.

Current state found:
scope `nonexistent` does not exist in config

Why dotsync stopped:
Dotsync stopped because it cannot place this change onto a scope that is not configured.

Correct flow:
- choose a real configured scope from the DAG.
- Pick the root-est appropriate ancestor scope that should own the change.
"
    );
}

#[test]
fn status_before_init_matches_full_recovery_message() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let status_output = machine.run("dotsync status");
    assert_eq!(
        status_output.status.code(),
        Some(1),
        "{}",
        render_output(&status_output)
    );

    let stderr = String::from_utf8_lossy(&status_output.stderr);
    let expected = format!(
        "dotsync: not initialized

What happened:
Dotsync could not find its hidden repo at {}.

What to do:
- Run `dotsync init <remote-url>` from this home directory.
- Then rerun `dotsync status`.

The remote URL is the git remote that stores your dotsync repo.
",
        machine.repo_dir.display()
    );
    assert_eq!(stderr, expected, "{}", render_output(&status_output));
}

#[test]
fn status_before_init_json_matches_recovery_message() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let status_output = machine.run("dotsync --output json status");
    assert_eq!(
        status_output.status.code(),
        Some(1),
        "{}",
        render_output(&status_output)
    );

    let expected = r#"{"current_state":"expected repo path: {repo}; standard location: ~/.local/share/dotsync/repo","drifts":[],"error":"not_initialized","forced_overwrites":[],"message":"Dotsync could not find its hidden repo at {repo}. Run `dotsync init <remote-url>` from this home directory, then rerun `dotsync status`.","status":"error"}
"#
    .replace("{repo}", &machine.repo_dir.display().to_string());
    let stdout = String::from_utf8_lossy(&status_output.stdout);
    assert_eq!(stdout, expected, "{}", render_output(&status_output));
}

#[test]
fn init_without_remote_noninteractive_matches_full_recovery_message() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.run("dotsync init");
    assert_eq!(
        init_output.status.code(),
        Some(2),
        "{}",
        render_output(&init_output)
    );

    let stderr = String::from_utf8_lossy(&init_output.stderr);
    let expected = "dotsync: init needs the repo remote URL

Usage:
  dotsync init <remote-url>

The remote URL is the git remote that stores your dotsync repo.

Example:
  dotsync init git@github.com:maxeonyx/dotfiles.git
";
    assert_eq!(stderr, expected, "{}", render_output(&init_output));
}

#[test]
fn status_shows_modified_file() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_file(".bashrc", "export DOTSYNC=modified\n");

    let status_output = machine.run("dotsync status");
    assert_eq!(
        status_output.status.code(),
        Some(0),
        "{}",
        render_output(&status_output)
    );

    assert_stderr_snapshot(
        &status_output,
        "\
dotsync: 1 changed managed file(s) for mx-xps-cy
  M .bashrc (edited here since the last sync)
",
    );
}

#[test]
fn force_is_refused_with_one_message_wherever_it_is_meaningless() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

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
    let commit_output = machine.run("dotsync commit all -m 'add apprc' --force -- .apprc");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );
    machine.write_file(".apprc", "ui_theme = light\n");
    let commit_before = machine.run("dotsync --force commit all -m 'light theme' -- .apprc");
    assert!(
        commit_before.status.success(),
        "{}",
        render_output(&commit_before)
    );
    let sync_output = machine.run("dotsync --force");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
}

#[test]
fn output_format_is_accepted_after_the_subcommand() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    // `--force` is global and `--output` was not, so the two flags on the same
    // struct had opposite positional rules and neither one said so.
    let status_output = machine.run("dotsync status --output json");
    assert_eq!(
        status_output.status.code(),
        Some(0),
        "{}",
        render_output(&status_output)
    );

    let payload = parse_stdout_json(&status_output);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["command"], "status");
    assert_eq!(payload["machine_scope"], "mx-xps-cy");
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

#[test]
fn status_shows_deleted_file() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.delete_file(".bashrc");

    let status_output = machine.run("dotsync status");
    assert_eq!(
        status_output.status.code(),
        Some(0),
        "{}",
        render_output(&status_output)
    );

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

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    let status_output = machine.run("dotsync status");
    assert_eq!(
        status_output.status.code(),
        Some(0),
        "{}",
        render_output(&status_output)
    );

    assert_stderr_snapshot(&status_output, "dotsync: no changes for mx-xps-cy\n");
}

#[test]
fn status_json_contract() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_file(".bashrc", "export DOTSYNC=modified\n");

    let status_output = machine.run("dotsync --output json status");
    assert_eq!(
        status_output.status.code(),
        Some(0),
        "{}",
        render_output(&status_output)
    );

    let json = parse_stdout_json(&status_output);
    assert_eq!(json["status"], "ok");
    assert_eq!(json["command"], "status");
    assert_eq!(json["machine_scope"], "mx-xps-cy");

    let groups = json["groups"]
        .as_array()
        .expect("groups should be an array");
    assert!(
        !groups.is_empty(),
        "expected at least one status group\n{}",
        render_output(&status_output)
    );

    let first_group = &groups[0];
    assert_eq!(first_group["scope"], serde_json::Value::Null);

    let files = first_group["files"]
        .as_array()
        .expect("group files should be an array");
    assert!(
        files.iter().any(|file| {
            file["path"]
                .as_str()
                .is_some_and(|path| path.contains(".bashrc"))
                && file["status"] == "modified"
        }),
        "expected .bashrc modified entry\n{}",
        render_output(&status_output)
    );
}

#[test]
fn status_ignores_unmanaged_files() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_file(".unmanaged-status-test", "this file is unmanaged\n");

    let status_output = machine.run("dotsync status");
    assert_eq!(
        status_output.status.code(),
        Some(0),
        "{}",
        render_output(&status_output)
    );

    assert_stderr_snapshot(&status_output, "dotsync: no changes for mx-xps-cy\n");
}

// Issue #19: an interrupted push leaves local scope bookmarks ahead of the
// remote. That is normal VCS state — unpushed commits — and no dotsync command
// may treat it as a fetch conflict.

#[test]
fn interrupted_push_reports_that_scope_updates_were_not_pushed() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_file(".config/fish/dev-certs.fish", "set -gx DEV_CERTS 1\n");
    block_remote_pushes(&machine);
    let commit_output = machine.run(
        "dotsync --output json commit all -m 'add dev-certs helper' -- .config/fish/dev-certs.fish",
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
fn status_works_while_local_scopes_are_ahead_of_remote() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

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

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

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

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

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

#[test]
fn plain_sync_pushes_scopes_left_unpushed_by_an_interrupted_commit() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

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
fn commit_with_nothing_to_commit_still_publishes_unpushed_scopes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    interrupt_push_after_cascade(
        &machine,
        ".config/fish/dev-certs.fish",
        "set -gx DEV_CERTS 1\n",
    );

    // Committing a path that already matches the scope adds no history — but
    // the run still has to publish what the interrupted run left behind.
    let commit_output = machine
        .run("dotsync --output json commit all -m 'no change' -- .config/fish/dev-certs.fish");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

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
fn commit_pushes_scopes_left_unpushed_by_an_interrupted_commit() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

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
fn drift_stop_during_commit_does_not_strand_unpushed_history() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".gitconfig",
        "[user]\nname = \"Repo\"\n",
    );
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

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
fn continue_json_reports_unpushed_scopes() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    let init_a = machine_a.init();
    assert!(init_a.status.success(), "{}", render_output(&init_a));
    let init_b = machine_b.init();
    assert!(init_b.status.success(), "{}", render_output(&init_b));
    let sync_a_after_join = machine_a.run("dotsync --force");
    assert!(
        sync_a_after_join.status.success(),
        "{}",
        render_output(&sync_a_after_join)
    );

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    let commit_base = machine_a.run("dotsync commit all -m 'add base config' -- .config/app.conf");
    assert!(
        commit_base.status.success(),
        "{}",
        render_output(&commit_base)
    );
    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    let commit_linux =
        machine_a.run("dotsync commit linux -m 'customize linux config' -- .config/app.conf");
    assert!(
        commit_linux.status.success(),
        "{}",
        render_output(&commit_linux)
    );

    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));
    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "this test needs a paused cascade\n{}",
        render_output(&conflict)
    );

    machine_b.write_file(".config/app.conf", "setting = \"all+linux\"\n");
    block_remote_pushes(&machine_b);
    let continued = machine_b.run("dotsync --output json continue");
    allow_remote_pushes(&machine_b);
    assert!(continued.status.success(), "{}", render_output(&continued));
    assert_ne!(
        bookmark_revision(&machine_b, "all"),
        remote_branch_revision(&machine_b, "all"),
        "this test needs a push that really was rejected"
    );

    let json = parse_stdout_json(&continued);
    let unpushed = json["unpushed_scopes"]
        .as_array()
        .expect("continue should report unpushed_scopes like every other publishing command")
        .iter()
        .map(|scope| {
            scope
                .as_str()
                .expect("scope should be a string")
                .to_string()
        })
        .collect::<Vec<_>>();
    for scope in ["all", "linux"] {
        assert!(
            unpushed.contains(&scope.to_string()),
            "`{scope}` was cascaded and refused, so it must be reported unpushed: {}",
            render_output(&continued)
        );
    }
}

#[test]
fn paused_cascade_withholds_publishing_until_it_is_resolved() {
    let harness = TestHarness::new();
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    let init_a = machine_a.init();
    assert!(init_a.status.success(), "{}", render_output(&init_a));
    let init_b = machine_b.init();
    assert!(init_b.status.success(), "{}", render_output(&init_b));
    let sync_a_after_join = machine_a.run("dotsync --force");
    assert!(
        sync_a_after_join.status.success(),
        "{}",
        render_output(&sync_a_after_join)
    );

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    let commit_base = machine_a.run("dotsync commit all -m 'add base config' -- .config/app.conf");
    assert!(
        commit_base.status.success(),
        "{}",
        render_output(&commit_base)
    );
    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    let commit_linux =
        machine_a.run("dotsync commit linux -m 'customize linux config' -- .config/app.conf");
    assert!(
        commit_linux.status.success(),
        "{}",
        render_output(&commit_linux)
    );

    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));
    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "this test needs a paused cascade\n{}",
        render_output(&conflict)
    );

    // Put home back to the machine scope's content so the sync below has no
    // drift to stop on: this test is about publishing, not about drift.
    machine_b.write_file(".config/app.conf", "setting = \"linux\"\n");
    let remote_before =
        ["all", "linux", "goof-a", "goof-b"].map(|scope| remote_branch_revision(&machine_b, scope));

    let sync_output = machine_b.run("dotsync --output json");
    assert!(
        sync_output.status.success(),
        "a paused cascade must not stop dotsync from running: {}",
        render_output(&sync_output)
    );

    for (scope, before) in ["all", "linux", "goof-a", "goof-b"]
        .iter()
        .zip(remote_before)
    {
        assert_eq!(
            remote_branch_revision(&machine_b, scope),
            before,
            "a half-cascaded `{scope}` must not be published while the cascade is paused — `dotsync abort` could not take it back"
        );
    }

    let json = parse_stdout_json(&sync_output);
    let unpushed = json["unpushed_scopes"]
        .as_array()
        .expect("unpushed_scopes should be an array")
        .iter()
        .map(|scope| {
            scope
                .as_str()
                .expect("scope should be a string")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert!(
        unpushed.contains(&"all".to_string()),
        "the withheld scopes must be reported: {}",
        render_output(&sync_output)
    );

    let stderr = String::from_utf8_lossy(&sync_output.stderr);
    assert!(
        stderr.to_lowercase().contains("paused"),
        "the run must say why it did not publish: {}",
        render_output(&sync_output)
    );
    assert!(
        stderr.contains("dotsync continue") && stderr.contains("dotsync abort"),
        "the run must say how to unblock publishing: {}",
        render_output(&sync_output)
    );
}

#[test]
fn diverged_leaf_scope_keeps_the_local_commit_and_the_home_file() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

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

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

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

fn dotsync_args(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote = None;

    for character in command.chars() {
        match (quote, character) {
            (Some(active), character) if character == active => quote = None,
            (Some(_), character) => current.push(character),
            (None, '\'' | '"') => quote = Some(character),
            (None, character) if character.is_whitespace() => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            (None, character) => current.push(character),
        }
    }

    assert!(quote.is_none(), "unterminated quote in command: {command}");
    if !current.is_empty() {
        parts.push(current);
    }

    assert_eq!(parts.first().map(String::as_str), Some("dotsync"));
    parts.into_iter().skip(1).collect()
}

#[test]
fn tdd_ratchet_gatekeeper() {
    if std::env::var("TDD_RATCHET").is_err() {
        panic!("Run tdd-ratchet instead of cargo test.");
    }
}

#[test]
fn selected_add_modify_and_delete_are_applied_without_touching_unselected_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(
        &machine,
        "all",
        ".config/fish/config.fish",
        "set -g fish_greeting on\n",
    );
    seed_remote_scope_file(&machine, "all", ".config/fish/removed.fish", "remove me\n");
    merge_remote_scope_into(&machine, "all", "linux");
    merge_remote_scope_into(&machine, "linux", "mx-xps-cy");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_file(".config/fish/config.fish", "set -g fish_greeting off\n");
    machine.write_file(".config/fish/completions/git.fish", "complete -c git\n");
    machine.delete_file(".config/fish/removed.fish");

    let commit_output = machine.run("dotsync commit all -m 'update fish dir' -- .config/fish/");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

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

// --- Wave 1: the three-way drift authority -------------------------------
//
// Every test below distinguishes three sides of one managed path: what dotsync
// last synced to this machine, what is in home now, and what the machine
// scope's tip holds now. Before Wave 1 each consumer answered "what differs"
// against a different baseline, which is what made DL-1 (a machine that is
// merely behind silently reverting another machine's published work)
// representable at all.

/// Two machines on one remote, both initialised and both synced to the same
/// state. `machine_a` syncs last because `machine_b`'s init adds its own scope
/// to the shared scope graph, which reaches `machine_a`'s home config.
fn two_synced_machines(harness: &TestHarness) -> (MachineEnvironment, MachineEnvironment) {
    let machine_a = harness.machine("machine-a", "linux", "goof-a");
    let machine_b = harness.machine("machine-b", "linux", "goof-b");

    let init_a = machine_a.init();
    assert!(init_a.status.success(), "{}", render_output(&init_a));
    let init_b = machine_b.init();
    assert!(init_b.status.success(), "{}", render_output(&init_b));
    let sync_a = machine_a.run("dotsync --force");
    assert!(sync_a.status.success(), "{}", render_output(&sync_a));

    (machine_a, machine_b)
}

/// Seeds `.apprc` on `all` from `machine_a` and brings `machine_b` up to it, so
/// both machines start from one synced version of one shared file.
fn seed_shared_apprc(machine_a: &MachineEnvironment, machine_b: &MachineEnvironment) {
    machine_a.write_file(".apprc", "ui_theme = dark\nfont = mono\n");
    let commit = machine_a.run("dotsync commit all -m 'seed apprc' -- .apprc");
    assert!(commit.status.success(), "{}", render_output(&commit));

    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));
    assert_eq!(
        machine_b.read_file(".apprc"),
        "ui_theme = dark\nfont = mono\n"
    );
}

#[test]
fn a_stale_home_file_cannot_be_committed_over_another_machines_change() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    // B adds a line and publishes it. A has done nothing since the seed: its
    // home `.apprc` is not edited, it is simply behind.
    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\nsize = 14\n");
    let commit_b = machine_b.run("dotsync commit all -m 'add size' -- .apprc");
    assert!(commit_b.status.success(), "{}", render_output(&commit_b));
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".apprc"),
        "ui_theme = dark\nfont = mono\nsize = 14\n"
    );

    // The taught workflow starts with `status`, and every dotsync command
    // fetches on entry. Whatever `status` reports, the commit that follows must
    // not re-record A's older content on top of B's published change.
    let status_a = machine_a.run("dotsync status");
    assert!(status_a.status.success(), "{}", render_output(&status_a));

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
    let sync_a = machine_a.run("dotsync");
    assert!(sync_a.status.success(), "{}", render_output(&sync_a));
    assert_eq!(
        machine_a.read_file(".apprc"),
        "ui_theme = dark\nfont = mono\nsize = 14\n"
    );
}

#[test]
fn concurrent_edits_still_require_resolution_after_an_intervening_status() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".config/shared.conf", "setting = \"base\"\n");
    let commit_base =
        machine_a.run("dotsync commit all -m 'add shared base' -- .config/shared.conf");
    assert!(
        commit_base.status.success(),
        "{}",
        render_output(&commit_base)
    );
    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));

    // Two genuinely different edits to one file on one scope.
    machine_a.write_file(".config/shared.conf", "setting = \"all-a\"\n");
    machine_b.write_file(".config/shared.conf", "setting = \"all-b\"\n");
    let commit_a =
        machine_a.run("dotsync commit all -m 'update shared from a' -- .config/shared.conf");
    assert!(commit_a.status.success(), "{}", render_output(&commit_a));

    // The read-only command that the workflow tells B to run first. It must not
    // turn a genuine two-sided conflict into a silent overwrite of A's edit.
    let status_b = machine_b.run("dotsync status");
    assert!(status_b.status.success(), "{}", render_output(&status_b));

    let conflict =
        machine_b.run("dotsync commit all -m 'update shared from b' -- .config/shared.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "a two-sided conflict must still pause after an intervening read-only command\n{}",
        render_output(&conflict)
    );
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/shared.conf"),
        "setting = \"all-a\"\n",
        "machine A's published edit must still be on the remote"
    );
    assert_eq!(
        machine_b.read_file(".config/shared.conf"),
        "setting = \"all-b\"\n",
        "the paused commit must not overwrite B's unresolved home edit"
    );

    machine_b.write_file(".config/shared.conf", "setting = \"all-a+all-b\"\n");
    let continued = machine_b.run("dotsync continue");
    assert!(continued.status.success(), "{}", render_output(&continued));
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/shared.conf"),
        "setting = \"all-a+all-b\"\n"
    );
}

#[test]
fn a_remote_advance_is_reported_as_incoming_by_status_diff_and_sync() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    machine_b.write_file(".apprc", "ui_theme = light\nfont = mono\n");
    let commit_b = machine_b.run("dotsync commit all -m 'light theme' -- .apprc");
    assert!(commit_b.status.success(), "{}", render_output(&commit_b));

    // A is merely behind: nothing in its home changed. `status`, `diff` and
    // plain `dotsync` have to agree about that.
    let status_a = machine_a.run("dotsync status");
    assert!(status_a.status.success(), "{}", render_output(&status_a));
    assert_stderr_snapshot(
        &status_a,
        "\
dotsync: 1 incoming file(s) for goof-a — plain `dotsync` applies these
  U .apprc (changed on another machine, and not edited here)
",
    );

    let status_json = machine_a.run("dotsync --output json status");
    assert!(
        status_json.status.success(),
        "{}",
        render_output(&status_json)
    );
    let json = parse_stdout_json(&status_json);
    assert_eq!(
        json["changed_count"],
        0,
        "a file this machine has not changed is not a change\n{}",
        render_output(&status_json)
    );
    assert_eq!(json["incoming_count"], 1);

    let diff_a = machine_a.run("dotsync diff");
    assert_eq!(
        diff_a.status.code(),
        Some(0),
        "a routine remote advance is not drift\n{}",
        render_output(&diff_a)
    );
    assert_stderr_snapshot(&diff_a, "dotsync: no changes for goof-a\n");

    let sync_a = machine_a.run("dotsync");
    assert!(sync_a.status.success(), "{}", render_output(&sync_a));
    assert_eq!(
        machine_a.read_file(".apprc"),
        "ui_theme = light\nfont = mono\n"
    );
}

#[test]
fn an_untracked_home_file_is_not_overwritten_by_an_incoming_add() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    // Real content in A's home that dotsync has never seen.
    machine_a.write_file(".newfile", "mine\n");

    machine_b.write_file(".newfile", "theirs\n");
    let commit_b = machine_b.run("dotsync commit all -m 'add newfile' -- .newfile");
    assert!(commit_b.status.success(), "{}", render_output(&commit_b));

    let sync_a = machine_a.run("dotsync");
    assert_eq!(
        sync_a.status.code(),
        Some(1),
        "an incoming add that collides with untracked home content is drift\n{}",
        render_output(&sync_a)
    );
    assert_eq!(
        machine_a.read_file(".newfile"),
        "mine\n",
        "dotsync must not silently overwrite home content it has never seen"
    );

    let forced = machine_a.run("dotsync --force");
    assert!(forced.status.success(), "{}", render_output(&forced));
    assert_eq!(machine_a.read_file(".newfile"), "theirs\n");
}

#[test]
fn deleting_a_managed_file_blocks_sync_and_is_committable() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.delete_file(".bashrc");

    let diff_output = machine.run("dotsync diff");
    assert_eq!(
        diff_output.status.code(),
        Some(1),
        "a deleted managed file is drift, and `diff` must show it\n{}",
        render_output(&diff_output)
    );
    let diff_stderr = String::from_utf8_lossy(&diff_output.stderr).into_owned();
    assert!(
        diff_stderr.contains(".bashrc") && diff_stderr.contains("-export DOTSYNC=repo"),
        "`diff` must render what the deletion would discard\n{diff_stderr}"
    );

    let sync_output = machine.run("dotsync");
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "deletion drift blocks sync like an edit\n{}",
        render_output(&sync_output)
    );
    assert!(
        !machine.file_exists(".bashrc"),
        "the blocked sync must not quietly restore the deleted file"
    );

    let commit_output = machine.run("dotsync commit mx-xps-cy -m 'drop bashrc' -- .bashrc");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );
    assert!(!bookmark_has_file(&machine, "mx-xps-cy", ".bashrc"));
    assert!(!machine.file_exists(".bashrc"));

    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
}

#[test]
fn a_sync_interrupted_before_saving_state_is_not_drift_on_the_next_run() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", ".apprc", "version = 1\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    let state_before = machine.read_sync_state_raw();

    seed_remote_scope_file(&machine, "mx-xps-cy", ".apprc", "version = 2\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert_eq!(machine.read_file(".apprc"), "version = 2\n");

    // A run that dies between finishing the home writes and saving sync state
    // leaves exactly this: home already holds the new bytes, and the state file
    // still points at the revision before them. The rerun must converge, not
    // report the bytes it just wrote as local drift.
    machine.write_sync_state_raw(&state_before);

    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "an interrupted sync must converge on rerun\n{}",
        render_output(&sync_output)
    );
    assert_eq!(machine.read_file(".apprc"), "version = 2\n");
}

#[test]
fn abort_restores_a_drifted_file_outside_the_paused_selection() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(".config/app.conf", "setting = \"base\"\n");
    machine_a.write_file(".config/unrelated.conf", "unrelated = \"base\"\n");
    let commit_base = machine_a
        .run("dotsync commit all -m 'add base config' -- .config/app.conf .config/unrelated.conf");
    assert!(
        commit_base.status.success(),
        "{}",
        render_output(&commit_base)
    );

    machine_a.write_file(".config/app.conf", "setting = \"linux\"\n");
    let commit_linux =
        machine_a.run("dotsync commit linux -m 'customize linux config' -- .config/app.conf");
    assert!(
        commit_linux.status.success(),
        "{}",
        render_output(&commit_linux)
    );

    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));

    // Drift on a file the paused commit never named.
    machine_b.write_file(".config/unrelated.conf", "unrelated = \"drifted\"\n");
    machine_b.write_file(".config/app.conf", "setting = \"all\"\n");
    let conflict =
        machine_b.run("dotsync commit all -m 'update shared config' -- .config/app.conf");
    assert_eq!(
        conflict.status.code(),
        Some(3),
        "conflicting all-to-linux cascade should pause\n{}",
        render_output(&conflict)
    );

    let aborted = machine_b.run("dotsync abort");
    assert!(
        aborted.status.success(),
        "abort reverts all the config files, so unrelated drift cannot block it\n{}",
        render_output(&aborted)
    );
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        "setting = \"linux\"\n"
    );
    assert_eq!(
        machine_b.read_file(".config/unrelated.conf"),
        "unrelated = \"base\"\n",
        "abort is a full sync of home, not a selective restore"
    );
}

#[test]
fn commit_force_applies_to_the_named_paths_and_not_to_unrelated_drift() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", ".gitconfig", "[user]\nname = Repo\n");
    seed_remote_scope_file(&machine, "mx-xps-cy", ".config/app.conf", "setting = one\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_file(".gitconfig", "[user]\nname = Drifted\n");
    machine.write_file(".config/app.conf", "setting = two\n");

    let commit_output =
        machine.run("dotsync commit mx-xps-cy -m 'update app' --force -- .config/app.conf");
    assert_eq!(
        commit_output.status.code(),
        Some(1),
        "`--force` on commit covers the paths it names, so unrelated drift still stops the home sync\n{}",
        render_output(&commit_output)
    );
    assert_eq!(
        machine.read_file(".gitconfig"),
        "[user]\nname = Drifted\n",
        "`--force` on a commit must not revert a file the commit never named"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".config/app.conf"),
        "setting = two\n",
        "the named change is still recorded"
    );
}

#[test]
fn forcing_a_stale_commit_records_what_it_overwrote() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\nsize = 14\n");
    let commit_b = machine_b.run("dotsync commit all -m 'add size' -- .apprc");
    assert!(commit_b.status.success(), "{}", render_output(&commit_b));

    let status_a = machine_a.run("dotsync status");
    assert!(status_a.status.success(), "{}", render_output(&status_a));

    let commit_a =
        machine_a.run("dotsync --output json commit all -m 'revert on purpose' --force -- .apprc");
    assert!(commit_a.status.success(), "{}", render_output(&commit_a));

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
fn diff_reports_an_inserted_line_as_a_single_addition() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(
        &machine,
        "mx-xps-cy",
        ".config/app.conf",
        "one\ntwo\nthree\n",
    );
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.write_file(".config/app.conf", "one\ninserted\ntwo\nthree\n");

    let diff_output = machine.run("dotsync diff");
    assert_eq!(
        diff_output.status.code(),
        Some(1),
        "{}",
        render_output(&diff_output)
    );
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

#[test]
fn sync_does_not_rewrite_files_that_already_match() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", ".apprc", "ui_theme = dark\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    let written_at = machine.modified_time(".apprc");

    std::thread::sleep(std::time::Duration::from_millis(20));
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    assert_eq!(
        machine.modified_time(".apprc"),
        written_at,
        "a sync that changes nothing must not rewrite the file"
    );
}

#[test]
fn init_reports_no_drift() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    // A machine with no sync state has no record of putting anything in home,
    // so it cannot claim a file missing from home was deleted there. On a fresh
    // init that is every file the scope holds.
    let init_output = machine.init();
    assert_stderr_snapshot(
        &init_output,
        "dotsync: initialized mx-xps-cy and synced 1 file(s)\n",
    );
}

#[test]
fn a_machine_that_lost_its_sync_state_still_syncs() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=repo\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    machine.delete_sync_state();
    machine.delete_file(".bashrc");

    // Deleting a managed file from home is drift only when dotsync can show it
    // put the file there. Without sync state it cannot, so this is an ordinary
    // incoming file and the machine converges rather than needing `--force`.
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "a machine with no sync state must still be able to sync\n{}",
        render_output(&sync_output)
    );
    assert_eq!(machine.read_file(".bashrc"), "export DOTSYNC=repo\n");
}

#[test]
fn a_concurrent_merge_leaves_no_drift_behind() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);

    machine_a.write_file(
        ".config/app.conf",
        "alpha = 1\nbeta = 2\ngamma = 3\ndelta = 4\nepsilon = 5\n",
    );
    let seed = machine_a.run("dotsync commit all -m 'seed app.conf' -- .config/app.conf");
    assert!(seed.status.success(), "{}", render_output(&seed));
    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));

    // Line-disjoint edits, so the merge succeeds. B is behind when it commits.
    machine_a.write_file(
        ".config/app.conf",
        "alpha = 100\nbeta = 2\ngamma = 3\ndelta = 4\nepsilon = 5\n",
    );
    let commit_a = machine_a.run("dotsync commit all -m 'a changes alpha' -- .config/app.conf");
    assert!(commit_a.status.success(), "{}", render_output(&commit_a));

    machine_b.write_file(
        ".config/app.conf",
        "alpha = 1\nbeta = 2\ngamma = 3\ndelta = 4\nepsilon = 500\n",
    );
    let commit_b = machine_b.run("dotsync commit all -m 'b changes epsilon' -- .config/app.conf");
    assert!(
        commit_b.status.success(),
        "a commit whose merge succeeded must not then stop on its own result\n{}",
        render_output(&commit_b)
    );

    // Home holds only B's side until the commit's own sync writes the merge
    // down. That sync is the one step that can do it, so it has to run.
    let merged = "alpha = 100\nbeta = 2\ngamma = 3\ndelta = 4\nepsilon = 500\n";
    assert_eq!(
        machine_b.read_file(".config/app.conf"),
        merged,
        "the merge dotsync just pushed has to reach the home it came from"
    );
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".config/app.conf"),
        merged
    );

    let status_b = machine_b.run("dotsync status");
    assert_stderr_snapshot(&status_b, "dotsync: no changes for goof-b\n");
}

#[test]
fn a_machine_with_no_sync_record_says_so_instead_of_guessing() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=v1\n");
    let sync_output = machine.run("dotsync");
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );

    // Home holds exactly what dotsync wrote there. Losing the state file does
    // not change that — it only means dotsync can no longer prove it.
    machine.delete_sync_state();
    seed_remote_scope_file(&machine, "mx-xps-cy", ".bashrc", "export DOTSYNC=v2\n");

    let sync_output = machine.run("dotsync");
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "without a record dotsync cannot tell an edit here from an incoming change, so it must stop\n{}",
        render_output(&sync_output)
    );

    let stderr = String::from_utf8_lossy(&sync_output.stderr).into_owned();
    assert!(
        !stderr.contains("never synced here"),
        "the file was synced here; the diagnosis must not claim otherwise\n{stderr}"
    );
    assert!(
        !stderr.contains("just added"),
        "the repo changed this file rather than adding it\n{stderr}"
    );
    assert!(
        stderr.contains("no sync record"),
        "the diagnosis must name the actual problem: the record is gone\n{stderr}"
    );

    // Both ways out still work.
    let forced = machine.run("dotsync --force");
    assert!(forced.status.success(), "{}", render_output(&forced));
    assert_eq!(machine.read_file(".bashrc"), "export DOTSYNC=v2\n");
}

#[test]
fn committing_a_path_another_machine_deleted_is_refused() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    // B removes the file and publishes the removal. A has not synced since.
    machine_b.delete_file(".apprc");
    let commit_b = machine_b.run("dotsync commit all -m 'drop apprc' -- .apprc");
    assert!(commit_b.status.success(), "{}", render_output(&commit_b));

    let status_a = machine_a.run("dotsync status");
    assert!(status_a.status.success(), "{}", render_output(&status_a));

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
    let sync_a = machine_a.run("dotsync");
    assert!(sync_a.status.success(), "{}", render_output(&sync_a));
    assert!(!machine_a.file_exists(".apprc"));

    // Putting it back on purpose is the other, and it says so in the JSON.
    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\n");
    let restore = machine_b.run("dotsync --output json commit all -m 'put it back' -- .apprc");
    assert!(restore.status.success(), "{}", render_output(&restore));
    assert_eq!(
        remote_branch_file_contents(&machine_b, "all", ".apprc"),
        "ui_theme = dark\nfont = mono\n"
    );
}

#[test]
fn a_forced_overwrite_is_reported_even_when_the_run_then_fails() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    machine_a.write_file(".config/other.conf", "other = base\n");
    let seed_other = machine_a.run("dotsync commit all -m 'add other' -- .config/other.conf");
    assert!(
        seed_other.status.success(),
        "{}",
        render_output(&seed_other)
    );
    let sync_b = machine_b.run("dotsync");
    assert!(sync_b.status.success(), "{}", render_output(&sync_b));

    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\nsize = 14\n");
    let commit_b = machine_b.run("dotsync commit all -m 'add size' -- .apprc");
    assert!(commit_b.status.success(), "{}", render_output(&commit_b));

    // A forces the revert of `.apprc`, and separately has drift on a file the
    // commit does not name — so the commit's own home sync stops after the
    // forced history has already been written and pushed.
    let status_a = machine_a.run("dotsync status");
    assert!(status_a.status.success(), "{}", render_output(&status_a));
    machine_a.write_file(".config/other.conf", "other = drifted\n");

    let commit_a =
        machine_a.run("dotsync --output json commit all -m 'revert apprc' --force -- .apprc");
    assert_eq!(
        commit_a.status.code(),
        Some(1),
        "unrelated drift still stops the home sync\n{}",
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

#[test]
fn a_successful_forced_commit_says_what_it_overwrote() {
    let harness = TestHarness::new();
    let (machine_a, machine_b) = two_synced_machines(&harness);
    seed_shared_apprc(&machine_a, &machine_b);

    machine_b.write_file(".apprc", "ui_theme = dark\nfont = mono\nsize = 14\n");
    let commit_b = machine_b.run("dotsync commit all -m 'add size' -- .apprc");
    assert!(commit_b.status.success(), "{}", render_output(&commit_b));

    let status_a = machine_a.run("dotsync status");
    assert!(status_a.status.success(), "{}", render_output(&status_a));

    // Succeeding is not a reason to stay quiet. A run that reverted another
    // machine's published change has to say so on the way past, exactly as it
    // does when it goes on to fail — the successful one is the commoner case.
    let commit_a = machine_a.run("dotsync commit all -m 'revert on purpose' --force -- .apprc");
    assert!(commit_a.status.success(), "{}", render_output(&commit_a));
    assert_stderr_snapshot(
        &commit_a,
        "\
dotsync: recorded 1 file(s) over an incoming change, because you passed `--force`
- .apprc
dotsync: committed all and synced 2 file(s)
",
    );
}
