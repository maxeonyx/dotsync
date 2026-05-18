mod cascade;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gix::remote::fetch::Tags;
use jj_lib::backend::{CommitId, TreeValue};
use jj_lib::config::StackedConfig;
use jj_lib::git::{
    self, GitBranchPushTargets, GitFetch, GitFetchRefExpression, GitImportOptions, GitProgress,
    GitPushOptions, GitSidebandLineTerminator, GitSubprocessCallback, GitSubprocessOptions,
};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::merge::MergedTreeValue;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::refs::BookmarkPushUpdate;
use jj_lib::repo::{MutableRepo, ReadonlyRepo, Repo as _, StoreFactories};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_lib::str_util::StringExpression;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::workspace::{default_working_copy_factories, Workspace};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::cascade::{
    build_cascade_plan, build_paused_state, commit_resolved_pause, create_scope_head_if_missing,
    enrich_pause_with_scope_dag, execute_cascade_plan, resume_cascade, CascadeCommand,
    CascadeOutcome, CascadePlan, CascadeStateStore, JsonCascadeStateStore, ScopeHeads,
};

const DOTSYNC_CONFIG_RELATIVE_PATH: &str = ".config/dotsync/config.toml";
const DEFAULT_SYNC_STATE_RELATIVE_PATH: &str = ".config/dotsync/sync-state.json";

#[derive(Debug, Clone)]
pub struct DotsyncPaths {
    pub repo_root: PathBuf,
    pub home_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SyncOptions {
    pub force: bool,
}

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
pub struct InitReport {
    pub current_scope: String,
    pub created_scopes: Vec<String>,
    pub sync: SyncReport,
}

#[derive(Debug, Clone)]
pub struct FileDrift {
    pub repo_path: PathBuf,
    pub system_path: PathBuf,
    pub diff: String,
}

#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub current_scope: String,
    pub synced_paths: Vec<PathBuf>,
    pub drifts: Vec<FileDrift>,
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
pub struct ErrorReport {
    pub code: &'static str,
    pub message: String,
    pub drifts: Vec<FileDrift>,
    pub current_state: Option<String>,
}

#[derive(Debug, Clone)]
struct CommitSession {
    current_scope: String,
    graph: ScopeGraph,
}

#[derive(Debug, Clone)]
struct WorkingCopySnapshot {
    tree: jj_lib::merged_tree::MergedTree,
    changed_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum CommandOutcome<T> {
    Success(T),
    Conflict(CascadePause),
}

impl DotsyncError {
    pub fn to_error_report(&self) -> ErrorReport {
        match self {
            DotsyncError::DriftDetected { drifts, .. } => ErrorReport {
                code: "drift_detected",
                message: self.to_string(),
                drifts: drifts.clone(),
                current_state: Some(
                    "managed files in home differ from the repo version for this machine scope"
                        .to_string(),
                ),
            },
            DotsyncError::DirtyWorkingCopy { .. } => basic_error_report("dirty_working_copy", self),
            DotsyncError::NoPausedCascade => basic_error_report("no_paused_cascade", self),
            DotsyncError::InvalidScope { .. } => basic_error_report("invalid_scope", self),
            DotsyncError::ScopeNotAncestor { .. } => basic_error_report("scope_not_ancestor", self),
            DotsyncError::CascadeInProgress { .. } => {
                basic_error_report("cascade_in_progress", self)
            }
            DotsyncError::CommitSelectionRequired => {
                basic_error_report("commit_selection_required", self)
            }
            DotsyncError::ConflictingCommitSelection => {
                basic_error_report("conflicting_commit_selection", self)
            }
            DotsyncError::CommitPathOutsideRepo { .. } => {
                basic_error_report("commit_path_outside_repo", self)
            }
            DotsyncError::CommitPathMissing { .. } => {
                basic_error_report("commit_path_missing", self)
            }
            DotsyncError::CommitSelectionEmpty => {
                basic_error_report("commit_selection_empty", self)
            }
            DotsyncError::ConfigOnlyAllowedOnBaseScope { .. } => {
                basic_error_report("config_base_scope_only", self)
            }
            DotsyncError::NoCurrentScope => basic_error_report("no_current_scope", self),
            DotsyncError::MissingScopeBookmark { .. } => {
                basic_error_report("missing_scope_bookmark", self)
            }
            DotsyncError::MissingParent { .. } => basic_error_report("missing_parent", self),
            DotsyncError::ScopeCycle { .. } => basic_error_report("scope_cycle", self),
            DotsyncError::ConfigParse { .. } => basic_error_report("config_parse", self),
            DotsyncError::CascadeState { .. } => basic_error_report("cascade_state", self),
            DotsyncError::SyncState { .. } => basic_error_report("sync_state", self),
            DotsyncError::RepoAlreadyExists { .. } => basic_error_report("repo_exists", self),
            DotsyncError::MissingHostname => basic_error_report("missing_hostname", self),
            DotsyncError::Io { .. } => basic_error_report("io", self),
            DotsyncError::Jj { .. } => basic_error_report("jj", self),
            DotsyncError::NotImplemented(_) => basic_error_report("not_implemented", self),
        }
    }
}

fn basic_error_report(code: &'static str, error: &DotsyncError) -> ErrorReport {
    ErrorReport {
        code,
        message: error.to_string(),
        drifts: Vec::new(),
        current_state: error_current_state(error),
    }
}

fn error_current_state(error: &DotsyncError) -> Option<String> {
    match error {
        DotsyncError::CascadeInProgress { scope } => Some(format!("paused cascade scope: {scope}")),
        DotsyncError::InvalidScope { scope } => Some(format!("requested scope: {scope}")),
        DotsyncError::ScopeNotAncestor {
            scope,
            current_scope,
        } => Some(format!(
            "requested scope: {scope}; current machine scope: {current_scope}"
        )),
        DotsyncError::SyncState { path, .. } => {
            Some(format!("sync state path: {}", path.display()))
        }
        DotsyncError::CommitPathOutsideRepo { path } | DotsyncError::CommitPathMissing { path } => {
            Some(format!("requested path: {}", path.display()))
        }
        DotsyncError::ConfigOnlyAllowedOnBaseScope { config_path, scope } => Some(format!(
            "requested scope: {scope}; restricted path: {config_path}"
        )),
        DotsyncError::DirtyWorkingCopy { count } => Some(format!(
            "working copy has uncommitted changes in {count} path(s)"
        )),
        DotsyncError::NoPausedCascade => Some("no cascade is currently paused".to_string()),
        _ => None,
    }
}

pub use crate::cascade::CascadePause;

#[derive(Debug, Error)]
pub enum DotsyncError {
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("scope `{scope}` references missing parent `{parent}`")]
    MissingParent { scope: String, parent: String },
    #[error("scope graph contains a cycle involving `{scope}`")]
    ScopeCycle { scope: String },
    #[error("no scope bookmark points at the working copy commit")]
    NoCurrentScope,
    #[error("scope `{scope}` does not exist in config")]
    InvalidScope { scope: String },
    #[error("commit mode requires explicit file/directory paths or --all")]
    CommitSelectionRequired,
    #[error("commit mode accepts explicit paths or --all, not both")]
    ConflictingCommitSelection,
    #[error("commit path `{path}` is outside the repo root")]
    CommitPathOutsideRepo { path: PathBuf },
    #[error("commit path `{path}` does not exist in the working copy or selected deletion set")]
    CommitPathMissing { path: PathBuf },
    #[error("commit selection did not match any working-copy changes")]
    CommitSelectionEmpty,
    #[error("{config_path} may only be committed to scope `all`; requested scope `{scope}`")]
    ConfigOnlyAllowedOnBaseScope { config_path: String, scope: String },
    #[error("scope `{scope}` is not an ancestor of `{current_scope}`")]
    ScopeNotAncestor {
        scope: String,
        current_scope: String,
    },
    #[error("scope `{scope}` does not have a local bookmark")]
    MissingScopeBookmark { scope: String },
    #[error("cascade already in progress on `{scope}`")]
    CascadeInProgress { scope: String },
    #[error("no cascade is currently paused")]
    NoPausedCascade,
    #[error("cascade state error at {path}: {source}")]
    CascadeState {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("sync state error at {path}: {message}")]
    SyncState { path: PathBuf, message: String },
    #[error("detected drift in {count} file(s)")]
    DriftDetected {
        count: usize,
        drifts: Vec<FileDrift>,
    },
    #[error(
        "working copy has uncommitted changes in {count} path(s); plain `dotsync` requires a clean working copy. Use `dotsync <scope> -m \"message\"` instead"
    )]
    DirtyWorkingCopy { count: usize },
    #[error("repo already exists at {path}")]
    RepoAlreadyExists { path: PathBuf },
    #[error("unable to determine machine hostname")]
    MissingHostname,
    #[error("jj operation failed: {message}")]
    Jj { message: String },
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    scopes: HashMap<String, RawScope>,
    #[serde(default)]
    sync: RawSyncConfig,
}

