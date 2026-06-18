use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jj_lib::backend::CommitId;
use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::merge::Merge;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::rewrite::merge_commit_trees;

use crate::cascade::{
    build_cascade_plan, execute_cascade_plan, CascadeCommand, CascadeOutcome, CascadeStep,
    ScopeHeads,
};
use crate::config::{internal_repo_paths, load_config, DotsyncPaths};
use crate::error::DotsyncError;

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

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct PausedCascadeState {
    machine_scope: String,
    paused_scope: String,
    parent_commit_ids: Vec<String>,
    conflicted_files: Vec<PathBuf>,
    remaining_steps: Vec<PausedCascadeStep>,
    description: String,
    #[serde(default)]
    original_scope_commit_ids: BTreeMap<String, String>,
    #[serde(default)]
    abort_restore_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct PausedCascadeStep {
    scope: String,
    parent_scopes: Vec<String>,
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

#[derive(Debug, Clone, Default)]
pub struct AbortReport {
    pub aborted_scope: String,
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
    reject_commit_if_cascade_paused(paths)?;

    let pre_fetch_repo = load_repo_direct(paths).await?;
    let repo = fetch_origin(pre_fetch_repo.clone()).await?;
    let config = load_config(paths).await?;
    let graph = config.graph.clone();

    if !graph.parents.contains_key(&options.scope) {
        return Err(DotsyncError::InvalidScope {
            scope: options.scope.clone(),
        });
    }

    let internal_paths = internal_repo_paths(&config);
    let sync_state = crate::sync::load_sync_state(paths, &config)?;
    let machine_scope = crate::sync::resolve_current_scope(&config, sync_state.as_ref(), None)?;

    let old_machine_commit = load_scope_commit(repo.as_ref(), &machine_scope)?;
    let machine_entries =
        load_current_machine_entries(repo.as_ref(), &machine_scope, &internal_paths).await?;
    let pre_fetch_target_entries =
        load_scope_entries(pre_fetch_repo.as_ref(), &options.scope, &internal_paths)?;
    let target_entries = load_scope_entries(repo.as_ref(), &options.scope, &internal_paths)?;
    let selected_paths = select_commit_paths(
        paths,
        repo.as_ref(),
        &options.selection,
        &target_entries,
        &internal_paths,
    )
    .await?;

    let stale_selected_paths = stale_selected_scope_paths(
        paths,
        repo.as_ref(),
        &pre_fetch_target_entries,
        &target_entries,
        &selected_paths,
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
    let original_scope_commit_ids = scope_heads.commit_ids_by_scope();
    let base_commit = scope_heads.require(&options.scope)?;
    let new_commit = if stale_selected_paths.is_empty() {
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

        tx.repo_mut()
            .new_commit(vec![base_commit.id().clone()], new_tree)
            .set_description(&options.message)
            .write()
            .await
            .map_err(|err| DotsyncError::Jj {
                message: format!("write commit for {}: {err}", options.scope),
            })?
    } else {
        let local_base_commit = load_scope_commit(pre_fetch_repo.as_ref(), &options.scope)?;
        let local_base_tree = local_base_commit.tree();
        let mut builder = MergedTreeBuilder::new(local_base_tree.clone());

        for relative in &selected_paths {
            apply_home_path_to_tree(tx.repo_mut(), paths, relative, &mut builder).await?;
        }

        let local_tree = builder.write_tree().await.map_err(|err| DotsyncError::Jj {
            message: format!("write local commit tree for {}: {err}", options.scope),
        })?;
        let local_commit = tx
            .repo_mut()
            .new_commit(vec![local_base_commit.id().clone()], local_tree)
            .set_description(&options.message)
            .write()
            .await
            .map_err(|err| DotsyncError::Jj {
                message: format!("write local commit for {}: {err}", options.scope),
            })?;
        let merged_tree =
            merge_commit_trees(tx.repo_mut(), &[base_commit.clone(), local_commit.clone()])
                .await
                .map_err(|err| DotsyncError::Jj {
                    message: format!("merge concurrent commits for {}: {err}", options.scope),
                })?;

        if merged_tree.has_conflict() {
            let conflicted_files = conflicted_files_from_tree(&merged_tree, &options.scope)?;
            let cascade_command = CascadeCommand {
                root_scope: options.scope.clone(),
                description: format!("dotsync: cascade from {}", options.scope),
            };
            let plan = build_cascade_plan(&graph, &scope_heads, &cascade_command);
            tx.commit("dotsync: pause concurrent scope merge")
                .await
                .map_err(|err| DotsyncError::Jj {
                    message: format!(
                        "commit paused concurrent merge for {}: {err}",
                        options.scope
                    ),
                })?;
            save_paused_cascade_state(
                paths,
                &PausedCascadeState {
                    machine_scope,
                    paused_scope: options.scope.clone(),
                    parent_commit_ids: vec![base_commit.id().hex(), local_commit.id().hex()],
                    conflicted_files: conflicted_files.iter().map(PathBuf::from).collect(),
                    remaining_steps: plan
                        .remaining_steps()
                        .iter()
                        .map(|step| PausedCascadeStep {
                            scope: step.scope.clone(),
                            parent_scopes: step.parent_scopes.clone(),
                        })
                        .collect(),
                    description: options.message,
                    original_scope_commit_ids: original_scope_commit_ids.clone(),
                    abort_restore_paths: selected_paths.clone(),
                },
            )?;
            return Err(DotsyncError::CascadePaused {
                scope: options.scope,
                conflicted_files: conflicted_files.join(", "),
            });
        }

        tx.repo_mut()
            .new_commit(
                vec![base_commit.id().clone(), local_commit.id().clone()],
                merged_tree,
            )
            .set_description(&options.message)
            .write()
            .await
            .map_err(|err| DotsyncError::Jj {
                message: format!(
                    "write merged concurrent commit for {}: {err}",
                    options.scope
                ),
            })?
    };
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
    let cascaded_scopes =
        match execute_cascade_plan(tx.repo_mut(), &mut scope_heads, &plan, &cascade_command).await?
        {
            CascadeOutcome::Completed(success) => success.progress.completed_scopes,
            CascadeOutcome::Paused {
                scope,
                conflicted_files,
            } => {
                let paused_step = plan
                    .remaining_steps()
                    .iter()
                    .find(|step| step.scope == scope)
                    .ok_or_else(|| DotsyncError::Jj {
                        message: format!("paused cascade step `{scope}` was not in plan"),
                    })?;
                let parent_commit_ids = parent_commit_ids_for_step(&scope_heads, paused_step)?;
                let remaining_steps = remaining_steps_after_pause(plan.remaining_steps(), &scope);
                tx.commit("dotsync: pause cascade")
                    .await
                    .map_err(|err| DotsyncError::Jj {
                        message: format!(
                            "commit paused cascade state for {}: {err}",
                            options.scope
                        ),
                    })?;
                save_paused_cascade_state(
                    paths,
                    &PausedCascadeState {
                        machine_scope,
                        paused_scope: scope.clone(),
                        parent_commit_ids,
                        conflicted_files: conflicted_files.iter().map(PathBuf::from).collect(),
                        remaining_steps,
                        description: cascade_command.description,
                        original_scope_commit_ids,
                        abort_restore_paths: selected_paths.clone(),
                    },
                )?;
                return Err(DotsyncError::CascadePaused {
                    scope,
                    conflicted_files: conflicted_files.join(", "),
                });
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

async fn stale_selected_scope_paths(
    paths: &DotsyncPaths,
    repo: &dyn jj_lib::repo::Repo,
    pre_fetch_target_entries: &BTreeMap<PathBuf, TreeValue>,
    current_target_entries: &BTreeMap<PathBuf, TreeValue>,
    selected_paths: &[PathBuf],
) -> Result<Vec<PathBuf>, DotsyncError> {
    let mut conflicted = Vec::new();

    for relative in selected_paths {
        let pre_fetch_bytes =
            read_entry_bytes(repo, relative, pre_fetch_target_entries.get(relative)).await?;
        let current_target_bytes =
            read_entry_bytes(repo, relative, current_target_entries.get(relative)).await?;
        let home_bytes = read_home_bytes(paths, relative)?;

        if current_target_bytes != pre_fetch_bytes && home_bytes != current_target_bytes {
            conflicted.push(relative.clone());
        }
    }

    Ok(conflicted)
}

fn conflicted_files_from_tree(
    tree: &jj_lib::merged_tree::MergedTree,
    scope: &str,
) -> Result<Vec<String>, DotsyncError> {
    tree.conflicts()
        .map(|(path, value)| {
            value.map_err(|err| DotsyncError::Jj {
                message: format!("read conflict for {scope}: {err}"),
            })?;
            Ok(path.as_internal_file_string().to_string())
        })
        .collect()
}

async fn read_entry_bytes(
    repo: &dyn jj_lib::repo::Repo,
    relative: &Path,
    value: Option<&TreeValue>,
) -> Result<Option<Vec<u8>>, DotsyncError> {
    match value {
        Some(value) => Ok(Some(
            read_tree_entry_bytes(repo.store(), relative, value).await?,
        )),
        None => Ok(None),
    }
}

fn read_home_bytes(paths: &DotsyncPaths, relative: &Path) -> Result<Option<Vec<u8>>, DotsyncError> {
    let home_path = paths.home_dir.join(relative);
    match fs::read(&home_path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(DotsyncError::Io {
            path: home_path,
            source,
        }),
    }
}

async fn load_current_machine_entries(
    repo: &dyn jj_lib::repo::Repo,
    machine_scope: &str,
    internal_paths: &std::collections::BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, TreeValue>, DotsyncError> {
    let machine_commit = load_scope_commit(repo, machine_scope)?;
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
        CommitSelection::Paths(_) => {
            detect_changed_managed_paths(paths, repo, target_entries).await
        }
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
            || target_entries.keys().any(|candidate| {
                candidate != selection_path && path_has_prefix(candidate, selection_path)
            });
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

        let relative = path
            .strip_prefix(home_root)
            .map_err(|source| DotsyncError::Jj {
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
    let repo_path =
        RepoPathBuf::from_internal_string(relative_str).map_err(|err| DotsyncError::Jj {
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
            if home_dir_has_unmanaged_files(
                root,
                repo_root,
                &path,
                machine_entries,
                internal_paths,
            )? {
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
    paths: &DotsyncPaths,
    options: SyncOptions,
) -> Result<CommandOutcome<ContinueReport>, DotsyncError> {
    let state = load_paused_cascade_state(paths)?;
    let repo = load_repo_direct(paths).await?;
    let config = load_config(paths).await?;
    let internal_paths = internal_repo_paths(&config);
    let old_machine_commit = load_scope_commit(repo.as_ref(), &state.machine_scope)?;
    let mut tx = repo.start_transaction();
    let mut scope_heads = ScopeHeads::load_existing(tx.repo_mut().base_repo(), &config.graph)?;
    let parent_commits = state
        .parent_commit_ids
        .iter()
        .map(|id| load_commit_by_hex(tx.repo_mut(), id))
        .collect::<Result<Vec<_>, DotsyncError>>()?;
    if parent_commits.is_empty() {
        return Err(DotsyncError::Jj {
            message: "paused cascade has no parent commits".to_string(),
        });
    }
    let merged_tree = merge_commit_trees(tx.repo_mut(), &parent_commits)
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!(
                "merge paused cascade parents for {}: {err}",
                state.paused_scope
            ),
        })?;
    let mut builder = MergedTreeBuilder::new(merged_tree);
    for relative in &state.conflicted_files {
        apply_home_path_to_tree(tx.repo_mut(), paths, relative, &mut builder).await?;
    }
    let resolved_tree = builder.write_tree().await.map_err(|err| DotsyncError::Jj {
        message: format!("write resolved tree for {}: {err}", state.paused_scope),
    })?;
    let resolved_commit = tx
        .repo_mut()
        .new_commit(
            parent_commits
                .iter()
                .map(|commit| commit.id().clone())
                .collect(),
            resolved_tree,
        )
        .set_description(&state.description)
        .write()
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!(
                "write resolved cascade commit for {}: {err}",
                state.paused_scope
            ),
        })?;
    tx.repo_mut().set_local_bookmark_target(
        RefNameBuf::from(state.paused_scope.as_str()).as_ref(),
        RefTarget::normal(resolved_commit.id().clone()),
    );
    scope_heads.update(state.paused_scope.clone(), resolved_commit);

    let command = CascadeCommand {
        root_scope: state.paused_scope.clone(),
        description: state.description.clone(),
    };
    let remaining_plan = crate::cascade::CascadePlan::from_steps(
        state
            .remaining_steps
            .iter()
            .map(|step| CascadeStep {
                scope: step.scope.clone(),
                parent_scopes: step.parent_scopes.clone(),
            })
            .collect(),
    );
    let mut cascaded_scopes = vec![state.paused_scope.clone()];
    match execute_cascade_plan(tx.repo_mut(), &mut scope_heads, &remaining_plan, &command).await? {
        CascadeOutcome::Completed(success) => {
            cascaded_scopes.extend(success.progress.completed_scopes);
        }
        CascadeOutcome::Paused {
            scope,
            conflicted_files,
        } => {
            let paused_step = remaining_plan
                .remaining_steps()
                .iter()
                .find(|step| step.scope == scope)
                .ok_or_else(|| DotsyncError::Jj {
                    message: format!("paused cascade step `{scope}` was not in remaining plan"),
                })?;
            let parent_commit_ids = parent_commit_ids_for_step(&scope_heads, paused_step)?;
            let remaining_steps =
                remaining_steps_after_pause(remaining_plan.remaining_steps(), &scope);
            tx.commit("dotsync: pause cascade again")
                .await
                .map_err(|err| DotsyncError::Jj {
                    message: format!("commit repeated paused cascade state: {err}"),
                })?;
            save_paused_cascade_state(
                paths,
                &PausedCascadeState {
                    machine_scope: state.machine_scope,
                    paused_scope: scope.clone(),
                    parent_commit_ids,
                    conflicted_files: conflicted_files.iter().map(PathBuf::from).collect(),
                    remaining_steps,
                    description: state.description,
                    original_scope_commit_ids: state.original_scope_commit_ids,
                    abort_restore_paths: state.abort_restore_paths,
                },
            )?;
            return Err(DotsyncError::CascadePaused {
                scope,
                conflicted_files: conflicted_files.join(", "),
            });
        }
    }

    let expected_changes = expected_machine_changes(
        tx.repo_mut(),
        &old_machine_commit,
        &scope_heads.require(&state.machine_scope)?,
        &internal_paths,
    )
    .await?;
    tx.commit("dotsync: continue cascade")
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!("commit continued cascade: {err}"),
        })?;
    remove_paused_cascade_state(paths)?;
    let sync = crate::sync::sync_repo_to_home(
        paths,
        options,
        &expected_changes,
        Some(&state.machine_scope),
    )
    .await?;
    push_scope_updates(paths).await?;
    Ok(CommandOutcome::Success(ContinueReport {
        cascaded_scopes,
        sync,
    }))
}

pub async fn abort_paused_cascade(
    paths: &DotsyncPaths,
    options: SyncOptions,
) -> Result<CommandOutcome<AbortReport>, DotsyncError> {
    let state = load_paused_cascade_state(paths)?;
    if state.original_scope_commit_ids.is_empty() {
        return Err(DotsyncError::Jj {
            message: "paused cascade state does not include an abort checkpoint; resolve the conflict and run `dotsync continue` instead".to_string(),
        });
    }

    let repo = load_repo_direct(paths).await?;
    let mut tx = repo.start_transaction();
    for (scope, commit_id) in &state.original_scope_commit_ids {
        let commit = load_commit_by_hex(tx.repo_mut(), commit_id)?;
        tx.repo_mut().set_local_bookmark_target(
            RefNameBuf::from(scope.as_str()).as_ref(),
            RefTarget::normal(commit.id().clone()),
        );
    }
    tx.commit("dotsync: abort cascade")
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!("commit aborted cascade: {err}"),
        })?;
    remove_paused_cascade_state(paths)?;

    let restore_paths = if state.abort_restore_paths.is_empty() {
        &state.conflicted_files
    } else {
        &state.abort_restore_paths
    };

    let sync =
        crate::sync::sync_repo_to_home(paths, options, restore_paths, Some(&state.machine_scope))
            .await?;

    Ok(CommandOutcome::Success(AbortReport {
        aborted_scope: state.paused_scope,
        sync,
    }))
}

fn paused_cascade_state_path(paths: &DotsyncPaths) -> PathBuf {
    paths.repo_root.join(".dotsync-paused-cascade.json")
}

fn save_paused_cascade_state(
    paths: &DotsyncPaths,
    state: &PausedCascadeState,
) -> Result<(), DotsyncError> {
    let path = paused_cascade_state_path(paths);
    let contents = serde_json::to_vec_pretty(state).map_err(|err| DotsyncError::Jj {
        message: format!("serialize paused cascade state: {err}"),
    })?;
    fs::write(&path, contents).map_err(|source| DotsyncError::Io { path, source })
}

fn load_paused_cascade_state(paths: &DotsyncPaths) -> Result<PausedCascadeState, DotsyncError> {
    let path = paused_cascade_state_path(paths);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(DotsyncError::NoPausedCascade);
        }
        Err(source) => return Err(DotsyncError::Io { path, source }),
    };
    serde_json::from_str(&contents).map_err(|err| DotsyncError::Jj {
        message: format!("parse paused cascade state {}: {err}", path.display()),
    })
}

fn remove_paused_cascade_state(paths: &DotsyncPaths) -> Result<(), DotsyncError> {
    let path = paused_cascade_state_path(paths);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DotsyncError::Io { path, source }),
    }
}

