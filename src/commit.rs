use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::backend::TreeValue;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::{RefNameBuf, WorkspaceNameBuf};
use jj_lib::repo::{MutableRepo, ReadonlyRepo};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::workspace::Workspace;

use crate::cascade::{
    build_cascade_plan, build_paused_state, commit_resolved_pause, create_scope_head_if_missing,
    enrich_pause_with_scope_dag, execute_cascade_plan, resume_cascade, CascadeCommand,
    CascadeOutcome, CascadePlan, CascadeStateStore, JsonCascadeStateStore, ScopeHeads,
};
use crate::config::{load_config, DotsyncPaths, DOTSYNC_CONFIG_RELATIVE_PATH};
use crate::error::{jj_error, DotsyncError};
use crate::repo::{
    checkout_workspace_to_commit, checkout_workspace_to_scope, fetch_origin, load_repo,
    load_scope_commit, load_workspace, read_tree_entry_bytes, snapshot_working_copy,
};
use crate::scope_graph::{is_ancestor_scope, ScopeGraph};
use crate::sync::{sync_repo_to_home, SyncOptions, SyncReport};

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
    Conflict(crate::cascade::CascadePause),
}

#[derive(Debug, Clone)]
pub(crate) struct CommitSession {
    pub(crate) current_scope: String,
    pub(crate) graph: ScopeGraph,
}

pub async fn commit_and_sync(
    paths: &DotsyncPaths,
    options: CommitOptions,
) -> Result<CommandOutcome<CommitReport>, DotsyncError> {
    ensure_no_paused_cascade(paths)?;
    let session = prepare_commit_session(paths, &options.scope).await?;
    let snapshot = snapshot_working_copy(paths).await?;
    let mut workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("reload repo after snapshot: {err}")))?;
    let base_tree = load_scope_commit(repo.as_ref(), &session.current_scope)?.tree();
    let selected_paths = resolve_commit_selection(paths, &snapshot.changed_paths, &options)?;
    let selected_tree = match &options.selection {
        CommitSelection::All => snapshot.tree.clone(),
        CommitSelection::Paths(_) => {
            project_selected_paths(&snapshot.tree, &base_tree, &selected_paths).await?
        }
    };

    match commit_snapshot_and_apply_cascade(
        paths,
        &session,
        repo,
        &mut workspace,
        &options.scope,
        &selected_tree,
        &options.message,
    )
    .await?
    {
        CommandOutcome::Conflict(pause) => {
            let unselected_paths = snapshot
                .changed_paths
                .iter()
                .filter(|path| !selected_paths.contains(path))
                .cloned()
                .collect::<Vec<_>>();
            restore_working_copy_paths(paths, &snapshot.tree, &unselected_paths).await?;
            Ok(CommandOutcome::Conflict(pause))
        }
        CommandOutcome::Success(cascaded_scopes) => {
            checkout_workspace_to_scope(paths, &mut workspace, &session.current_scope).await?;
            let unselected_paths = snapshot
                .changed_paths
                .iter()
                .filter(|path| !selected_paths.contains(path))
                .cloned()
                .collect::<Vec<_>>();
            restore_working_copy_paths(paths, &snapshot.tree, &unselected_paths).await?;
            let sync = sync_repo_to_home(
                paths,
                SyncOptions {
                    force: options.force,
                },
                &selected_paths,
                Some(&session.current_scope),
            )
            .await?;
            crate::repo::push_scope_updates(paths).await?;

            Ok(CommandOutcome::Success(CommitReport {
                committed_scope: options.scope,
                cascaded_scopes,
                sync,
            }))
        }
    }
}