#[derive(Debug, Default, Deserialize)]
struct RawScope {
    #[serde(default)]
    parents: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawSyncConfig {
    #[serde(default = "default_sync_state_relative_path")]
    state_path: String,
}

impl Default for RawSyncConfig {
    fn default() -> Self {
        Self {
            state_path: default_sync_state_relative_path(),
        }
    }
}

#[derive(Debug, Clone)]
struct DotsyncConfig {
    graph: ScopeGraph,
    sync_state_relative_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncStatePayload {
    machine_scope: String,
    last_synced_revision: String,
}

#[derive(Debug, Clone)]
struct SyncState {
    machine_scope: String,
    last_synced_revision: CommitId,
}

#[derive(Debug, Clone)]
struct ScopeGraph {
    parents: HashMap<String, Vec<String>>,
    children: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
struct MachineIdentity {
    os_scope: String,
    machine_scope: String,
}

pub async fn init(paths: &DotsyncPaths, remote_url: &str) -> Result<InitReport, DotsyncError> {
    if paths.repo_root.exists() {
        return Err(DotsyncError::RepoAlreadyExists {
            path: paths.repo_root.clone(),
        });
    }
    if let Some(parent) = paths.repo_root.parent() {
        fs::create_dir_all(parent).map_err(|source| DotsyncError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::create_dir_all(&paths.repo_root).map_err(|source| DotsyncError::Io {
        path: paths.repo_root.clone(),
        source,
    })?;

    let settings = default_settings()?;
    let (_workspace, repo) = Workspace::init_colocated_git(&settings, &paths.repo_root)
        .await
        .map_err(|err| jj_error(format!("init colocated repo: {err}")))?;
    let _repo = add_origin_remote(repo, remote_url).await?;
    let mut workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("reload repo after adding origin: {err}")))?;
    let repo = fetch_origin(repo).await?;
    let identity = detect_machine()?;

    let remote_empty = repo.view().all_remote_bookmarks().next().is_none();
    let (created_scopes, current_scope) = if remote_empty {
        bootstrap_empty_remote(paths, &mut workspace, &identity).await?
    } else {
        join_existing_remote(paths, &mut workspace, repo, &identity).await?
    };

    let sync = sync_repo_to_home(
        paths,
        SyncOptions { force: true },
        &[],
        Some(&current_scope),
    )
    .await?;
    push_scope_updates(paths).await?;

    Ok(InitReport {
        current_scope,
        created_scopes,
        sync,
    })
}

pub async fn sync(paths: &DotsyncPaths, options: SyncOptions) -> Result<SyncReport, DotsyncError> {
    ensure_no_paused_cascade(paths)?;
    let workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("load repo at head: {err}")))?;

    let snapshot = snapshot_working_copy(paths).await?;
    if !snapshot.changed_paths.is_empty() {
        return Err(DotsyncError::DirtyWorkingCopy {
            count: snapshot.changed_paths.len(),
        });
    }

    let _repo = fetch_origin(repo).await?;

    sync_repo_to_home(paths, options, &[], None).await
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
            push_scope_updates(paths).await?;

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
    let graph = load_scope_graph(paths).await?;
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
            push_scope_updates(paths).await?;

            Ok(CommandOutcome::Success(ContinueReport {
                cascaded_scopes: success.progress.completed_scopes,
                sync,
            }))
        }
    }
}

async fn prepare_commit_session(
    paths: &DotsyncPaths,
    target_scope: &str,
) -> Result<CommitSession, DotsyncError> {
    let workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("load repo at head: {err}")))?;
    let initial_graph = load_scope_graph(paths).await?;
    let current_scope = detect_current_scope(&initial_graph, &workspace, repo.as_ref())?;

    let _repo = fetch_origin(repo).await?;
    let graph = load_scope_graph(paths).await?;
    validate_commit_scope(&graph, target_scope, &current_scope)?;

    Ok(CommitSession {
        current_scope,
        graph,
    })
}

