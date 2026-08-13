// The black-box test harness: a fake remote, per-machine environments that run
// the real `dotsync` binary against a temporary `HOME`, fixture builders that
// put state on the remote the way another machine would have, and the
// assertion helpers the scenarios in `user_flows.rs` share.
//
// The jj-lib reading below is a deliberately separate client from
// `src/repo.rs`, not duplication waiting to be removed. These are black-box
// tests: reading a scope through the code under test would make the answer
// agree with the bug. `src/repo.rs`'s `read_tree_entry_bytes` is the concrete
// case — it returns a symlink's target as file bytes, which is exactly what
// `a_symlink_on_a_scope_materialises_in_home_as_a_symlink` is pending on, and
// a harness that reused it could not see that.
//
// Everything in `src/repo.rs` is `pub(crate)` anyway, so sharing would mean
// widening the library's public API for the tests' benefit.

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

pub struct TestHarness {
    _tempdir: TempDir,
    pub root_dir: PathBuf,
    remote_dir: PathBuf,
}

impl TestHarness {
    pub fn new() -> Self {
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

    /// Puts the remote out of reach, the way a machine off the network finds
    /// it: the configured URL is unchanged and nothing local is touched, there
    /// is simply nothing at the other end.
    pub fn disconnect_remote(&self) {
        fs::rename(&self.remote_dir, self.disconnected_remote_dir())
            .expect("move the remote out of reach");
    }

    pub fn reconnect_remote(&self) {
        fs::rename(self.disconnected_remote_dir(), &self.remote_dir).expect("put the remote back");
    }

    pub fn disconnected_remote_dir(&self) -> PathBuf {
        self.root_dir.join("remote-disconnected.git")
    }

    pub fn machine(&self, name: &str, os: &str, hostname: &str) -> MachineEnvironment {
        MachineEnvironment::new(
            self.root_dir.join(name),
            self.remote_dir.clone(),
            self.root_dir.join("git-shim"),
            os,
            hostname,
        )
    }
}

pub struct MachineEnvironment {
    pub home_dir: PathBuf,
    pub repo_dir: PathBuf,
    pub remote_dir: PathBuf,
    /// Outside the managed home on purpose: a fixture that lived in home would
    /// show up in anything that reads home, and one of the things dotsync has
    /// to get right is what it does with files it finds there.
    shim_dir: PathBuf,
    os: String,
    hostname: String,
}

impl MachineEnvironment {
    pub fn new(
        root_dir: PathBuf,
        remote_dir: PathBuf,
        shim_dir: PathBuf,
        os: &str,
        hostname: &str,
    ) -> Self {
        let home_dir = root_dir.join("home");
        let repo_dir = home_dir.join(".local/share/dotsync/repo");
        fs::create_dir_all(&home_dir).expect("create home dir");
        Self {
            home_dir,
            repo_dir,
            remote_dir,
            shim_dir,
            os: os.to_string(),
            hostname: hostname.to_string(),
        }
    }

    pub fn init(&self) -> Output {
        self.run(&format!(
            "dotsync init {}",
            self.remote_dir
                .to_str()
                .expect("remote path should be valid UTF-8")
        ))
    }

    /// `init`, for the tests that do not care how it went — which is every
    /// test whose subject is what happens afterwards.
    pub fn init_ok(&self) -> Output {
        let output = self.init();
        assert!(
            output.status.success(),
            "expected `dotsync init` to succeed\n{}",
            render_output(&output)
        );
        output
    }

    /// Runs dotsync, asserts it exited 0, and hands the output back.
    ///
    /// The idiom this replaces asserted exactly this and nothing more, so a
    /// success assertion that carries its own explanation is still written out
    /// against `run`: the explanation is the part `run_ok` cannot keep.
    pub fn run_ok(&self, command: &str) -> Output {
        let output = self.run(command);
        assert!(
            output.status.success(),
            "expected `{command}` to succeed\n{}",
            render_output(&output)
        );
        output
    }

    /// Runs dotsync, asserts which exit code it ended on, and hands the output
    /// back. The code is a bare number for the same reason the assertions this
    /// replaces used one: DESIGN's exit-code table is the meaning of it, and
    /// several of the codes these tests meet are still open questions, so a
    /// name here would decide them.
    pub fn run_expecting(&self, command: &str, exit_code: i32) -> Output {
        let output = self.run(command);
        assert_eq!(
            output.status.code(),
            Some(exit_code),
            "expected `{command}` to exit {exit_code}\n{}",
            render_output(&output)
        );
        output
    }

