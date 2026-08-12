use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jj_lib::backend::TreeValue;

use crate::config::{internal_repo_paths, DotsyncConfig, DotsyncPaths};
use crate::error::DotsyncError;
use crate::repo::{collect_managed_tree_entries, load_scope_commit, read_tree_entry_bytes};
use crate::sync::SyncState;

/// Where one managed path stands across the three sides dotsync knows about:
///
/// - `L` — the tree this machine last synced, from the sync-state file
/// - `H` — what is in home right now
/// - `T` — what this machine's scope tip holds right now
///
/// This is dotsync's whole drift vocabulary. The sync gate, `status`, `diff`
/// and `commit`'s selection are filters and renderings over a map of these;
/// none of them compares two sides on its own. That matters because the
/// interesting cases are exactly the ones a two-sided comparison cannot name:
/// a home file that equals `L` while `T` has moved on is *not* a local edit,
/// but every two-sided check that looks only at home against the tip calls it
/// one — and a commit that believes it then re-records the older bytes over
/// another machine's published change.
///
/// The variants are the full cross of presence for the three sides, collapsed
/// by equality wherever two present sides hold the same bytes. Every situation
/// therefore lands in exactly one variant, and none needs a special case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    /// Absent from all three sides. Only reachable when a caller asks about a
    /// path outside the managed set — a commit naming a path that does not
    /// exist anywhere, for instance. Kept explicit so that question has an
    /// answer instead of silently falling through a wildcard.
    AbsentEverywhere,

    /// Present in home only. Never synced here and not on the scope: a new
    /// file, which is only interesting to `commit`.
    UntrackedInHome,

    /// Added on another machine and not in home yet. Not drift — sync writes
    /// it.
    IncomingNew,

    /// Added on another machine, and home already holds exactly those bytes.
    /// Not drift, and the write is a no-op.
    IncomingNewAlreadyMatchesHome,

    /// Home holds real content dotsync has never seen, and the repo has just
    /// introduced a different file at the same path. Drift: writing the repo's
    /// version would destroy home content nothing has a record of.
    IncomingNewCollidesWithUntrackedHome,

    /// Was synced here and then deleted from home. Deletion drift — blocks
    /// like an edit, and is recorded to a scope like any other change.
    DeletedInHome,

    /// Deleted from home, and the repo changed the file independently. Still
    /// deletion drift; the message names both sides because the resolutions
    /// differ (record the deletion, or reconcile with the new content).
    DeletedInHomeTipAlsoChanged,

    /// A local edit that was never recorded. Edit drift — blocks.
    EditedInHome,

    /// A local edit to a file that someone else deleted from the repo. Still
    /// edit drift; home holds real unsynced content either way.
    EditedInHomeButRemovedFromRepo,

    /// Home and the tip both moved away from the last-synced tree, to the same
    /// bytes. Most often this run's own commit: home already held the bytes
    /// because the commit read them from there, and the tip holds them because
    /// the commit just wrote them. Not drift — already applied.
    AlreadyApplied,

    /// Home and the tip both moved away from the last-synced tree, differently.
    /// This machine's unrecorded edit against someone else's published change:
    /// a real three-way conflict, which only `commit` can resolve.
    DivergedEdit,

    /// Home is exactly what was last synced here, and the tip has moved on.
    /// Not drift and not this machine's business — plain `dotsync` applies it.
    /// Committing it would revert whoever published it.
    StaleNotYours,

    /// Deleted from the repo elsewhere, and home still holds what was synced.
    /// Not drift — sync removes it.
    RemovedFromRepo,

    /// Deleted from home and from the repo. Already converged.
    RemovedEverywhere,

    /// Byte-identical on all three sides.
    InSync,
}

impl FileState {
    /// Home holds something dotsync neither put there nor has a record of.
    /// These block a sync unless it is forced, and they are what `status` and
    /// `diff` report as changes.
    pub fn is_drift(self) -> bool {
        matches!(
            self,
            Self::EditedInHome
                | Self::EditedInHomeButRemovedFromRepo
                | Self::DeletedInHome
                | Self::DeletedInHomeTipAlsoChanged
                | Self::DivergedEdit
                | Self::IncomingNewCollidesWithUntrackedHome
        )
    }

    /// The repo has moved and home has not. Plain `dotsync` resolves these
    /// with no decision from anyone, which is exactly what distinguishes them
    /// from drift in `status` output.
    pub fn is_incoming(self) -> bool {
        matches!(
            self,
            Self::IncomingNew | Self::StaleNotYours | Self::RemovedFromRepo
        )
    }

    /// Home holds no change of this machine's own at this path, so recording
    /// home's bytes here would overwrite someone else's change rather than
    /// contribute one. `commit` refuses these unless the same command forces
    /// the path, which is what makes "a machine that is merely behind reverts
    /// another machine's work" unrepresentable rather than merely unlikely.
    pub fn blocks_commit(self) -> bool {
        matches!(
            self,
            Self::StaleNotYours | Self::IncomingNew | Self::IncomingNewCollidesWithUntrackedHome
        )
    }