fn validate_commit_scope(
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

fn ensure_no_paused_cascade(paths: &DotsyncPaths) -> Result<(), DotsyncError> {
    if let Some(state) = cascade_state_store(paths).load()? {
        return Err(DotsyncError::CascadeInProgress {
            scope: state.paused_scope,
        });
    }
    Ok(())
}

async fn commit_snapshot_and_apply_cascade(
    paths: &DotsyncPaths,
    session: &CommitSession,
    repo: std::sync::Arc<ReadonlyRepo>,
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
            let paused_state =
                build_paused_state(&cascade_plan, &pause, &cascade_command, &scope_heads);
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
    pause: &CascadePause,
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
    pause: &CascadePause,
    description: &str,
) -> Result<jj_lib::commit::Commit, DotsyncError> {
    let current_commit =
        create_temporary_conflict_commit(mut_repo, scope_heads, pause, description).await?;
    mut_repo
        .set_wc_commit(workspace_name, current_commit.id().clone())
        .map_err(|err| jj_error(format!("set paused working copy commit: {err}")))?;
    Ok(current_commit)
}

async fn load_scope_graph(paths: &DotsyncPaths) -> Result<ScopeGraph, DotsyncError> {
    Ok(load_config(paths).await?.graph)
}

fn resolve_commit_selection(
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

fn normalize_selected_repo_paths(
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

fn path_matches_selection(selected_paths: &[PathBuf], changed_path: &Path) -> bool {
    selected_paths.iter().any(|selected| {
        changed_path == selected || changed_path.starts_with(selected) || selected.starts_with(changed_path)
    })
}

fn validate_config_commit_scope(selected_paths: &[PathBuf], scope: &str) -> Result<(), DotsyncError> {
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

async fn project_selected_paths(
    selected_tree: &jj_lib::merged_tree::MergedTree,
    base_tree: &jj_lib::merged_tree::MergedTree,
    selected_paths: &[PathBuf],
) -> Result<jj_lib::merged_tree::MergedTree, DotsyncError> {
    let mut builder = MergedTreeBuilder::new(base_tree.clone());
    for path in selected_paths {
        let repo_path = RepoPathBuf::from_internal_string(path.to_string_lossy().replace('\\', "/"))
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

async fn restore_working_copy_paths(
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
        let resolved = value
            .into_resolved()
            .map_err(|conflict| jj_error(format!("restore path {} is conflicted: {conflict:?}", path.display())))?;

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

impl ScopeGraph {
    fn new(parents: HashMap<String, Vec<String>>) -> Result<Self, DotsyncError> {
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for scope in parents.keys() {
            children.entry(scope.clone()).or_default();
        }
        for (scope, scope_parents) in &parents {
            for parent in scope_parents {
                if !parents.contains_key(parent) {
                    return Err(DotsyncError::MissingParent {
                        scope: scope.clone(),
                        parent: parent.clone(),
                    });
                }
                children
                    .entry(parent.clone())
                    .or_default()
                    .push(scope.clone());
            }
        }
        let graph = Self { parents, children };
        validate_scope_graph(&graph)?;
        Ok(graph)
    }
}

fn validate_scope_graph(graph: &ScopeGraph) -> Result<(), DotsyncError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Visiting,
        Visited,
    }

    fn visit(
        graph: &ScopeGraph,
        scope: &str,
        states: &mut HashMap<String, VisitState>,
    ) -> Result<(), DotsyncError> {
        if let Some(state) = states.get(scope) {
            return match state {
                VisitState::Visiting => Err(DotsyncError::ScopeCycle {
                    scope: scope.to_string(),
                }),
                VisitState::Visited => Ok(()),
            };
        }

        states.insert(scope.to_string(), VisitState::Visiting);
        if let Some(parents) = graph.parents.get(scope) {
            for parent in parents {
                visit(graph, parent, states)?;
            }
        }
        states.insert(scope.to_string(), VisitState::Visited);
        Ok(())
    }

    let mut states = HashMap::new();
    for scope in graph.parents.keys() {
        visit(graph, scope, &mut states)?;
    }
    Ok(())
}

fn load_workspace(paths: &DotsyncPaths) -> Result<Workspace, DotsyncError> {
    let settings = default_settings()?;
    Workspace::load(
        &settings,
        &paths.repo_root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .map_err(|err| jj_error(format!("load workspace: {err}")))
}

fn default_settings() -> Result<UserSettings, DotsyncError> {
    let config = StackedConfig::with_defaults();
    UserSettings::from_config(config).map_err(|err| jj_error(format!("load jj settings: {err}")))
}

fn detect_current_scope(
    graph: &ScopeGraph,
    workspace: &Workspace,
    repo: &dyn jj_lib::repo::Repo,
) -> Result<String, DotsyncError> {
    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(workspace.workspace_name())
        .ok_or(DotsyncError::NoCurrentScope)?;

    let mut best: Option<(String, usize)> = None;
    let mut memo = HashMap::new();
    for (name, _) in repo.view().local_bookmarks_for_commit(wc_commit_id) {
        let scope = name.as_str().to_owned();
        if !graph.parents.contains_key(&scope) {
            continue;
        }
        let depth = scope_depth(graph, &scope, &mut memo)?;
        match &best {
            Some((_, best_depth)) if *best_depth >= depth => {}
            _ => best = Some((scope, depth)),
        }
    }

    best.map(|(scope, _)| scope)
        .ok_or(DotsyncError::NoCurrentScope)
}

fn scope_depth(
    graph: &ScopeGraph,
    scope: &str,
    memo: &mut HashMap<String, usize>,
) -> Result<usize, DotsyncError> {
    if let Some(depth) = memo.get(scope) {
        return Ok(*depth);
    }
    let parents = graph
        .parents
        .get(scope)
        .ok_or_else(|| DotsyncError::InvalidScope {
            scope: scope.to_string(),
        })?;
    let depth = if parents.is_empty() {
        0
    } else {
        parents
            .iter()
            .map(|parent| scope_depth(graph, parent, memo))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(0)
            + 1
    };
    memo.insert(scope.to_string(), depth);
    Ok(depth)
}

async fn detect_drifts(
    paths: &DotsyncPaths,
    repo: &dyn jj_lib::repo::Repo,
    managed_entries: &BTreeMap<PathBuf, TreeValue>,
) -> Result<Vec<FileDrift>, DotsyncError> {
    let mut drifts = Vec::new();
    for (relative, value) in managed_entries {
        let system_path = paths.home_dir.join(relative);
        let repo_bytes = read_tree_entry_bytes(repo.store(), relative, value).await?;
        let system_bytes = match fs::read(&system_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(DotsyncError::Io {
                    path: system_path.clone(),
                    source,
                })
            }
        };
        if repo_bytes != system_bytes {
            drifts.push(FileDrift {
                repo_path: relative.clone(),
                system_path: system_path.clone(),
                diff: render_diff(&repo_bytes, &system_bytes),
            });
        }
    }
    Ok(drifts)
}

fn render_diff(repo_bytes: &[u8], system_bytes: &[u8]) -> String {
    match (
        String::from_utf8(repo_bytes.to_vec()),
        String::from_utf8(system_bytes.to_vec()),
    ) {
        (Ok(repo), Ok(system)) => format!("--- repo\n+++ system\n- {repo:?}\n+ {system:?}"),
        _ => "binary content differs".to_string(),
    }
}

async fn copy_repo_file_to_home(
    paths: &DotsyncPaths,
    repo: &dyn jj_lib::repo::Repo,
    relative: &Path,
    value: &TreeValue,
) -> Result<(), DotsyncError> {
    let system_path = paths.home_dir.join(relative);
    if let Some(parent) = system_path.parent() {
        fs::create_dir_all(parent).map_err(|source| DotsyncError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let contents = read_tree_entry_bytes(repo.store(), relative, value).await?;
    fs::write(&system_path, contents).map_err(|source| DotsyncError::Io {
        path: system_path,
        source,
    })
}

async fn sync_repo_to_home(
    paths: &DotsyncPaths,
    options: SyncOptions,
    expected_repo_changes: &[PathBuf],
    machine_scope_hint: Option<&str>,
) -> Result<SyncReport, DotsyncError> {
    let config = load_config(paths).await?;
    let graph = config.graph.clone();
    let workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("load repo at head: {err}")))?;

    let sync_state = load_sync_state(paths, &config)?;
    let current_scope = match (&sync_state, machine_scope_hint) {
        (Some(state), _) => state.machine_scope.clone(),
        (None, Some(scope)) => scope.to_string(),
        (None, None) => detect_current_scope(&graph, &workspace, repo.as_ref())?,
    };
    let current_commit = load_scope_commit(repo.as_ref(), &current_scope)?;
    let internal_paths = internal_repo_paths(&config);
    let repo_entries = collect_managed_tree_entries(&current_commit.tree(), &internal_paths)?;
    let expected_repo_changes: BTreeSet<&PathBuf> = expected_repo_changes.iter().collect();
    let drifts = detect_drifts(paths, repo.as_ref(), &repo_entries)
        .await?
        .into_iter()
        .filter(|drift| !expected_repo_changes.contains(&drift.repo_path))
        .collect::<Vec<_>>();
    if !drifts.is_empty() && !options.force {
        return Err(DotsyncError::DriftDetected {
            count: drifts.len(),
            drifts,
        });
    }

    let mut synced_paths = Vec::with_capacity(repo_entries.len());
    for (relative, value) in &repo_entries {
        copy_repo_file_to_home(paths, repo.as_ref(), relative, value).await?;
        synced_paths.push(relative.clone());
    }

    if let Some(state) = &sync_state {
        let previous_commit = repo
            .store()
            .get_commit(&state.last_synced_revision)
            .map_err(|err| DotsyncError::SyncState {
                path: sync_state_path(paths, &config),
                message: format!(
                    "last_synced_revision `{}` does not resolve to a commit: {err}",
                    state.last_synced_revision.hex()
                ),
            })?;
        let previous_entries =
            collect_managed_tree_entries(&previous_commit.tree(), &internal_paths)?;
        for removed_path in previous_entries
            .keys()
            .filter(|path| !repo_entries.contains_key(*path))
        {
            remove_home_path(paths, removed_path)?;
        }
    }

    save_sync_state(paths, &config, &current_scope, current_commit.id())?;

    Ok(SyncReport {
        current_scope,
        synced_paths,
        drifts,
    })
}

async fn snapshot_working_copy(paths: &DotsyncPaths) -> Result<WorkingCopySnapshot, DotsyncError> {
    let mut workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("load repo at head: {err}")))?;

    let mut locked_ws = workspace
        .start_working_copy_mutation()
        .map_err(|err| jj_error(format!("lock working copy: {err}")))?;
    let snapshot_options = SnapshotOptions {
        base_ignores: GitIgnoreFile::empty(),
        progress: None,
        start_tracking_matcher: &EverythingMatcher,
        force_tracking_matcher: &EverythingMatcher,
        max_new_file_size: u64::MAX,
    };
    let old_tree = locked_ws.locked_wc().old_tree().clone();
    let (tree, _) = locked_ws
        .locked_wc()
        .snapshot(&snapshot_options)
        .await
        .map_err(|err| jj_error(format!("snapshot working copy: {err}")))?;
    locked_ws
        .finish(repo.op_id().clone())
        .map_err(|err| jj_error(format!("finish working copy mutation: {err}")))?;

    let changed_paths = changed_repo_paths(&old_tree, &tree)?;
    Ok(WorkingCopySnapshot {
        tree,
        changed_paths,
    })
}

async fn add_origin_remote(
    repo: std::sync::Arc<ReadonlyRepo>,
    remote_url: &str,
) -> Result<std::sync::Arc<ReadonlyRepo>, DotsyncError> {
    let mut tx = repo.start_transaction();
    git::add_remote(
        tx.repo_mut(),
        "origin".as_ref(),
        remote_url,
        None,
        Tags::None,
        &StringExpression::all(),
    )
    .map_err(|err| jj_error(format!("add origin remote: {err}")))?;
    tx.commit("dotsync: add origin remote")
        .await
        .map_err(|err| jj_error(format!("commit remote setup: {err}")))
}

async fn fetch_origin(
    repo: std::sync::Arc<ReadonlyRepo>,
) -> Result<std::sync::Arc<ReadonlyRepo>, DotsyncError> {
    let settings = default_settings()?;
    let subprocess_options = GitSubprocessOptions::from_settings(&settings)
        .map_err(|err| jj_error(format!("load git subprocess settings: {err}")))?;
    let import_options = default_import_options();
    let mut tx = repo.start_transaction();
    let mut fetch = GitFetch::new(tx.repo_mut(), subprocess_options, &import_options)
        .map_err(|err| jj_error(format!("prepare fetch: {err}")))?;
    let refspecs = git::expand_fetch_refspecs(
        "origin".as_ref(),
        GitFetchRefExpression {
            bookmark: StringExpression::all(),
            tag: StringExpression::none(),
        },
    )
    .map_err(|err| jj_error(format!("expand fetch refspecs: {err}")))?;
    fetch
        .fetch(
            "origin".as_ref(),
            refspecs,
            &mut QuietGitCallback,
            None,
            None,
        )
        .map_err(|err| jj_error(format!("fetch origin: {err}")))?;
    fetch
        .import_refs()
        .map_err(|err| jj_error(format!("import fetched refs: {err}")))?;
    sync_local_bookmarks_from_remote(tx.repo_mut(), "origin".as_ref());
    tx.commit("dotsync: fetch origin")
        .await
        .map_err(|err| jj_error(format!("commit fetch operation: {err}")))
}

fn sync_local_bookmarks_from_remote(
    mut_repo: &mut MutableRepo,
    remote_name: &jj_lib::ref_name::RemoteName,
) {
    let updates: Vec<(RefNameBuf, jj_lib::backend::CommitId)> = mut_repo
        .view()
        .remote_bookmarks(remote_name)
        .filter_map(|(name, remote_ref)| {
            remote_ref
                .target
                .as_normal()
                .map(|id| (RefNameBuf::from(name.as_str()), id.clone()))
        })
        .collect();
    for (name, id) in updates {
        mut_repo.set_local_bookmark_target(name.as_ref(), RefTarget::normal(id));
    }
}

async fn bootstrap_empty_remote(
    paths: &DotsyncPaths,
    workspace: &mut Workspace,
    identity: &MachineIdentity,
) -> Result<(Vec<String>, String), DotsyncError> {
    let graph = ScopeGraph::new(HashMap::from([
        ("all".to_string(), vec![]),
        (identity.os_scope.clone(), vec!["all".to_string()]),
        (
            identity.machine_scope.clone(),
            vec![identity.os_scope.clone()],
        ),
    ]))?;
    write_config(paths, &render_config(&graph))?;

    let snapshot = snapshot_working_copy(paths).await?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("reload repo after init snapshot: {err}")))?;
    let root_commit = repo.store().root_commit();

    let mut tx = repo.start_transaction();
    let all_commit = tx
        .repo_mut()
        .new_commit(vec![root_commit.id().clone()], snapshot.tree)
        .set_description("dotsync: initialize all scope")
        .write()
        .await
        .map_err(|err| jj_error(format!("write all scope commit: {err}")))?;
    tx.repo_mut()
        .set_local_bookmark_target("all".as_ref(), RefTarget::normal(all_commit.id().clone()));

    let os_commit = tx
        .repo_mut()
        .new_commit(vec![all_commit.id().clone()], all_commit.tree())
        .set_description(format!("dotsync: create {} scope", identity.os_scope))
        .write()
        .await
        .map_err(|err| jj_error(format!("write os scope commit: {err}")))?;
    tx.repo_mut().set_local_bookmark_target(
        RefNameBuf::from(identity.os_scope.as_str()).as_ref(),
        RefTarget::normal(os_commit.id().clone()),
    );

    let machine_commit = tx
        .repo_mut()
        .new_commit(vec![os_commit.id().clone()], os_commit.tree())
        .set_description(format!("dotsync: create {} scope", identity.machine_scope))
        .write()
        .await
        .map_err(|err| jj_error(format!("write machine scope commit: {err}")))?;
    tx.repo_mut().set_local_bookmark_target(
        RefNameBuf::from(identity.machine_scope.as_str()).as_ref(),
        RefTarget::normal(machine_commit.id().clone()),
    );
    tx.repo_mut()
        .set_wc_commit(
            workspace.workspace_name().to_owned(),
            machine_commit.id().clone(),
        )
        .map_err(|err| jj_error(format!("set init working copy commit: {err}")))?;
    let repo = tx
        .commit("dotsync: initialize scopes")
        .await
        .map_err(|err| jj_error(format!("commit init scopes: {err}")))?;

    workspace
        .check_out(repo.op_id().clone(), None, &machine_commit)
        .await
        .map_err(|err| jj_error(format!("check out machine scope: {err}")))?;

    Ok((
        vec![
            "all".to_string(),
            identity.os_scope.clone(),
            identity.machine_scope.clone(),
        ],
        identity.machine_scope.clone(),
    ))
}

async fn join_existing_remote(
    paths: &DotsyncPaths,
    workspace: &mut Workspace,
    _repo: std::sync::Arc<ReadonlyRepo>,
    identity: &MachineIdentity,
) -> Result<(Vec<String>, String), DotsyncError> {
    checkout_workspace_to_scope(paths, workspace, "all").await?;
    let graph = load_scope_graph(paths).await?;

    let mut parents = graph.parents.clone();
    let mut created_scopes = Vec::new();
    if !parents.contains_key(&identity.os_scope) {
        parents.insert(identity.os_scope.clone(), vec!["all".to_string()]);
        created_scopes.push(identity.os_scope.clone());
    }
    if !parents.contains_key(&identity.machine_scope) {
        parents.insert(
            identity.machine_scope.clone(),
            vec![identity.os_scope.clone()],
        );
        created_scopes.push(identity.machine_scope.clone());
    }

    if created_scopes.is_empty() {
        checkout_workspace_to_scope(paths, workspace, &identity.machine_scope).await?;
        return Ok((created_scopes, identity.machine_scope.clone()));
    }

    let updated_graph = ScopeGraph::new(parents)?;
    write_config(paths, &render_config(&updated_graph))?;

    let snapshot = snapshot_working_copy(paths).await?;
    let repo = load_repo(workspace).await?;

    let mut tx = repo.start_transaction();
    let mut scope_heads = ScopeHeads::load_existing(tx.repo_mut().base_repo(), &updated_graph)?;
    let all_head = scope_heads.require("all")?;

    let config_commit = tx
        .repo_mut()
        .new_commit(vec![all_head.id().clone()], snapshot.tree)
        .set_description("dotsync: update scope config")
        .write()
        .await
        .map_err(|err| jj_error(format!("write config update commit: {err}")))?;
    tx.repo_mut().set_local_bookmark_target(
        "all".as_ref(),
        RefTarget::normal(config_commit.id().clone()),
    );
    scope_heads.update("all".to_string(), config_commit.clone());

    let cascade_command = CascadeCommand {
        root_scope: "all".to_string(),
        description: "dotsync: cascade init config".to_string(),
        original_scope: "all".to_string(),
        machine_scope: identity.machine_scope.clone(),
    };
    let cascade_plan = build_cascade_plan(&updated_graph, &scope_heads, &cascade_command);
    let descendant_scopes = match execute_cascade_plan(
        tx.repo_mut(),
        &mut scope_heads,
        &cascade_plan,
        &cascade_command,
    )
    .await?
    {
        CascadeOutcome::Completed(success) => success.progress.completed_scopes,
        CascadeOutcome::Paused(_pause) => {
            return Err(DotsyncError::Jj {
                message: "unexpected conflict while cascading init config".to_string(),
            })
        }
    };

    if !scope_heads.contains(&identity.os_scope) {
        let parent = scope_heads.require("all")?;
        let commit = tx
            .repo_mut()
            .new_commit(vec![parent.id().clone()], parent.tree())
            .set_description(format!("dotsync: create {} scope", identity.os_scope))
            .write()
            .await
            .map_err(|err| jj_error(format!("write new os scope: {err}")))?;
        tx.repo_mut().set_local_bookmark_target(
            RefNameBuf::from(identity.os_scope.as_str()).as_ref(),
            RefTarget::normal(commit.id().clone()),
        );
        scope_heads.update(identity.os_scope.clone(), commit);
    }

    if !scope_heads.contains(&identity.machine_scope) {
        let parent = scope_heads.require(&identity.os_scope)?;
        let commit = tx
            .repo_mut()
            .new_commit(vec![parent.id().clone()], parent.tree())
            .set_description(format!("dotsync: create {} scope", identity.machine_scope))
            .write()
            .await
            .map_err(|err| jj_error(format!("write new machine scope: {err}")))?;
        tx.repo_mut().set_local_bookmark_target(
            RefNameBuf::from(identity.machine_scope.as_str()).as_ref(),
            RefTarget::normal(commit.id().clone()),
        );
        scope_heads.update(identity.machine_scope.clone(), commit);
    }

    let machine_commit = scope_heads.require(&identity.machine_scope)?;
    tx.repo_mut()
        .set_wc_commit(
            workspace.workspace_name().to_owned(),
            machine_commit.id().clone(),
        )
        .map_err(|err| jj_error(format!("set join working copy commit: {err}")))?;
    let repo = tx
        .commit("dotsync: initialize machine scope")
        .await
        .map_err(|err| jj_error(format!("commit join scope changes: {err}")))?;
    workspace
        .check_out(repo.op_id().clone(), None, &machine_commit)
        .await
        .map_err(|err| jj_error(format!("check out joined machine scope: {err}")))?;

    let mut created = descendant_scopes;
    created.extend(created_scopes);
    created.sort();
    created.dedup();
    Ok((created, identity.machine_scope.clone()))
}

async fn checkout_workspace_to_scope(
    paths: &DotsyncPaths,
    workspace: &mut Workspace,
    scope: &str,
) -> Result<(), DotsyncError> {
    let repo = load_repo(workspace).await?;
    let commit_id = repo
        .view()
        .get_local_bookmark(RefNameBuf::from(scope).as_ref())
        .as_normal()
        .cloned()
        .ok_or_else(|| DotsyncError::MissingScopeBookmark {
            scope: scope.to_string(),
        })?;
    let commit = repo
        .store()
        .get_commit(&commit_id)
        .map_err(|err| jj_error(format!("load checkout commit for {scope}: {err}")))?;

    let mut tx = repo.start_transaction();
    tx.repo_mut()
        .set_wc_commit(workspace.workspace_name().to_owned(), commit.id().clone())
        .map_err(|err| jj_error(format!("set workspace commit for {scope}: {err}")))?;
    let repo = tx
        .commit(format!("dotsync: check out {scope}"))
        .await
        .map_err(|err| jj_error(format!("commit checkout operation for {scope}: {err}")))?;
    checkout_workspace_to_commit(workspace, repo.op_id().clone(), &commit).await?;

    if !repo_config_path(paths).exists() && scope == "all" {
        return Err(DotsyncError::Io {
            path: repo_config_path(paths),
            source: io::Error::new(io::ErrorKind::NotFound, "config missing after checkout"),
        });
    }
    Ok(())
}

async fn checkout_workspace_to_commit(
    workspace: &mut Workspace,
    op_id: jj_lib::op_store::OperationId,
    commit: &jj_lib::commit::Commit,
) -> Result<(), DotsyncError> {
    workspace
        .check_out(op_id, None, commit)
        .await
        .map_err(|err| jj_error(format!("materialize checkout: {err}")))?;
    Ok(())
}

async fn load_repo(workspace: &Workspace) -> Result<std::sync::Arc<ReadonlyRepo>, DotsyncError> {
    workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("load repo at head: {err}")))
}

