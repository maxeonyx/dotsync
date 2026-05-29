use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use jj_lib::backend::TreeValue;
use jj_lib::config::StackedConfig;
use jj_lib::merge::Merge;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::{Repo as _, StoreFactories};
use jj_lib::repo_path::RepoPath;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_lib::workspace::{default_working_copy_factories, Workspace};
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

    fn sync_json(&self) -> Output {
        self.run_dotsync_json(&[])
    }

    fn commit(&self, scope: &str, message: &str) -> Output {
        self.run_dotsync(&[scope, "-m", message])
    }

    fn commit_all(&self, scope: &str, message: &str) -> Output {
        self.run_dotsync(&[scope, "--all", "-m", message])
    }

    fn commit_paths(&self, scope: &str, message: &str, paths: &[&str]) -> Output {
        let mut args = vec![scope, "-m", message];
        args.extend_from_slice(paths);
        self.run_dotsync(&args)
    }

    fn commit_json(&self, scope: &str, message: &str, paths: &[&str]) -> Output {
        let mut args = vec![scope, "-m", message];
        args.extend_from_slice(paths);
        self.run_dotsync_json(&args)
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

    fn write_repo_file(&self, relative: &str, contents: &str) {
        self.write_file(self.repo_dir.join(relative), contents);
    }

    fn delete_repo_file(&self, relative: &str) {
        fs::remove_file(self.repo_dir.join(relative)).expect("delete repo file");
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

    fn read_repo_file(&self, relative: &str) -> String {
        fs::read_to_string(self.repo_dir.join(relative)).expect("read repo file")
    }

    fn home_file_exists(&self, relative: &str) -> bool {
        self.home_dir.join(relative).exists()
    }

    fn sync_state_relative_path(&self) -> PathBuf {
        let config = fs::read_to_string(self.repo_dir.join(".config/dotsync/config.toml"))
            .expect("read dotsync config");
        let value: toml::Value = toml::from_str(&config).expect("parse dotsync config");
        PathBuf::from(
            value["sync"]["state_path"]
                .as_str()
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

    fn set_checkout_scope(&self, scope: &str) {
        let mut workspace = load_workspace(&self.repo_dir);
        let repo = load_repo(&workspace);
        let commit_id = repo
            .view()
            .get_local_bookmark(RefNameBuf::from(scope).as_ref())
            .as_normal()
            .cloned()
            .unwrap_or_else(|| panic!("missing bookmark `{scope}`"));
        let commit = repo
            .store()
            .get_commit(&commit_id)
            .unwrap_or_else(|err| panic!("load commit for `{scope}`: {err}"));

        let repo = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime")
            .block_on(async {
                let mut tx = repo.start_transaction();
                tx.repo_mut()
                    .set_wc_commit(workspace.workspace_name().to_owned(), commit.id().clone())
                    .expect("set working copy commit");
                tx.commit(format!("test: checkout {scope}"))
                    .await
                    .expect("commit working copy update")
            });

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime")
            .block_on(workspace.check_out(repo.op_id().clone(), None, &commit))
            .expect("check out scope");
    }

    fn current_bookmarks(&self) -> Vec<String> {
        let workspace = load_workspace(&self.repo_dir);
        let repo = load_repo(&workspace);
        let wc_commit_id = repo
            .view()
            .get_wc_commit_id(workspace.workspace_name())
            .expect("working copy commit should exist")
            .clone();

        let mut bookmarks = repo
            .view()
            .local_bookmarks_for_commit(&wc_commit_id)
            .map(|(name, _)| name.as_str().to_string())
            .collect::<Vec<_>>();
        bookmarks.sort();
        bookmarks
    }

    fn bookmark_file_contents(&self, scope: &str, relative: &str) -> String {
        let workspace = load_workspace(&self.repo_dir);
        let repo = load_repo(&workspace);
        let commit_id = repo
            .view()
            .get_local_bookmark(RefNameBuf::from(scope).as_ref())
            .as_normal()
            .cloned()
            .unwrap_or_else(|| panic!("missing bookmark `{scope}`"));
        let commit = repo
            .store()
            .get_commit(&commit_id)
            .unwrap_or_else(|err| panic!("load bookmark commit `{scope}`: {err}"));
        let path = RepoPath::from_internal_string(relative)
            .unwrap_or_else(|err| panic!("invalid repo path `{relative}`: {err}"));

        let value = commit
            .tree()
            .path_value(path)
            .unwrap_or_else(|err| panic!("read `{relative}` from `{scope}` tree: {err}"));
        let TreeValue::File { id, .. } = value
            .into_resolved()
            .unwrap_or_else(|conflict| {
                panic!(
                    "expected resolved file value for `{relative}` on `{scope}`, found conflict: {conflict:?}"
                )
            })
            .unwrap_or_else(|| panic!("expected file at `{relative}` on `{scope}`"))
        else {
            panic!("expected file at `{relative}` on `{scope}`")
        };

        let mut reader = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime")
            .block_on(commit.store().read_file(path, &id))
            .unwrap_or_else(|err| {
                panic!("read file contents for `{relative}` on `{scope}`: {err}")
            });
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

    fn bookmark_revision(&self, scope: &str) -> String {
        let workspace = load_workspace(&self.repo_dir);
        let repo = load_repo(&workspace);
        repo.view()
            .get_local_bookmark(RefNameBuf::from(scope).as_ref())
            .as_normal()
            .unwrap_or_else(|| panic!("missing bookmark `{scope}`"))
            .hex()
    }
}

fn load_workspace(repo_dir: &Path) -> Workspace {
    let settings = UserSettings::from_config(StackedConfig::with_defaults())
        .expect("load jj settings for test assertions");
    Workspace::load(
        &settings,
        repo_dir,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .expect("load workspace")
}

fn load_repo(workspace: &Workspace) -> std::sync::Arc<jj_lib::repo::ReadonlyRepo> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(workspace.repo_loader().load_at_head())
        .expect("load repo at head")
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

fn clone_remote_to(path: &Path, remote_dir: &Path) {
    let output = Command::new("git")
        .args(["clone", "--branch", "all", "--single-branch"])
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

fn git_in(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("run git {:?}: {err}", args))
}

fn git_rev_parse(dir: &Path, rev: &str) -> String {
    let output = git_in(dir, &["rev-parse", rev]);
    assert!(output.status.success(), "{}", render_output(&output));
    String::from_utf8(output.stdout)
        .expect("rev-parse stdout should be utf-8")
        .trim()
        .to_string()
}

fn advance_local_bookmark_without_push(
    machine: &MachineEnvironment,
    scope: &str,
    relative: &str,
    contents: &str,
) {
    let mut workspace = load_workspace(&machine.repo_dir);
    let repo = load_repo(&workspace);
    let bookmark_name = RefNameBuf::from(scope);
    let bookmark_id = repo
        .view()
        .get_local_bookmark(bookmark_name.as_ref())
        .as_normal()
        .cloned()
        .unwrap_or_else(|| panic!("missing bookmark `{scope}`"));
    let bookmark_commit = repo
        .store()
        .get_commit(&bookmark_id)
        .unwrap_or_else(|err| panic!("load bookmark commit `{scope}`: {err}"));
    let path = RepoPathBuf::from_internal_string(relative)
        .unwrap_or_else(|err| panic!("invalid repo path `{relative}`: {err}"));

    let new_tree = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(async {
            let mut reader = contents.as_bytes();
            let file_id = repo
                .store()
                .write_file(path.as_ref(), &mut reader)
                .await
                .unwrap_or_else(|err| panic!("write file for local bookmark advance: {err}"));
            let mut builder = MergedTreeBuilder::new(bookmark_commit.tree());
            builder.set_or_remove(
                path.clone(),
                Merge::normal(TreeValue::File {
                    id: file_id,
                    executable: false,
                    copy_id: jj_lib::backend::CopyId::placeholder(),
                }),
            );
            builder
                .write_tree()
                .await
                .unwrap_or_else(|err| panic!("write local bookmark tree: {err}"))
        });

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(async {
            let mut tx = repo.start_transaction();
            let new_commit = tx
                .repo_mut()
                .new_commit(vec![bookmark_commit.id().clone()], new_tree)
                .set_description(format!("test: unpublished {scope} advance"))
                .write()
                .await
                .unwrap_or_else(|err| panic!("write unpublished local commit `{scope}`: {err}"));
            tx.repo_mut().set_local_bookmark_target(
                bookmark_name.as_ref(),
                jj_lib::op_store::RefTarget::normal(new_commit.id().clone()),
            );
            tx.repo_mut()
                .set_wc_commit(
                    workspace.workspace_name().to_owned(),
                    new_commit.id().clone(),
                )
                .expect("set working copy to unpublished commit");
            let repo = tx
                .commit(format!("test: advance local bookmark {scope} without push"))
                .await
                .unwrap_or_else(|err| {
                    panic!("commit unpublished local bookmark advance `{scope}`: {err}")
                });
            workspace
                .check_out(repo.op_id().clone(), None, &new_commit)
                .await
                .unwrap_or_else(|err| {
                    panic!("check out unpublished local commit `{scope}`: {err}")
                });
        });
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
fn plain_dotsync_rejects_working_copy_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let sync_output = machine.sync();
    assert!(
        !sync_output.status.success(),
        "plain dotsync should reject working-copy changes before syncing\n{}",
        render_output(&sync_output)
    );
    let stderr = String::from_utf8_lossy(&sync_output.stderr);
    assert!(
        stderr.contains("dotsync <scope> -m")
            || stderr.contains("requires a scope")
            || stderr.contains("use `dotsync")
            || stderr.contains("use dotsync"),
        "plain dotsync should direct the user toward the scoped commit workflow\n{}",
        render_output(&sync_output)
    );
    assert!(
        !machine.home_file_exists(".gitconfig"),
        "plain dotsync should not apply dirty working-copy changes to fake home\n{}",
        render_output(&sync_output)
    );
}

#[test]
fn pending_scoped_commit_requires_paths_or_all_in_human_and_json_modes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let human_output = machine.commit("all", "missing selection");
    assert_eq!(
        human_output.status.code(),
        Some(2),
        "{}",
        render_output(&human_output)
    );
    let human_stderr = String::from_utf8_lossy(&human_output.stderr);
    assert!(
        human_stderr.contains("requires explicit file/directory paths or --all"),
        "{}",
        render_output(&human_output)
    );

    let json_output = machine.commit_json("all", "missing selection", &[]);
    assert_eq!(
        json_output.status.code(),
        Some(2),
        "{}",
        render_output(&json_output)
    );
    let json = parse_stdout_json(&json_output);
    assert_eq!(json["status"], "error");
    assert_eq!(json["error"], "usage");
    assert_eq!(
        json["message"],
        "commit mode requires explicit file/directory paths or --all"
    );
}

#[test]
fn pending_commit_mode_rejects_all_plus_paths() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let output =
        machine.run_dotsync_json(&["all", "--all", "-m", "conflicting selection", ".gitconfig"]);
    assert_eq!(output.status.code(), Some(2), "{}", render_output(&output));
    let json = parse_stdout_json(&output);
    assert_eq!(json["status"], "error");
    assert_eq!(json["error"], "usage");
    assert_eq!(
        json["message"],
        "commit mode accepts explicit paths or --all, not both"
    );
}

#[test]
fn pending_explicit_path_commit_only_commits_selected_paths_and_leaves_other_changes_dirty() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");
    machine.write_repo_file(".config/fish/config.fish", "set -g fish_greeting hi\n");

    let commit_output = machine.commit_paths("all", "commit only gitconfig", &[".gitconfig"]);
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert_eq!(
        machine.read_home_file(".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
    assert!(
        !machine.home_file_exists(".config/fish/config.fish"),
        "unselected path should stay unsynced\n{}",
        render_output(&commit_output)
    );

    let sync_output = machine.sync();
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "{}",
        render_output(&sync_output)
    );
    let stderr = String::from_utf8_lossy(&sync_output.stderr);
    assert!(
        stderr.contains("working copy has uncommitted changes"),
        "{}",
        render_output(&sync_output)
    );
    assert_eq!(
        machine.read_repo_file(".config/fish/config.fish"),
        "set -g fish_greeting hi\n"
    );
}

#[test]
fn pending_commit_all_preserves_whole_tree_commit_behavior() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");
    machine.write_repo_file(".config/fish/config.fish", "set -g fish_greeting hi\n");

    let commit_output = machine.commit_all("all", "commit whole tree");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );
    assert_eq!(
        machine.read_home_file(".gitconfig"),
        "[user]\nname = \"Max\"\n"
    );
    assert_eq!(
        machine.read_home_file(".config/fish/config.fish"),
        "set -g fish_greeting hi\n"
    );
}

#[test]
fn pending_selected_add_modify_and_delete_are_applied_without_touching_unselected_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");
    machine.write_repo_file(".config/fish/config.fish", "set -g fish_greeting hi\n");
    let seed_output = machine.commit_all("all", "seed files");
    assert!(
        seed_output.status.success(),
        "{}",
        render_output(&seed_output)
    );

    machine.write_repo_file(
        ".gitconfig",
        "[user]\nname = \"Max\"\nemail = \"max@example.com\"\n",
    );
    machine.delete_repo_file(".config/fish/config.fish");
    machine.write_repo_file(".config/starship.toml", "command_timeout = 500\n");
    machine.write_repo_file(
        ".config/alacritty/alacritty.toml",
        "[window]\nopacity = 0.9\n",
    );

    let commit_output = machine.commit_paths(
        "all",
        "apply selected changes",
        &[
            ".gitconfig",
            ".config/fish/config.fish",
            ".config/starship.toml",
        ],
    );
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    assert_eq!(
        machine.read_home_file(".gitconfig"),
        "[user]\nname = \"Max\"\nemail = \"max@example.com\"\n"
    );
    assert!(!machine.home_file_exists(".config/fish/config.fish"));
    assert_eq!(
        machine.read_home_file(".config/starship.toml"),
        "command_timeout = 500\n"
    );
    assert!(!machine.home_file_exists(".config/alacritty/alacritty.toml"));

    let sync_output = machine.sync();
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "{}",
        render_output(&sync_output)
    );
    assert_eq!(
        machine.read_repo_file(".config/alacritty/alacritty.toml"),
        "[window]\nopacity = 0.9\n"
    );
}

