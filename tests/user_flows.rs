use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use dotsync::{CommitOptions, DotsyncPaths, DotsyncError, SyncOptions, commit_and_sync, sync};
use jj_lib::config::StackedConfig;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::op_store::RefTarget;
use jj_lib::repo::{ReadonlyRepo, Repo as _, StoreFactories};
use jj_lib::ref_name::RefNameBuf;
use jj_lib::settings::UserSettings;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::workspace::{Workspace, default_working_copy_factories};
use tempfile::TempDir;

fn test_settings() -> UserSettings {
    let config = StackedConfig::with_defaults();
    UserSettings::from_config(config).expect("settings")
}

struct TestHarness {
    _tempdir: TempDir,
    repo_root: PathBuf,
    home_dir: PathBuf,
    settings: UserSettings,
}

impl TestHarness {
    async fn new() -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let repo_root = tempdir.path().join("repo");
        let home_dir = tempdir.path().join("home");
        fs::create_dir_all(&repo_root).expect("repo dir");
        fs::create_dir_all(&home_dir).expect("home dir");

        let settings = test_settings();
        let _ = Workspace::init_colocated_git(&settings, &repo_root)
            .await
            .expect("init repo");

        Self {
            _tempdir: tempdir,
            repo_root,
            home_dir,
            settings,
        }
    }

    fn dotsync_paths(&self) -> DotsyncPaths {
        DotsyncPaths {
            repo_root: self.repo_root.clone(),
            home_dir: self.home_dir.clone(),
        }
    }

    fn write_repo_file(&self, relative: &str, contents: &str) {
        let path = self.repo_root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("repo parent dir");
        }
        fs::write(path, contents).expect("write repo file");
    }

    fn write_home_file(&self, relative: &str, contents: &str) {
        let path = self.home_dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("home parent dir");
        }
        fs::write(path, contents).expect("write home file");
    }

    fn home_file(&self, relative: &str) -> PathBuf {
        self.home_dir.join(relative)
    }

    async fn snapshot(&self) {
        let mut workspace = self.load_workspace();
        let repo = workspace
            .repo_loader()
            .load_at_head()
            .await
            .expect("load repo");
        let mut locked_ws = workspace
            .start_working_copy_mutation()
            .expect("lock workspace");
        let snapshot_options = SnapshotOptions {
            base_ignores: GitIgnoreFile::empty(),
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &EverythingMatcher,
            max_new_file_size: u64::MAX,
        };
        let (tree, _stats) = locked_ws
            .locked_wc()
            .snapshot(&snapshot_options)
            .await
            .expect("snapshot");
        locked_ws
            .finish(repo.op_id().clone())
            .expect("finish working copy mutation");

        let wc_commit_id = repo
            .view()
            .get_wc_commit_id(workspace.workspace_name())
            .expect("wc commit id")
            .clone();
        let wc_commit = repo.store().get_commit(&wc_commit_id).expect("wc commit");

        let mut tx = repo.start_transaction();
        let new_wc = tx
            .repo_mut()
            .new_commit(vec![wc_commit.id().clone()], tree)
            .write()
            .await
            .expect("write wc commit");
        tx.repo_mut()
            .set_wc_commit(workspace.workspace_name().to_owned(), new_wc.id().clone())
            .expect("set wc commit");
        tx.commit("snapshot test fixture").await.expect("commit tx");
    }

    fn load_workspace(&self) -> Workspace {
        Workspace::load(
            &self.settings,
            &self.repo_root,
            &StoreFactories::default(),
            &default_working_copy_factories(),
        )
        .expect("load workspace")
    }

    async fn head_repo(&self) -> Arc<ReadonlyRepo> {
        self.load_workspace()
            .repo_loader()
            .load_at_head()
            .await
            .expect("head repo")
    }

    async fn create_bookmark(&self, name: &str) {
        let repo = self.head_repo().await;
        let wc_commit_id = repo
            .view()
            .get_wc_commit_id(self.load_workspace().workspace_name())
            .expect("wc commit id")
            .clone();
        let mut tx = repo.start_transaction();
        let bookmark = RefNameBuf::from(name);
        tx.repo_mut()
            .set_local_bookmark_target(bookmark.as_ref(), RefTarget::normal(wc_commit_id));
        tx.commit(format!("bookmark {name}"))
            .await
            .expect("commit bookmark tx");
    }
}

