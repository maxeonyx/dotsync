use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::repo::Repo as _;

use crate::config::DotsyncPaths;
use crate::drift::{
    changed_paths, classify_managed_trees, read_entry_bytes, ClassifiedPath, FileState,
};
use crate::error::{jj_error, ConflictRole, ConflictedFile, ConflictedVersion, DotsyncError};
use crate::home::{repo_path_of, Home, Materialized};
use crate::repo::{
    collect_managed_tree_entries, load_scope_commit, pending_push_scopes, push_scope_updates,
    PushReport,
};
use crate::session::{in_session, Run, Session};
use crate::status::FileChange;

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
pub(crate) async fn finishing<T, E: From<DotsyncError>>(
    home: Home,
    session: &Session,
    outcome: Result<T, E>,
) -> Result<T, E> {
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
    let push = match crate::pause::paused_cascade_scope(session.paths())? {
        Some(paused_scope) => PushReport::WithheldPausedCascade {
            scopes: pending_push_scopes(session),
            paused_scope,
        },
        None => push_scope_updates(session).await?,
    };
    let sync = sync_home_to_machine_scope(session, home, discard_local).await?;
    Ok(SyncCommandReport { sync, push })
}

/// The home sync itself: `merge(home, mark, head)` and what it came to.
///
/// Every command that writes home ends here — plain `dotsync`, `commit`,
/// `continue`, `abort` and `init` — because moving home is one operation
/// whatever moved the head first. What differs between them is only
/// `discard_local`: `init` and `abort` exist to take the head's side, and the
/// rest carry a local change across.
///
/// The classification is read before the merge moves anything, because two of
/// its three sides are the working copy's own and the merge replaces them —
/// and it is what says which home files a forced sync discarded.
pub(crate) async fn sync_home_to_machine_scope(
    session: &mut Session,
    home: &mut Home,
    discard_local: bool,
) -> Result<SyncReport, DotsyncError> {
    let machine_scope = home.machine_scope().to_string();
    let head = load_scope_commit(session.repo().as_ref(), &machine_scope)?;
    home.observe(session, &head).await?;

    let classified = classify_home_against_head(session, home, &head).await?;
    let local_changes = changed_paths(&classified, FileState::is_drift);
    let head_paths = collect_managed_tree_entries(&head.tree())?;

    let materialized = if discard_local {
        home.materialize_discarding_local(session, &head).await?
    } else {
        home.materialize(session, &head, &machine_scope).await?
    };
    if let Materialized::Conflicted { merged } = materialized {
        return Err(sync_conflict(session, &machine_scope, &classified, &merged).await?);
    }

    // Every local change went one way or the other: a forced sync discarded all
    // of them, a merged one carried all of them.
    let (drifts, carried_changes) = if discard_local {
        (
            local_changes
                .iter()
                .map(|(relative, path)| file_drift(session.paths(), relative, path))
                .collect(),
            Vec::new(),
        )
    } else {
        (
            Vec::new(),
            local_changes
                .iter()
                .map(|(relative, path)| FileChange {
                    path: relative.clone(),
                    state: path.state,
                })
                .collect(),
        )
    };

    Ok(SyncReport {
        current_scope: machine_scope,
        synced_paths: head_paths.into_keys().collect(),
        drifts,
        carried_changes,
    })
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

fn file_drift(paths: &DotsyncPaths, relative: &Path, path: &ClassifiedPath) -> FileDrift {
    FileDrift {
        repo_path: relative.to_path_buf(),
        system_path: paths.home_dir.join(relative),
        state: path.state,
        repo_bytes: path.tip_bytes.clone(),
        home_bytes: path.home_bytes.clone(),
    }
}