#[test]
fn pending_config_path_is_rejected_for_non_all_scope_commits() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(
        ".config/dotsync/config.toml",
        "[scopes]\nall = {}\nlinux = { parents = [\"all\"] }\nmx-xps-cy = { parents = [\"linux\"] }\n\n[sync]\nstate_path = \".config/dotsync/other-state.json\"\n",
    );

    let commit_output = machine.commit_paths(
        "linux",
        "bad config commit",
        &[".config/dotsync/config.toml"],
    );
    assert_eq!(
        commit_output.status.code(),
        Some(1),
        "{}",
        render_output(&commit_output)
    );
    let stderr = String::from_utf8_lossy(&commit_output.stderr);
    assert!(
        stderr.contains("Commit `.config/dotsync/config.toml` on `all`."),
        "{}",
        render_output(&commit_output)
    );
}

#[test]
fn pending_sync_loads_config_from_committed_all_scope_not_working_copy_edit() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");
    let commit_output = machine.commit_paths("all", "add gitconfig", &[".gitconfig"]);
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );

    machine.delete_home_file(".gitconfig");
    machine.write_repo_file(
        ".config/dotsync/config.toml",
        "[scopes]\nall = {}\nlinux = { parents = [\"all\"] }\n\n[sync]\nstate_path = \".config/dotsync/other-state.json\"\n",
    );

    let sync_output = machine.sync();
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "{}",
        render_output(&sync_output)
    );
    let stderr = String::from_utf8_lossy(&sync_output.stderr);
    assert!(
        stderr.contains("working copy has uncommitted changes"),
        "{}",
        render_output(&sync_output)
    );
    assert!(
        machine.home_file_exists(".config/dotsync/sync-state.json"),
        "committed base-scope config should still define the original state path"
    );
    assert!(
        !machine
            .home_dir
            .join(".config/dotsync/other-state.json")
            .exists(),
        "mutable working-tree config edit should not affect config loading"
    );
}

