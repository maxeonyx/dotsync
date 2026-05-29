use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::merge::Merge;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;

use crate::cascade::{
    build_cascade_plan, execute_cascade_plan, CascadeCommand, CascadeOutcome, ScopeHeads,
};
use crate::config::{internal_repo_paths, load_config, DotsyncPaths};
use crate::error::DotsyncError;
use crate::machine::detect_machine;
use crate::repo::{
    collect_managed_tree_entries, fetch_origin, load_repo_direct, load_scope_commit,
    push_scope_updates, read_tree_entry_bytes,
};
use crate::sync::{SyncOptions, SyncReport};

#[derive(Debug, Clone)]
pub struct CommitOptions {
    pub scope: String,
    pub message: String,
    pub force: bool,
    pub selection: CommitSelection,
}

#[derive(Debug, Clone)]
pub enum CommitSelection {
    All,
    Paths(Vec<PathBuf>),
}

#[derive(Debug, Clone, Default)]
pub struct CommitReport {
    pub committed_scope: String,
    pub cascaded_scopes: Vec<String>,
    pub sync: SyncReport,
}

#[derive(Debug, Clone, Default)]
pub struct ContinueReport {
    pub cascaded_scopes: Vec<String>,
    pub sync: SyncReport,
}

#[derive(Debug, Clone)]
pub enum CommandOutcome<T> {
    Success(T),
}

pub async fn commit_and_sync(
    paths: &DotsyncPaths,
    options: CommitOptions,
) -> Result<CommandOutcome<CommitReport>, DotsyncError> {
    let repo = load_repo_direct(paths).await?;
    let repo = fetch_origin(repo).await?;
    let config = load_config(paths).await?;
    let graph = config.graph.clone();

    if !graph.parents.contains_key(&options.scope) {
        return Err(DotsyncError::InvalidScope {
            scope: options.scope.clone(),
        });
    }

    let internal_paths = internal_repo_paths(&config);
    let machine_scope = detect_machine()?.machine_scope;
    if !graph.parents.contains_key(&machine_scope) {
        return Err(DotsyncError::NoCurrentScope);
    }

    let old_machine_commit = load_scope_commit(repo.as_ref(), &machine_scope)?;
    let machine_entries = load_current_machine_entries(repo.as_ref(), &graph, &internal_paths).await?;
    let target_entries = load_scope_entries(repo.as_ref(), &options.scope, &internal_paths)?;
    let selected_paths = select_commit_paths(
        paths,
        repo.as_ref(),
        &options.selection,
        &target_entries,
        &internal_paths,
    )
    .await?;

    if selected_paths.is_empty() {
        if matches!(options.selection, CommitSelection::Paths(_))
            && home_has_unmanaged_files(paths, &machine_entries, &internal_paths)?
        {
            return Err(DotsyncError::NotImplemented(
                "scoped commit is not available until home-diff commit flow lands",
            ));
        }
        return Ok(CommandOutcome::Success(CommitReport::default()));
    }

    let mut tx = repo.start_transaction();
    let mut scope_heads = ScopeHeads::load_existing(tx.repo_mut().base_repo(), &graph)?;
    let base_commit = scope_heads.require(&options.scope)?;
    let base_tree = base_commit.tree();
    let mut builder = MergedTreeBuilder::new(base_tree.clone());

    for relative in &selected_paths {
        apply_home_path_to_tree(tx.repo_mut(), paths, relative, &mut builder).await?;
    }

    let new_tree = builder.write_tree().await.map_err(|err| DotsyncError::Jj {
        message: format!("write commit tree for {}: {err}", options.scope),
    })?;

    if new_tree.tree_ids() == base_tree.tree_ids() {
        return Ok(CommandOutcome::Success(CommitReport::default()));
    }

    let new_commit = tx
        .repo_mut()
        .new_commit(vec![base_commit.id().clone()], new_tree)
        .set_description(&options.message)
        .write()
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!("write commit for {}: {err}", options.scope),
        })?;
    tx.repo_mut().set_local_bookmark_target(
        RefNameBuf::from(options.scope.as_str()).as_ref(),
        RefTarget::normal(new_commit.id().clone()),
    );
    scope_heads.update(options.scope.clone(), new_commit);

    let cascade_command = CascadeCommand {
        root_scope: options.scope.clone(),
        description: format!("dotsync: cascade from {}", options.scope),
    };
    let plan = build_cascade_plan(&graph, &scope_heads, &cascade_command);
    let cascaded_scopes = match execute_cascade_plan(
        tx.repo_mut(),
        &mut scope_heads,
        &plan,
        &cascade_command,
    )
    .await?
    {
        CascadeOutcome::Completed(success) => success.progress.completed_scopes,
        CascadeOutcome::Paused {
            scope: _,
            conflicted_files: _,
        } => {
            return Err(DotsyncError::NotImplemented(
                "cascade conflict resolution",
            ));
        }
    };

    let expected_changes = expected_machine_changes(
        tx.repo_mut(),
        &old_machine_commit,
        &scope_heads.require(&machine_scope)?,
        &internal_paths,
    )
    .await?;

    tx.commit("dotsync: commit and cascade")
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!("commit scoped change for {}: {err}", options.scope),
        })?;

    let sync = crate::sync::sync_repo_to_home(
        paths,
        SyncOptions {
            force: options.force,
        },
        &expected_changes,
        Some(&machine_scope),
    )
    .await?;
    push_scope_updates(paths).await?;

    Ok(CommandOutcome::Success(CommitReport {
        committed_scope: options.scope,
        cascaded_scopes,
        sync,
    }))
}

