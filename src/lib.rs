use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gix::remote::fetch::Tags;
use jj_lib::commit::Commit;
use jj_lib::config::StackedConfig;
use jj_lib::git::{
    self, GitBranchPushTargets, GitFetch, GitFetchRefExpression, GitImportOptions, GitProgress,
    GitPushOptions, GitSidebandLineTerminator, GitSubprocessCallback, GitSubprocessOptions,
};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::{RefNameBuf, WorkspaceNameBuf};
use jj_lib::refs::BookmarkPushUpdate;
use jj_lib::repo::{MutableRepo, ReadonlyRepo, Repo as _, StoreFactories};
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::settings::UserSettings;
use jj_lib::str_util::StringExpression;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::workspace::{default_working_copy_factories, Workspace};
use serde::Deserialize;
use thiserror::Error;
use walkdir::{DirEntry, WalkDir};

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
    #[error("scope `{scope}` is not an ancestor of `{current_scope}`")]
    ScopeNotAncestor {
        scope: String,
        current_scope: String,
    },
    #[error("scope `{scope}` does not have a local bookmark")]
    MissingScopeBookmark { scope: String },
    #[error("detected drift in {count} file(s)")]
    DriftDetected {
        count: usize,
        drifts: Vec<FileDrift>,
    },
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
}

#[derive(Debug, Default, Deserialize)]
struct RawScope {
    #[serde(default)]
    parents: Vec<String>,
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

    let sync = sync_repo_to_home(paths, SyncOptions { force: true }).await?;
    push_scope_updates(paths).await?;

    Ok(InitReport {
        current_scope,
        created_scopes,
        sync,
    })
}

pub async fn sync(paths: &DotsyncPaths, options: SyncOptions) -> Result<SyncReport, DotsyncError> {
    let graph = load_scope_graph(paths)?;
    let workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("load repo at head: {err}")))?;
    let current_scope = detect_current_scope(&graph, &workspace, repo.as_ref())?;

    let _repo = fetch_origin(repo).await?;
    checkout_workspace_to_scope(paths, &mut load_workspace(paths)?, &current_scope).await?;

    sync_repo_to_home(paths, options).await
}

pub async fn commit_and_sync(
    paths: &DotsyncPaths,
    options: CommitOptions,
) -> Result<CommitReport, DotsyncError> {
    let workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("load repo at head: {err}")))?;
    let initial_graph = load_scope_graph(paths)?;
    let current_scope = detect_current_scope(&initial_graph, &workspace, repo.as_ref())?;

    let _repo = fetch_origin(repo).await?;
    let mut workspace = load_workspace(paths)?;
    checkout_workspace_to_scope(paths, &mut workspace, &current_scope).await?;
    sync_primary_config_to_home(paths)?;

    let graph = load_scope_graph(paths)?;
    if !graph.parents.contains_key(&options.scope) {
        return Err(DotsyncError::InvalidScope {
            scope: options.scope,
        });
    }
    if !is_ancestor_scope(&graph, &options.scope, &current_scope)? {
        return Err(DotsyncError::ScopeNotAncestor {
            scope: options.scope,
            current_scope,
        });
    }

    let snapshot_commit_id = snapshot_working_copy(paths).await?;
    let mut workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("reload repo after snapshot: {err}")))?;
    let snapshot_commit = repo
        .store()
        .get_commit(&snapshot_commit_id)
        .map_err(|err| jj_error(format!("load snapshot commit: {err}")))?;

    let (_repo, cascaded_scopes) = commit_to_scope_and_cascade(
        &graph,
        repo,
        &workspace.workspace_name().to_owned(),
        &current_scope,
        &options.scope,
        &snapshot_commit.tree(),
        &options.message,
    )
    .await?;

    checkout_workspace_to_scope(paths, &mut workspace, &current_scope).await?;
    let sync = sync_repo_to_home(
        paths,
        SyncOptions {
            force: options.force,
        },
    )
    .await?;
    push_scope_updates(paths).await?;

    Ok(CommitReport {
        committed_scope: options.scope,
        cascaded_scopes,
        sync,
    })
}