#[test]
fn pending_selective_commit_preserves_unselected_dirty_paths_when_cascade_pauses() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let selected = ".gitconfig";
    let unselected = ".config/fish/config.fish";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(selected, "[user]\nname = \"Base\"\n");
    let base_output = machine.commit_all("all", "add base gitconfig");
    assert!(
        base_output.status.success(),
        "{}",
        render_output(&base_output)
    );

    machine.write_repo_file(selected, "[user]\nname = \"Linux\"\n");
    let linux_output = machine.commit_all("linux", "linux override");
    assert!(
        linux_output.status.success(),
        "{}",
        render_output(&linux_output)
    );

    machine.write_repo_file(selected, "[user]\nname = \"All\"\n");
    machine.write_repo_file(unselected, "set -g fish_greeting keep-me\n");

    let paused_output = machine.commit_paths("all", "conflicting all change", &[selected]);
    assert_eq!(
        paused_output.status.code(),
        Some(3),
        "{}",
        render_output(&paused_output)
    );

    assert_eq!(
        machine.read_repo_file(unselected),
        "set -g fish_greeting keep-me\n",
        "paused selective commit must leave unrelated dirty working-copy paths intact"
    );

    let sync_output = machine.sync();
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "{}",
        render_output(&sync_output)
    );
    let stderr = String::from_utf8_lossy(&sync_output.stderr);
    assert!(
        stderr.contains("cascade already in progress") || stderr.contains("already paused"),
        "{}",
        render_output(&sync_output)
    );
}

