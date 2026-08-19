use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use jj_lib::backend::TreeValue;
use jj_lib::merged_tree::MergedTree;

use crate::error::{jj_error, DotsyncError};
use crate::home::repo_path_of;
use crate::repo::collect_managed_tree_entries;

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
/// by equality wherever two present sides hold the same tree entry. Every
/// situation therefore lands in exactly one variant, and none needs a special
/// case.
///
/// Two sides are the same when their `TreeValue`s are: same kind, same content
/// id, same executable bit. That is what a difference *is* here, so a chmod is
/// a change and a symlink is not a regular file that happens to hold its own
/// target — neither needs a rule of its own. It is also free, because the trees
/// already hold the ids.
///
/// Whether the two sides that both moved can be reconciled is not a question
/// this vocabulary answers on its own: it is answered by the same
/// `merge(home, mark, head)` the sync materializes, so `DivergedEdit` and
/// `DivergedEditThatMerges` cannot disagree with what the sync then does.
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
    /// introduced a file at the same path that will not merge with it. Drift:
    /// writing the repo's version would destroy home content nothing has a
    /// record of.
    ///
    /// Two adds that *do* merge are `DivergedEditThatMerges` — an add from
    /// nothing is still a change, and jj merges two of them against an empty
    /// base like any other pair.
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

    /// A local edit that changed what the path *is*: a symlink here where the
    /// scope has a regular file, or the reverse. The same drift as
    /// `EditedInHome`, and it exists only to be able to say so — a reader told
    /// that a file was "edited here" goes looking for edited lines, and a link
    /// to `real.conf` against a file holding the nine characters `real.conf`
    /// has none to find.
    KindDiffersFromScope,

    /// A local edit to a file that someone else deleted from the repo. Still
    /// edit drift; home holds real unsynced content either way.
    EditedInHomeButRemovedFromRepo,

    /// Home and the tip both moved away from the last-synced tree, to the same
    /// entry. Most often this run's own commit: home already held the bytes
    /// because the commit read them from there, and the tip holds them because
    /// the commit just wrote them. Not drift — already applied.
    AlreadyApplied,

    /// Home and the tip both moved away from the last-synced tree, differently,
    /// and the merge cannot reconcile them. This machine's unrecorded edit
    /// against someone else's published change, in the same place: a real
    /// three-way conflict, which only the agent can resolve.
    DivergedEdit,

    /// Home and the tip both moved away from the last-synced tree, differently,
    /// and the merge reconciles them: an edit here *and* a change from another
    /// machine, which combine.
    ///
    /// Drift, because home holds a change of this machine's own — and nothing
    /// more than drift, because both of the things that can happen next combine
    /// the two rather than choosing: a plain `dotsync` merges them into home,
    /// and a `commit` merges home's side into the scope against the version home
    /// started from. It is `EditedInHome` with a second thing also true, which
    /// is the whole of why it is worth saying separately: a reader told only
    /// "edited here" would not know a sync is about to change the file under
    /// them.
    DivergedEditThatMerges,

    /// Home is exactly what was last synced here, and the tip has moved on.
    /// Not drift and not this machine's business — plain `dotsync` applies it.
    /// Committing it would revert whoever published it.
    StaleNotYours,

    /// Deleted from the repo elsewhere, and home still holds what was synced.
    /// Not drift — sync removes it.
    RemovedFromRepo,

    /// Deleted from home and from the repo. Already converged.
    RemovedEverywhere,

    /// The same entry on all three sides.
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
                | Self::DivergedEditThatMerges
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
    ///
    /// The two states where home *and* the tip both moved are not here, because
    /// `commit` does not write home's bytes over the tip: it merges them against
    /// the version home started from, so the other machine's change survives
    /// without anyone being refused. What it cannot merge, it pauses on.
    pub fn blocks_commit(self) -> bool {
        matches!(
            self,
            Self::StaleNotYours
                | Self::IncomingNew
                | Self::RemovedFromRepo
                | Self::IncomingNewCollidesWithUntrackedHome
        )
    }

    /// Forcing this path decided something. Without `--force` the commit would
    /// have been refused, or it would have merged home's bytes with a change
    /// that arrived from another machine rather than writing them over it — and
    /// a forced commit does write over it, so a run that forced this owes the
    /// reader the path.
    pub fn forcing_decides_something(self) -> bool {
        self.blocks_commit() || matches!(self, Self::DivergedEdit | Self::DivergedEditThatMerges)
    }

    /// What happened, naming every side that moved. The remedy depends on
    /// which ones did, so a message that mentions only home leaves the reader
    /// to guess at the rest.
    pub fn reason(self) -> &'static str {
        match self {
            Self::EditedInHome => "edited here since the last sync",
            Self::KindDiffersFromScope => {
                "a symlink here where the scope has a regular file, or the reverse: a different kind of file, not a different set of lines"
            }
            Self::EditedInHomeButRemovedFromRepo => {
                "edited here since the last sync, and removed from the repo on another machine"
            }
            Self::DeletedInHome => "deleted here since the last sync",
            Self::DeletedInHomeTipAlsoChanged => {
                "deleted here since the last sync, and changed in the repo on another machine"
            }
            Self::DivergedEdit => "edited here, and changed in the repo on another machine",
            Self::DivergedEditThatMerges => {
                "edited here, and changed in the repo on another machine; the two changes combine"
            }
            Self::IncomingNewCollidesWithUntrackedHome => {
                "never synced here, and the repo has just added a file at this path that will not merge with it"
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
            Self::DivergedEditThatMerges => "modified_changed_in_repo",
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

/// Classifies one path from the tree entry each side holds, plus the merge's
/// own answer about whether the two moving sides can be reconciled there.
/// Pure: no repo, no filesystem, no rendering.
///
/// `merge_reconciles` is only consulted where both home and the tip moved,
/// which is the only question the three entries cannot settle between them. It
/// comes from the merged tree the sync materializes, so a path this calls
/// `DivergedEdit` is a path that sync stops on, and one it calls
/// `DivergedEditThatMerges` is one sync carries.
pub(crate) fn classify(
    last_synced: Option<&TreeValue>,
    home: Option<&TreeValue>,
    tip: Option<&TreeValue>,
    merge_reconciles: bool,
) -> FileState {
    // Both sides moved to something different. Whether that is a conflict is
    // the merge's answer, not a comparison's — and the two ways of reaching
    // here differ only in what to call the conflict.
    let both_moved = || match (merge_reconciles, last_synced) {
        (true, _) => FileState::DivergedEditThatMerges,
        // Two adds of different things, with no version between them to merge
        // against. Home's is the one nothing has a record of, which is what
        // makes this worth a reason of its own.
        (false, None) => FileState::IncomingNewCollidesWithUntrackedHome,
        (false, Some(_)) => FileState::DivergedEdit,
    };
    match (last_synced, home, tip) {
        (None, None, None) => FileState::AbsentEverywhere,
        (None, None, Some(_)) => FileState::IncomingNew,
        (None, Some(_), None) => FileState::UntrackedInHome,
        (None, Some(home), Some(tip)) if home == tip => FileState::IncomingNewAlreadyMatchesHome,
        (None, Some(_), Some(_)) => both_moved(),
        (Some(_), None, None) => FileState::RemovedEverywhere,
        (Some(last), None, Some(tip)) if last == tip => FileState::DeletedInHome,
        (Some(_), None, Some(_)) => FileState::DeletedInHomeTipAlsoChanged,
        (Some(last), Some(home), None) if last == home => FileState::RemovedFromRepo,
        (Some(_), Some(_), None) => FileState::EditedInHomeButRemovedFromRepo,
        (Some(last), Some(home), Some(tip)) => match (last == home, last == tip) {
            (true, true) => FileState::InSync,
            (true, false) => FileState::StaleNotYours,
            (false, true) => edited_in_home(home, tip),
            (false, false) if home == tip => FileState::AlreadyApplied,
            (false, false) => both_moved(),
        },
    }
}

/// An edit home made and the tip did not, said the way it will read best: a
/// link where the scope has a file has no changed lines to show, so the reason
/// has to be about the kind rather than about the content.
fn edited_in_home(home: &TreeValue, tip: &TreeValue) -> FileState {
    let is_link = |value: &TreeValue| matches!(value, TreeValue::Symlink(_));
    if is_link(home) == is_link(tip) {
        FileState::EditedInHome
    } else {
        FileState::KindDiffersFromScope
    }
}

/// One classified path together with the tree entries the two sides a consumer
/// might render hold. Entries rather than bytes: an id and a mode are what the
/// classification needs, and the handful of paths that are actually shown or
/// recorded read their content from these.
#[derive(Debug, Clone)]
pub(crate) struct ClassifiedPath {
    pub(crate) state: FileState,
    pub(crate) home: Option<TreeValue>,
    pub(crate) tip: Option<TreeValue>,
}

/// Classifies every managed path across the trees a run holding `Home` has:
/// the mark, home's snapshot, the head home is being compared against, and the
/// merge of the three.
///
/// The first three are the sides `classify` has always taken — what this
/// machine last synced, what is in home now, what the scope holds now — read
/// from trees, so there is no state file to lose and no second read of home.
/// `Home::acquire` made the middle side true by snapshotting home into the wc
/// commit, and `Home::observe` widened it to cover every path the head holds.
///
/// `merged` is the fourth, and it is the one that keeps this honest: it is the
/// very merge `Home::materialize` writes into home, so "is this path a
/// conflict?" has one answer for the whole run instead of one here and a
/// different one at the moment of the write.
pub(crate) fn classify_managed_trees(
    mark: &MergedTree,
    snapshot: &MergedTree,
    head: &MergedTree,
    merged: &MergedTree,
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
        // Asked of the merge per path, the same way the conflict presentation
        // asks it, so the two cannot name different paths: a merge as a whole
        // can be conflicted while most of its paths resolved perfectly well.
        let merge_reconciles = merged
            .path_value(repo_path_of(&relative)?.as_ref())
            .map_err(|err| jj_error(format!("read merged {}: {err}", relative.display())))?
            .is_resolved();
        let state = classify(
            mark.get(&relative),
            snapshot.get(&relative),
            head.get(&relative),
            merge_reconciles,
        );
        classified.insert(
            relative.clone(),
            ClassifiedPath {
                state,
                home: snapshot.get(&relative).cloned(),
                tip: head.get(&relative).cloned(),
            },
        );
    }
    Ok(classified)
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
