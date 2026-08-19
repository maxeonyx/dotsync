use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::Repo as _;
use serde::{Deserialize, Serialize};

use crate::config::{internal_repo_paths, DotsyncConfig, DotsyncPaths};
use crate::drift::{
    changed_paths, classify_home_against_scope, classify_managed_trees, read_entry_bytes,
    ClassifiedPath, FileState, RecordedFromHome,
};
use crate::error::{jj_error, ConflictRole, ConflictedFile, ConflictedVersion, DotsyncError};
use crate::home::{Home, Materialized};
use crate::machine::detect_machine;
use crate::repo::{
    collect_managed_tree_entries, load_scope_commit, pending_push_scopes, push_scope_updates,
    PushReport,
};
use crate::session::{in_session, Run, Session};
use crate::status::FileChange;

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

/// What one sync wrote into home.
///
/// Deliberately not `Default`, for the reason `PushReport` is not: a
/// default-constructed one carries an empty machine scope and an empty file
/// list, which reads exactly like a sync that ran and found nothing to do. A
/// command that did not sync says so by having no `SyncReport` at all.
#[derive(Debug, Clone)]
pub struct SyncReport {
    pub current_scope: String,
    pub synced_paths: Vec<PathBuf>,
    pub drifts: Vec<FileDrift>,
    /// The local changes the sync merged around and left standing in home.
    ///
    /// A sync carries an edit it did not collide with rather than stopping on
    /// it, so a run that applied incoming changes can also have left this
    /// machine holding uncommitted work — and an agent that reads "synced 4
    /// file(s)" and exit 0 would otherwise have no reason to think so. The
    /// edit stays this machine's to decide about, which is only true if the
    /// run that carried it says it is still there.
    pub carried_changes: Vec<FileChange>,
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

/// Plain `dotsync`: bring home to this machine's scope.
///
/// `discard_local` is `--force`: home loses instead of being merged. Without
/// it a local edit is an input to the sync rather than a wall in front of it —
/// the merge carries it across — and only a collision on the same file stops
/// the run.
pub async fn sync(
    paths: &DotsyncPaths,
    discard_local: bool,
) -> Run<Result<SyncCommandReport, DotsyncError>> {
    in_session(paths, async |session, paths| {
        let mut home = Home::acquire(session, paths).await?;
        let outcome = sync_home(session, &mut home, discard_local).await;
        finishing(home, session, outcome).await
    })
    .await
}

/// Ends a run at the home boundary whichever way the run went.
///
/// The working copy holds a lock and a record of where home stands for the
/// whole run, and both are released here — on the conflict stop and on a
/// failure just as much as on a success, which is why the outcome passes
/// through this rather than being returned around it. The run's own failure
/// wins over a failure to persist, because the run's is what the reader has to
/// act on.
pub(crate) async fn finishing<T>(
    home: Home,
    session: &Session,
    outcome: Result<T, DotsyncError>,
) -> Result<T, DotsyncError> {
    let persisted = home.finish(session).await;
    let value = outcome?;
    persisted?;
    Ok(value)
}

async fn sync_home(
    session: &mut Session,
    home: &mut Home,
    discard_local: bool,
) -> Result<SyncCommandReport, DotsyncError> {
    session.fetch().await?;
    // Publish before touching home: scope commits left behind by an
    // interrupted run must reach the remote even if the home sync stops.
    // The exception is a paused cascade, whose scopes are only half
    // cascaded.
    let push = match crate::commit::paused_cascade_scope(session.paths())? {
        Some(paused_scope) => PushReport::WithheldPausedCascade {
            scopes: pending_push_scopes(session),
            paused_scope,
        },
        None => push_scope_updates(session).await?,
    };
    let sync = materialize_machine_scope(session, home, discard_local).await?;
    Ok(SyncCommandReport { sync, push })
}

/// The sync itself: `merge(home, mark, head)` and what it came to.
///
/// The classification is read before the merge writes anything, because two of
/// the three trees it reads stop existing the moment the merge lands — and it
/// is what says which home files a forced sync discarded.
async fn materialize_machine_scope(
    session: &mut Session,
    home: &mut Home,
    discard_local: bool,
) -> Result<SyncReport, DotsyncError> {
    let machine_scope = home.machine_scope().to_string();
    let head = load_scope_commit(session.repo().as_ref(), &machine_scope)?;
    home.observe(session, &head).await?;

    let classified = classify_home_against_head(session, home, &head).await?;
    let local_changes = changed_paths(&classified, FileState::is_drift);
    let head_paths =
        collect_managed_tree_entries(&head.tree(), &internal_repo_paths(session.config()))?;

    let materialized = if discard_local {
        home.materialize_discarding_local(session, &head).await?
    } else {
        home.materialize(session, &head, &machine_scope).await?
    };
    if let Materialized::Conflicted { merged } = materialized {
        return Err(sync_conflict(session, &machine_scope, &classified, &merged).await?);
    }

    // The commit path still reads this file to know what this machine last
    // synced; keeping it in step with the mark is what lets the two paths
    // coexist. It goes when the commit path moves onto `Home`.
    save_sync_state(session.paths(), session.config(), &machine_scope, head.id())?;

    Ok(SyncReport {
        current_scope: machine_scope,
        synced_paths: head_paths.into_keys().collect(),
        // A forced sync overwrote every local change; a merged one overwrote
        // none, and carried them instead.
        drifts: if discard_local {
            local_changes
                .iter()
                .map(|(relative, path)| file_drift(session.paths(), relative, path))
                .collect()
        } else {
            Vec::new()
        },
        carried_changes: if discard_local {
            Vec::new()
        } else {
            local_changes
                .iter()
                .map(|(relative, path)| FileChange {
                    path: relative.clone(),
                    state: path.state,
                })
                .collect()
        },
    })
}

/// Walks the working copy forward over a home sync another code path just did.
///
/// The commands that still sync home themselves — `init`, `commit`, `continue`,
/// `abort` — move a scope head and rewrite home without telling the working
/// copy. The mark would then stay wherever the last plain `dotsync` left it, and
/// the very files the run wrote would read afterwards as edits made on this
/// machine: `status` reporting a change nobody made, and the next sync merging
/// against a version of home that is two commits old. This says what those runs
/// made true instead — home derives from the scope's new head — and writes
/// nothing into home.
///
/// It goes when those commands sync through `Home` themselves, which is the
/// commit path's own chunk of the rewrite.
pub(crate) async fn record_the_home_sync(
    session: &mut Session,
    paths: &DotsyncPaths,
) -> Result<(), DotsyncError> {
    let mut home = Home::acquire(session, paths).await?;
    let outcome = advance_the_mark(session, &mut home).await;
    finishing(home, session, outcome).await
}

async fn advance_the_mark(session: &mut Session, home: &mut Home) -> Result<(), DotsyncError> {
    let machine_scope = home.machine_scope().to_string();
    let head = load_scope_commit(session.repo().as_ref(), &machine_scope)?;
    // The paths only the new head holds are the ones the sync just added to
    // home, so they have to be read before the mark says home derives from it.
    home.observe(session, &head).await?;
    home.record_mark(session, &head).await
}

/// Home against a head, across the three trees `Home` holds.
pub(crate) async fn classify_home_against_head(
    session: &Session,
    home: &Home,
    head: &jj_lib::commit::Commit,
) -> Result<BTreeMap<PathBuf, ClassifiedPath>, DotsyncError> {
    let mark = home.mark().await?;
    classify_managed_trees(
        session.repo().store(),
        &internal_repo_paths(session.config()),
        &mark.tree(),
        &home.snapshot_tree(),
        &head.tree(),
    )
    .await
}

/// Reads a conflicted merge out into the stop that presents it: every
/// conflicted path, with the base and both sides, labeled.
///
/// Nothing is stored. The merge is recomputed from the mark, home and the head
/// on every run, so a rerun presents the same conflict and a resolution is
/// visible the moment it is made.
async fn sync_conflict(
    session: &Session,
    machine_scope: &str,
    classified: &BTreeMap<PathBuf, ClassifiedPath>,
    merged: &jj_lib::merged_tree::MergedTree,
) -> Result<DotsyncError, DotsyncError> {
    let store = session.repo().store();
    let labels = merged.labels_by_term(machine_scope);
    let mut files = Vec::new();
    // The classification is the domain rather than the merged tree, because
    // every path the merge could touch is in it — it was built from the same
    // three trees — and it is what says where each file stands.
    for (relative, path) in classified {
        let repo_path = repo_path_of(relative)?;
        let value = merged
            .path_value(&repo_path)
            .map_err(|err| jj_error(format!("read merged {}: {err}", relative.display())))?;
        if value.is_resolved() {
            continue;
        }
        let mut versions = Vec::new();
        // Base first: it is the version the reader needs to make sense of the
        // other two, and jj holds the bases and the sides interleaved.
        for (label, term) in labels.removes().zip(value.removes()) {
            versions
                .push(conflicted_version(store, relative, ConflictRole::Base, label, term).await?);
        }
        for (label, term) in labels.adds().zip(value.adds()) {
            versions
                .push(conflicted_version(store, relative, ConflictRole::Side, label, term).await?);
        }
        files.push(ConflictedFile {
            path: relative.clone(),
            state: path.state,
            versions,
        });
    }
    Ok(DotsyncError::SyncConflict {
        scope: machine_scope.to_string(),
        files,
    })
}

fn repo_path_of(relative: &Path) -> Result<jj_lib::repo_path::RepoPathBuf, DotsyncError> {
    let relative_str = relative.to_str().ok_or_else(|| DotsyncError::NonUtf8Path {
        path: relative.to_path_buf(),
    })?;
    jj_lib::repo_path::RepoPathBuf::from_internal_string(relative_str)
        .map_err(|err| jj_error(format!("invalid repo path {}: {err}", relative.display())))
}

async fn conflicted_version(
    store: &Arc<jj_lib::store::Store>,
    relative: &Path,
    role: ConflictRole,
    label: &str,
    term: &Option<jj_lib::backend::TreeValue>,
) -> Result<ConflictedVersion, DotsyncError> {
    Ok(ConflictedVersion {
        role,
        label: label.to_string(),
        contents: read_entry_bytes(store, relative, term.as_ref()).await?,
    })
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
    session: &Session,
    force: ForceScope,
    recorded_from_home: &RecordedFromHome,
    machine_scope_hint: Option<&str>,
) -> Result<SyncReport, DotsyncError> {
    let paths = session.paths();
    let config = session.config();

    let sync_state = load_sync_state(paths, config)?;
    let valid_sync_state = sync_state
        .as_ref()
        .filter(|state| config.graph.parents.contains_key(&state.machine_scope));
    let current_scope = resolve_current_scope(config, sync_state.as_ref(), machine_scope_hint)?;
    let classification = classify_home_against_scope(
        session,
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

    save_sync_state(paths, config, &current_scope, classification.tip.id())?;

    Ok(SyncReport {
        current_scope,
        synced_paths,
        drifts,
        // This path stops on a local change rather than merging around one, so
        // it never carries any.
        carried_changes: Vec::new(),
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
