//! Home as jj's working copy, over the managed path set.
//!
//! This module implements jj's `WorkingCopy` and `LockedWorkingCopy` traits
//! with `$HOME` (a sandboxed stand-in in tests) as the working directory and
//! the managed paths as the tracked set. `Home` constructs these types
//! directly, so there is no `WorkingCopyFactory`: nothing loads this working
//! copy through jj's own loader, and a factory that no loader is registered
//! with would be a claim rather than a capability.
//!
//! Three properties are load-bearing, and every method holds them:
//!
//! - **No third copy, no scanning.** Snapshot probes exactly the tracked
//!   paths and streams bytes into the store; check-out writes exactly the
//!   paths that differ. Nothing ever walks `$HOME`.
//! - **Materialized trees are always resolved.** Under the no-markers rule a
//!   conflicted tree is never written into home, so `check_out` refuses one,
//!   and the persisted state records a single tree id rather than a merge.
//! - **Symlinks are never followed.** A path whose parent chain contains a
//!   symlink is an error, in both directions. A path that *is* a symlink is
//!   an entry whose content is its target string.
//!
//! Locking is this module's job (the traits leave it to the implementation,
//! and jj's own local working copy does the same thing): `start_mutation`
//! takes a file lock beside the persisted state, so two dotsync runs on one
//! machine serialize at the working copy instead of corrupting each other.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use jj_lib::backend::{CopyId, TreeId, TreeValue};
use jj_lib::commit::Commit;
use jj_lib::lock::FileLock;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::store::Store;
use jj_lib::working_copy::{
    CheckoutError, CheckoutStats, LockedWorkingCopy, ResetError, SnapshotError, SnapshotOptions,
    SnapshotStats, WorkingCopy, WorkingCopyStateError,
};

pub(crate) const WORKING_COPY_TYPE: &str = "dotsync-home";
const STATE_FILE: &str = "dotsync-home.json";
const LOCK_FILE: &str = "working_copy.lock";

/// The persisted working-copy record: which operation the working copy was
/// last synced to, and the single (always resolved) tree last materialized.
///
/// This is jj's own working-copy protocol, not a second `sync-state.json`:
/// the *authoritative* position (the wc commit) lives in the repo view and
/// moves atomically with every operation; this file exists so
/// `WorkingCopyFreshness::check_stale` can tell whether home has caught up
/// with the view, and its staleness has a defined answer (`recover`).
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedState {
    workspace: String,
    operation_id: String,
    tree_id: String,
}

