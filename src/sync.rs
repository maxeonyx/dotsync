use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::Repo as _;
use serde::{Deserialize, Serialize};

use crate::config::{internal_repo_paths, load_config, DotsyncConfig, DotsyncPaths};
use crate::error::DotsyncError;
use crate::repo::{
    collect_managed_tree_entries, detect_current_scope, load_scope_commit, load_workspace,
    read_tree_entry_bytes,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct SyncOptions {
    pub force: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SyncStatePayload {
    machine_scope: String,
    last_synced_revision: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SyncState {
    pub(crate) machine_scope: String,
    pub(crate) last_synced_revision: CommitId,
}

pub async fn sync(paths: &DotsyncPaths, options: SyncOptions) -> Result<SyncReport, DotsyncError> {
    crate::commit::ensure_no_paused_cascade(paths)?;
    let workspace = load_workspace(paths)?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|err| crate::error::jj_error(format!("load repo at head: {err}")))?;

    let snapshot = crate::repo::snapshot_working_copy(paths).await?;
    if !snapshot.changed_paths.is_empty() {
        return Err(DotsyncError::DirtyWorkingCopy {
            count: snapshot.changed_paths.len(),
        });
    }

    let _repo = crate::repo::fetch_origin(repo).await?;

    sync_repo_to_home(paths, options, &[], None).await
}

pub(crate) async fn detect_drifts(
    paths: &DotsyncPaths,
    repo: &dyn jj_lib::repo::Repo,
    managed_entries: &BTreeMap<PathBuf, jj_lib::backend::TreeValue>,
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

pub(crate) async fn copy_repo_file_to_home(
    paths: &DotsyncPaths,
    repo: &dyn jj_lib::repo::Repo,
    relative: &Path,
    value: &jj_lib::backend::TreeValue,
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

pub(crate) async fn sync_repo_to_home(
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
        .map_err(|err| crate::error::jj_error(format!("load repo at head: {err}")))?;

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

pub(crate) fn load_sync_state(
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

pub(crate) fn save_sync_state(
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

pub(crate) fn sync_state_path(paths: &DotsyncPaths, config: &DotsyncConfig) -> PathBuf {
    paths.home_dir.join(&config.sync_state_relative_path)
}

pub(crate) fn remove_home_path(paths: &DotsyncPaths, relative: &Path) -> Result<(), DotsyncError> {
    let path = paths.home_dir.join(relative);
    match fs::remove_file(&path) {
        Ok(()) => remove_empty_parent_dirs(paths, &path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DotsyncError::Io { path, source }),
    }
}

pub(crate) fn remove_empty_parent_dirs(
    paths: &DotsyncPaths,
    path: &Path,
) -> Result<(), DotsyncError> {
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