#[tokio::test]
async fn basic_sync_copies_all_scope_file_to_home() {
    let harness = TestHarness::new().await;
    harness.write_repo_file(
        ".config/dotsync/config.toml",
        "[scopes]\nall = {}\nlinux = { parents = [\"all\"] }\nmx-xps-cy = { parents = [\"linux\"] }\n",
    );
    harness.write_repo_file(".gitconfig", "[user]\nname = \"Max\"\n");
    harness.snapshot().await;
    harness.create_bookmark("all").await;
    harness.create_bookmark("linux").await;
    harness.create_bookmark("mx-xps-cy").await;

    let result = sync(&harness.dotsync_paths(), SyncOptions { force: false }).await;

    assert!(result.is_ok(), "expected sync success, got {result:?}");
    assert_eq!(
        fs::read_to_string(harness.home_file(".gitconfig")).expect("synced file"),
        "[user]\nname = \"Max\"\n"
    );
}

#[tokio::test]
async fn drift_detection_reports_drift_without_overwriting() {
    let harness = TestHarness::new().await;
    harness.write_repo_file(
        ".config/dotsync/config.toml",
        "[scopes]\nall = {}\nlinux = { parents = [\"all\"] }\nmx-xps-cy = { parents = [\"linux\"] }\n",
    );
    harness.write_repo_file(".gitconfig", "repo version\n");
    harness.write_home_file(".gitconfig", "system drift\n");
    harness.snapshot().await;
    harness.create_bookmark("all").await;
    harness.create_bookmark("linux").await;
    harness.create_bookmark("mx-xps-cy").await;

    let result = sync(&harness.dotsync_paths(), SyncOptions { force: false }).await;

    assert!(result.is_err(), "expected drift failure, got success: {result:?}");
    assert!(
        !matches!(result, Err(DotsyncError::NotImplemented(_))),
        "expected drift-specific failure, got not implemented"
    );

    assert_eq!(
        fs::read_to_string(harness.home_file(".gitconfig")).expect("home file"),
        "system drift\n"
    );
}

#[tokio::test]
async fn force_sync_overwrites_drift() {
    let harness = TestHarness::new().await;
    harness.write_repo_file(
        ".config/dotsync/config.toml",
        "[scopes]\nall = {}\nlinux = { parents = [\"all\"] }\nmx-xps-cy = { parents = [\"linux\"] }\n",
    );
    harness.write_repo_file(".gitconfig", "repo version\n");
    harness.write_home_file(".gitconfig", "system drift\n");
    harness.snapshot().await;
    harness.create_bookmark("all").await;
    harness.create_bookmark("linux").await;
    harness.create_bookmark("mx-xps-cy").await;

    let result = sync(&harness.dotsync_paths(), SyncOptions { force: true }).await;

    assert!(result.is_ok(), "expected force sync success, got {result:?}");
    assert_eq!(
        fs::read_to_string(harness.home_file(".gitconfig")).expect("home file"),
        "repo version\n"
    );
}