fn load_scope_graph(paths: &DotsyncPaths) -> Result<ScopeGraph, DotsyncError> {
    let config_path = config_path(paths);
    let contents = fs::read_to_string(&config_path).map_err(|source| DotsyncError::Io {
        path: config_path.clone(),
        source,
    })?;
    let raw: RawConfig = toml::from_str(&contents).map_err(|source| DotsyncError::ConfigParse {
        path: config_path,
        source,
    })?;
    ScopeGraph::new(
        raw.scopes
            .into_iter()
            .map(|(name, scope)| (name, scope.parents))
            .collect(),
    )
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

fn repo_files(paths: &DotsyncPaths) -> Result<Vec<PathBuf>, DotsyncError> {
    let mut files = Vec::new();
    for entry in WalkDir::new(&paths.repo_root)
        .into_iter()
        .filter_entry(|entry| should_walk(paths, entry))
    {
        let entry = entry.map_err(|err| jj_error(format!("walk repo: {err}")))?;
        if entry.file_type().is_file() {
            let relative = entry
                .path()
                .strip_prefix(&paths.repo_root)
                .expect("walked path is within repo root")
                .to_path_buf();
            files.push(relative);
        }
    }
    files.sort();
    Ok(files)
}

fn should_walk(paths: &DotsyncPaths, entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let Ok(relative) = entry.path().strip_prefix(&paths.repo_root) else {
        return false;
    };
    relative != Path::new(".git") && relative != Path::new(".jj")
}

fn detect_drifts(
    paths: &DotsyncPaths,
    repo_files: &[PathBuf],
) -> Result<Vec<FileDrift>, DotsyncError> {
    let mut drifts = Vec::new();
    for relative in repo_files {
        let repo_path = paths.repo_root.join(relative);
        let system_path = paths.home_dir.join(relative);
        let repo_bytes = fs::read(&repo_path).map_err(|source| DotsyncError::Io {
            path: repo_path.clone(),
            source,
        })?;
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

fn copy_repo_file_to_home(paths: &DotsyncPaths, relative: &Path) -> Result<(), DotsyncError> {
    let repo_path = paths.repo_root.join(relative);
    let system_path = paths.home_dir.join(relative);
    if let Some(parent) = system_path.parent() {
        fs::create_dir_all(parent).map_err(|source| DotsyncError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let contents = fs::read(&repo_path).map_err(|source| DotsyncError::Io {
        path: repo_path,
        source,
    })?;
    fs::write(&system_path, contents).map_err(|source| DotsyncError::Io {
        path: system_path,
        source,
    })
}

async fn sync_repo_to_home(
    paths: &DotsyncPaths,
    options: SyncOptions,
) -> Result<SyncReport, DotsyncError> {
    let graph = load_scope_graph(paths)?;
    let workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("load repo at head: {err}")))?;

    let current_scope = detect_current_scope(&graph, &workspace, repo.as_ref())?;
    let repo_files = repo_files(paths)?;
    let drifts = detect_drifts(paths, &repo_files)?;
    if !drifts.is_empty() && !options.force {
        return Err(DotsyncError::DriftDetected {
            count: drifts.len(),
            drifts,
        });
    }

    let mut synced_paths = Vec::with_capacity(repo_files.len());
    for relative in &repo_files {
        copy_repo_file_to_home(paths, relative)?;
        synced_paths.push(relative.clone());
    }

    Ok(SyncReport {
        current_scope,
        synced_paths,
        drifts,
    })
}

async fn snapshot_working_copy(
    paths: &DotsyncPaths,
) -> Result<jj_lib::backend::CommitId, DotsyncError> {
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
    let (tree, _) = locked_ws
        .locked_wc()
        .snapshot(&snapshot_options)
        .await
        .map_err(|err| jj_error(format!("snapshot working copy: {err}")))?;
    locked_ws
        .finish(repo.op_id().clone())
        .map_err(|err| jj_error(format!("finish working copy mutation: {err}")))?;

    let old_wc_commit_id = repo
        .view()
        .get_wc_commit_id(workspace.workspace_name())
        .ok_or(DotsyncError::NoCurrentScope)?
        .clone();
    let old_wc_commit = repo
        .store()
        .get_commit(&old_wc_commit_id)
        .map_err(|err| jj_error(format!("load working copy commit: {err}")))?;

    let mut tx = repo.start_transaction();
    let new_wc_commit = tx
        .repo_mut()
        .new_commit(vec![old_wc_commit.id().clone()], tree)
        .set_description("dotsync: snapshot working copy")
        .write()
        .await
        .map_err(|err| jj_error(format!("write working copy commit: {err}")))?;
    tx.repo_mut()
        .set_wc_commit(
            workspace.workspace_name().to_owned(),
            new_wc_commit.id().clone(),
        )
        .map_err(|err| jj_error(format!("set working copy commit: {err}")))?;
    tx.commit("snapshot working copy")
        .await
        .map_err(|err| jj_error(format!("commit working copy snapshot: {err}")))?;

    Ok(new_wc_commit.id().clone())
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

    let snapshot_commit_id = snapshot_working_copy(paths).await?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("reload repo after init snapshot: {err}")))?;
    let snapshot_commit = repo
        .store()
        .get_commit(&snapshot_commit_id)
        .map_err(|err| jj_error(format!("load init snapshot commit: {err}")))?;
    let root_commit = repo.store().root_commit();

    let mut tx = repo.start_transaction();
    let all_commit = tx
        .repo_mut()
        .new_commit(vec![root_commit.id().clone()], snapshot_commit.tree())
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
    let graph = load_scope_graph(paths)?;

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

    let snapshot_commit_id = snapshot_working_copy(paths).await?;
    let repo = load_repo(workspace).await?;
    let snapshot_commit = repo
        .store()
        .get_commit(&snapshot_commit_id)
        .map_err(|err| jj_error(format!("load join snapshot commit: {err}")))?;

    let mut tx = repo.start_transaction();
    let mut scope_heads = load_scope_heads(tx.repo_mut().base_repo(), &updated_graph).await?;
    let all_head =
        scope_heads
            .get("all")
            .cloned()
            .ok_or_else(|| DotsyncError::MissingScopeBookmark {
                scope: "all".to_string(),
            })?;

    let config_commit = tx
        .repo_mut()
        .new_commit(vec![all_head.id().clone()], snapshot_commit.tree())
        .set_description("dotsync: update scope config")
        .write()
        .await
        .map_err(|err| jj_error(format!("write config update commit: {err}")))?;
    tx.repo_mut().set_local_bookmark_target(
        "all".as_ref(),
        RefTarget::normal(config_commit.id().clone()),
    );
    scope_heads.insert("all".to_string(), config_commit.clone());

    let descendant_scopes = cascade_descendants(
        tx.repo_mut(),
        &updated_graph,
        &mut scope_heads,
        "all",
        "dotsync: cascade init config",
    )
    .await?;

    if !scope_heads.contains_key(&identity.os_scope) {
        let parent = scope_heads.get("all").cloned().expect("all scope exists");
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
        scope_heads.insert(identity.os_scope.clone(), commit);
    }

    if !scope_heads.contains_key(&identity.machine_scope) {
        let parent = scope_heads
            .get(&identity.os_scope)
            .cloned()
            .expect("os scope exists");
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
        scope_heads.insert(identity.machine_scope.clone(), commit);
    }

    let machine_commit = scope_heads
        .get(&identity.machine_scope)
        .cloned()
        .expect("machine scope exists");
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

async fn commit_to_scope_and_cascade(
    graph: &ScopeGraph,
    repo: std::sync::Arc<ReadonlyRepo>,
    workspace_name: &WorkspaceNameBuf,
    current_scope: &str,
    target_scope: &str,
    tree: &jj_lib::merged_tree::MergedTree,
    message: &str,
) -> Result<(std::sync::Arc<ReadonlyRepo>, Vec<String>), DotsyncError> {
    let mut tx = repo.start_transaction();
    let mut scope_heads = load_scope_heads(tx.repo_mut().base_repo(), graph).await?;
    let target_head = scope_heads.get(target_scope).cloned().ok_or_else(|| {
        DotsyncError::MissingScopeBookmark {
            scope: target_scope.to_string(),
        }
    })?;

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
    scope_heads.insert(target_scope.to_string(), target_commit);

    let cascaded_scopes = cascade_descendants(
        tx.repo_mut(),
        graph,
        &mut scope_heads,
        target_scope,
        &format!("dotsync: cascade {message}"),
    )
    .await?;

    let current_commit = scope_heads.get(current_scope).cloned().ok_or_else(|| {
        DotsyncError::MissingScopeBookmark {
            scope: current_scope.to_string(),
        }
    })?;
    tx.repo_mut()
        .set_wc_commit(workspace_name.clone(), current_commit.id().clone())
        .map_err(|err| jj_error(format!("update working copy bookmark: {err}")))?;

    let repo = tx
        .commit(format!("dotsync: {message}"))
        .await
        .map_err(|err| jj_error(format!("commit scope update: {err}")))?;
    Ok((repo, cascaded_scopes))
}

async fn cascade_descendants(
    mut_repo: &mut MutableRepo,
    graph: &ScopeGraph,
    scope_heads: &mut HashMap<String, Commit>,
    root_scope: &str,
    description: &str,
) -> Result<Vec<String>, DotsyncError> {
    let ordered = descendants_in_topological_order(graph, root_scope);
    let mut cascaded = Vec::new();
    for scope in ordered {
        let Some(existing_head) = scope_heads.get(&scope).cloned() else {
            continue;
        };

        let mut parents = vec![existing_head.clone()];
        for parent in graph.parents.get(&scope).into_iter().flatten() {
            let Some(parent_head) = scope_heads.get(parent).cloned() else {
                continue;
            };
            parents.push(parent_head);
        }
        if parents.len() <= 1 {
            continue;
        }

        let merged_tree = merge_commit_trees(mut_repo, &parents)
            .await
            .map_err(|err| jj_error(format!("merge trees for {scope}: {err}")))?;
        let new_commit = mut_repo
            .new_commit(
                parents.iter().map(|commit| commit.id().clone()).collect(),
                merged_tree,
            )
            .set_description(description)
            .write()
            .await
            .map_err(|err| jj_error(format!("write cascade commit for {scope}: {err}")))?;
        mut_repo.set_local_bookmark_target(
            RefNameBuf::from(scope.as_str()).as_ref(),
            RefTarget::normal(new_commit.id().clone()),
        );
        scope_heads.insert(scope.clone(), new_commit);
        cascaded.push(scope);
    }
    Ok(cascaded)
}

fn descendants_in_topological_order(graph: &ScopeGraph, scope: &str) -> Vec<String> {
    let descendants: HashSet<String> = descendants_of(graph, scope).into_iter().collect();
    let mut remaining = descendants.clone();
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let mut ready: Vec<String> = remaining
            .iter()
            .filter(|candidate| {
                graph.parents[*candidate]
                    .iter()
                    .all(|parent| !descendants.contains(parent) || ordered.contains(parent))
            })
            .cloned()
            .collect();
        ready.sort();
        for candidate in ready {
            remaining.remove(&candidate);
            ordered.push(candidate);
        }
    }
    ordered
}

fn descendants_of(graph: &ScopeGraph, scope: &str) -> Vec<String> {
    let mut descendants = Vec::new();
    let mut stack = graph.children.get(scope).cloned().unwrap_or_default();
    let mut seen = HashSet::new();
    while let Some(child) = stack.pop() {
        if seen.insert(child.clone()) {
            descendants.push(child.clone());
            if let Some(grandchildren) = graph.children.get(&child) {
                stack.extend(grandchildren.iter().cloned());
            }
        }
    }
    descendants
}

async fn load_scope_heads(
    repo: &ReadonlyRepo,
    graph: &ScopeGraph,
) -> Result<HashMap<String, Commit>, DotsyncError> {
    let mut heads = HashMap::new();
    for scope in graph.parents.keys() {
        let target = repo
            .view()
            .get_local_bookmark(RefNameBuf::from(scope.as_str()).as_ref());
        if let Some(commit_id) = target.as_normal() {
            let commit = repo
                .store()
                .get_commit(commit_id)
                .map_err(|err| jj_error(format!("load scope head for {scope}: {err}")))?;
            heads.insert(scope.clone(), commit);
        }
    }
    Ok(heads)
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
    workspace
        .check_out(repo.op_id().clone(), None, &commit)
        .await
        .map_err(|err| jj_error(format!("materialize checkout for {scope}: {err}")))?;

    if !config_path(paths).exists() && scope == "all" {
        return Err(DotsyncError::Io {
            path: config_path(paths),
            source: io::Error::new(io::ErrorKind::NotFound, "config missing after checkout"),
        });
    }
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
    rendered
}

fn write_config(paths: &DotsyncPaths, contents: &str) -> Result<(), DotsyncError> {
    let path = config_path(paths);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| DotsyncError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(&path, contents).map_err(|source| DotsyncError::Io { path, source })
}

fn sync_primary_config_to_home(paths: &DotsyncPaths) -> Result<(), DotsyncError> {
    copy_repo_file_to_home(paths, Path::new(".config/dotsync/config.toml"))
}

fn config_path(paths: &DotsyncPaths) -> PathBuf {
    paths.repo_root.join(".config/dotsync/config.toml")
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