fn state_error(
    message: impl Into<String>,
    err: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> WorkingCopyStateError {
    WorkingCopyStateError {
        message: message.into(),
        err: err.into(),
    }
}

fn snapshot_error(
    message: impl Into<String>,
    err: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> SnapshotError {
    SnapshotError::Other {
        message: message.into(),
        err: err.into(),
    }
}

fn checkout_error(
    message: impl Into<String>,
    err: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> CheckoutError {
    CheckoutError::Other {
        message: message.into(),
        err: err.into(),
    }
}

/// Resolves a repo path to its on-disk location without ever passing through
/// a symlink, and refuses paths inside dotsync's own repo directory.
///
/// Following a symlinked parent is how a managed write escapes the managed
/// set (the `selflink` sweep, PLAN §1.5), and writing under the repo root is
/// how a sync clobbers dotsync's own `git_target` (PLAN §2.2). Both guards
/// live here — the single place every home read and write goes through — so
/// no caller can forget them.
fn home_disk_path(home: &Path, repo_root: &Path, path: &RepoPathBuf) -> Result<PathBuf, String> {
    let mut disk = home.to_path_buf();
    let components: Vec<&str> = path.as_internal_file_string().split('/').collect();
    for (index, component) in components.iter().enumerate() {
        disk.push(component);
        let is_last = index == components.len() - 1;
        if !is_last {
            match std::fs::symlink_metadata(&disk) {
                Ok(md) if md.file_type().is_symlink() => {
                    return Err(format!(
                        "{} traverses a symlink at {}: dotsync never follows links",
                        path.as_internal_file_string(),
                        disk.display()
                    ));
                }
                _ => {}
            }
        }
    }
    if disk.starts_with(repo_root) {
        return Err(format!(
            "{} is inside dotsync's own repository and cannot be managed",
            path.as_internal_file_string()
        ));
    }
    Ok(disk)
}

/// What is on disk at a managed path, as a tree entry: a file (with its
/// executable bit), a symlink (whose content is its target string), or
/// absent. Directories are not entries — a managed path that is a directory
/// on disk differs *in kind* from any file entry, which the merge machinery
/// then reports; snapshot represents it as absent plus an untracked-kind
/// error at the site that cares.
async fn read_disk_entry(
    store: &Arc<Store>,
    path: &RepoPathBuf,
    disk: &Path,
) -> Result<Merge<Option<TreeValue>>, SnapshotError> {
    let metadata = match std::fs::symlink_metadata(disk) {
        Err(_) => return Ok(Merge::absent()),
        Ok(md) => md,
    };
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(disk)
            .map_err(|err| snapshot_error(format!("read link {}", disk.display()), err))?;
        let target = target
            .to_str()
            .ok_or_else(|| SnapshotError::InvalidUtf8SymlinkTarget {
                path: disk.to_path_buf(),
            })?;
        let id = store.write_symlink(path.as_ref(), target).await?;
        return Ok(Merge::normal(TreeValue::Symlink(id)));
    }
    if metadata.is_dir() {
        // A directory where a file is tracked: not representable as content.
        // Absent is the honest tree entry; the kind difference surfaces as
        // "deleted here" until the model grows a directory answer, which is
        // the same behaviour the old classifier had.
        return Ok(Merge::absent());
    }
    if !metadata.is_file() {
        // A fifo, a socket, a device. Refused rather than read, because
        // reading one can never return: `fs::read` on a fifo blocks until
        // something writes to the other end, and nothing ever does. The
        // caller checks for these first so the reader gets dotsync's own
        // explanation; this makes the hang unrepresentable whatever the
        // caller did.
        return Err(snapshot_error(
            irregular_file_message(disk),
            "unsupported file kind",
        ));
    }
    let bytes = std::fs::read(disk)
        .map_err(|err| snapshot_error(format!("read file {}", disk.display()), err))?;
    let executable = file_is_executable(&metadata);
    let mut reader = bytes.as_slice();
    let file_id = store.write_file(path.as_ref(), &mut reader).await?;
    Ok(Merge::normal(TreeValue::File {
        id: file_id,
        executable,
        copy_id: CopyId::placeholder(),
    }))
}

#[cfg(unix)]
fn file_is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn file_is_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// The paths a tree holds, which is the definition of "managed" — the
/// tracked set is derived from repo state, never stored.
fn tree_paths(tree: &MergedTree) -> Result<Vec<RepoPathBuf>, WorkingCopyStateError> {
    let mut paths = Vec::new();
    for (path, value) in tree.entries() {
        value.map_err(|err| state_error(format!("read tree entry {path:?}"), err))?;
        paths.push(path);
    }
    Ok(paths)
}

pub(crate) struct HomeWorkingCopy {
    store: Arc<Store>,
    home: PathBuf,
    repo_root: PathBuf,
    state_path: PathBuf,
    workspace_name: WorkspaceNameBuf,
    operation_id: OperationId,
    tree: MergedTree,
    /// Derived from `tree` at load; cached because the trait hands out `&[_]`.
    tracked: Vec<RepoPathBuf>,
}

impl HomeWorkingCopy {
    /// Creates the persisted state for a working copy that has materialized
    /// nothing yet: the empty tree, at the given operation.
    pub(crate) fn init(
        store: Arc<Store>,
        home: PathBuf,
        repo_root: PathBuf,
        state_path: PathBuf,
        operation_id: OperationId,
        workspace_name: WorkspaceNameBuf,
    ) -> Result<Self, WorkingCopyStateError> {
        let tree = MergedTree::resolved(store.clone(), store.empty_tree_id().clone());
        let wc = Self {
            store,
            home,
            repo_root,
            state_path,
            workspace_name,
            operation_id,
            tree,
            tracked: Vec::new(),
        };
        wc.persist()?;
        Ok(wc)
    }

    pub(crate) fn load(
        store: Arc<Store>,
        home: PathBuf,
        repo_root: PathBuf,
        state_path: PathBuf,
    ) -> Result<Self, WorkingCopyStateError> {
        let state_file = state_path.join(STATE_FILE);
        let raw = std::fs::read_to_string(&state_file)
            .map_err(|err| state_error(format!("read {}", state_file.display()), err))?;
        let state: PersistedState = serde_json::from_str(&raw)
            .map_err(|err| state_error(format!("parse {}", state_file.display()), err))?;
        let operation_id = OperationId::try_from_hex(&state.operation_id)
            .ok_or_else(|| state_error("parse working-copy state", "invalid operation id"))?;
        let tree_id = TreeId::try_from_hex(&state.tree_id)
            .ok_or_else(|| state_error("parse working-copy state", "invalid tree id"))?;
        let tree = MergedTree::resolved(store.clone(), tree_id);
        let tracked = tree_paths(&tree)?;
        Ok(Self {
            store,
            home,
            repo_root,
            state_path,
            workspace_name: WorkspaceNameBuf::from(state.workspace),
            operation_id,
            tree,
            tracked,
        })
    }

    fn persist(&self) -> Result<(), WorkingCopyStateError> {
        let tree_id = self
            .tree
            .tree_ids()
            .as_resolved()
            .expect("a materialized tree is always resolved")
            .hex();
        let state = PersistedState {
            workspace: self.workspace_name.as_str().to_owned(),
            operation_id: self.operation_id.hex(),
            tree_id,
        };
        std::fs::create_dir_all(&self.state_path)
            .map_err(|err| state_error(format!("create {}", self.state_path.display()), err))?;
        let state_file = self.state_path.join(STATE_FILE);
        let raw = serde_json::to_string_pretty(&state)
            .map_err(|err| state_error("serialize working-copy state", err))?;
        std::fs::write(&state_file, raw)
            .map_err(|err| state_error(format!("write {}", state_file.display()), err))?;
        Ok(())
    }
}

impl WorkingCopy for HomeWorkingCopy {
    fn name(&self) -> &str {
        WORKING_COPY_TYPE
    }

    fn workspace_name(&self) -> &WorkspaceName {
        &self.workspace_name
    }

    fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    fn tree(&self) -> Result<&MergedTree, WorkingCopyStateError> {
        Ok(&self.tree)
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        Ok(&self.tracked)
    }

    fn start_mutation(&self) -> Result<Box<dyn LockedWorkingCopy>, WorkingCopyStateError> {
        Ok(Box::new(self.start_mutation_concrete()?))
    }
}

impl HomeWorkingCopy {
    /// The concrete version of `start_mutation`, because dotsync constructs
    /// this type itself and the session wants the concrete methods
    /// (`probe_also`) that the trait cannot carry.
    pub(crate) fn start_mutation_concrete(
        &self,
    ) -> Result<HomeLockedWorkingCopy, WorkingCopyStateError> {
        std::fs::create_dir_all(&self.state_path)
            .map_err(|err| state_error(format!("create {}", self.state_path.display()), err))?;
        let lock = FileLock::lock(self.state_path.join(LOCK_FILE))
            .map_err(|err| state_error("lock the working copy", err))?;
        // Re-read the state after taking the lock: another run may have
        // finished between our load and our lock.
        let current = Self::load(
            self.store.clone(),
            self.home.clone(),
            self.repo_root.clone(),
            self.state_path.clone(),
        )?;
        Ok(HomeLockedWorkingCopy {
            store: current.store,
            home: current.home,
            repo_root: current.repo_root,
            state_path: current.state_path,
            workspace_name: current.workspace_name,
            old_operation_id: current.operation_id,
            old_tree: current.tree.clone(),
            tree: current.tree,
            probe: current.tracked,
            _lock: lock,
        })
    }
}

pub(crate) struct HomeLockedWorkingCopy {
    store: Arc<Store>,
    home: PathBuf,
    repo_root: PathBuf,
    state_path: PathBuf,
    workspace_name: WorkspaceNameBuf,
    old_operation_id: OperationId,
    old_tree: MergedTree,
    tree: MergedTree,
    /// The paths snapshot examines: the materialized tree's paths, plus
    /// whatever the session adds via `probe_also` (the mark's paths, so a
    /// file deleted from home and then recreated is still seen until the
    /// deletion is committed).
    probe: Vec<RepoPathBuf>,
    _lock: FileLock,
}

impl HomeLockedWorkingCopy {
    /// Adds paths to the set snapshot examines, and says whether any of them
    /// were new — so a caller can tell a widening that needs another read of
    /// home from one that changes nothing.
    ///
    /// The working copy itself only knows what it materialized; the caller also
    /// knows the mark (the wc commit's parent) and the head it is heading for,
    /// and calls this with their paths.
    pub(crate) fn probe_also(&mut self, paths: impl IntoIterator<Item = RepoPathBuf>) -> bool {
        let mut widened = false;
        for path in paths {
            if !self.probe.contains(&path) {
                self.probe.push(path);
                widened = true;
            }
        }
        widened
    }

    /// Probed paths that home holds as something with no content to read: a
    /// fifo, a socket, a device.
    ///
    /// Asked before `snapshot`, so the answer is dotsync's own explanation of
    /// what such a path is rather than a failure from inside a read. A tracked
    /// file can be replaced by one of these at any time, and every run reads
    /// every tracked path, so this is on the ordinary path rather than only
    /// where a command names one.
    pub(crate) fn irregular_home_paths(&self) -> Vec<PathBuf> {
        self.probe
            .iter()
            .filter_map(|path| home_disk_path(&self.home, &self.repo_root, path).ok())
            .filter(|disk| {
                std::fs::symlink_metadata(disk).is_ok_and(|metadata| {
                    let kind = metadata.file_type();
                    !kind.is_file() && !kind.is_symlink() && !kind.is_dir()
                })
            })
            .collect()
    }
}

/// What a path with no readable content is, in the words dotsync explains it
/// with. Shared so the refusal reads the same whether it was reached before a
/// snapshot or inside one.
pub(crate) fn irregular_file_message(disk: &Path) -> String {
    format!(
        "{} is not a regular file, so dotsync cannot record what it holds",
        disk.display()
    )
}

#[async_trait]
impl LockedWorkingCopy for HomeLockedWorkingCopy {
    fn old_operation_id(&self) -> &OperationId {
        &self.old_operation_id
    }

    fn old_tree(&self) -> &MergedTree {
        &self.old_tree
    }

    /// Disk -> tree, over exactly the probe set. File contents stream into
    /// the store; nothing is copied anywhere else.
    ///
    /// The internal tree becomes the snapshot: it records what is on disk —
    /// which a snapshot just made true — not what any transaction later does
    /// with the result. `finish` persists it and `check_out` diffs against
    /// it, and both of those want disk truth.
    async fn snapshot(
        &mut self,
        _options: &SnapshotOptions,
    ) -> Result<(MergedTree, SnapshotStats), SnapshotError> {
        let mut builder = MergedTreeBuilder::new(self.tree.clone());
        for path in &self.probe {
            let disk = home_disk_path(&self.home, &self.repo_root, path)
                .map_err(|message| snapshot_error(message, "refused path"))?;
            let value = read_disk_entry(&self.store, path, &disk).await?;
            builder.set_or_remove(path.clone(), value);
        }
        let tree = builder.write_tree().await?;
        self.tree = tree.clone();
        Ok((tree, SnapshotStats::default()))
    }

    /// Tree -> disk. Only resolved trees, only the paths that differ, and
    /// writes replace rather than write through.
    async fn check_out(&mut self, commit: &Commit) -> Result<CheckoutStats, CheckoutError> {
        let new_tree = commit.tree();
        if new_tree.has_conflict() {
            return Err(checkout_error(
                "a conflicted tree is never materialized into home; present it instead",
                "conflicted tree at check_out",
            ));
        }
        let mut stats = CheckoutStats::default();
        let mut paths = tree_paths(&self.tree).map_err(CheckoutError::WorkingCopyStateError)?;
        for path in tree_paths(&new_tree).map_err(CheckoutError::WorkingCopyStateError)? {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        for path in paths {
            let old_value = self
                .tree
                .path_value(path.as_ref())?
                .as_resolved()
                .cloned()
                .flatten();
            let new_value = new_tree
                .path_value(path.as_ref())?
                .as_resolved()
                .cloned()
                .flatten();
            if old_value == new_value {
                continue;
            }
            let disk = home_disk_path(&self.home, &self.repo_root, &path)
                .map_err(|message| checkout_error(message, "refused path"))?;
            let existed = std::fs::symlink_metadata(&disk).is_ok();
            match new_value {
                None => {
                    if existed {
                        std::fs::remove_file(&disk).map_err(|err| {
                            checkout_error(format!("remove {}", disk.display()), err)
                        })?;
                        stats.removed_files += 1;
                    }
                }
                Some(value) => {
                    if let Some(parent) = disk.parent() {
                        std::fs::create_dir_all(parent).map_err(|err| {
                            checkout_error(format!("create directory {}", parent.display()), err)
                        })?;
                    }
                    write_disk_entry(&self.store, &path, &disk, existed, &value).await?;
                    if existed {
                        stats.updated_files += 1;
                    } else {
                        stats.added_files += 1;
                    }
                }
            }
        }
        self.tree = new_tree;
        Ok(stats)
    }

    fn rename_workspace(&mut self, new_workspace_name: WorkspaceNameBuf) {
        self.workspace_name = new_workspace_name;
    }

    async fn reset(&mut self, commit: &Commit) -> Result<(), ResetError> {
        self.tree = commit.tree();
        Ok(())
    }

    async fn recover(&mut self, commit: &Commit) -> Result<(), ResetError> {
        self.tree = commit.tree();
        Ok(())
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        Ok(&self.probe)
    }

    async fn set_sparse_patterns(
        &mut self,
        new_sparse_patterns: Vec<RepoPathBuf>,
    ) -> Result<CheckoutStats, CheckoutError> {
        self.probe = new_sparse_patterns;
        Ok(CheckoutStats::default())
    }

    async fn finish(
        self: Box<Self>,
        operation_id: OperationId,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        let tracked = tree_paths(&self.tree)?;
        let wc = HomeWorkingCopy {
            store: self.store,
            home: self.home,
            repo_root: self.repo_root,
            state_path: self.state_path,
            workspace_name: self.workspace_name,
            operation_id,
            tree: self.tree,
            tracked,
        };
        wc.persist()?;
        Ok(Box::new(wc))
    }
}

/// Writes one resolved tree entry to disk. Replaces whatever is there rather
/// than writing through it, chmods only when the executable bit differs from
/// what a fresh write produces, and writes bytes only when they differ.
async fn write_disk_entry(
    store: &Arc<Store>,
    path: &RepoPathBuf,
    disk: &Path,
    existed: bool,
    value: &TreeValue,
) -> Result<(), CheckoutError> {
    match value {
        TreeValue::File { id, executable, .. } => {
            let mut reader = store.read_file(path.as_ref(), id).await?;
            let mut contents = Vec::new();
            use tokio::io::AsyncReadExt as _;
            reader
                .read_to_end(&mut contents)
                .await
                .map_err(|err| checkout_error(format!("read store file {path:?}"), err))?;
            // A symlink (or anything that isn't a plain file) at this path is
            // removed first so the bytes land at the path itself.
            let plain_file_here = std::fs::symlink_metadata(disk)
                .map(|md| md.is_file())
                .unwrap_or(false);
            if existed && !plain_file_here {
                std::fs::remove_file(disk)
                    .map_err(|err| checkout_error(format!("replace {}", disk.display()), err))?;
            }
            let already = std::fs::symlink_metadata(disk).is_ok();
            let same_bytes = already && std::fs::read(disk).ok().as_deref() == Some(&contents);
            if !same_bytes {
                std::fs::write(disk, &contents)
                    .map_err(|err| checkout_error(format!("write {}", disk.display()), err))?;
            }
            set_executable(disk, *executable)?;
        }
        TreeValue::Symlink(id) => {
            let target = store.read_symlink(path.as_ref(), id).await?;
            if existed {
                std::fs::remove_file(disk)
                    .map_err(|err| checkout_error(format!("replace {}", disk.display()), err))?;
            }
            make_symlink(&target, disk)?;
        }
        other => {
            return Err(checkout_error(
                format!("unsupported tree entry {other:?} at {path:?}"),
                "unsupported tree entry kind",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(disk: &Path, executable: bool) -> Result<(), CheckoutError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(disk, std::fs::Permissions::from_mode(mode))
        .map_err(|err| checkout_error(format!("chmod {}", disk.display()), err))
}

#[cfg(not(unix))]
fn set_executable(_disk: &Path, _executable: bool) -> Result<(), CheckoutError> {
    Ok(())
}

#[cfg(unix)]
fn make_symlink(target: &str, disk: &Path) -> Result<(), CheckoutError> {
    std::os::unix::fs::symlink(target, disk)
        .map_err(|err| checkout_error(format!("symlink {}", disk.display()), err))
}

#[cfg(not(unix))]
fn make_symlink(_target: &str, disk: &Path) -> Result<(), CheckoutError> {
    Err(checkout_error(
        "symlinks are not materialized on this platform",
        "symlink on non-unix platform",
    ))
}
