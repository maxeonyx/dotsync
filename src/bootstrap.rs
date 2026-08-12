use std::collections::HashMap;
use std::path::PathBuf;

use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::workspace::Workspace;

use crate::cascade::{
    build_cascade_plan, execute_cascade_steps, CascadeCommand, CascadeOutcome, ScopeHeads,
};
use crate::config::{
    config_with_added_scopes, default_sync_state_relative_path, load_config, load_config_text,
    new_config, repo_config_path, write_config, DotsyncPaths, NewScope, ScopeKind, ALL_SCOPE,
};
use crate::drift::RecordedFromHome;
use crate::error::{jj_error, DotsyncError};
use crate::machine::{detect_machine, MachineIdentity};
use crate::repo::{
    add_origin_remote, default_settings, fetch_origin, load_repo_direct, push_scope_updates,
    PushReport,
};
use crate::scope_graph::ScopeGraph;
use crate::session::{Run, Session};
use crate::sync::{sync_repo_to_home, ForceScope, SyncReport};

#[derive(Debug, Clone)]
pub struct InitReport {
    /// Includes the machine scope this init settled on, as `sync.current_scope`.
    pub sync: SyncReport,
    pub push: PushReport,
}

/// Unlike every other command, `init` cannot carry on against a last-fetched
/// state, because there isn't one yet — so its run never reports an unreachable
/// remote as an aside. It reports it as the error it is.
pub async fn init(paths: &DotsyncPaths, remote_url: &str) -> Run<Result<InitReport, DotsyncError>> {
    Run {
        report: init_repo(paths, remote_url).await,
        unreachable_remote: None,
    }
}

async fn init_repo(paths: &DotsyncPaths, remote_url: &str) -> Result<InitReport, DotsyncError> {
    if paths.repo_root.exists() {
        return Err(DotsyncError::RepoAlreadyExists {
            path: paths.repo_root.clone(),
        });
    }

    match create_repo_and_join(paths, remote_url).await {
        Ok(report) => Ok(report),
        // Everything under the repo root was made by this run — init refuses
        // to start when it already exists — so an init that stopped part-way
        // takes its own leavings with it. Otherwise the remedy for the
        // commonest failure there is, a remote this machine cannot reach yet,
        // would be deleting a directory by hand before the retry is even
        // allowed to start.
        Err(error) => Err(match std::fs::remove_dir_all(&paths.repo_root) {
            Ok(()) => error,
            // Nothing was created yet, so there is nothing to say.
            Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => error,
            // A cleanup that failed silently would be the worst of both: the
            // retry refuses to start and nothing ever said why.
            Err(source) => DotsyncError::PartialInitLeftBehind {
                path: paths.repo_root.clone(),
                source,
                original: Box::new(error),
            },
        }),
    }
}

async fn create_repo_and_join(
    paths: &DotsyncPaths,
    remote_url: &str,
) -> Result<InitReport, DotsyncError> {
    if let Some(parent) = paths.repo_root.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DotsyncError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::create_dir_all(&paths.repo_root).map_err(|source| DotsyncError::Io {
        path: paths.repo_root.clone(),
        source,
    })?;

    let settings = default_settings()?;
    let (_workspace, repo) = Workspace::init_internal_git(&settings, &paths.repo_root)
        .await
        .map_err(|err| jj_error(format!("init repo: {err}")))?;
    let _repo = add_origin_remote(repo, remote_url).await?;
    // The remote lives in the git config rather than in the repo view, and a
    // repo handle carries the git config it was opened with — so unlike every
    // other transaction in dotsync, this one is only visible after re-opening.
    let repo = load_repo_direct(paths).await?;
    let repo = fetch_origin(repo).await?;
    let identity = detect_machine()?;

    // Only after this does the repo hold an `all` scope to read a scope graph
    // out of, which is what a session is: everything before it works on the
    // repo handle directly.
    let remote_empty = repo.view().all_remote_bookmarks().next().is_none();
    let (current_scope, repo) = if remote_empty {
        bootstrap_empty_remote(repo, &identity).await?
    } else {
        join_existing_remote(paths, repo, &identity).await?
    };

    let mut session = Session::from_repo(paths, repo).await?;
    let push = push_scope_updates(&mut session).await?;
    let sync = sync_repo_to_home(
        &session,
        ForceScope::Everything,
        &RecordedFromHome::default(),
        Some(&current_scope),
    )
    .await?;

    Ok(InitReport { sync, push })
}