#[test]
fn pending_joining_existing_remote_creates_new_scope_and_first_commit_works() {
    let harness = TestHarness::new();
    let linux_machine = harness.machine("machine-linux", "linux", "mx-xps-cy");
    let windows_machine = harness.machine("machine-windows", "windows", "mx-pc-win");

    let linux_init = linux_machine.init();
    assert!(
        linux_init.status.success(),
        "{}",
        render_output(&linux_init)
    );

    let windows_init = windows_machine.init();
    assert!(
        windows_init.status.success(),
        "{}",
        render_output(&windows_init)
    );
    assert!(
        windows_machine
            .current_bookmarks()
            .contains(&"mx-pc-win".to_string()),
        "windows machine scope should be created on join\n{}",
        render_output(&windows_init)
    );

    windows_machine.write_repo_file(".gitconfig", "[user]\nname = \"Win\"\n");
    let first_commit =
        windows_machine.commit_paths("windows", "first windows commit", &[".gitconfig"]);
    assert!(
        first_commit.status.success(),
        "{}",
        render_output(&first_commit)
    );
    assert_eq!(
        windows_machine.read_home_file(".gitconfig"),
        "[user]\nname = \"Win\"\n"
    );
    assert_eq!(
        windows_machine.bookmark_file_contents("windows", ".gitconfig"),
        "[user]\nname = \"Win\"\n"
    );
}

