use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use jj_lib::backend::TreeValue;
use jj_lib::config::StackedConfig;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::{Repo as _, StoreFactories};
use jj_lib::repo_path::RepoPath;
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

    fn commit(&self, scope: &str, message: &str) -> Output {
        self.run_dotsync(&[scope, "-m", message])
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

    fn bookmark_has_file(&self, scope: &str, relative: &str) -> bool {
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

        match commit
            .tree()
            .path_value(path)
            .unwrap_or_else(|err| panic!("read `{relative}` from `{scope}` tree: {err}"))
            .into_resolved()
        {
            Ok(Some(TreeValue::File { .. })) => true,
            Ok(Some(other)) => panic!(
                "expected file at `{relative}` on `{scope}`, found different tree value: {other:?}"
            ),
            Ok(None) => false,
            Err(value) => panic!(
                "expected resolved value for `{relative}` on `{scope}`, found conflict: {value:?}"
            ),
        }
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

#[test]
fn plain_dotsync_rejects_working_copy_changes() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");

    let init_output = machine.init();
    assert!(init_output.status.success(), "{}", render_output(&init_output));

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
fn ancestor_scope_commit_from_machine_working_copy_stays_consistent_across_stages() {
    let harness = TestHarness::new();
    let machine = harness.machine("machine-a", "linux", "mx-xps-cy");
    let relative = ".gitconfig";

    let init_output = machine.init();
    assert!(init_output.status.success(), "{}", render_output(&init_output));
    assert_eq!(machine.current_bookmarks(), vec!["mx-xps-cy".to_string()]);
    assert!(!machine.home_file_exists(relative));

    machine.write_repo_file(relative, "[user]\nname = \"Max\"\n");

    let stage_one = machine.commit("all", "add gitconfig");
    assert!(stage_one.status.success(), "{}", render_output(&stage_one));
    assert_eq!(machine.current_bookmarks(), vec!["mx-xps-cy".to_string()]);
    assert_eq!(machine.read_home_file(relative), "[user]\nname = \"Max\"\n");
    assert_eq!(machine.bookmark_file_contents("all", relative), "[user]\nname = \"Max\"\n");
    assert_eq!(machine.bookmark_file_contents("linux", relative), "[user]\nname = \"Max\"\n");
    assert_eq!(
        machine.bookmark_file_contents("mx-xps-cy", relative),
        "[user]\nname = \"Max\"\n"
    );

    machine.write_repo_file(relative, "[user]\nname = \"Max\"\nemail = \"max@example.com\"\n");

    let stage_two = machine.commit("all", "update gitconfig");
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

    let stage_three = machine.commit("all", "add signing key");
    assert!(stage_three.status.success(), "{}", render_output(&stage_three));
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