async fn push_scope_updates(paths: &DotsyncPaths) -> Result<(), DotsyncError> {
    let workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("load repo before push: {err}")))?;
    let settings = default_settings()?;
    let subprocess_options = GitSubprocessOptions::from_settings(&settings)
        .map_err(|err| jj_error(format!("load git subprocess settings: {err}")))?;

    let updates: Vec<(RefNameBuf, BookmarkPushUpdate)> = repo
        .view()
        .local_remote_bookmarks("origin".as_ref())
        .filter_map(|(name, targets)| {
            let local = targets.local_target.as_normal()?.clone();
            let remote = targets.remote_ref.target.as_normal().cloned();
            if remote.as_ref() == Some(&local) {
                return None;
            }
            Some((
                RefNameBuf::from(name.as_str()),
                BookmarkPushUpdate {
                    old_target: remote,
                    new_target: Some(local),
                },
            ))
        })
        .collect();

    if updates.is_empty() {
        return Ok(());
    }

    let mut tx = repo.start_transaction();
    git::push_branches(
        tx.repo_mut(),
        subprocess_options,
        "origin".as_ref(),
        &GitBranchPushTargets {
            branch_updates: updates,
        },
        &mut QuietGitCallback,
        &GitPushOptions::default(),
    )
    .map_err(|err| jj_error(format!("push branches: {err}")))?;
    tx.commit("dotsync: push scope updates")
        .await
        .map_err(|err| jj_error(format!("commit push operation: {err}")))?;
    Ok(())
}