#[test]
fn pending_fetch_stops_when_remote_would_reset_local_bookmark() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(".gitconfig", "[user]\nname = \"Base\"\n");
    let base_output = machine.commit_all("all", "seed base gitconfig");
    assert!(
        base_output.status.success(),
        "{}",
        render_output(&base_output)
    );

    let remote_clone = harness.root_dir.join("remote-reset");
    clone_remote_to(&remote_clone, &harness.remote_dir);

    advance_local_bookmark_without_push(
        &machine,
        "all",
        ".gitconfig",
        "[user]\nname = \"Local unpublished\"\n",
    );
    let local_before = machine.bookmark_revision("all");

    let reset_target = git_rev_parse(&remote_clone, "HEAD");
    let push = git_in(
        &remote_clone,
        &["push", "origin", &format!("{reset_target}:all")],
    );
    assert!(push.status.success(), "{}", render_output(&push));

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
            "fetch would overwrite local bookmark",
            "must not move a local bookmark backward or sideways",
            "discard or bypass unpublished local state",
            "bookmark: all",
            &local_before,
            &reset_target,
        ],
        &sync_output,
    );
    assert_eq!(
        machine.bookmark_revision("all"),
        local_before,
        "unsafe fetch must leave the local bookmark untouched"
    );
    assert_eq!(
        machine.bookmark_file_contents("all", ".gitconfig"),
        "[user]\nname = \"Local unpublished\"\n"
    );
}

