use std::collections::HashMap;
use std::path::{Path, PathBuf};

use jj_lib::repo::Repo as _;

use crate::config::{internal_repo_paths, load_config, DotsyncPaths, DOTSYNC_CONFIG_RELATIVE_PATH};
use crate::error::{jj_error, DotsyncError};
use crate::repo::{
    collect_managed_tree_entries, fetch_origin, load_repo_direct, load_scope_commit,
    read_tree_entry_bytes,
};
use crate::scope_graph::scope_depth;
use crate::sync::{detect_drifts, load_sync_state, resolve_current_scope, FileDrift};

#[derive(Debug, Clone)]
pub struct ScopeInfo {
    pub name: String,
    pub parents: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ScopeListReport {
    pub scopes: Vec<ScopeInfo>,
}

#[derive(Debug, Clone)]
pub struct TreeReport {
    pub scope: String,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct FileReport {
    pub scope: String,
    pub path: PathBuf,
    pub contents: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct DiffReport {
    pub machine_scope: String,
    pub drifts: Vec<FileDrift>,
}

pub async fn list_scopes(paths: &DotsyncPaths) -> Result<ScopeListReport, DotsyncError> {
    let repo = load_repo_direct(paths).await?;
    let _repo = fetch_origin(repo).await?;
    let config = load_config(paths).await?;
    let mut memo = HashMap::new();
    let mut scopes = config
        .graph
        .parents
        .iter()
        .map(|(name, parents)| {
            Ok((
                scope_depth(&config.graph, name, &mut memo)?,
                ScopeInfo {
                    name: name.clone(),
                    parents: parents.clone(),
                },
            ))
        })
        .collect::<Result<Vec<_>, DotsyncError>>()?;
    scopes.sort_by(|(left_depth, left), (right_depth, right)| {
        left_depth
            .cmp(right_depth)
            .then_with(|| left.name.cmp(&right.name))
    });

    Ok(ScopeListReport {
        scopes: scopes.into_iter().map(|(_, scope)| scope).collect(),
    })
}

pub async fn read_config_at_scope(
    paths: &DotsyncPaths,
    scope: &str,
) -> Result<FileReport, DotsyncError> {
    read_scope_file(paths, scope, Path::new(DOTSYNC_CONFIG_RELATIVE_PATH)).await
}

pub async fn list_scope_tree(
    paths: &DotsyncPaths,
    scope: &str,
) -> Result<TreeReport, DotsyncError> {
    let repo = load_repo_direct(paths).await?;
    let repo = fetch_origin(repo).await?;
    let config = load_config(paths).await?;
    let commit = load_scope_commit(repo.as_ref(), scope)?;
    let entries = collect_managed_tree_entries(&commit.tree(), &internal_repo_paths(&config))?;

    Ok(TreeReport {
        scope: scope.to_string(),
        paths: entries.into_keys().collect(),
    })
}

pub async fn read_scope_file(
    paths: &DotsyncPaths,
    scope: &str,
    relative: &Path,
) -> Result<FileReport, DotsyncError> {
    let repo = load_repo_direct(paths).await?;
    let repo = fetch_origin(repo).await?;
    let commit = load_scope_commit(repo.as_ref(), scope)?;
    let relative_str = relative.to_str().ok_or(DotsyncError::NotImplemented(
        "non-utf8 repo paths are not supported yet",
    ))?;
    let repo_path = jj_lib::repo_path::RepoPath::from_internal_string(relative_str)
        .map_err(|err| jj_error(format!("invalid repo path {}: {err}", relative.display())))?;
    let value = commit
        .tree()
        .path_value(repo_path)
        .map_err(|err| jj_error(format!("read {} from {scope}: {err}", relative.display())))?;
    let value = value
        .into_resolved()
        .map_err(|conflict| {
            jj_error(format!(
                "{} is conflicted on {scope}: {conflict:?}",
                relative.display()
            ))
        })?
        .ok_or_else(|| {
            jj_error(format!(
                "{} does not exist on scope {scope}",
                relative.display()
            ))
        })?;
    let contents = read_tree_entry_bytes(repo.store(), relative, &value).await?;

    Ok(FileReport {
        scope: scope.to_string(),
        path: relative.to_path_buf(),
        contents,
    })
}

pub async fn diff_home(paths: &DotsyncPaths) -> Result<DiffReport, DotsyncError> {
    let config = load_config(paths).await?;
    let repo = load_repo_direct(paths).await?;
    let repo = fetch_origin(repo).await?;
    let sync_state = load_sync_state(paths, &config)?;
    let machine_scope = resolve_current_scope(&config, sync_state.as_ref(), None)?;
    let machine_commit = load_scope_commit(repo.as_ref(), &machine_scope)?;
    let managed_entries =
        collect_managed_tree_entries(&machine_commit.tree(), &internal_repo_paths(&config))?;
    let drifts = detect_drifts(paths, repo.as_ref(), &managed_entries).await?;

    Ok(DiffReport {
        machine_scope,
        drifts,
    })
}
