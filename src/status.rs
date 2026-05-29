use std::fs;
use std::io;
use std::path::PathBuf;

use jj_lib::repo::Repo as _;

use crate::config::{internal_repo_paths, load_config, DotsyncPaths};
use crate::error::DotsyncError;
use crate::repo::{
    collect_managed_tree_entries, fetch_origin, load_repo_direct, load_scope_commit,
    read_tree_entry_bytes,
};
use crate::sync::{load_sync_state, resolve_current_scope};

#[derive(Debug, Clone)]
pub struct StatusReport {
    pub machine_scope: String,
    pub changes: Vec<FileChange>,
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    pub status: ChangeStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    Modified,
    Deleted,
}

pub async fn status(paths: &DotsyncPaths) -> Result<StatusReport, DotsyncError> {
    let config = load_config(paths).await?;
    let repo = load_repo_direct(paths).await?;
    let repo = fetch_origin(repo).await?;
    let sync_state = load_sync_state(paths, &config)?;
    let machine_scope = resolve_current_scope(&config, sync_state.as_ref(), None)?;
    let machine_commit = load_scope_commit(repo.as_ref(), &machine_scope)?;
    let internal_paths = internal_repo_paths(&config);
    let managed_entries = collect_managed_tree_entries(&machine_commit.tree(), &internal_paths)?;

    let mut changes = Vec::new();
    for (relative, value) in managed_entries {
        let system_path = paths.home_dir.join(&relative);
        let repo_bytes = read_tree_entry_bytes(repo.store(), &relative, &value).await?;
        let status = match fs::read(&system_path) {
            Ok(system_bytes) if system_bytes == repo_bytes => None,
            Ok(_) => Some(ChangeStatus::Modified),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Some(ChangeStatus::Deleted),
            Err(source) => {
                return Err(DotsyncError::Io {
                    path: system_path,
                    source,
                });
            }
        };

        if let Some(status) = status {
            changes.push(FileChange {
                path: relative,
                status,
            });
        }
    }

    Ok(StatusReport {
        machine_scope,
        changes,
    })
}