    pub fn run(&self, command: &str) -> Output {
        let args = dotsync_args(command);
        let mut command = Command::new(env!("CARGO_BIN_EXE_dotsync"));
        command.args(args);
        command.current_dir(&self.home_dir);
        command.env("HOME", &self.home_dir);
        command.env("DOTSYNC_OS", &self.os);
        command.env("DOTSYNC_HOSTNAME", &self.hostname);
        command.output().expect("run dotsync")
    }

    /// Runs dotsync with a `git` on the front of `PATH` that records every
    /// invocation, and returns those invocations alongside the output.
    ///
    /// dotsync reaches the remote by shelling out to `git` — jj-lib's
    /// supported fetch and push mechanism, and a documented runtime dependency
    /// — so counting `git fetch` calls is how many times a run talked to the
    /// network, observed from outside the binary.
    pub fn run_recording_git(&self, command: &str) -> (Output, Vec<String>) {
        let shim_dir = &self.shim_dir;
        let log_path = shim_dir.join("git-calls.log");
        write_file_at(
            &shim_dir.join("git"),
            &format!(
                "#!/bin/sh\necho \"$@\" >> \"$DOTSYNC_TEST_GIT_LOG\"\nexec {} \"$@\"\n",
                real_git_path().display()
            ),
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(shim_dir.join("git"), fs::Permissions::from_mode(0o755))
                .expect("make git shim executable");
        }
        if log_path.exists() {
            fs::remove_file(&log_path).expect("clear git call log");
        }

        let args = dotsync_args(command);
        let mut command = Command::new(env!("CARGO_BIN_EXE_dotsync"));
        command.args(args);
        command.current_dir(&self.home_dir);
        command.env("HOME", &self.home_dir);
        command.env("DOTSYNC_OS", &self.os);
        command.env("DOTSYNC_HOSTNAME", &self.hostname);
        command.env("DOTSYNC_TEST_GIT_LOG", &log_path);
        command.env(
            "PATH",
            format!(
                "{}:{}",
                shim_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        let output = command.output().expect("run dotsync");

        let calls = fs::read_to_string(&log_path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        (output, calls)
    }

    /// How many times a run asked git to talk to the remote.
    pub fn fetches_during(&self, command: &str) -> (Output, usize) {
        let (output, calls) = self.run_recording_git(command);
        let fetches = calls
            .iter()
            .filter(|call| call.split_whitespace().any(|word| word == "fetch"))
            .count();
        (output, fetches)
    }

    /// Runs dotsync with no `HOME` in the environment, which is how dotsync
    /// finds both the home directory it manages and its hidden repo.
    /// Runs dotsync and gives up on it after `limit`.
    ///
    /// For the one failure a plain `output()` cannot report: dotsync never
    /// returning at all. A hang shows up as a test the runner kills minutes
    /// later, with nothing said about which call blocked; a run that is killed
    /// here fails the exit-code assertion immediately.
    pub fn run_within(&self, command: &str, limit: std::time::Duration) -> Output {
        let args = dotsync_args(command);
        let mut child = Command::new(env!("CARGO_BIN_EXE_dotsync"))
            .args(args)
            .current_dir(&self.home_dir)
            .env("HOME", &self.home_dir)
            .env("DOTSYNC_OS", &self.os)
            .env("DOTSYNC_HOSTNAME", &self.hostname)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn dotsync");

        let deadline = std::time::Instant::now() + limit;
        loop {
            match child.try_wait().expect("poll dotsync") {
                Some(_) => break,
                None if std::time::Instant::now() >= deadline => {
                    child
                        .kill()
                        .expect("kill a dotsync run that never returned");
                    break;
                }
                None => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
        child.wait_with_output().expect("collect dotsync output")
    }

    pub fn run_without_home(&self, command: &str) -> Output {
        let args = dotsync_args(command);
        let mut command = Command::new(env!("CARGO_BIN_EXE_dotsync"));
        command.args(args);
        command.current_dir(&self.home_dir);
        command.env_remove("HOME");
        command.env("DOTSYNC_OS", &self.os);
        command.env("DOTSYNC_HOSTNAME", &self.hostname);
        command.output().expect("run dotsync")
    }

    pub fn delete_file(&self, relative: &str) {
        fs::remove_file(self.home_dir.join(relative)).expect("delete file");
    }

    pub fn write_file(&self, relative: &str, contents: &str) {
        write_file_at(&self.home_dir.join(relative), contents);
    }

    pub fn read_file(&self, relative: &str) -> String {
        fs::read_to_string(self.home_dir.join(relative)).expect("read file")
    }

    pub fn file_exists(&self, relative: &str) -> bool {
        self.home_dir.join(relative).exists()
    }

    /// Asked without following the link, which is the whole question: a link
    /// to a regular file answers `true` to every other test on this struct.
    pub fn is_symlink(&self, relative: &str) -> bool {
        fs::symlink_metadata(self.home_dir.join(relative))
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
    }

    pub fn read_link(&self, relative: &str) -> PathBuf {
        fs::read_link(self.home_dir.join(relative))
            .unwrap_or_else(|err| panic!("read home symlink `{relative}`: {err}"))
    }

    /// Removes first, because writing to a path that holds a link writes
    /// through it — which is the bug several of these tests are about, and
    /// would silently corrupt their own fixtures.
    pub fn replace_with_regular_file(&self, relative: &str, contents: &str) {
        let path = self.home_dir.join(relative);
        fs::remove_file(&path).unwrap_or_else(|err| panic!("remove `{relative}`: {err}"));
        write_file_at(&path, contents);
    }

    pub fn replace_with_symlink(&self, relative: &str, target: &Path) {
        let path = self.home_dir.join(relative);
        fs::remove_file(&path).unwrap_or_else(|err| panic!("remove `{relative}`: {err}"));
        symlink_at(target, &path);
    }

    pub fn sync_state_relative_path(&self) -> PathBuf {
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

    pub fn sync_state_path(&self) -> PathBuf {
        self.home_dir.join(self.sync_state_relative_path())
    }

    pub fn delete_sync_state(&self) {
        fs::remove_file(self.sync_state_path()).expect("delete sync state file");
    }

    pub fn write_sync_state_raw(&self, contents: &str) {
        write_file_at(&self.sync_state_path(), contents);
    }

    pub fn read_sync_state_raw(&self) -> String {
        fs::read_to_string(self.sync_state_path()).expect("read sync state file")
    }

    pub fn modified_time(&self, relative: &str) -> std::time::SystemTime {
        fs::metadata(self.home_dir.join(relative))
            .expect("stat home file")
            .modified()
            .expect("read home file mtime")
    }
}

/// The real `git`, resolved before any shim goes on `PATH`.
pub fn real_git_path() -> PathBuf {
    let output = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("look up git");
    assert!(output.status.success(), "{}", render_output(&output));
    PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("git path should be utf-8")
            .trim(),
    )
}

pub fn test_settings() -> UserSettings {
    UserSettings::from_config(StackedConfig::with_defaults())
        .expect("load jj settings for test assertions")
}

/// Runs a jj-lib future to completion. jj-lib's store reads are async and
/// these helpers are not, so each one bridges with a runtime of its own.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(future)
}

pub fn load_repo_direct(repo_dir: &Path) -> Arc<jj_lib::repo::ReadonlyRepo> {
    let settings = test_settings();
    block_on(async {
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

pub fn bookmark_commit(machine: &MachineEnvironment, scope: &str) -> jj_lib::commit::Commit {
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

pub fn read_bookmark_file_contents(
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

    let contents = block_on(async {
        use tokio::io::AsyncReadExt;
        let mut reader = commit
            .store()
            .read_file(path, &id)
            .await
            .unwrap_or_else(|err| {
                panic!("read file contents for `{relative}` on `{scope}`: {err}")
            });
        let mut contents = Vec::new();
        reader
            .read_to_end(&mut contents)
            .await
            .expect("read bookmark file bytes");
        contents
    });
    String::from_utf8(contents).expect("bookmark file should be utf-8")
}

pub fn bookmark_revision(machine: &MachineEnvironment, scope: &str) -> String {
    let repo = load_repo_direct(&machine.repo_dir);
    repo.view()
        .get_local_bookmark(RefNameBuf::from(scope).as_ref())
        .as_normal()
        .unwrap_or_else(|| panic!("missing bookmark `{scope}`"))
        .hex()
}

pub fn bookmark_has_file(machine: &MachineEnvironment, scope: &str, relative: &str) -> bool {
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
pub fn seed_remote_scope_file(
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

/// Puts a symlink on a scope the way anything that is not dotsync would: a
/// plain git client, which records mode 120000 with the target as the blob.
/// Nothing about this needs dotsync to be able to *commit* a link, which is
/// what makes the materialization question reachable on its own.
pub fn seed_remote_scope_symlink(
    machine: &MachineEnvironment,
    scope: &str,
    relative: &str,
    target: &str,
) {
    let clone_dir = machine.home_dir.join(format!("remote-{scope}.ignore"));
    if clone_dir.exists() {
        fs::remove_dir_all(&clone_dir).expect("remove old remote clone dir");
    }
    clone_remote_branch_to(&clone_dir, &machine.remote_dir, scope);
    symlink_at(Path::new(target), &clone_dir.join(relative));
    git_commit_all(
        &clone_dir,
        &format!("test: seed {scope} {relative} -> {target}"),
    );
    git_push(&clone_dir, scope);
}

/// The file mode a branch records for a path: `100644` for a regular file,
/// `120000` for a symlink. Read from the shared remote rather than from the
/// hidden repo, because the remote is what every other machine will read.
pub fn remote_branch_entry_mode(
    machine: &MachineEnvironment,
    branch: &str,
    relative: &str,
) -> Option<String> {
    remote_branch_entries(machine, branch)
        .into_iter()
        .find(|(path, _)| path == relative)
        .map(|(_, mode)| mode)
}

/// Every path a branch holds, with its mode.
pub fn remote_branch_entries(machine: &MachineEnvironment, branch: &str) -> Vec<(String, String)> {
    let output = git_in(&machine.remote_dir, &["ls-tree", "-r", branch]);
    assert!(output.status.success(), "{}", render_output(&output));
    String::from_utf8(output.stdout)
        .expect("git ls-tree output should be utf-8")
        .lines()
        .map(|line| {
            let (meta, path) = line
                .split_once('\t')
                .unwrap_or_else(|| panic!("unexpected ls-tree line `{line}`"));
            let mode = meta
                .split_whitespace()
                .next()
                .unwrap_or_else(|| panic!("unexpected ls-tree line `{line}`"));
            (path.to_string(), mode.to_string())
        })
        .collect()
}

pub fn remove_remote_scope_file(machine: &MachineEnvironment, scope: &str, relative: &str) {
    let clone_dir = machine.home_dir.join(format!("remote-{scope}.ignore"));
    if clone_dir.exists() {
        fs::remove_dir_all(&clone_dir).expect("remove old remote clone dir");
    }
    clone_remote_branch_to(&clone_dir, &machine.remote_dir, scope);
    fs::remove_file(clone_dir.join(relative)).expect("remove remote scope file");
    git_commit_all(&clone_dir, &format!("test: remove {scope} {relative}"));
    git_push(&clone_dir, scope);
}

pub fn add_hyprland_scope(machine: &MachineEnvironment) {
    let clone_dir = machine.home_dir.join("remote-all.ignore");
    if clone_dir.exists() {
        fs::remove_dir_all(&clone_dir).expect("remove old remote all clone dir");
    }
    clone_remote_branch_to(&clone_dir, &machine.remote_dir, "all");

    let config_path = clone_dir.join(".config/dotsync/config.toml");
    let original = fs::read_to_string(&config_path).expect("read remote config");
    // Edited the way a person would: one scope entry at a time, leaving the
    // comments dotsync wrote between them where they are.
    let updated = original.replace(
        "mx-xps-cy = { parents = [\"linux\"] }",
        "hyprland = { parents = [\"linux\"] }\nmx-xps-cy = { parents = [\"hyprland\"] }",
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

pub fn merge_remote_scope_into(machine: &MachineEnvironment, source: &str, target: &str) {
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
pub fn block_remote_pushes(machine: &MachineEnvironment) {
    let hook_path = machine.remote_dir.join("hooks/pre-receive");
    write_file_at(&hook_path, "#!/bin/sh\nexit 1\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))
            .expect("make pre-receive hook executable");
    }
}

pub fn allow_remote_pushes(machine: &MachineEnvironment) {
    fs::remove_file(machine.remote_dir.join("hooks/pre-receive")).expect("remove pre-receive hook");
}

pub fn remote_branch_revision(machine: &MachineEnvironment, branch: &str) -> String {
    let output = git_in(&machine.remote_dir, &["rev-parse", branch]);
    assert!(output.status.success(), "{}", render_output(&output));
    String::from_utf8(output.stdout)
        .expect("git rev-parse output should be utf-8")
        .trim()
        .to_string()
}

pub fn remote_branch_file_contents(
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
pub fn interrupt_push_after_cascade(machine: &MachineEnvironment, relative: &str, contents: &str) {
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

pub fn init_bare_remote(remote_dir: &Path) {
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

pub fn clone_remote_branch_to(path: &Path, remote_dir: &Path, branch: &str) {
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

/// Unix-only, like the `pre-receive` hook fixtures: dotsync's Windows story
/// has its own open questions and no test here pretends to cover them. Gated
/// inside the body rather than on the function so the suite still compiles
/// everywhere, which is what CI's Windows build would notice.
pub fn symlink_at(target: &Path, link: &Path) {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).expect("create symlink");
    #[cfg(not(unix))]
    {
        let _ = (target, link);
        panic!("symlink fixtures are unix-only");
    }
}

/// A named pipe: a path that exists, is not a regular file, and blocks
/// forever if anything opens it for reading.
pub fn make_fifo(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo {} failed", path.display());
}

pub fn write_file_at(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, contents).expect("write fixture file");
}

pub fn git_in(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("run git {:?}: {err}", args))
}

pub fn git_commit_all(dir: &Path, message: &str) {
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

pub fn git_checkout_new_branch(dir: &Path, branch: &str) {
    let checkout = git_in(dir, &["checkout", "-b", branch]);
    assert!(checkout.status.success(), "{}", render_output(&checkout));
}

pub fn git_push(dir: &Path, branch: &str) {
    let push = git_in(dir, &["push", "origin", branch]);
    assert!(push.status.success(), "{}", render_output(&push));
}

pub fn assert_stdout_snapshot(output: &Output, expected: &str) {
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        expected,
        "{}",
        render_output(output)
    );
}

pub fn assert_stderr_snapshot(output: &Output, expected: &str) {
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        expected,
        "{}",
        render_output(output)
    );
}

/// Two machines, a base file on `all`, a `linux` override of it, then a
/// conflicting edit committed to `all` from the second machine — which leaves
/// that machine with a cascade paused at `linux` over `.config/app.conf`.
/// Returns the paused machine and the output of the run that paused.
pub fn pause_a_conflict_on_linux(harness: &TestHarness) -> (MachineEnvironment, Output) {
    let (_machine_a, machine_b, conflict) = pause_a_conflict_on(harness, "linux");
    (machine_b, conflict)
}

/// The same two machines and the same collision, with the scope that holds the
/// colliding override as the parameter — which is the whole difference between
/// the two shapes a conflict comes in.
///
/// `linux` is a scope the paused machine descends from, so the conflict is on
/// its own path and the resolution is its own config. `goof-a` is the *other*
/// machine's leaf scope: the cascade from `all` still has to merge into it, so
/// the same collision happens, but the paused machine does not descend from it
/// and the resolution is not its config. PLAN item 3: "Every conflict test in
/// the suite today pauses on a scope the machine descends from, which is why
/// this survived three waves."
///
/// Returns both machines and the output of the run that paused, because half
/// of what the second shape is about is what the *other* machine ends up with.
pub fn pause_a_conflict_on(
    harness: &TestHarness,
    override_scope: &str,
) -> (MachineEnvironment, MachineEnvironment, Output) {
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

    machine_a.write_file(
        ".config/app.conf",
        &format!("setting = \"{override_scope}\"\n"),
    );
    let commit_override = machine_a.run(&format!(
        "dotsync commit {override_scope} -m 'customize {override_scope} config' -- .config/app.conf"
    ));
    assert!(
        commit_override.status.success(),
        "{}",
        render_output(&commit_override)
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

    (machine_a, machine_b, conflict)
}

pub fn parse_stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).expect("stdout should be valid json")
}

pub fn render_output(output: &Output) -> String {
    format!(
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Splits `dotsync commit all -m 'add base' -- .apprc` into argv the way a
/// shell would, so a test can write the command a user would type.
///
/// It looks like a convenience worth deleting in favour of `&["commit", "all",
/// ...]`, and it is not: `every_command_the_advice_names_is_a_command_dotsync_knows`
/// pulls invocations out of dotsync's own stderr and runs them. That test
/// cannot build an array from a string dotsync printed, so something has to
/// split it, and having two ways to say "run this command" would be worse than
/// having one.
pub fn dotsync_args(command: &str) -> Vec<String> {
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

/// Two machines on one remote, both initialised and both synced to the same
/// state. `machine_a` syncs last because `machine_b`'s init adds its own scope
/// to the shared scope graph, which reaches `machine_a`'s home config.
pub fn two_synced_machines(harness: &TestHarness) -> (MachineEnvironment, MachineEnvironment) {
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
pub fn seed_shared_apprc(machine_a: &MachineEnvironment, machine_b: &MachineEnvironment) {
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

/// The dotsync invocations a message quotes in backticks, skipping the ones
/// holding a `<placeholder>` for the reader to fill in.
pub fn quoted_dotsync_invocations(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|quoted| quoted.starts_with("dotsync") && !quoted.contains('<'))
        .map(str::to_string)
        .collect()
}

/// A conflict as DESIGN asks for it to be materialized: git-style markers
/// ("Agents have deep priors on this exact format"), the base included, and
/// the sides labelled with the scopes whose changes are colliding rather than
/// with commit ids.
///
/// The labels are asked of the marker lines only, and only that the scope
/// names appear on them — where each one goes, and what else the line says, is
/// jj's materialization and the implementer's.
pub fn assert_materialized_conflict(
    contents: &str,
    colliding_scopes: [&str; 2],
    versions: [&str; 3],
    context: &str,
) {
    for marker in ["<<<<<<<", "|||||||", "=======", ">>>>>>>"] {
        assert!(
            contents.lines().any(|line| line.starts_with(marker)),
            "home holds no `{marker}` line, so the conflict is not in front of the agent at all\n--- home ---\n{contents}\n--- the run that paused ---\n{context}"
        );
    }
    for version in versions {
        assert!(
            contents.contains(version),
            "the materialized conflict is missing `{version}`, and the base is one of the three\n--- home ---\n{contents}"
        );
    }
    let labels: String = contents
        .lines()
        .filter(|line| {
            ["<<<<<<<", "|||||||", "=======", ">>>>>>>"]
                .iter()
                .any(|marker| line.starts_with(marker))
        })
        .collect::<Vec<_>>()
        .join("\n");
    for scope in colliding_scopes {
        assert!(
            labels.contains(scope),
            "the sides have to be labelled with the scopes whose changes are colliding, and `{scope}` is not on any marker line\n--- marker lines ---\n{labels}"
        );
    }
}

/// Removes every machine-local record dotsync keeps beside its repo, so a test
/// can ask whether an answer was derived from the repo or read out of a file.
/// Deliberately says nothing about what it found: a test that asserts the file
/// was there pins the file, and the file is the thing being removed.
pub fn remove_machine_local_records(machine: &MachineEnvironment) {
    for entry in fs::read_dir(&machine.repo_dir).expect("read the repo directory") {
        let entry = entry.expect("read a repo directory entry");
        if entry.file_name().to_string_lossy().starts_with(".dotsync-") {
            fs::remove_file(entry.path()).expect("remove a machine-local record");
        }
    }
}

/// Asserts a run said a scope diverged, in the channel humans read. The
/// wording is the implementer's; that it is said, and about which scope, is
/// not.
pub fn assert_reports_divergence(output: &Output, scope: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.to_lowercase().contains("diverg"),
        "the run has to say the scope diverged rather than leaving it to be discovered\n{}",
        render_output(output)
    );
    assert!(
        stderr.contains(scope),
        "and it has to name `{scope}`\n{}",
        render_output(output)
    );
}

/// Whether a payload says this anywhere — in a field name or in a string
/// value. For facts whose key name nobody has decided yet: the test's business
/// is that the payload carries the fact at all, and pinning a key it does not
/// have would be choosing the schema rather than recording a decision. Field
/// names count because `{"diverged_scopes": ["all"]}` is a payload that says
/// both of the things this is used to ask about.
pub fn payload_says(payload: &serde_json::Value, needle: &str) -> bool {
    match payload {
        serde_json::Value::String(text) => text.to_lowercase().contains(needle),
        serde_json::Value::Array(items) => items.iter().any(|item| payload_says(item, needle)),
        serde_json::Value::Object(fields) => fields.iter().any(|(name, value)| {
            name.to_lowercase().contains(needle) || payload_says(value, needle)
        }),
        _ => false,
    }
}

/// The one line of a rendering that names something, so a test can ask how
/// that thing was rendered rather than whether the text contains it.
pub fn the_line_naming<'a>(text: &'a str, needle: &str) -> &'a str {
    let mut lines = text.lines().filter(|line| line.contains(needle));
    let line = lines
        .next()
        .unwrap_or_else(|| panic!("nothing in the rendering names `{needle}`\n{text}"));
    assert!(
        lines.next().is_none(),
        "`{needle}` is named on more than one line, so there is no single rendering of it to compare\n{text}"
    );
    line
}
