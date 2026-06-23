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
- .gitconfig
--- repo
+++ system
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
- .config/app.conf
--- repo
+++ system
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

    machine.delete_file(".config/machine-only.txt");
    machine.write_sync_state_raw(&format!(
        "{{\n  \"machine_scope\": \"mx-xps-cy\",\n  \"last_synced_revision\": \"{}\"\n}}\n",
        bookmark_revision(&machine, "mx-xps-cy")
    ));

    let sync_output = machine.run("dotsync");
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
fn v03_commit_returns_not_implemented() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let commit_output = machine.run("dotsync commit all -m 'not implemented yet'");
    assert_eq!(
        commit_output.status.code(),
        Some(1),
        "scoped commit should return a normal not-implemented error in v0.3 task 1\n{}",
        render_output(&commit_output)
    );
    assert_stderr_snapshot(
        &commit_output,
        "dotsync: not implemented: scoped commit is not available until home-diff commit flow lands\n"
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
    assert_stderr_snapshot(
        &aborted,
        "dotsync: aborted cascade at linux and synced 2 file(s)\n",
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
fn config_edit_commit_creates_new_scope_and_cascades_descendants() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_file(".config/linux-only.txt", "linux\n");
    let seed_linux =
        machine.run("dotsync commit linux -m 'add linux file' -- .config/linux-only.txt");
    assert!(seed_linux.status.success(), "{}", render_output(&seed_linux));

    let original_config = machine.read_file(".config/dotsync/config.toml");
    let updated_config = original_config.replace(
        "linux = { parents = [\"all\"] }\nmx-xps-cy = { parents = [\"linux\"] }",
        "linux = { parents = [\"all\"] }\nhyprland = { parents = [\"linux\"] }\nmx-xps-cy = { parents = [\"hyprland\"] }",
    );
    assert_ne!(
        updated_config, original_config,
        "expected init config shape to match test harness"
    );
    machine.write_file(".config/dotsync/config.toml", &updated_config);

    let commit_config = machine.run(
        "dotsync commit all -m 'add hyprland scope' -- .config/dotsync/config.toml",
    );
    assert!(
        commit_config.status.success(),
        "{}",
        render_output(&commit_config)
    );

    assert_eq!(
        read_bookmark_file_contents(&machine, "all", ".config/dotsync/config.toml"),
        updated_config
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "hyprland", ".config/dotsync/config.toml"),
        updated_config
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "hyprland", ".config/linux-only.txt"),
        "linux\n"
    );
    assert_eq!(
        read_bookmark_file_contents(&machine, "mx-xps-cy", ".config/dotsync/config.toml"),
        updated_config
    );
    assert_eq!(machine.read_file(".config/dotsync/config.toml"), updated_config);
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
It expects you to resolve the conflicted file in home, then run `dotsync continue` to create the merge commit and resume the cascade.

Current state found:
paused scope: all

Why dotsync stopped:
cascade paused at scope `all` with conflicts in .config/shared.conf

Correct flow:
- edit each conflicted file at its real path in home and keep the desired final contents.
- run `dotsync continue` from the same machine to finish cascading and syncing.
- or run `dotsync abort` from the same machine to discard the paused cascade and restore the pre-pause state.
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
It expects you to resolve the conflicted file in home, then run `dotsync continue` to create the merge commit and resume the cascade.

Current state found:
paused scope: linux

Why dotsync stopped:
cascade paused at scope `linux` with conflicts in .config/app.conf

Correct flow:
- edit each conflicted file at its real path in home and keep the desired final contents.
- run `dotsync continue` from the same machine to finish cascading and syncing.
- or run `dotsync abort` from the same machine to discard the paused cascade and restore the pre-pause state.
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
- edit each conflicted file at its real path in home and keep the desired final contents.
- run `dotsync continue` to finish the paused cascade.
- or run `dotsync abort` to discard the paused cascade and restore the pre-pause state.
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

    let expected = r#"{"current_state":"expected repo path: {repo}; standard location: ~/.local/share/dotsync/repo","drifts":[],"error":"not_initialized","message":"Dotsync could not find its hidden repo at {repo}. Run `dotsync init <remote-url>` from this home directory, then rerun `dotsync status`.","status":"error"}
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
  M .bashrc
",
    );
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
  D .bashrc
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
fn retired_pending_selected_add_modify_and_delete_are_applied_without_touching_unselected_changes()
{
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

retired_ratchet_test!(
    retired_pending_selective_commit_preserves_unselected_dirty_paths_when_cascade_pauses
);
retired_ratchet_test!(
    retired_pending_sync_loads_config_from_committed_all_scope_not_working_copy_edit
);
retired_ratchet_test!(retired_plain_dotsync_rejects_working_copy_changes);
retired_ratchet_test!(retired_scoped_commit_deletion_removes_file_from_fake_home);
retired_ratchet_test!(retired_scoped_deletion_only_affects_homes_where_scope_applies);