fn is_ancestor_scope(
    graph: &ScopeGraph,
    ancestor: &str,
    scope: &str,
) -> Result<bool, DotsyncError> {
    if ancestor == scope {
        return Ok(true);
    }
    let parents = graph
        .parents
        .get(scope)
        .ok_or_else(|| DotsyncError::InvalidScope {
            scope: scope.to_string(),
        })?;
    for parent in parents {
        if is_ancestor_scope(graph, ancestor, parent)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn render_config(graph: &ScopeGraph) -> String {
    let mut scopes: Vec<String> = graph.parents.keys().cloned().collect();
    let mut memo = HashMap::new();
    scopes.sort_by(|a, b| {
        let depth_a = scope_depth(graph, a, &mut memo).unwrap_or(usize::MAX);
        let depth_b = scope_depth(graph, b, &mut memo).unwrap_or(usize::MAX);
        depth_a.cmp(&depth_b).then_with(|| a.cmp(b))
    });

    let mut rendered = String::from("[scopes]\n");
    for scope in scopes {
        let parents = &graph.parents[&scope];
        if parents.is_empty() {
            rendered.push_str(&format!("{scope} = {{}}\n"));
        } else {
            let parents = parents
                .iter()
                .map(|parent| format!("\"{parent}\""))
                .collect::<Vec<_>>()
                .join(", ");
            rendered.push_str(&format!("{scope} = {{ parents = [{parents}] }}\n"));
        }
    }
    rendered.push_str("\n[sync]\n");
    rendered.push_str(&format!(
        "state_path = \"{}\"\n",
        default_sync_state_relative_path()
    ));
    rendered
}

fn write_config(paths: &DotsyncPaths, contents: &str) -> Result<(), DotsyncError> {
    let path = repo_config_path(paths);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| DotsyncError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(&path, contents).map_err(|source| DotsyncError::Io { path, source })
}

fn changed_repo_paths(
    old_tree: &jj_lib::merged_tree::MergedTree,
    new_tree: &jj_lib::merged_tree::MergedTree,
) -> Result<Vec<PathBuf>, DotsyncError> {
    let old_entries = collect_tree_entries(old_tree)?;
    let new_entries = collect_tree_entries(new_tree)?;
    let all_paths = old_entries
        .keys()
        .chain(new_entries.keys())
        .cloned()
        .collect::<BTreeSet<_>>();

    Ok(all_paths
        .into_iter()
        .filter(|path| old_entries.get(path) != new_entries.get(path))
        .map(|path| PathBuf::from(path.as_internal_file_string()))
        .collect())
}

fn collect_tree_entries(
    tree: &jj_lib::merged_tree::MergedTree,
) -> Result<BTreeMap<RepoPathBuf, MergedTreeValue>, DotsyncError> {
    tree.entries()
        .map(|(path, value)| {
            let display_path = path.as_internal_file_string().to_string();
            value
                .map(|value| (path, value))
                .map_err(|err| jj_error(format!("read tree entry {}: {err}", display_path)))
        })
        .collect()
}

async fn load_config(paths: &DotsyncPaths) -> Result<DotsyncConfig, DotsyncError> {
    let workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("load repo for config: {err}")))?;
    let all_commit = load_scope_commit(repo.as_ref(), "all")?;
    let repo_path = jj_lib::repo_path::RepoPath::from_internal_string(DOTSYNC_CONFIG_RELATIVE_PATH)
        .map_err(|err| jj_error(format!("invalid config repo path: {err}")))?;
    let value = all_commit
        .tree()
        .path_value(repo_path)
        .map_err(|err| jj_error(format!("read config tree entry: {err}")))?;
    let value = value
        .into_resolved()
        .map_err(|conflict| jj_error(format!("config path is conflicted on all: {conflict:?}")))?
        .ok_or_else(|| DotsyncError::Io {
            path: repo_config_path(paths),
            source: io::Error::new(io::ErrorKind::NotFound, "config missing on all scope"),
        })?;
    let contents = read_tree_entry_bytes(repo.store(), Path::new(DOTSYNC_CONFIG_RELATIVE_PATH), &value)
        .await?;
    let contents = String::from_utf8(contents)
        .map_err(|err| jj_error(format!("config file is not valid utf-8: {err}")))?;
    parse_config(&repo_config_path(paths), &contents)
}