async fn load_current_machine_entries(
    repo: &dyn jj_lib::repo::Repo,
    graph: &crate::scope_graph::ScopeGraph,
    internal_paths: &std::collections::BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, TreeValue>, DotsyncError> {
    let machine_scope = detect_machine()?.machine_scope;
    if !graph.parents.contains_key(&machine_scope) {
        return Err(DotsyncError::NoCurrentScope);
    }
    let machine_commit = load_scope_commit(repo, &machine_scope)?;
    collect_managed_tree_entries(&machine_commit.tree(), internal_paths)
}

fn load_scope_entries(
    repo: &dyn jj_lib::repo::Repo,
    scope: &str,
    internal_paths: &std::collections::BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, TreeValue>, DotsyncError> {
    let commit = load_scope_commit(repo, scope)?;
    collect_managed_tree_entries(&commit.tree(), internal_paths)
}

async fn select_commit_paths(
    paths: &DotsyncPaths,
    repo: &dyn jj_lib::repo::Repo,
    selection: &CommitSelection,
    target_entries: &BTreeMap<PathBuf, TreeValue>,
    internal_paths: &BTreeSet<PathBuf>,
) -> Result<Vec<PathBuf>, DotsyncError> {
    match selection {
        CommitSelection::Paths(selection_paths) if !selection_paths.is_empty() => {
            expand_selection_paths(paths, selection_paths, target_entries, internal_paths)
        }
        CommitSelection::Paths(_) => detect_changed_managed_paths(paths, repo, target_entries).await,
        CommitSelection::All => {
            // `--all` intentionally means "all currently managed files on the target scope".
            // We compare that tracked set against `~/` so modifications and deletions are
            // committed, but we do not scan all of home looking for unrelated new files.
            // New files must be opted into with explicit paths.
            Ok(target_entries.keys().cloned().collect())
        }
    }
}

async fn detect_changed_managed_paths(
    paths: &DotsyncPaths,
    repo: &dyn jj_lib::repo::Repo,
    target_entries: &BTreeMap<PathBuf, TreeValue>,
) -> Result<Vec<PathBuf>, DotsyncError> {
    let mut changed = Vec::new();
    for (relative, value) in target_entries {
        let repo_bytes = read_tree_entry_bytes(repo.store(), relative, value).await?;
        let home_path = paths.home_dir.join(relative);
        let home_bytes = match fs::read(&home_path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(DotsyncError::Io {
                    path: home_path,
                    source,
                });
            }
        };
        if home_bytes.as_deref() != Some(repo_bytes.as_slice()) {
            changed.push(relative.clone());
        }
    }
    Ok(changed)
}

fn expand_selection_paths(
    paths: &DotsyncPaths,
    selection_paths: &[PathBuf],
    target_entries: &BTreeMap<PathBuf, TreeValue>,
    internal_paths: &BTreeSet<PathBuf>,
) -> Result<Vec<PathBuf>, DotsyncError> {
    let mut expanded = BTreeSet::new();
    // Relative path of the repo root within home — reject anything under it
    let repo_relative = paths
        .repo_root
        .strip_prefix(&paths.home_dir)
        .ok()
        .map(|p| p.to_path_buf());

    for selection_path in selection_paths {
        if internal_paths.contains(selection_path) {
            continue;
        }
        // Reject paths that are inside or equal to the repo directory
        if let Some(ref repo_rel) = repo_relative {
            if selection_path.starts_with(repo_rel) {
                continue;
            }
        }

        let home_path = paths.home_dir.join(selection_path);
        let is_directory_selection = home_path.is_dir()
            || target_entries
                .keys()
                .any(|candidate| candidate != selection_path && path_has_prefix(candidate, selection_path));
        if is_directory_selection {
            if home_path.exists() {
                collect_home_directory_files(
                    &paths.home_dir,
                    &home_path,
                    &mut expanded,
                    internal_paths,
                    &paths.repo_root,
                )?;
            }
            expanded.extend(
                target_entries
                    .keys()
                    .filter(|candidate| path_has_prefix(candidate, selection_path))
                    .cloned(),
            );
        } else {
            expanded.insert(selection_path.clone());
        }
    }

    Ok(expanded.into_iter().collect())
}

fn collect_home_directory_files(
    home_root: &Path,
    current: &Path,
    expanded: &mut BTreeSet<PathBuf>,
    internal_paths: &BTreeSet<PathBuf>,
    repo_root: &Path,
) -> Result<(), DotsyncError> {
    for entry in fs::read_dir(current).map_err(|source| DotsyncError::Io {
        path: current.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| DotsyncError::Io {
            path: current.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| DotsyncError::Io {
            path: path.clone(),
            source,
        })?;

        if file_type.is_dir() {
            // Never recurse into the dotsync repo directory itself
            if path.starts_with(repo_root) {
                continue;
            }
            collect_home_directory_files(home_root, &path, expanded, internal_paths, repo_root)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let relative = path.strip_prefix(home_root).map_err(|source| DotsyncError::Jj {
            message: format!(
                "failed to make home path {} relative to {}: {source}",
                path.display(),
                home_root.display()
            ),
        })?;
        let relative = relative.to_path_buf();
        if internal_paths.contains(&relative) {
            continue;
        }
        expanded.insert(relative);
    }

    Ok(())
}

fn path_has_prefix(path: &Path, prefix: &Path) -> bool {
    path == prefix || path.starts_with(prefix)
}

async fn apply_home_path_to_tree(
    mut_repo: &mut jj_lib::repo::MutableRepo,
    paths: &DotsyncPaths,
    relative: &Path,
    builder: &mut MergedTreeBuilder,
) -> Result<(), DotsyncError> {
    let relative_str = relative.to_str().ok_or(DotsyncError::NotImplemented(
        "non-utf8 repo paths are not supported yet",
    ))?;
    let repo_path = RepoPathBuf::from_internal_string(relative_str).map_err(|err| DotsyncError::Jj {
        message: format!("invalid repo path {}: {err}", relative.display()),
    })?;

    let home_path = paths.home_dir.join(relative);
    if home_path.exists() {
        let bytes = fs::read(&home_path).map_err(|source| DotsyncError::Io {
            path: home_path,
            source,
        })?;
        let mut reader = bytes.as_slice();
        let file_id = mut_repo
            .store()
            .write_file(repo_path.as_ref(), &mut reader)
            .await
            .map_err(|err| DotsyncError::Jj {
                message: format!("write repo file {}: {err}", relative.display()),
            })?;
        builder.set_or_remove(
            repo_path,
            Merge::normal(TreeValue::File {
                id: file_id,
                executable: false,
                copy_id: CopyId::placeholder(),
            }),
        );
    } else {
        builder.set_or_remove(repo_path, Merge::absent());
    }

    Ok(())
}

async fn expected_machine_changes(
    repo: &dyn jj_lib::repo::Repo,
    old_machine_commit: &jj_lib::commit::Commit,
    new_machine_commit: &jj_lib::commit::Commit,
    internal_paths: &std::collections::BTreeSet<PathBuf>,
) -> Result<Vec<PathBuf>, DotsyncError> {
    let old_entries = collect_managed_tree_entries(&old_machine_commit.tree(), internal_paths)?;
    let new_entries = collect_managed_tree_entries(&new_machine_commit.tree(), internal_paths)?;
    let mut all_paths = old_entries
        .keys()
        .chain(new_entries.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut changed = Vec::new();

    for path in all_paths.iter() {
        let old_bytes = match old_entries.get(path) {
            Some(value) => Some(read_tree_entry_bytes(repo.store(), path, value).await?),
            None => None,
        };
        let new_bytes = match new_entries.get(path) {
            Some(value) => Some(read_tree_entry_bytes(repo.store(), path, value).await?),
            None => None,
        };
        if old_bytes != new_bytes {
            changed.push(path.clone());
        }
    }

    all_paths.clear();
    Ok(changed)
}

fn home_has_unmanaged_files(
    paths: &DotsyncPaths,
    machine_entries: &BTreeMap<PathBuf, TreeValue>,
    internal_paths: &std::collections::BTreeSet<PathBuf>,
) -> Result<bool, DotsyncError> {
    home_dir_has_unmanaged_files(
        &paths.home_dir,
        &paths.repo_root,
        &paths.home_dir,
        machine_entries,
        internal_paths,
    )
}

fn home_dir_has_unmanaged_files(
    root: &Path,
    repo_root: &Path,
    current: &Path,
    machine_entries: &BTreeMap<PathBuf, TreeValue>,
    internal_paths: &std::collections::BTreeSet<PathBuf>,
) -> Result<bool, DotsyncError> {
    for entry in fs::read_dir(current).map_err(|source| DotsyncError::Io {
        path: current.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| DotsyncError::Io {
            path: current.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| DotsyncError::Io {
            path: path.clone(),
            source,
        })?;

        if path.starts_with(repo_root) {
            continue;
        }

        if file_type.is_dir() {
            if home_dir_has_unmanaged_files(root, repo_root, &path, machine_entries, internal_paths)? {
                return Ok(true);
            }
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let relative = path.strip_prefix(root).map_err(|source| DotsyncError::Jj {
            message: format!(
                "failed to make home path {} relative to {}: {source}",
                path.display(),
                root.display()
            ),
        })?;
        let relative = relative.to_path_buf();

        if relative
            .components()
            .any(|component| component.as_os_str().to_string_lossy().contains(".ignore"))
        {
            continue;
        }

        if internal_paths.contains(&relative) {
            continue;
        }
        if !machine_entries.contains_key(&relative) {
            return Ok(true);
        }
    }

    Ok(false)
}

pub async fn continue_after_conflict(
    _paths: &DotsyncPaths,
    _options: SyncOptions,
) -> Result<CommandOutcome<ContinueReport>, DotsyncError> {
    Err(DotsyncError::NotImplemented(
        "continue is not available until home-diff commit flow lands",
    ))
}