/// The scopes this machine needs that the graph does not have yet, parents
/// first, each with what dotsync can say about what it is for.
///
/// One function for both paths into `init`: an empty remote is the case where
/// every one of them is missing, including `all`.
fn scopes_to_create(
    identity: &MachineIdentity,
    existing: &HashMap<String, Vec<String>>,
) -> Vec<NewScope> {
    let mut new_scopes = Vec::new();
    if !existing.contains_key(ALL_SCOPE) {
        new_scopes.push(NewScope {
            name: ALL_SCOPE.to_string(),
            parents: Vec::new(),
            kind: ScopeKind::Root,
        });
    }
    if !existing.contains_key(&identity.os_scope) {
        new_scopes.push(NewScope {
            name: identity.os_scope.clone(),
            parents: vec![ALL_SCOPE.to_string()],
            kind: ScopeKind::Os,
        });
    }
    if !existing.contains_key(&identity.machine_scope) {
        new_scopes.push(NewScope {
            name: identity.machine_scope.clone(),
            parents: vec![identity.os_scope.clone()],
            kind: ScopeKind::Machine,
        });
    }
    new_scopes
}

pub(crate) async fn bootstrap_empty_remote(
    repo: std::sync::Arc<jj_lib::repo::ReadonlyRepo>,
    identity: &MachineIdentity,
) -> Result<(String, std::sync::Arc<jj_lib::repo::ReadonlyRepo>), DotsyncError> {
    let root_commit = repo.store().root_commit();
    let config_text = new_config(
        &PathBuf::from(default_sync_state_relative_path()),
        &scopes_to_create(identity, &HashMap::new()),
    );

    let mut tx = repo.start_transaction();
    let config_tree = write_config(tx.repo_mut(), &root_commit.tree(), &config_text).await?;
    let all_commit = tx
        .repo_mut()
        .new_commit(vec![root_commit.id().clone()], config_tree)
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
    let repo = tx
        .commit("dotsync: initialize scopes")
        .await
        .map_err(|err| jj_error(format!("commit init scopes: {err}")))?;

    Ok((identity.machine_scope.clone(), repo))
}

pub(crate) async fn join_existing_remote(
    paths: &DotsyncPaths,
    repo: std::sync::Arc<jj_lib::repo::ReadonlyRepo>,
    identity: &MachineIdentity,
) -> Result<(String, std::sync::Arc<jj_lib::repo::ReadonlyRepo>), DotsyncError> {
    let config = load_config(paths, repo.as_ref()).await?;
    let new_scopes = scopes_to_create(identity, &config.graph.parents);

    if new_scopes.is_empty() {
        return Ok((identity.machine_scope.clone(), repo));
    }

    // The file this machine adds its scopes to is the file as it is written,
    // not a re-rendering of the graph parsed out of it: the comments beside
    // the scopes are what an agent reads to choose one, and they do not
    // survive a round trip through the graph.
    let updated_text = config_with_added_scopes(
        &load_config_text(paths, repo.as_ref()).await?,
        &new_scopes,
        &repo_config_path(paths),
    )?;

    let mut parents = config.graph.parents.clone();
    for scope in &new_scopes {
        parents.insert(scope.name.clone(), scope.parents.clone());
    }
    let updated_graph = ScopeGraph::new(parents)?;

    let mut tx = repo.start_transaction();
    let mut scope_heads = ScopeHeads::load_existing(tx.repo_mut().base_repo(), &updated_graph)?;
    let all_head = scope_heads.require("all")?;
    let config_tree = write_config(tx.repo_mut(), &all_head.tree(), &updated_text).await?;

    let config_commit = tx
        .repo_mut()
        .new_commit(vec![all_head.id().clone()], config_tree)
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
    };
    let cascade_plan = build_cascade_plan(&updated_graph, &scope_heads, &cascade_command);
    match execute_cascade_steps(
        tx.repo_mut(),
        &mut scope_heads,
        &cascade_plan,
        &cascade_command,
    )
    .await?
    {
        CascadeOutcome::Completed => {}
        CascadeOutcome::Paused {
            scope,
            conflicted_files,
        } => {
            return Err(DotsyncError::Jj {
                message: format!(
                    "unexpected conflict while cascading init config at `{scope}`: {}",
                    conflicted_files.join(", ")
                ),
            })
        }
    }

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

    let repo = tx
        .commit("dotsync: initialize machine scope")
        .await
        .map_err(|err| jj_error(format!("commit join scope changes: {err}")))?;

    Ok((identity.machine_scope.clone(), repo))
}