fn parse_config(path: &Path, contents: &str) -> Result<DotsyncConfig, DotsyncError> {
    let raw: RawConfig = toml::from_str(contents).map_err(|source| DotsyncError::ConfigParse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(DotsyncConfig {
        graph: ScopeGraph::new(
            raw.scopes
                .into_iter()
                .map(|(name, scope)| (name, scope.parents))
                .collect(),
        )?,
        sync_state_relative_path: PathBuf::from(raw.sync.state_path),
    })
}

fn internal_repo_paths(config: &DotsyncConfig) -> BTreeSet<PathBuf> {
    BTreeSet::from([config.sync_state_relative_path.clone()])
}

fn load_sync_state(
    paths: &DotsyncPaths,
    config: &DotsyncConfig,
) -> Result<Option<SyncState>, DotsyncError> {
    let path = sync_state_path(paths, config);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(DotsyncError::Io { path, source }),
    };
    let payload: SyncStatePayload =
        serde_json::from_str(&contents).map_err(|err| DotsyncError::SyncState {
            path: path.clone(),
            message: format!("failed to parse sync state: {err}"),
        })?;
    if payload.machine_scope.trim().is_empty() {
        return Err(DotsyncError::SyncState {
            path,
            message: "machine_scope is empty".to_string(),
        });
    }
    let last_synced_revision =
        CommitId::try_from_hex(&payload.last_synced_revision).ok_or_else(|| {
            DotsyncError::SyncState {
                path: path.clone(),
                message: format!(
                    "last_synced_revision `{}` is not valid hex",
                    payload.last_synced_revision
                ),
            }
        })?;
    Ok(Some(SyncState {
        machine_scope: payload.machine_scope,
        last_synced_revision,
    }))
}