#[test]
fn ancestor_scope_commit_from_machine_working_copy_stays_consistent_across_stages() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".gitconfig";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );
    assert_eq!(machine.current_bookmarks(), vec!["mx-xps-cy".to_string()]);
    assert!(!machine.home_file_exists(relative));

    machine.write_repo_file(relative, "[user]\nname = \"Max\"\n");

    let stage_one = machine.commit_all("all", "add gitconfig");
    assert!(stage_one.status.success(), "{}", render_output(&stage_one));
    assert_eq!(machine.current_bookmarks(), vec!["mx-xps-cy".to_string()]);
    assert_eq!(machine.read_home_file(relative), "[user]\nname = \"Max\"\n");
    assert_eq!(
        machine.bookmark_file_contents("all", relative),
        "[user]\nname = \"Max\"\n"
    );
    assert_eq!(
        machine.bookmark_file_contents("linux", relative),
        "[user]\nname = \"Max\"\n"
    );
    assert_eq!(
        machine.bookmark_file_contents("mx-xps-cy", relative),
        "[user]\nname = \"Max\"\n"
    );

    machine.write_repo_file(
        relative,
        "[user]\nname = \"Max\"\nemail = \"max@example.com\"\n",
    );

    let stage_two = machine.commit_all("all", "update gitconfig");
    assert!(stage_two.status.success(), "{}", render_output(&stage_two));
    assert_eq!(machine.current_bookmarks(), vec!["mx-xps-cy".to_string()]);
    assert_eq!(
        machine.read_home_file(relative),
        "[user]\nname = \"Max\"\nemail = \"max@example.com\"\n"
    );
    assert_eq!(
        machine.bookmark_file_contents("all", relative),
        "[user]\nname = \"Max\"\nemail = \"max@example.com\"\n"
    );
    assert_eq!(
        machine.bookmark_file_contents("linux", relative),
        "[user]\nname = \"Max\"\nemail = \"max@example.com\"\n"
    );
    assert_eq!(
        machine.bookmark_file_contents("mx-xps-cy", relative),
        "[user]\nname = \"Max\"\nemail = \"max@example.com\"\n"
    );

    machine.write_repo_file(
        relative,
        "[user]\nname = \"Max\"\nemail = \"max@example.com\"\nsigningkey = \"abc123\"\n",
    );

    let stage_three = machine.commit_all("all", "add signing key");
    assert!(
        stage_three.status.success(),
        "{}",
        render_output(&stage_three)
    );
    assert_eq!(machine.current_bookmarks(), vec!["mx-xps-cy".to_string()]);
    assert_eq!(
        machine.read_home_file(relative),
        "[user]\nname = \"Max\"\nemail = \"max@example.com\"\nsigningkey = \"abc123\"\n"
    );
    assert_eq!(
        machine.bookmark_file_contents("all", relative),
        "[user]\nname = \"Max\"\nemail = \"max@example.com\"\nsigningkey = \"abc123\"\n"
    );
    assert_eq!(
        machine.bookmark_file_contents("linux", relative),
        "[user]\nname = \"Max\"\nemail = \"max@example.com\"\nsigningkey = \"abc123\"\n"
    );
    assert_eq!(
        machine.bookmark_file_contents("mx-xps-cy", relative),
        "[user]\nname = \"Max\"\nemail = \"max@example.com\"\nsigningkey = \"abc123\"\n"
    );
}

#[test]
fn scoped_commit_deletion_removes_file_from_fake_home() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".gitconfig";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(relative, "[user]\nname = \"Max\"\n");

    let add_output = machine.commit_all("all", "add gitconfig");
    assert!(
        add_output.status.success(),
        "{}",
        render_output(&add_output)
    );
    assert!(machine.home_file_exists(relative));

    machine.delete_repo_file(relative);

    let delete_output = machine.commit_all("all", "remove gitconfig");
    assert!(
        delete_output.status.success(),
        "{}",
        render_output(&delete_output)
    );
    assert!(
        !machine.home_file_exists(relative),
        "scoped deletion should remove the managed file from fake home\n{}",
        render_output(&delete_output)
    );
}