pub async fn continue_after_conflict(
    paths: &DotsyncPaths,
    _options: SyncOptions,
) -> Result<CommandOutcome<ContinueReport>, DotsyncError> {
    let state_store = cascade_state_store(paths);
    let state = state_store.load()?.ok_or(DotsyncError::NoPausedCascade)?;

    let resolved_snapshot = snapshot_working_copy(paths).await?;
    let mut workspace = load_workspace(paths)?;
    let repo = load_repo(&workspace).await?;
    let graph = load_config(paths).await?.graph;
    let mut tx = repo.start_transaction();
    let mut scope_heads = ScopeHeads::load_existing(tx.repo_mut().base_repo(), &graph)?;
    commit_resolved_pause(
        tx.repo_mut(),
        &mut scope_heads,
        &state,
        &resolved_snapshot.tree,
    )
    .await?;

    let mut resumed_state = state.clone();
    resumed_state
        .completed_scopes
        .push(resumed_state.paused_scope.clone());

    let outcome = resume_cascade(tx.repo_mut(), &mut scope_heads, &resumed_state).await?;
    match outcome {
        CascadeOutcome::Paused(pause) => {
            let pause = enrich_pause_with_scope_dag(pause, &graph);
            let paused_plan = CascadePlan::from_steps(resumed_state.remaining_steps.clone());
            let current_commit = set_working_copy_to_paused_conflict(
                tx.repo_mut(),
                &scope_heads,
                workspace.workspace_name().to_owned(),
                &pause,
                &resumed_state.command_description,
            )
            .await?;
            let paused_state = build_paused_state(
                &paused_plan,
                &pause,
                &CascadeCommand {
                    root_scope: resumed_state.committed_scope.clone(),
                    description: resumed_state.command_description.clone(),
                    original_scope: resumed_state.original_scope.clone(),
                    machine_scope: resumed_state.machine_scope.clone(),
                },
                &scope_heads,
            );
            let repo = tx
                .commit("dotsync: pause merge cascade")
                .await
                .map_err(|err| jj_error(format!("commit paused cascade state: {err}")))?;
            state_store.save(&paused_state)?;
            checkout_workspace_to_commit(&mut workspace, repo.op_id().clone(), &current_commit)
                .await?;
            Ok(CommandOutcome::Conflict(pause))
        }
        CascadeOutcome::Completed(success) => {
            let current_commit = scope_heads.require(&state.machine_scope)?;
            tx.repo_mut()
                .set_wc_commit(
                    workspace.workspace_name().to_owned(),
                    current_commit.id().clone(),
                )
                .map_err(|err| jj_error(format!("restore machine working copy bookmark: {err}")))?;
            let repo = tx
                .commit("dotsync: continue merge cascade")
                .await
                .map_err(|err| jj_error(format!("commit continued cascade: {err}")))?;
            state_store.clear()?;
            checkout_workspace_to_commit(&mut workspace, repo.op_id().clone(), &current_commit)
                .await?;

            let sync = sync_repo_to_home(paths, SyncOptions { force: true }, &[], None).await?;
            crate::repo::push_scope_updates(paths).await?;

            Ok(CommandOutcome::Success(ContinueReport {
                cascaded_scopes: success.progress.completed_scopes,
                sync,
            }))
        }
    }
}

pub(crate) async fn prepare_commit_session(
    paths: &DotsyncPaths,
    target_scope: &str,
) -> Result<CommitSession, DotsyncError> {
    let workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("load repo at head: {err}")))?;
    let initial_graph = load_config(paths).await?.graph;
    let current_scope = crate::repo::detect_current_scope(&initial_graph, &workspace, repo.as_ref())?;

    let _repo = fetch_origin(repo).await?;
    let graph = load_config(paths).await?.graph;
    validate_commit_scope(&graph, target_scope, &current_scope)?;

    Ok(CommitSession {
        current_scope,
        graph,
    })
}

pub(crate) fn validate_commit_scope(
    graph: &ScopeGraph,
    target_scope: &str,
    current_scope: &str,
) -> Result<(), DotsyncError> {
    if !graph.parents.contains_key(target_scope) {
        return Err(DotsyncError::InvalidScope {
            scope: target_scope.to_string(),
        });
    }
    if !is_ancestor_scope(graph, target_scope, current_scope)? {
        return Err(DotsyncError::ScopeNotAncestor {
            scope: target_scope.to_string(),
            current_scope: current_scope.to_string(),
        });
    }
    Ok(())
}