fn save_sync_state(
    paths: &DotsyncPaths,
    config: &DotsyncConfig,
    machine_scope: &str,
    last_synced_revision: &CommitId,
) -> Result<(), DotsyncError> {
    let path = sync_state_path(paths, config);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| DotsyncError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let payload = SyncStatePayload {
        machine_scope: machine_scope.to_string(),
        last_synced_revision: last_synced_revision.hex(),
    };
    let contents = serde_json::to_vec_pretty(&payload).map_err(|err| DotsyncError::SyncState {
        path: path.clone(),
        message: format!("failed to serialize sync state: {err}"),
    })?;
    fs::write(&path, contents).map_err(|source| DotsyncError::Io { path, source })
}

fn sync_state_path(paths: &DotsyncPaths, config: &DotsyncConfig) -> PathBuf {
    paths.home_dir.join(&config.sync_state_relative_path)
}

fn load_scope_commit(
    repo: &dyn jj_lib::repo::Repo,
    scope: &str,
) -> Result<jj_lib::commit::Commit, DotsyncError> {
    let commit_id = repo
        .view()
        .get_local_bookmark(RefNameBuf::from(scope).as_ref())
        .as_normal()
        .cloned()
        .ok_or_else(|| DotsyncError::MissingScopeBookmark {
            scope: scope.to_string(),
        })?;
    repo.store()
        .get_commit(&commit_id)
        .map_err(|err| jj_error(format!("load scope commit for {scope}: {err}")))
}