    /// What happened, naming every side that moved. The remedy depends on
    /// which ones did, so a message that mentions only home leaves the reader
    /// to guess at the rest.
    pub fn reason(self) -> &'static str {
        match self {
            Self::EditedInHome => "edited here since the last sync",
            Self::EditedInHomeButRemovedFromRepo => {
                "edited here since the last sync, and removed from the repo on another machine"
            }
            Self::DeletedInHome => "deleted here since the last sync",
            Self::DeletedInHomeTipAlsoChanged => {
                "deleted here since the last sync, and changed in the repo on another machine"
            }
            Self::DivergedEdit => "edited here, and changed in the repo on another machine",
            Self::IncomingNewCollidesWithUntrackedHome => {
                "never synced here, and the repo has just added a different file at this path"
            }
            Self::IncomingNew => "added on another machine",
            Self::StaleNotYours => "changed on another machine, and not edited here",
            Self::RemovedFromRepo => "removed on another machine",
            Self::UntrackedInHome => "in home only; dotsync does not manage it",
            Self::IncomingNewAlreadyMatchesHome | Self::AlreadyApplied | Self::InSync => {
                "already up to date"
            }
            Self::RemovedEverywhere | Self::AbsentEverywhere => "not present",
        }
    }

    /// The machine-readable name of this state, for `--output json`.
    pub fn code(self) -> &'static str {
        match self {
            Self::EditedInHome => "modified",
            Self::EditedInHomeButRemovedFromRepo => "modified_removed_from_repo",
            Self::DeletedInHome => "deleted",
            Self::DeletedInHomeTipAlsoChanged => "deleted_changed_in_repo",
            Self::DivergedEdit => "conflicted",
            Self::IncomingNewCollidesWithUntrackedHome => "untracked_collision",
            Self::IncomingNew => "incoming_add",
            Self::StaleNotYours => "incoming_update",
            Self::RemovedFromRepo => "incoming_delete",
            Self::UntrackedInHome => "untracked",
            Self::IncomingNewAlreadyMatchesHome => "incoming_add_already_applied",
            Self::AlreadyApplied => "already_applied",
            Self::InSync => "in_sync",
            Self::RemovedEverywhere => "removed_everywhere",
            Self::AbsentEverywhere => "absent",
        }
    }
}

/// Classifies one path from the bytes each side holds. Pure: no repo, no
/// filesystem, no rendering.
pub(crate) fn classify(
    last_synced: Option<&[u8]>,
    home: Option<&[u8]>,
    tip: Option<&[u8]>,
) -> FileState {
    match (last_synced, home, tip) {
        (None, None, None) => FileState::AbsentEverywhere,
        (None, None, Some(_)) => FileState::IncomingNew,
        (None, Some(_), None) => FileState::UntrackedInHome,
        (None, Some(home), Some(tip)) if home == tip => FileState::IncomingNewAlreadyMatchesHome,
        (None, Some(_), Some(_)) => FileState::IncomingNewCollidesWithUntrackedHome,
        (Some(_), None, None) => FileState::RemovedEverywhere,
        (Some(last), None, Some(tip)) if last == tip => FileState::DeletedInHome,
        (Some(_), None, Some(_)) => FileState::DeletedInHomeTipAlsoChanged,
        (Some(last), Some(home), None) if last == home => FileState::RemovedFromRepo,
        (Some(_), Some(_), None) => FileState::EditedInHomeButRemovedFromRepo,
        (Some(last), Some(home), Some(tip)) => match (last == home, last == tip) {
            (true, true) => FileState::InSync,
            (true, false) => FileState::StaleNotYours,
            (false, true) => FileState::EditedInHome,
            (false, false) if home == tip => FileState::AlreadyApplied,
            (false, false) => FileState::DivergedEdit,
        },
    }
}

/// One classified path together with the bytes the sides hold, read once so
/// that the consumers never go back to the repo or to home for them.
#[derive(Debug, Clone)]
pub(crate) struct ClassifiedPath {
    pub(crate) state: FileState,
    pub(crate) last_synced_bytes: Option<Vec<u8>>,
    pub(crate) home_bytes: Option<Vec<u8>>,
    pub(crate) tip_bytes: Option<Vec<u8>>,
}

/// The paths a classification covers by default: everything dotsync last
/// synced here, plus everything the tip holds now. Home contributes no paths
/// of its own — an unmanaged home file is not dotsync's business, and looking
/// for them would mean walking the whole home directory.
pub(crate) fn managed_domain(
    last_synced_entries: Option<&BTreeMap<PathBuf, TreeValue>>,
    tip_entries: &BTreeMap<PathBuf, TreeValue>,
) -> BTreeSet<PathBuf> {
    last_synced_entries
        .into_iter()
        .flat_map(|entries| entries.keys())
        .chain(tip_entries.keys())
        .cloned()
        .collect()
}

