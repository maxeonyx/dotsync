use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId;
use serde::{Deserialize, Serialize};

use crate::config::{load_config, DotsyncConfig, DotsyncPaths};
use crate::drift::{classify_home_against_scope, ClassifiedPath, FileState, RecordedFromHome};
use crate::error::DotsyncError;
use crate::machine::detect_machine;
use crate::repo::{
    fetch_origin, load_repo_direct, pending_push_scopes, push_scope_updates, PushReport,
};

/// Which drifted home files a run may overwrite.
///
/// `--force` answers one question — "overwrite the drift?" — but the commands
/// that ask it do not all have the same thing to scope the answer to. Plain
/// `dotsync` and `continue` name no paths, so their `--force` is necessarily
/// blanket. `commit` names paths, so its `--force` rides that same list and
/// reaches nothing else. `init` and `abort` never really ask: `init` has
/// nothing of yours to overwrite and `abort` exists to discard home edits, so
/// both always overwrite and both refuse the flag.
#[derive(Debug, Clone, Default)]
pub enum ForceScope {
    /// Any drift stops the run.
    #[default]
    Nothing,
    /// Every drifted file.
    Everything,
    /// Only these paths; drift anywhere else still stops the run.
    Paths(BTreeSet<PathBuf>),
}

impl ForceScope {
    pub fn from_paths(paths: &[PathBuf]) -> Self {
        if paths.is_empty() {
            Self::Nothing
        } else {
            Self::Paths(paths.iter().cloned().collect())
        }
    }

    fn allows(&self, relative: &Path) -> bool {
        match self {
            Self::Nothing => false,
            Self::Everything => true,
            Self::Paths(paths) => paths.contains(relative),
        }
    }
}

/// One managed path whose home content is not what the repo says it should be.
///
/// This carries the two sides rather than a rendered diff: a drift is a fact
/// about content, and how it reads — unified diff, one line in `status`, a JSON
/// field — is a decision for whichever edge is reporting it.
#[derive(Debug, Clone)]
pub struct FileDrift {
    pub repo_path: PathBuf,
    pub system_path: PathBuf,
    /// Which of the three sides moved. The remedy depends on it, so every
    /// rendering of a drift can say so rather than leaving the reader to
    /// infer it from the diff.
    pub state: FileState,
    /// What the repo holds, or `None` when the repo has no such file.
    pub repo_bytes: Option<Vec<u8>>,
    /// What home holds, or `None` when the file was deleted from home.
    pub home_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub current_scope: String,
    pub synced_paths: Vec<PathBuf>,
    pub drifts: Vec<FileDrift>,
}