fn collect_managed_tree_entries(
    tree: &jj_lib::merged_tree::MergedTree,
    excluded_paths: &BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, TreeValue>, DotsyncError> {
    let mut entries = BTreeMap::new();
    for (path, value) in tree.entries() {
        let display_path = PathBuf::from(path.as_internal_file_string());
        if excluded_paths.contains(&display_path) {
            continue;
        }
        let value = value.map_err(|err| {
            jj_error(format!("read tree entry {}: {err}", display_path.display()))
        })?;
        let Some(value) = value.as_resolved() else {
            return Err(jj_error(format!(
                "tree entry {} is conflicted during sync",
                display_path.display()
            )));
        };
        let Some(value) = value.clone() else {
            continue;
        };
        match value {
            TreeValue::Tree(_) => {}
            other => {
                entries.insert(display_path, other);
            }
        }
    }
    Ok(entries)
}

async fn read_tree_entry_bytes(
    store: &std::sync::Arc<jj_lib::store::Store>,
    relative: &Path,
    value: &TreeValue,
) -> Result<Vec<u8>, DotsyncError> {
    let relative_str = relative.to_str().ok_or(DotsyncError::NotImplemented(
        "non-utf8 repo paths are not supported yet",
    ))?;
    let repo_path = jj_lib::repo_path::RepoPath::from_internal_string(relative_str)
        .map_err(|err| jj_error(format!("invalid repo path {}: {err}", relative.display())))?;
    match value {
        TreeValue::File { id, .. } => {
            let mut reader = store
                .read_file(repo_path, id)
                .await
                .map_err(|err| jj_error(format!("read repo file {}: {err}", relative.display())))?;
            let mut contents = Vec::new();
            use tokio::io::AsyncReadExt;
            reader.read_to_end(&mut contents).await.map_err(|err| {
                jj_error(format!(
                    "read repo file bytes {}: {err}",
                    relative.display()
                ))
            })?;
            Ok(contents)
        }
        TreeValue::Symlink(id) => {
            let target = store.read_symlink(repo_path, id).await.map_err(|err| {
                jj_error(format!("read repo symlink {}: {err}", relative.display()))
            })?;
            Ok(target.into_bytes())
        }
        TreeValue::GitSubmodule(_) => Err(DotsyncError::NotImplemented(
            "git submodule sync is not supported yet",
        )),
        TreeValue::Tree(_) => unreachable!("tree entries are filtered out before copying"),
    }
}

fn remove_home_path(paths: &DotsyncPaths, relative: &Path) -> Result<(), DotsyncError> {
    let path = paths.home_dir.join(relative);
    match fs::remove_file(&path) {
        Ok(()) => remove_empty_parent_dirs(paths, &path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DotsyncError::Io { path, source }),
    }
}

fn remove_empty_parent_dirs(paths: &DotsyncPaths, path: &Path) -> Result<(), DotsyncError> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == paths.home_dir {
            break;
        }
        match fs::remove_dir(dir) {
            Ok(()) => current = dir.parent(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => break,
            Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => break,
            Err(source) => {
                return Err(DotsyncError::Io {
                    path: dir.to_path_buf(),
                    source,
                })
            }
        }
    }
    Ok(())
}

fn cascade_state_store(paths: &DotsyncPaths) -> JsonCascadeStateStore {
    JsonCascadeStateStore::new(paths.repo_root.join(".jj/dotsync/cascade-state.json"))
}

fn repo_config_path(paths: &DotsyncPaths) -> PathBuf {
    paths.repo_root.join(DOTSYNC_CONFIG_RELATIVE_PATH)
}

fn default_sync_state_relative_path() -> String {
    DEFAULT_SYNC_STATE_RELATIVE_PATH.to_string()
}

fn detect_machine() -> Result<MachineIdentity, DotsyncError> {
    let os_scope = std::env::var("DOTSYNC_OS").unwrap_or_else(|_| detect_os().to_string());
    let machine_scope = match std::env::var("DOTSYNC_HOSTNAME") {
        Ok(hostname) => hostname,
        Err(_) => detect_hostname()?,
    };
    Ok(MachineIdentity {
        os_scope,
        machine_scope,
    })
}

fn detect_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

fn detect_hostname() -> Result<String, DotsyncError> {
    if let Some(hostname) = std::env::var_os("HOSTNAME")
        .and_then(non_empty_os_string)
        .and_then(|hostname| hostname.into_string().ok())
    {
        return Ok(hostname);
    }
    if let Some(hostname) = std::env::var_os("COMPUTERNAME")
        .and_then(non_empty_os_string)
        .and_then(|hostname| hostname.into_string().ok())
    {
        return Ok(hostname);
    }
    let etc_hostname = Path::new("/etc/hostname");
    if etc_hostname.exists() {
        let hostname = fs::read_to_string(etc_hostname).map_err(|source| DotsyncError::Io {
            path: etc_hostname.to_path_buf(),
            source,
        })?;
        let hostname = hostname.trim();
        if !hostname.is_empty() {
            return Ok(hostname.to_string());
        }
    }
    Err(DotsyncError::MissingHostname)
}

fn non_empty_os_string(value: OsString) -> Option<OsString> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn default_import_options() -> GitImportOptions {
    GitImportOptions {
        auto_local_bookmark: false,
        abandon_unreachable_commits: true,
        remote_auto_track_bookmarks: HashMap::new(),
    }
}

#[derive(Debug, Default)]
struct QuietGitCallback;

impl GitSubprocessCallback for QuietGitCallback {
    fn needs_progress(&self) -> bool {
        false
    }

    fn progress(&mut self, _progress: &GitProgress) -> io::Result<()> {
        Ok(())
    }

    fn local_sideband(
        &mut self,
        _message: &[u8],
        _term: Option<GitSidebandLineTerminator>,
    ) -> io::Result<()> {
        Ok(())
    }

    fn remote_sideband(
        &mut self,
        _message: &[u8],
        _term: Option<GitSidebandLineTerminator>,
    ) -> io::Result<()> {
        Ok(())
    }
}

fn jj_error(message: String) -> DotsyncError {
    DotsyncError::Jj { message }
}