pub(crate) fn ensure_no_paused_cascade(paths: &DotsyncPaths) -> Result<(), DotsyncError> {
    if let Some(state) = cascade_state_store(paths).load()? {
        return Err(DotsyncError::CascadeInProgress {
            scope: state.paused_scope,
        });
    }
    Ok(())
}

pub(crate) async fn commit_snapshot_and_apply_cascade(
    paths: &DotsyncPaths,
    session: &CommitSession,
    repo: Arc<ReadonlyRepo>,
    workspace: &mut Workspace,
    target_scope: &str,
    tree: &jj_lib::merged_tree::MergedTree,
    message: &str,
) -> Result<CommandOutcome<Vec<String>>, DotsyncError> {
    ensure_no_paused_cascade(paths)?;
    let state_store = cascade_state_store(paths);
    let mut tx = repo.start_transaction();
    let mut scope_heads = ScopeHeads::load_existing(tx.repo_mut().base_repo(), &session.graph)?;
    create_scope_head_if_missing(
        tx.repo_mut(),
        &mut scope_heads,
        &session.graph,
        target_scope,
        &format!("dotsync: create {target_scope} scope"),
    )
    .await?;
    let target_head = scope_heads.require(target_scope)?;

    let target_commit = tx
        .repo_mut()
        .new_commit(vec![target_head.id().clone()], tree.clone())
        .set_description(message)
        .write()
        .await
        .map_err(|err| jj_error(format!("write scope commit: {err}")))?;
    tx.repo_mut().set_local_bookmark_target(
        RefNameBuf::from(target_scope).as_ref(),
        RefTarget::normal(target_commit.id().clone()),
    );
    scope_heads.update(target_scope.to_string(), target_commit);

    let cascade_command = CascadeCommand {
        root_scope: target_scope.to_string(),
        description: format!("dotsync: cascade {message}"),
        original_scope: target_scope.to_string(),
        machine_scope: session.current_scope.clone(),
    };
    let cascade_plan = build_cascade_plan(&session.graph, &scope_heads, &cascade_command);
    let cascade_result = execute_cascade_plan(
        tx.repo_mut(),
        &mut scope_heads,
        &cascade_plan,
        &cascade_command,
    )
    .await?;

    match cascade_result {
        CascadeOutcome::Paused(pause) => {
            let pause = enrich_pause_with_scope_dag(pause, &session.graph);
            let current_commit = set_working_copy_to_paused_conflict(
                tx.repo_mut(),
                &scope_heads,
                workspace.workspace_name().to_owned(),
                &pause,
                &cascade_command.description,
            )
            .await?;
            let paused_state = build_paused_state(&cascade_plan, &pause, &cascade_command, &scope_heads);
            let repo = tx
                .commit(format!("dotsync: {message}"))
                .await
                .map_err(|err| jj_error(format!("commit paused scope update: {err}")))?;
            state_store.save(&paused_state)?;
            checkout_workspace_to_commit(workspace, repo.op_id().clone(), &current_commit).await?;
            Ok(CommandOutcome::Conflict(pause))
        }
        CascadeOutcome::Completed(success) => {
            let current_commit = scope_heads.require(&session.current_scope)?;
            tx.repo_mut()
                .set_wc_commit(
                    workspace.workspace_name().to_owned(),
                    current_commit.id().clone(),
                )
                .map_err(|err| jj_error(format!("update working copy bookmark: {err}")))?;

            tx.commit(format!("dotsync: {message}"))
                .await
                .map_err(|err| jj_error(format!("commit scope update: {err}")))?;
            state_store.clear()?;

            Ok(CommandOutcome::Success(success.progress.completed_scopes))
        }
    }
}