/// The `dotsync` (sync) command: what reached home, and what reached the
/// remote.
#[derive(Debug, Clone)]
pub struct SyncCommandReport {
    pub sync: SyncReport,
    pub push: PushReport,
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

pub async fn sync(
    paths: &DotsyncPaths,
    force: ForceScope,
) -> Result<SyncCommandReport, DotsyncError> {
    let repo = load_repo_direct(paths).await?;
    let _repo = fetch_origin(repo).await?;
    // Publish before touching home: scope commits left behind by an
    // interrupted run must reach the remote even if the home sync stops. The
    // exception is a paused cascade, whose scopes are only half cascaded.
    let push = match crate::commit::paused_cascade_scope(paths)? {
        Some(paused_scope) => PushReport::WithheldPausedCascade {
            scopes: pending_push_scopes(paths).await?,
            paused_scope,
        },
        None => push_scope_updates(paths).await?,
    };
    let sync = sync_repo_to_home(paths, force, &RecordedFromHome::default(), None).await?;
    Ok(SyncCommandReport { sync, push })
}

pub(crate) fn resolve_current_scope(
    config: &DotsyncConfig,
    sync_state: Option<&SyncState>,
    machine_scope_hint: Option<&str>,
) -> Result<String, DotsyncError> {
    let graph = &config.graph;
    let valid_sync_state =
        sync_state.filter(|state| graph.parents.contains_key(&state.machine_scope));
    match (machine_scope_hint, valid_sync_state) {
        (Some(scope), _) => Ok(scope.to_string()),
        (None, Some(state)) => Ok(state.machine_scope.clone()),
        (None, None) => {
            let detected = detect_machine()?;
            if graph.parents.contains_key(&detected.machine_scope) {
                Ok(detected.machine_scope)
            } else {
                Err(DotsyncError::NoCurrentScope)
            }
        }
    }
}

fn write_home_file(
    paths: &DotsyncPaths,
    relative: &Path,
    contents: &[u8],
) -> Result<(), DotsyncError> {
    let system_path = paths.home_dir.join(relative);
    if let Some(parent) = system_path.parent() {
        fs::create_dir_all(parent).map_err(|source| DotsyncError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(&system_path, contents).map_err(|source| DotsyncError::Io {
        path: system_path,
        source,
    })
}

/// Writes the machine scope's tip into home, stopping first on anything home
/// holds that dotsync neither put there nor has a record of.
///
/// `recorded_from_home` is empty for every caller but the two that have just
/// written home content into the repo — see `RecordedFromHome`.
pub(crate) async fn sync_repo_to_home(
    paths: &DotsyncPaths,
    force: ForceScope,
    recorded_from_home: &RecordedFromHome,
    machine_scope_hint: Option<&str>,
) -> Result<SyncReport, DotsyncError> {
    let config = load_config(paths).await?;
    let repo = load_repo_direct(paths).await?;

    let sync_state = load_sync_state(paths, &config)?;
    let valid_sync_state = sync_state
        .as_ref()
        .filter(|state| config.graph.parents.contains_key(&state.machine_scope));
    let current_scope = resolve_current_scope(&config, sync_state.as_ref(), machine_scope_hint)?;
    let classification = classify_home_against_scope(
        paths,
        repo.as_ref(),
        &config,
        valid_sync_state,
        &current_scope,
        &BTreeSet::new(),
        recorded_from_home,
    )
    .await?;

    let drifts = classification
        .paths
        .iter()
        .filter(|(_, path)| path.state.is_drift())
        .map(|(relative, path)| file_drift(paths, relative, path))
        .collect::<Vec<_>>();
    let (overwritten, blocking): (Vec<FileDrift>, Vec<FileDrift>) = drifts
        .into_iter()
        .partition(|drift| force.allows(&drift.repo_path));
    if !blocking.is_empty() {
        return Err(DotsyncError::DriftDetected {
            count: blocking.len(),
            drifts: blocking,
        });
    }
    let drifts = overwritten;

    // The tip is the source of truth for every managed path: if it holds the
    // file, home gets those bytes; if it once held the file and no longer does,
    // home loses it. The classification above already decided whether dotsync
    // is allowed to get this far, so this loop needs no cases of its own.
    let mut synced_paths = Vec::with_capacity(classification.tip_entries.len());
    for (relative, path) in &classification.paths {
        match &path.tip_bytes {
            Some(tip_bytes) => {
                if path.home_bytes.as_deref() != Some(tip_bytes.as_slice()) {
                    write_home_file(paths, relative, tip_bytes)?;
                }
                synced_paths.push(relative.clone());
            }
            // Only a path dotsync knows it wrote may be taken away again. That
            // is what the last-synced side is for, and why a machine with no
            // usable sync state deletes nothing.
            None if path.last_synced_bytes.is_some() => remove_home_path(paths, relative)?,
            None => {}
        }
    }

    save_sync_state(paths, &config, &current_scope, classification.tip.id())?;

    Ok(SyncReport {
        current_scope,
        synced_paths,
        drifts,
    })
}

fn file_drift(paths: &DotsyncPaths, relative: &Path, path: &ClassifiedPath) -> FileDrift {
    FileDrift {
        repo_path: relative.to_path_buf(),
        system_path: paths.home_dir.join(relative),
        state: path.state,
        repo_bytes: path.tip_bytes.clone(),
        home_bytes: path.home_bytes.clone(),
    }
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