#[test]
fn scoped_deletion_only_affects_homes_where_scope_applies() {
    let harness = TestHarness::new();
    let linux_machine = harness.machine("machine-linux", "linux", "mx-xps-cy");
    let windows_machine = harness.machine("machine-windows", "windows", "mx-pc-win");
    let relative = ".config/hypr/hyprland.conf";

    let linux_init = linux_machine.init();
    assert!(
        linux_init.status.success(),
        "{}",
        render_output(&linux_init)
    );

    linux_machine.write_repo_file(relative, "monitor=,preferred,auto,1\n");

    let add_output = linux_machine.commit_all("linux", "add hyprland config");
    assert!(
        add_output.status.success(),
        "{}",
        render_output(&add_output)
    );
    assert!(linux_machine.home_file_exists(relative));

    let windows_init = windows_machine.init();
    assert!(
        windows_init.status.success(),
        "{}",
        render_output(&windows_init)
    );
    assert!(!windows_machine.home_file_exists(relative));

    windows_machine.write_home_file(relative, "manual local config\n");

    linux_machine.delete_repo_file(relative);

    let delete_output = linux_machine.commit_all("linux", "remove hyprland config");
    assert!(
        delete_output.status.success(),
        "{}",
        render_output(&delete_output)
    );
    assert!(!linux_machine.home_file_exists(relative));

    let windows_sync = windows_machine.sync();
    assert!(
        windows_sync.status.success(),
        "{}",
        render_output(&windows_sync)
    );
    assert_eq!(
        windows_machine.read_home_file(relative),
        "manual local config\n",
        "deleting a linux-scoped file should not remove the same path from a windows home"
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

    machine.write_repo_file(relative, "machine config\n");
    let commit_output = machine.commit_all("mx-xps-cy", "add machine config");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
    );
    assert_eq!(machine.read_home_file(relative), "machine config\n");

    machine.delete_home_file(relative);
    machine.set_checkout_scope("all");

    let sync_output = machine.sync();
    assert!(
        sync_output.status.success(),
        "{}",
        render_output(&sync_output)
    );
    assert_eq!(
        machine.read_home_file(relative),
        "machine config\n",
        "sync state machine scope should govern sync even if checkout moved to another scope"
    );
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

    machine.write_repo_file(relative, "[user]\nname = \"Max\"\n");
    let add_output = machine.commit_all("all", "add gitconfig");
    assert!(
        add_output.status.success(),
        "{}",
        render_output(&add_output)
    );
    assert!(machine.home_file_exists(relative));

    machine.delete_sync_state();
    machine.delete_repo_file(relative);

    let delete_output = machine.commit_all("all", "remove gitconfig");
    assert!(
        delete_output.status.success(),
        "{}",
        render_output(&delete_output)
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
fn dirty_working_copy_human_error_stands_alone() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");

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
            "dirty",
            "Plain `dotsync` is sync-only",
            "working copy has uncommitted changes",
            "cannot safely sync changes that have not been assigned to a scope",
            "dotsync <scope> -m \"message\"",
        ],
        &sync_output,
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

    machine.write_repo_file(relative, "[user]\nname = \"Repo\"\n");
    let commit_output = machine.commit_all("all", "add gitconfig");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
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
fn continue_without_pause_human_error_stands_alone() {
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
        "{}",
        render_output(&continue_output)
    );

    let stderr = String::from_utf8_lossy(&continue_output.stderr);
    assert_standalone_error(
        &stderr,
        &[
            "nothing to resume",
            "`dotsync continue` resumes a merge cascade",
            "no cascade is currently paused",
            "Use `dotsync continue` only after a previous cascade paused",
            "dotsync <scope> -m \"message\"",
        ],
        &continue_output,
    );
}