fn reject_commit_if_cascade_paused(paths: &DotsyncPaths) -> Result<(), DotsyncError> {
    match load_paused_cascade_state(paths) {
        Ok(state) => Err(DotsyncError::PausedCascadeInProgress {
            scope: state.paused_scope,
        }),
        Err(DotsyncError::NoPausedCascade) => Ok(()),
        Err(error) => Err(error),
    }
}

fn parent_commit_ids_for_step(
    scope_heads: &ScopeHeads,
    step: &CascadeStep,
) -> Result<Vec<String>, DotsyncError> {
    let mut ids = Vec::with_capacity(step.parent_scopes.len() + 1);
    ids.push(scope_heads.require(&step.scope)?.id().hex());
    for parent_scope in &step.parent_scopes {
        ids.push(scope_heads.require(parent_scope)?.id().hex());
    }
    Ok(ids)
}

fn remaining_steps_after_pause(
    steps: &[CascadeStep],
    paused_scope: &str,
) -> Vec<PausedCascadeStep> {
    steps
        .iter()
        .skip_while(|step| step.scope != paused_scope)
        .skip(1)
        .map(|step| PausedCascadeStep {
            scope: step.scope.clone(),
            parent_scopes: step.parent_scopes.clone(),
        })
        .collect()
}

fn load_commit_by_hex(
    repo: &dyn jj_lib::repo::Repo,
    id: &str,
) -> Result<jj_lib::commit::Commit, DotsyncError> {
    let commit_id = CommitId::try_from_hex(id).ok_or_else(|| DotsyncError::Jj {
        message: format!("paused cascade commit id `{id}` is not valid hex"),
    })?;
    repo.store()
        .get_commit(&commit_id)
        .map_err(|err| DotsyncError::Jj {
            message: format!("load paused cascade commit `{id}`: {err}"),
        })
}