/// Classifies every path in `domain`.
///
/// `last_synced_entries` is `None` when there is no usable sync state — the
/// file is missing, or it names a revision this repo does not have, and on a
/// brand new machine there has never been one. Dotsync then has no record of
/// putting anything in home, so the last-synced side is empty: it will not
/// remove a file it cannot show it wrote, and it will not read a file missing
/// from home as a deletion someone made here. Real home content that
/// disagrees with the scope is still drift, because that judgement needs no
/// history — it is visible in the two sides that are there.
pub(crate) async fn classify_paths(
    paths: &DotsyncPaths,
    repo: &dyn jj_lib::repo::Repo,
    last_synced_entries: Option<&BTreeMap<PathBuf, TreeValue>>,
    tip_entries: &BTreeMap<PathBuf, TreeValue>,
    domain: &BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, ClassifiedPath>, DotsyncError> {
    let no_record = BTreeMap::new();
    let last_synced_entries = last_synced_entries.unwrap_or(&no_record);

    let mut classified = BTreeMap::new();
    for relative in domain {
        let last_synced_bytes =
            read_entry_bytes(repo, relative, last_synced_entries.get(relative)).await?;
        let tip_bytes = read_entry_bytes(repo, relative, tip_entries.get(relative)).await?;
        let home_bytes = read_home_bytes(paths, relative)?;
        let state = classify(
            last_synced_bytes.as_deref(),
            home_bytes.as_deref(),
            tip_bytes.as_deref(),
        );
        classified.insert(
            relative.clone(),
            ClassifiedPath {
                state,
                last_synced_bytes,
                home_bytes,
                tip_bytes,
            },
        );
    }
    Ok(classified)
}

pub(crate) async fn read_entry_bytes(
    repo: &dyn jj_lib::repo::Repo,
    relative: &Path,
    value: Option<&TreeValue>,
) -> Result<Option<Vec<u8>>, DotsyncError> {
    match value {
        Some(value) => Ok(Some(
            read_tree_entry_bytes(repo.store(), relative, value).await?,
        )),
        None => Ok(None),
    }
}

pub(crate) fn read_home_bytes(
    paths: &DotsyncPaths,
    relative: &Path,
) -> Result<Option<Vec<u8>>, DotsyncError> {
    let home_path = paths.home_dir.join(relative);
    match fs::read(&home_path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(DotsyncError::Io {
            path: home_path,
            source,
        }),
    }
}

/// Everything one command needs to know about how home stands against a scope.
#[derive(Debug, Clone)]
pub(crate) struct HomeClassification {
    /// The scope's current head, which is also what a completed sync records
    /// as the new last-synced revision.
    pub(crate) tip: jj_lib::commit::Commit,
    pub(crate) tip_entries: BTreeMap<PathBuf, TreeValue>,
    pub(crate) paths: BTreeMap<PathBuf, ClassifiedPath>,
}

impl HomeClassification {
    /// A path outside the classified set is a path nothing knows about, which
    /// is a real answer rather than a missing one.
    pub(crate) fn state(&self, relative: &Path) -> FileState {
        self.paths
            .get(relative)
            .map(|path| path.state)
            .unwrap_or(FileState::AbsentEverywhere)
    }
}

/// Classifies home against one scope's current head. This is the single drift
/// computation: the sync gate acts on it, `status` and `diff` report it, and
/// `commit` asks it whether a named path holds a change of this machine's own.
///
/// `extra_paths` widens the classified set beyond the managed domain, for
/// callers that name paths dotsync has never seen — a commit adding a new file.
pub(crate) async fn classify_home_against_scope(
    paths: &DotsyncPaths,
    repo: &dyn jj_lib::repo::Repo,
    config: &DotsyncConfig,
    sync_state: Option<&SyncState>,
    scope: &str,
    extra_paths: &BTreeSet<PathBuf>,
) -> Result<HomeClassification, DotsyncError> {
    let internal_paths = internal_repo_paths(config);
    let tip = load_scope_commit(repo, scope)?;
    let tip_entries = collect_managed_tree_entries(&tip.tree(), &internal_paths)?;
    let last_synced_entries = last_synced_entries(repo, sync_state, &internal_paths)?;

    let mut domain = managed_domain(last_synced_entries.as_ref(), &tip_entries);
    domain.extend(extra_paths.iter().cloned());
    let paths = classify_paths(
        paths,
        repo,
        last_synced_entries.as_ref(),
        &tip_entries,
        &domain,
    )
    .await?;

    Ok(HomeClassification {
        tip,
        tip_entries,
        paths,
    })
}

/// The tree this machine last synced, or `None` when there is no usable record
/// of one — no state file, or a revision this repo does not have (stale state
/// left by a different repo instance).
fn last_synced_entries(
    repo: &dyn jj_lib::repo::Repo,
    sync_state: Option<&SyncState>,
    internal_paths: &BTreeSet<PathBuf>,
) -> Result<Option<BTreeMap<PathBuf, TreeValue>>, DotsyncError> {
    let Some(state) = sync_state else {
        return Ok(None);
    };
    let Ok(commit) = repo.store().get_commit(&state.last_synced_revision) else {
        return Ok(None);
    };
    Ok(Some(collect_managed_tree_entries(
        &commit.tree(),
        internal_paths,
    )?))
}