#[tokio::test]
async fn commit_and_cascade_commits_change_to_named_scope() {
    let harness = TestHarness::new().await;
    harness.write_repo_file(
        ".config/dotsync/config.toml",
        "[scopes]\nall = {}\nlinux = { parents = [\"all\"] }\nmx-xps-cy = { parents = [\"linux\"] }\n",
    );
    harness.write_repo_file(".gitconfig", "before\n");
    harness.snapshot().await;
    harness.create_bookmark("all").await;
    harness.create_bookmark("linux").await;
    harness.create_bookmark("mx-xps-cy").await;
    harness.write_repo_file(".gitconfig", "after\n");

    let result = commit_and_sync(
        &harness.dotsync_paths(),
        CommitOptions {
            scope: "all".to_string(),
            message: "update gitconfig".to_string(),
            force: false,
        },
    )
    .await;

    assert!(result.is_ok(), "expected commit flow success, got {result:?}");
    assert_eq!(
        fs::read_to_string(harness.home_file(".gitconfig")).expect("home file"),
        "after\n"
    );
}

#[tokio::test]
async fn scope_specific_commit_only_cascades_to_descendants() {
    let harness = TestHarness::new().await;
    harness.write_repo_file(
        ".config/dotsync/config.toml",
        "[scopes]\nall = {}\nlinux = { parents = [\"all\"] }\nwindows = { parents = [\"all\"] }\nmx-xps-cy = { parents = [\"linux\"] }\n",
    );
    harness.write_repo_file(".gitconfig", "before\n");
    harness.snapshot().await;
    harness.create_bookmark("all").await;
    harness.create_bookmark("linux").await;
    harness.create_bookmark("windows").await;
    harness.create_bookmark("mx-xps-cy").await;
    harness.write_repo_file(".gitconfig", "linux change\n");

    let result = commit_and_sync(
        &harness.dotsync_paths(),
        CommitOptions {
            scope: "linux".to_string(),
            message: "linux-only".to_string(),
            force: false,
        },
    )
    .await;

    assert!(result.is_ok(), "expected scope-specific commit success, got {result:?}");
}

#[tokio::test]
async fn invalid_scope_name_returns_error() {
    let harness = TestHarness::new().await;
    harness.write_repo_file(
        ".config/dotsync/config.toml",
        "[scopes]\nall = {}\nlinux = { parents = [\"all\"] }\nmx-xps-cy = { parents = [\"linux\"] }\n",
    );
    harness.write_repo_file(".gitconfig", "before\n");
    harness.snapshot().await;
    harness.create_bookmark("all").await;
    harness.create_bookmark("linux").await;
    harness.create_bookmark("mx-xps-cy").await;

    let result = commit_and_sync(
        &harness.dotsync_paths(),
        CommitOptions {
            scope: "does-not-exist".to_string(),
            message: "bad scope".to_string(),
            force: false,
        },
    )
    .await;

    assert!(result.is_err(), "expected invalid scope failure, got success: {result:?}");
    assert!(
        !matches!(result, Err(DotsyncError::NotImplemented(_))),
        "expected invalid-scope failure, got not implemented"
    );
}

#[tokio::test]
async fn config_validation_rejects_cycle() {
    let harness = TestHarness::new().await;
    harness.write_repo_file(
        ".config/dotsync/config.toml",
        "[scopes]\nall = { parents = [\"linux\"] }\nlinux = { parents = [\"all\"] }\n",
    );
    harness.snapshot().await;

    let result = sync(&harness.dotsync_paths(), SyncOptions { force: false }).await;

    assert!(result.is_err(), "expected invalid config failure, got success: {result:?}");
    assert!(
        !matches!(result, Err(DotsyncError::NotImplemented(_))),
        "expected config validation failure, got not implemented"
    );
}

#[tokio::test]
async fn config_validation_rejects_missing_parent() {
    let harness = TestHarness::new().await;
    harness.write_repo_file(
        ".config/dotsync/config.toml",
        "[scopes]\nall = {}\nlinux = { parents = [\"missing\"] }\n",
    );
    harness.snapshot().await;

    let result = sync(&harness.dotsync_paths(), SyncOptions { force: false }).await;

    assert!(result.is_err(), "expected invalid config failure, got success: {result:?}");
    assert!(
        !matches!(result, Err(DotsyncError::NotImplemented(_))),
        "expected config validation failure, got not implemented"
    );
}