async fn create_temporary_conflict_commit(
    mut_repo: &mut MutableRepo,
    scope_heads: &ScopeHeads,
    pause: &crate::cascade::CascadePause,
    description: &str,
) -> Result<jj_lib::commit::Commit, DotsyncError> {
    let mut parents = vec![scope_heads.require(&pause.scope)?];
    for parent_scope in &pause.parent_scopes {
        parents.push(scope_heads.require(parent_scope)?);
    }
    let merged_tree = jj_lib::rewrite::merge_commit_trees(mut_repo, &parents)
        .await
        .map_err(|err| jj_error(format!("recreate conflict tree for {}: {err}", pause.scope)))?;
    mut_repo
        .new_commit(
            parents.iter().map(|commit| commit.id().clone()).collect(),
            merged_tree,
        )
        .set_description(description)
        .write()
        .await
        .map_err(|err| {
            jj_error(format!(
                "write temporary conflict commit for {}: {err}",
                pause.scope
            ))
        })
}

async fn set_working_copy_to_paused_conflict(
    mut_repo: &mut MutableRepo,
    scope_heads: &ScopeHeads,
    workspace_name: WorkspaceNameBuf,
    pause: &crate::cascade::CascadePause,
    description: &str,
) -> Result<jj_lib::commit::Commit, DotsyncError> {
    let current_commit = create_temporary_conflict_commit(mut_repo, scope_heads, pause, description).await?;

    mut_repo
        .set_wc_commit(workspace_name, current_commit.id().clone())
        .map_err(|err| jj_error(format!("set paused working copy commit: {err}")))?;
    Ok(current_commit)
}

pub(crate) fn resolve_commit_selection(
    paths: &DotsyncPaths,
    changed_paths: &[PathBuf],
    options: &CommitOptions,
) -> Result<Vec<PathBuf>, DotsyncError> {
    match &options.selection {
        CommitSelection::All => {
            if changed_paths.is_empty() {
                return Err(DotsyncError::CommitSelectionEmpty);
            }
            validate_config_commit_scope(changed_paths, &options.scope)?;
            Ok(changed_paths.to_vec())
        }
        CommitSelection::Paths(requested_paths) => {
            if requested_paths.is_empty() {
                return Err(DotsyncError::CommitSelectionRequired);
            }

            let normalized_paths = normalize_selected_repo_paths(paths, requested_paths, changed_paths)?;
            let mut selected = changed_paths
                .iter()
                .filter(|changed| path_matches_selection(&normalized_paths, changed))
                .cloned()
                .collect::<Vec<_>>();
            selected.sort();
            selected.dedup();

            if selected.is_empty() {
                return Err(DotsyncError::CommitSelectionEmpty);
            }

            validate_config_commit_scope(&selected, &options.scope)?;
            Ok(selected)
        }
    }
}

pub(crate) fn normalize_selected_repo_paths(
    paths: &DotsyncPaths,
    requested_paths: &[PathBuf],
    changed_paths: &[PathBuf],
) -> Result<Vec<PathBuf>, DotsyncError> {
    let repo_root = fs::canonicalize(&paths.repo_root).map_err(|source| DotsyncError::Io {
        path: paths.repo_root.clone(),
        source,
    })?;
    let changed_set = changed_paths.iter().cloned().collect::<BTreeSet<_>>();
    let mut normalized = Vec::with_capacity(requested_paths.len());

    for requested in requested_paths {
        let candidate = if requested.is_absolute() {
            requested.clone()
        } else {
            paths.repo_root.join(requested)
        };

        let canonical = match fs::canonicalize(&candidate) {
            Ok(path) => path,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                let repo_relative = if requested.is_absolute() {
                    requested
                        .strip_prefix(&repo_root)
                        .map(Path::to_path_buf)
                        .map_err(|_| DotsyncError::CommitPathOutsideRepo {
                            path: requested.clone(),
                        })?
                } else {
                    requested.clone()
                };

                if changed_set.contains(&repo_relative) {
                    normalized.push(repo_relative);
                    continue;
                }

                return Err(DotsyncError::CommitPathMissing {
                    path: requested.clone(),
                });
            }
            Err(source) => {
                return Err(DotsyncError::Io {
                    path: candidate.clone(),
                    source,
                })
            }
        };

        let relative = canonical.strip_prefix(&repo_root).map_err(|_| {
            DotsyncError::CommitPathOutsideRepo {
                path: requested.clone(),
            }
        })?;
        normalized.push(relative.to_path_buf());
    }

    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