#[test]
fn command_while_cascade_paused_human_error_stands_alone() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".gitconfig";

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(relative, "[user]\nname = \"Base\"\n");
    let base_output = machine.commit_all("all", "add gitconfig");
    assert!(
        base_output.status.success(),
        "{}",
        render_output(&base_output)
    );

    machine.write_repo_file(relative, "[user]\nname = \"Linux\"\n");
    let linux_output = machine.commit_all("linux", "linux override");
    assert!(
        linux_output.status.success(),
        "{}",
        render_output(&linux_output)
    );

    machine.write_repo_file(relative, "[user]\nname = \"All\"\n");
    let paused_output = machine.commit_all("all", "conflicting all change");
    assert_eq!(
        paused_output.status.code(),
        Some(3),
        "{}",
        render_output(&paused_output)
    );

    let blocked_output = machine.commit_all("all", "second command while paused");
    assert_eq!(
        blocked_output.status.code(),
        Some(1),
        "{}",
        render_output(&blocked_output)
    );

    let stderr = String::from_utf8_lossy(&blocked_output.stderr);
    assert_standalone_error(
        &stderr,
        &[
            "already paused",
            "commit flow records the working-copy change on the selected scope",
            "expects no earlier cascade to still be paused",
            "cascade already in progress on `linux`",
            "resolve the paused scope in ~/dotfiles and then run `dotsync continue`",
            "Do not start another dotsync command until that resume finishes.",
        ],
        &blocked_output,
    );
}

#[test]
fn invalid_scope_human_error_stands_alone() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let commit_output = machine.commit_all("server", "bad scope");
    assert_eq!(
        commit_output.status.code(),
        Some(1),
        "{}",
        render_output(&commit_output)
    );

    let stderr = String::from_utf8_lossy(&commit_output.stderr);
    assert_standalone_error(
        &stderr,
        &[
            "invalid scope",
            "expects the scope you name to exist in the configured scope DAG",
            "scope `server` does not exist in config",
            "choose a real configured scope",
            "root-est appropriate ancestor scope",
        ],
        &commit_output,
    );
}

#[test]
fn non_ancestor_scope_human_error_stands_alone() {
    let harness = TestHarness::new();
    let linux_machine = harness.machine("machine-linux", "linux", "mx-xps-cy");
    let windows_machine = harness.machine("machine-windows", "windows", "mx-pc-win");

    let linux_init = linux_machine.init();
    assert!(
        linux_init.status.success(),
        "{}",
        render_output(&linux_init)
    );

    let windows_init = windows_machine.init();
    assert!(
        windows_init.status.success(),
        "{}",
        render_output(&windows_init)
    );

    windows_machine.write_repo_file(".gitconfig", "[user]\nname = \"Win\"\n");

    let commit_output = windows_machine.commit_all("linux", "bad scope");
    assert_eq!(
        commit_output.status.code(),
        Some(1),
        "{}",
        render_output(&commit_output)
    );

    let stderr = String::from_utf8_lossy(&commit_output.stderr);
    assert_standalone_error(
        &stderr,
        &[
            "not an ancestor",
            "expects the chosen scope to be the current machine scope or one of its ancestors",
            "scope `linux` is not an ancestor of `mx-pc-win`",
            "would let this machine write into an unrelated branch lineage",
            "choose `mx-pc-win` or one of its ancestors instead",
        ],
        &commit_output,
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
fn dirty_working_copy_json_contract_stays_compatible() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(
        init_output.status.success(),
        "{}",
        render_output(&init_output)
    );

    machine.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");

    let sync_output = machine.sync_json();
    assert_eq!(
        sync_output.status.code(),
        Some(1),
        "{}",
        render_output(&sync_output)
    );

    let json = parse_stdout_json(&sync_output);
    assert_eq!(json["status"], "error");
    assert_eq!(json["error"], "dirty_working_copy");
    assert!(json["message"].as_str().is_some());
    assert_eq!(json["drifts"], serde_json::json!([]));
    assert!(json["current_state"].as_str().is_some());
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

    machine.write_repo_file(relative, "[user]\nname = \"Repo\"\n");
    let commit_output = machine.commit_all("all", "add gitconfig");
    assert!(
        commit_output.status.success(),
        "{}",
        render_output(&commit_output)
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
        assert!(
            stderr.contains(heading),
            "missing heading `{heading}`\n{}",
            render_output(output)
        );
    }
    for fragment in expected_fragments {
        assert!(
            stderr.contains(fragment),
            "missing fragment `{fragment}`\n{}",
            render_output(output)
        );
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
fn tdd_ratchet_gatekeeper() {
    if std::env::var("TDD_RATCHET").is_err() {
        panic!("Run tdd-ratchet instead of cargo test.");
    }
}
