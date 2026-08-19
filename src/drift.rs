use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::backend::TreeValue;
use jj_lib::merged_tree::MergedTree;

use crate::error::DotsyncError;
use crate::repo::{collect_managed_tree_entries, read_tree_entry_bytes};

/// Where one managed path stands across the three sides dotsync knows about:
///
/// - `L` — the tree this machine last synced
/// - `H` — what is in home right now
/// - `T` — what this machine's scope tip holds right now
///
/// This is dotsync's whole vocabulary for a managed file. `status`, `diff`,
/// `commit`'s selection and the home sync a commit ends with are filters and
/// renderings over a map of these; none of them compares two sides on its own.
/// That matters because the
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

    /// Home and the scope hold the same bytes as different *kinds* of file: a
    /// symlink here where the scope has a regular file, or the reverse. Drift,
    /// because a link and a file are not the same thing however equal their
    /// content reads — a link's content is its target string.
    KindDiffersFromScope,

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
    /// Home holds a change of this machine's own: something dotsync neither put
    /// there nor has a record of. These are what `status` and `diff` report as
    /// changes, what a plain sync carries across and a forced one discards, and
    /// what the home sync at the end of a commit stops on.
    pub fn is_drift(self) -> bool {
        matches!(
            self,
            Self::EditedInHome
                | Self::KindDiffersFromScope
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
            Self::StaleNotYours
                | Self::IncomingNew
                | Self::RemovedFromRepo
                | Self::IncomingNewCollidesWithUntrackedHome
        )
    }

    /// What happened, naming every side that moved. The remedy depends on
    /// which ones did, so a message that mentions only home leaves the reader
    /// to guess at the rest.
    pub fn reason(self) -> &'static str {
        match self {
            Self::EditedInHome => "edited here since the last sync",
            Self::KindDiffersFromScope => {
                "a symlink here where the scope has a regular file, or the reverse: the same bytes, a different kind of file"
            }
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
            Self::KindDiffersFromScope => "kind_differs",
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
    pub(crate) home_bytes: Option<Vec<u8>>,
    pub(crate) tip_bytes: Option<Vec<u8>>,
}

pub(crate) async fn read_entry_bytes(
    store: &Arc<jj_lib::store::Store>,
    relative: &Path,
    value: Option<&TreeValue>,
) -> Result<Option<Vec<u8>>, DotsyncError> {
    match value {
        Some(value) => Ok(Some(read_tree_entry_bytes(store, relative, value).await?)),
        None => Ok(None),
    }
}

/// Classifies every managed path across the three trees a run holding `Home`
/// has: the mark, home's snapshot, and the head home is being compared
/// against.
///
/// These are the same three sides `classify` has always taken — what this
/// machine last synced, what is in home now, what the scope holds now — so
/// every state, marker and reason keeps its meaning. What changed is where
/// they are read from: three trees, so there is no state file to lose and no
/// second read of home. `Home::acquire` made the middle side true by
/// snapshotting home into the wc commit, and `Home::observe` widened it to
/// cover every path the head holds.
pub(crate) async fn classify_managed_trees(
    store: &Arc<jj_lib::store::Store>,
    mark: &MergedTree,
    snapshot: &MergedTree,
    head: &MergedTree,
) -> Result<BTreeMap<PathBuf, ClassifiedPath>, DotsyncError> {
    let mark = collect_managed_tree_entries(mark)?;
    let snapshot = collect_managed_tree_entries(snapshot)?;
    let head = collect_managed_tree_entries(head)?;

    let domain: BTreeSet<PathBuf> = mark
        .keys()
        .chain(snapshot.keys())
        .chain(head.keys())
        .cloned()
        .collect();

    let mut classified = BTreeMap::new();
    for relative in domain {
        let last_synced_bytes = read_entry_bytes(store, &relative, mark.get(&relative)).await?;
        let home_bytes = read_entry_bytes(store, &relative, snapshot.get(&relative)).await?;
        let tip_bytes = read_entry_bytes(store, &relative, head.get(&relative)).await?;
        let state = classify(
            last_synced_bytes.as_deref(),
            home_bytes.as_deref(),
            tip_bytes.as_deref(),
        );
        classified.insert(
            relative.clone(),
            ClassifiedPath {
                state: with_kind(
                    state,
                    mark.get(&relative),
                    snapshot.get(&relative),
                    head.get(&relative),
                ),
                home_bytes,
                tip_bytes,
            },
        );
    }
    Ok(classified)
}

/// A difference in kind is a difference, whatever the bytes say.
///
/// `classify` compares content, and content is the only thing the three sides
/// have in common — but a symlink's content *is* its target string, so a link
/// to `real.conf` and a regular file holding the nine characters `real.conf`
/// read as identical to it. They are not the same file, and the trees say so:
/// `TreeValue::Symlink` and `TreeValue::File` are different variants. So the
/// kind is asked of the tree, and only where the content comparison found a
/// difference already does that answer not matter.
fn with_kind(
    state: FileState,
    mark: Option<&TreeValue>,
    snapshot: Option<&TreeValue>,
    head: Option<&TreeValue>,
) -> FileState {
    let is_link = |value: Option<&TreeValue>| matches!(value, Some(TreeValue::Symlink(_)));
    if is_link(snapshot) == is_link(head) {
        return state;
    }
    if is_link(snapshot) == is_link(mark) {
        // Home is what it always was and the scope changed kind, which is the
        // same shape as any other change that arrived from another machine.
        return FileState::StaleNotYours;
    }
    FileState::KindDiffersFromScope
}

/// The paths `classify_managed_trees` reported a state for that a reader has
/// to act on, in the order they read in.
pub(crate) fn changed_paths(
    classified: &BTreeMap<PathBuf, ClassifiedPath>,
    include: fn(FileState) -> bool,
) -> Vec<(PathBuf, ClassifiedPath)> {
    classified
        .iter()
        .filter(|(_, path)| include(path.state))
        .map(|(relative, path)| (relative.clone(), path.clone()))
        .collect()
}

/// Where one path stands, for a caller asking about a path by name. A path
/// outside the classified set is a path nothing knows about, which is a real
/// answer rather than a missing one.
pub(crate) fn state_of(
    classified: &BTreeMap<PathBuf, ClassifiedPath>,
    relative: &Path,
) -> FileState {
    classified
        .get(relative)
        .map(|path| path.state)
        .unwrap_or(FileState::AbsentEverywhere)
}
