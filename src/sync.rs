use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::repo::Repo as _;

use crate::converge;
use crate::drift::{changed_paths, classify_managed_trees, ClassifiedPath, FileState};
use crate::error::{jj_error, ConflictRole, ConflictedFile, ConflictedVersion, DotsyncError};
use crate::home::{repo_path_of, Home, Materialized};
use crate::paths::DotsyncPaths;
use crate::pause::{converge_or_pause, publish_or_pause};
use crate::repo::{
    collect_managed_tree_entries, read_entry_bytes, scope_head, scope_head_commit, scope_head_tree,
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
/// A local edit is an input to the sync rather than a wall in front of it —
/// the merge carries it across — and only a collision on the same file stops
/// the run.
pub async fn sync(paths: &DotsyncPaths) -> Run<Result<SyncCommandReport, DotsyncError>> {
    in_session(paths, async |session, paths| {
        let mut home = Home::acquire(session, paths).await?;
        let outcome = sync_home(session, &mut home, LocalChanges::Carry).await;
        finishing(home, session, outcome).await
    })
    .await
}

/// `dotsync discard <paths>`: the same sync, having decided against the change
/// home holds at the paths it names.
///
/// The one way out of a local change other than committing it, and the reason
/// it cannot simply be `rm` plus a sync: deleting a managed file is itself a
/// local change, so home would come back empty rather than canonical.
///
/// It names paths because the two things it is used for are opposites — a
/// stale config file you no longer want, and a resolution somebody is part-way
/// through writing — and a run that could not tell them apart discarded both.
pub async fn discard(
    paths: &DotsyncPaths,
    targets: &[PathBuf],
) -> Run<Result<SyncCommandReport, DotsyncError>> {
    in_session(paths, async |session, paths| {
        let mut home = Home::acquire(session, paths).await?;
        let outcome = discard_home(session, &mut home, targets).await;
        finishing(home, session, outcome).await
    })
    .await
}

async fn discard_home(
    session: &mut Session,
    home: &mut Home,
    targets: &[PathBuf],
) -> Result<SyncCommandReport, DotsyncError> {
    session.fetch().await?;
    // Before anything moves. A path that holds no change of this machine's own
    // is a path this command has nothing to do at, and a run that answered
    // "discarded 0 file(s)" to a typo would leave the caller believing it had
    // decided something.
    let classified = classify_home_against_machine_scope(session, home).await?;
    let changed = changed_paths(&classified, FileState::is_drift);
    let nothing_to_discard: Vec<PathBuf> = targets
        .iter()
        .filter(|target| !changed.iter().any(|(path, _)| &path == target))
        .cloned()
        .collect();
    if !nothing_to_discard.is_empty() {
        return Err(DotsyncError::NothingToDiscard {
            paths: nothing_to_discard,
        });
    }

    sync_home(session, home, LocalChanges::DiscardAt(targets.to_vec())).await
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
    local: LocalChanges,
) -> Result<SyncCommandReport, DotsyncError> {
    session.fetch().await?;
    let checkpoint = converge::checkpoint(session.repo().as_ref(), session.graph());
    converge_or_pause(session, home, &checkpoint).await?;
    // Publish before touching home: scope commits left behind by an
    // interrupted run must reach the remote even if the home sync stops.
    let push = publish_or_pause(session, home, &checkpoint).await?;
    let sync = sync_home_to_machine_scope(session, home, local).await?;
    Ok(SyncCommandReport { sync, push })
}

/// What a sync does with the changes home is holding.
pub(crate) enum LocalChanges {
    /// Merged in: the ordinary sync, and the reason a local edit is an input
    /// rather than a wall.
    Carry,
    /// Dropped whole: `init` and `abort`, which exist to take the head's side
    /// and are given no choice about it.
    Discard,
    /// Dropped at these paths, carried everywhere else: `discard`, and the end
    /// of a resolution, where the conflicted files go back to being whatever
    /// this machine's scope says they are.
    DiscardAt(Vec<PathBuf>),
}

/// The home sync itself: `merge(home, mark, head)` and what it came to.
///
/// Every command that writes home ends here — plain `dotsync`, `commit`,
/// `continue`, `abort` and `init` — because moving home is one operation
/// whatever moved the head first. What differs between them is only what they
/// do with home's own changes.
///
/// The classification is read before the merge moves anything, because two of
/// its three sides are the working copy's own and the merge replaces them —
/// and it is what says which home files the sync discarded.
pub(crate) async fn sync_home_to_machine_scope(
    session: &mut Session,
    home: &mut Home,
    local: LocalChanges,
) -> Result<SyncReport, DotsyncError> {
    let machine_scope = home.machine_scope().to_string();
    let head = match scope_head(session.repo().as_ref(), &machine_scope).is_absent() {
        true => return Err(machine_scope_missing(session, &machine_scope)),
        false => scope_head_commit(session.repo().as_ref(), &machine_scope)?,
    };
    let classified = classify_home_against_head(session, home, &head.tree()).await?;
    let local_changes = changed_paths(&classified, FileState::is_drift);
    let head_paths = collect_managed_tree_entries(&head.tree())?;

    let materialized = match &local {
        LocalChanges::Carry => home.materialize(session, &head).await?,
        LocalChanges::Discard => home.materialize_discarding_local(session, &head).await?,
        LocalChanges::DiscardAt(paths) => {
            home.materialize_taking_head_at(session, &head, paths)
                .await?
        }
    };
    if let Materialized::Conflicted { merged } = materialized {
        return Err(sync_conflict(session, &machine_scope, &classified, &merged).await?);
    }

    // Which local changes this run destroyed and which it kept. Only the
    // destroyed ones are rendered as a two-sided diff, so only those pay for
    // their content — the classification carried tree entries, not bytes.
    let discarded = |relative: &PathBuf| match &local {
        LocalChanges::Carry => false,
        LocalChanges::Discard => true,
        LocalChanges::DiscardAt(paths) => paths.contains(relative),
    };
    let mut drifts = Vec::new();
    let mut carried_changes = Vec::new();
    for (relative, path) in &local_changes {
        if discarded(relative) {
            drifts.push(file_drift(session, relative, path).await?);
        } else {
            carried_changes.push(FileChange {
                path: relative.clone(),
                state: path.state,
            });
        }
    }

    Ok(SyncReport {
        current_scope: machine_scope,
        synced_paths: head_paths.into_keys().collect(),
        drifts,
        carried_changes,
    })
}

/// Home against a head: the three trees `Home` holds, and the merge of them
/// that a sync would write.
pub(crate) async fn classify_home_against_head(
    session: &mut Session,
    home: &mut Home,
    head: &jj_lib::merged_tree::MergedTree,
) -> Result<BTreeMap<PathBuf, ClassifiedPath>, DotsyncError> {
    let merged = home.merge_with(session, head).await?;
    let mark = home.mark().await?;
    classify_managed_trees(&mark.tree(), &home.snapshot_tree(), head, &merged)
}

/// Home against this machine's scope, for the commands that only report.
///
/// The head is the tree the scope head holds — the merge of both sides when it
/// is contested, which is the tree a convergence would write there. So a
/// contested scope changes what `status` and `diff` answer and not whether
/// they answer: there is no repo state in which the question has no answer.
pub(crate) async fn classify_home_against_machine_scope(
    session: &mut Session,
    home: &mut Home,
) -> Result<BTreeMap<PathBuf, ClassifiedPath>, DotsyncError> {
    let machine_scope = home.machine_scope().to_string();
    let head = match scope_head_tree(session.repo().as_ref(), &machine_scope).await? {
        Some(head) => head,
        None => return Err(machine_scope_missing(session, &machine_scope)),
    };
    classify_home_against_head(session, home, &head).await
}

/// The stop for a machine whose own scope the repo does not have.
///
/// Raised where the head is read rather than when the run starts, because the
/// fetch in between is what takes a scope away: the reachable way to lose one
/// is something that is not dotsync renaming or deleting the branch on the
/// shared remote, and the run finds out about that when it fetches.
fn machine_scope_missing(session: &Session, machine_scope: &str) -> DotsyncError {
    DotsyncError::MachineScopeMissing {
        scope: machine_scope.to_string(),
        scopes: session.graph().names().map(str::to_string).collect(),
        root: session.graph().a_root().map(str::to_string),
    }
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
    let mut files = Vec::new();
    // The classification is the domain rather than the merged tree, because
    // every path the merge could touch is in it — it was built from the same
    // three trees — and it is what says where each file stands.
    for (relative, path) in classified {
        let Some(versions) = conflicted_versions(session, merged, relative, machine_scope).await?
        else {
            continue;
        };
        files.push(ConflictedFile {
            path: relative.clone(),
            state: Some(path.state),
            versions,
        });
    }
    Ok(DotsyncError::SyncConflict {
        scope: machine_scope.to_string(),
        files,
    })
}

/// Every version of one path in a merge that did not resolve — the base and
/// both sides, each labeled with what it is.
///
/// `None` when the merge resolved this path, which is most of them: a stop
/// presents the files it could not merge, not the whole tree.
pub(crate) async fn conflicted_versions(
    session: &Session,
    merged: &jj_lib::merged_tree::MergedTree,
    relative: &Path,
    fallback_label: &str,
) -> Result<Option<Vec<ConflictedVersion>>, DotsyncError> {
    let store = session.repo().store();
    let value = merged
        .path_value(&repo_path_of(relative)?)
        .map_err(|err| jj_error(format!("read merged {}: {err}", relative.display())))?;
    if value.is_resolved() {
        return Ok(None);
    }
    let labels = merged.labels_by_term(fallback_label);
    let mut versions = Vec::new();
    // Base first: it is the version the reader needs to make sense of the
    // other two, and jj holds the bases and the sides interleaved.
    for (label, term) in labels.removes().zip(value.removes()) {
        versions.push(conflicted_version(store, relative, ConflictRole::Base, label, term).await?);
    }
    for (label, term) in labels.adds().zip(value.adds()) {
        versions.push(conflicted_version(store, relative, ConflictRole::Side, label, term).await?);
    }
    Ok(Some(versions))
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

/// One classified path with both sides' content read out, for the renderings
/// that show a diff. Read here rather than during classification because a run
/// shows a handful of paths and classifies every managed one.
pub(crate) async fn file_drift(
    session: &Session,
    relative: &Path,
    path: &ClassifiedPath,
) -> Result<FileDrift, DotsyncError> {
    let store = session.repo().store();
    Ok(FileDrift {
        repo_path: relative.to_path_buf(),
        system_path: session.paths().home_dir.join(relative),
        state: path.state,
        repo_bytes: read_entry_bytes(store, relative, path.tip.as_ref()).await?,
        home_bytes: read_entry_bytes(store, relative, path.home.as_ref()).await?,
    })
}