pub(crate) fn path_matches_selection(selected_paths: &[PathBuf], changed_path: &Path) -> bool {
    selected_paths.iter().any(|selected| {
        changed_path == selected || changed_path.starts_with(selected) || selected.starts_with(changed_path)
    })
}

pub(crate) fn validate_config_commit_scope(
    selected_paths: &[PathBuf],
    scope: &str,
) -> Result<(), DotsyncError> {
    let config_path = PathBuf::from(DOTSYNC_CONFIG_RELATIVE_PATH);
    if scope != "all"
        && selected_paths
            .iter()
            .any(|path| path == &config_path || path.starts_with(&config_path))
    {
        return Err(DotsyncError::ConfigOnlyAllowedOnBaseScope {
            config_path: DOTSYNC_CONFIG_RELATIVE_PATH.to_string(),
            scope: scope.to_string(),
        });
    }
    Ok(())
}

pub(crate) async fn project_selected_paths(
    selected_tree: &jj_lib::merged_tree::MergedTree,
    base_tree: &jj_lib::merged_tree::MergedTree,
    selected_paths: &[PathBuf],
) -> Result<jj_lib::merged_tree::MergedTree, DotsyncError> {
    let mut builder = MergedTreeBuilder::new(base_tree.clone());
    for path in selected_paths {
        let repo_path =
            RepoPathBuf::from_internal_string(path.to_string_lossy().replace('\\', "/"))
                .map_err(|err| jj_error(format!("invalid repo path {}: {err}", path.display())))?;
        let value = selected_tree
            .path_value(repo_path.as_ref())
            .map_err(|err| jj_error(format!("read selected path {}: {err}", path.display())))?;
        builder.set_or_remove(repo_path, value);
    }
    builder
        .write_tree()
        .await
        .map_err(|err| jj_error(format!("write selected tree: {err}")))
}

pub(crate) async fn restore_working_copy_paths(
    paths: &DotsyncPaths,
    snapshot_tree: &jj_lib::merged_tree::MergedTree,
    restore_paths: &[PathBuf],
) -> Result<(), DotsyncError> {
    for path in restore_paths {
        let repo_path = RepoPathBuf::from_internal_string(path.to_string_lossy().replace('\\', "/"))
            .map_err(|err| jj_error(format!("invalid restore path {}: {err}", path.display())))?;
        let value = snapshot_tree
            .path_value(repo_path.as_ref())
            .map_err(|err| jj_error(format!("read restore path {}: {err}", path.display())))?;
        let system_path = paths.repo_root.join(path);
        let resolved = value.into_resolved().map_err(|conflict| {
            jj_error(format!(
                "restore path {} is conflicted: {conflict:?}",
                path.display()
            ))
        })?;

        match resolved {
            Some(TreeValue::Tree(_)) => {
                fs::create_dir_all(&system_path).map_err(|source| DotsyncError::Io {
                    path: system_path.clone(),
                    source,
                })?;
            }
            Some(value) => {
                if let Some(parent) = system_path.parent() {
                    fs::create_dir_all(parent).map_err(|source| DotsyncError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
                let contents = read_tree_entry_bytes(snapshot_tree.store(), path, &value).await?;
                fs::write(&system_path, contents).map_err(|source| DotsyncError::Io {
                    path: system_path,
                    source,
                })?;
            }
            None => match fs::remove_file(&system_path) {
                Ok(()) => {}
                Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(DotsyncError::Io {
                        path: system_path,
                        source,
                    })
                }
            },
        }
    }
    Ok(())
}

fn cascade_state_store(paths: &DotsyncPaths) -> JsonCascadeStateStore {
    JsonCascadeStateStore::new(paths.repo_root.join(".jj/dotsync/cascade-state.json"))
}
