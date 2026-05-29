use std::collections::HashMap;

use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::workspace::Workspace;

use crate::cascade::{
    build_cascade_plan, execute_cascade_plan, CascadeCommand, CascadeOutcome, ScopeHeads,
};
use crate::config::{
    default_sync_state_relative_path, load_config, render_config, write_config, DotsyncConfig,
    DotsyncPaths,
};
use crate::error::{jj_error, DotsyncError};
use crate::machine::{detect_machine, MachineIdentity};
use crate::repo::{
    add_origin_remote, default_settings, fetch_origin, load_repo_direct, push_scope_updates,
};
use crate::scope_graph::ScopeGraph;
use crate::sync::{sync_repo_to_home, SyncOptions, SyncReport};

#[derive(Debug, Clone, Default)]
pub struct InitReport {
    pub current_scope: String,
    pub created_scopes: Vec<String>,
    pub sync: SyncReport,
}

pub async fn init(paths: &DotsyncPaths, remote_url: &str) -> Result<InitReport, DotsyncError> {
    if paths.repo_root.exists() {
        return Err(DotsyncError::RepoAlreadyExists {
            path: paths.repo_root.clone(),
        });
    }
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
    let repo = load_repo_direct(paths).await?;
    let repo = fetch_origin(repo).await?;
    let identity = detect_machine()?;

    let remote_empty = repo.view().all_remote_bookmarks().next().is_none();
    let (created_scopes, current_scope) = if remote_empty {
        bootstrap_empty_remote(paths, &identity).await?
    } else {
        join_existing_remote(paths, repo, &identity).await?
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

pub(crate) async fn bootstrap_empty_remote(
    paths: &DotsyncPaths,
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
    let repo = load_repo_direct(paths).await?;
    let root_commit = repo.store().root_commit();
    let config = DotsyncConfig {
        graph: graph.clone(),
        sync_state_relative_path: default_sync_state_relative_path().into(),
    };

    let mut tx = repo.start_transaction();
    let config_tree = write_config(tx.repo_mut(), &root_commit.tree(), &render_config(&config)).await?;
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
    tx
        .commit("dotsync: initialize scopes")
        .await
        .map_err(|err| jj_error(format!("commit init scopes: {err}")))?;

    Ok((
        vec![
            "all".to_string(),
            identity.os_scope.clone(),
            identity.machine_scope.clone(),
        ],
        identity.machine_scope.clone(),
    ))
}

pub(crate) async fn join_existing_remote(
    paths: &DotsyncPaths,
    _repo: std::sync::Arc<jj_lib::repo::ReadonlyRepo>,
    identity: &MachineIdentity,
) -> Result<(Vec<String>, String), DotsyncError> {
    let config = load_config(paths).await?;
    let graph = config.graph.clone();

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
        return Ok((created_scopes, identity.machine_scope.clone()));
    }

    let updated_graph = ScopeGraph::new(parents)?;
    let repo = load_repo_direct(paths).await?;

    let mut tx = repo.start_transaction();
    let mut scope_heads = ScopeHeads::load_existing(tx.repo_mut().base_repo(), &updated_graph)?;
    let all_head = scope_heads.require("all")?;
    let updated_config = DotsyncConfig {
        graph: updated_graph.clone(),
        sync_state_relative_path: config.sync_state_relative_path.clone(),
    };
    let config_tree =
        write_config(tx.repo_mut(), &all_head.tree(), &render_config(&updated_config)).await?;

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
    let descendant_scopes = match execute_cascade_plan(
        tx.repo_mut(),
        &mut scope_heads,
        &cascade_plan,
        &cascade_command,
    )
    .await?
    {
        CascadeOutcome::Completed(success) => success.progress.completed_scopes,
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

    let _repo = tx
        .commit("dotsync: initialize machine scope")
        .await
        .map_err(|err| jj_error(format!("commit join scope changes: {err}")))?;

    let mut created = descendant_scopes;
    created.extend(created_scopes);
    created.sort();
    created.dedup();
    Ok((created, identity.machine_scope.clone()))
}
