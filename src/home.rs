//! The home boundary of a run: one lock, one snapshot, one materialization
//! rule.
//!
//! `Home` is the run's handle on the working copy. A command that touches
//! home acquires it once, right after opening the session, and everything the
//! old code derived ad hoc — drift, the last-synced position, what a sync
//! may write — becomes a question about three trees this handle already
//! holds:
//!
//! - **the snapshot** — the wc commit's tree, which is home's current bytes
//!   (acquiring the handle re-snapshots, so it is never stale);
//! - **the mark** — the wc commit's parent, the commit this machine last
//!   materialized. Local changes are `diff(mark, snapshot)`; there is no
//!   drift classifier and no `sync-state.json`.
//! - **a head** — whatever commit a sync wants home to reach.
//!
//! The head arrives after `acquire`, because a run has to fetch and converge
//! before it knows which commit it is heading for — so `observe` is how the run
//! names it, and it widens the snapshot to cover the paths only that head
//! holds. Home's side of the merge has to be real at every path the merge could
//! write, or an arriving file silently overwrites home content dotsync has
//! never read.
//!
//! The materialization rule (spike.ignore/README.md, settled 2026-08-19) is
//! rule 3: `merge(snapshot, mark, head)` in memory; if it resolves, home
//! moves whole and non-conflicting local edits are carried across; if it
//! conflicts, home is not touched and the conflict is presented from the
//! in-memory merge. Partial materialization is unrepresentable here because
//! the only write path is `check_out` of one resolved tree.
//!
//! That merge is `merge_with`, and it is the only one a run makes: `drift`'s
//! classification asks it which paths conflict, so what `status` reports and
//! what a sync writes are two readings of one object rather than two answers
//! to the same question.
//!
//! The wc commit is always a resolved snapshot of home: snapshot amends it
//! (parent unchanged), sync switches it (new parent, merged tree). It never
//! holds a conflict — jj's own convention allows that only because jj
//! materializes conflict markers and re-parses them, which the no-markers
//! rule forbids. The one state that can break "the wc commit describes home"
//! is a run dying between committing a sync and materializing it; `acquire`
//! repairs exactly that, and the repair is the subtlest code in this module —
//! its comments carry the reasoning.

use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::working_copy::{LockedWorkingCopy as _, SnapshotOptions, WorkingCopyFreshness};

use crate::config::{DotsyncPaths, SHED_SYNC_STATE_RELATIVE_PATH};
use crate::error::{jj_error, DotsyncError};
use crate::machine::{detect_machine, machine_signature};
use crate::repo::load_scope_commit;
use crate::session::Session;
use crate::working_copy::{HomeLockedWorkingCopy, HomeWorkingCopy};

/// The run's handle on home: the locked working copy plus the wc commit, kept
/// in step with each other by construction — every method that moves one
/// moves the other.
pub(crate) struct Home {
    locked: HomeLockedWorkingCopy,
    wc_commit: Commit,
    machine_scope: String,
    /// The last `merge(snapshot, mark, head)` this handle computed, keyed by
    /// the wc commit and the head it was computed from. See `merge_with`.
    merged: Option<(CommitId, CommitId, MergedTree)>,
}

/// What completing a sync conflict came to.
pub(crate) enum Resolved {
    /// There was nothing to complete: home and the head merge cleanly, or home
    /// already derives from the head.
    NothingToResolve,
    /// Home's bytes were the resolution, and home now derives from the head.
    Applied,
}

/// What materializing a head did. There is no partial case to represent.
pub(crate) enum Materialized {
    /// Home already matched: the head is the mark and nothing had changed.
    AlreadyThere,
    /// The merge resolved; home moved whole, local edits carried across.
    Applied,
    /// The merge conflicted; home was not touched. The tree holds the base
    /// and both sides for every conflicted path — presentation reads them
    /// out; nothing stores them, so a rerun recomputes the same facts.
    Conflicted { merged: MergedTree },
}

impl Home {
    /// One per run. Locks the working copy, creates it if this is the first
    /// run of a step-2 binary (the migration), repairs it if the last run
    /// died between committing a sync and materializing it, and snapshots
    /// home — so by the time this returns, the wc commit's tree *is* home.
    pub(crate) async fn acquire(
        session: &mut Session,
        paths: &DotsyncPaths,
    ) -> Result<Self, DotsyncError> {
        let machine_scope = detect_machine()?.machine_scope;
        let workspace: WorkspaceNameBuf = machine_scope.as_str().into();
        let store = session.repo().store().clone();
        let state_path = paths.repo_root.join(".jj/working_copy");

        let wc = if state_exists(&state_path) {
            HomeWorkingCopy::load(
                store.clone(),
                paths.home_dir.clone(),
                paths.repo_root.clone(),
                state_path.clone(),
            )
            .map_err(|err| jj_error(format!("load working copy state: {err}")))?
        } else {
            HomeWorkingCopy::init(
                store.clone(),
                paths.home_dir.clone(),
                paths.repo_root.clone(),
                state_path.clone(),
                session.repo().op_id().clone(),
                workspace.clone(),
            )
            .map_err(|err| jj_error(format!("create working copy state: {err}")))?
        };
        let locked = wc
            .start_mutation_concrete()
            .map_err(|err| jj_error(format!("lock the working copy: {err}")))?;

        // The session opened the repo before this lock existed, so another
        // run may have finished in between. Re-reading the repo at head
        // *after* locking closes that window: from here on, nothing can move
        // the working copy under us, and the view we hold is at least as new
        // as anything the state file refers to. This is also what makes
        // `WorkingCopyFreshness::Updated` unreachable below.
        //
        // Through the loader, not through the path: a loader built afresh from
        // the file system builds a second `Store`, and jj asserts that a tree
        // handed to a commit builder came from the same one the transaction's
        // repo has. Every tree in this module is read through `store` above,
        // so the run has to keep one store.
        let reloaded = session
            .repo()
            .loader()
            .load_at_head()
            .await
            .map_err(|err| jj_error(format!("re-read the repo under the lock: {err}")))?;
        session.advance_to(reloaded).await?;

        let wc_commit = ensure_wc_commit(session, &machine_scope, &workspace).await?;
        shed_the_previous_releases_state(paths);

        let freshness = WorkingCopyFreshness::check_stale(&locked, &wc_commit, session.repo())
            .map_err(|err| jj_error(format!("check working copy freshness: {err}")))?;

        let mut home = Self {
            locked,
            wc_commit,
            machine_scope,
            merged: None,
        };
        match freshness {
            WorkingCopyFreshness::Fresh => {
                let snapshot = home.snapshot_home().await?;
                home.amend_if_changed(session, snapshot).await?;
            }
            WorkingCopyFreshness::Updated(_) => {
                // The state file names an operation newer than the repo's
                // head — but state is persisted only after the operation
                // commits, and we re-read the repo at head under the lock.
                return Err(jj_error(
                    "working copy state is ahead of the repository".to_string(),
                ));
            }
            WorkingCopyFreshness::WorkingCopyStale | WorkingCopyFreshness::SiblingOperation => {
                home.repair(session, &workspace).await?;
            }
        }
        Ok(home)
    }

    /// Home's current bytes, as of this run's snapshot.
    pub(crate) fn snapshot_tree(&self) -> MergedTree {
        self.wc_commit.tree()
    }

    /// What home holds at one path, as a tree entry: bytes *and* kind, which is
    /// what makes recording an executable script or a symlink work. Absent when
    /// home holds nothing there, which is how a deletion reads.
    pub(crate) fn entry(
        &self,
        relative: &std::path::Path,
    ) -> Result<Merge<Option<jj_lib::backend::TreeValue>>, DotsyncError> {
        self.snapshot_tree()
            .path_value(repo_path_of(relative)?.as_ref())
            .map_err(|err| jj_error(format!("read home's {}: {err}", relative.display())))
    }

    /// The commit this machine last materialized. Local changes are exactly
    /// `diff(mark tree, snapshot tree)`.
    pub(crate) async fn mark(&self) -> Result<Commit, DotsyncError> {
        let parents = self
            .wc_commit
            .parents()
            .await
            .map_err(|err| jj_error(format!("load the working copy's mark: {err}")))?;
        match <[Commit; 1]>::try_from(parents) {
            Ok([mark]) => Ok(mark),
            Err(_) => Err(jj_error(
                "the working copy commit has more than one parent".to_string(),
            )),
        }
    }

    pub(crate) fn machine_scope(&self) -> &str {
        &self.machine_scope
    }

    /// Widens home's snapshot to cover every path `head` holds, and re-reads
    /// them.
    ///
    /// `acquire` cannot do this, because it snapshots before the run knows
    /// which head it is heading for: a path only the head holds is outside its
    /// probe set, so home's side of the merge reads as *absent* at a path home
    /// actually holds — and the merge then resolves to the head's side and
    /// writes over home content dotsync has never seen. Every path a
    /// materialization could write has to be read before the merge decides
    /// anything about it, which is what this is.
    ///
    /// Every method here that merges against a head or writes one into home
    /// calls this itself, so a caller cannot reach a merge against an
    /// unobserved head by forgetting to — `status` and `diff` get the widened
    /// snapshot from the merge they classify against. Re-observing the same
    /// head is free: the probe set does not widen twice.
    async fn observe(&mut self, session: &mut Session, head: &Commit) -> Result<(), DotsyncError> {
        self.observe_paths(session, tree_paths(&head.tree())?).await
    }

    /// Widens home's snapshot to cover paths the run named itself, and re-reads
    /// them.
    ///
    /// `observe` covers every path some commit holds, which is every path
    /// dotsync already knows about. A commit can also name a path nothing
    /// knows about yet — a new config file — and home's side of that path has
    /// to be read before the commit can record it. Without this the path is
    /// outside the probe set, so the snapshot says home holds nothing there
    /// and the commit records a deletion of a file that does not exist.
    pub(crate) async fn observe_paths(
        &mut self,
        session: &mut Session,
        paths: impl IntoIterator<Item = jj_lib::repo_path::RepoPathBuf>,
    ) -> Result<(), DotsyncError> {
        if !self.locked.probe_also(paths) {
            // Every path was already read, which is the ordinary case — a head
            // that has not added a file holds no path the mark does not, and a
            // commit usually names a file dotsync already tracks. Home's side
            // of the merge is already complete, so there is nothing to re-read.
            return Ok(());
        }
        let snapshot = self.snapshot_home().await?;
        self.amend_if_changed(session, snapshot).await
    }

    /// `merge(snapshot, mark, head)`: the one merge a run makes.
    ///
    /// It is rule 3's merge, and it is also where "is this path a conflict?"
    /// gets its answer for the classification in `drift`. Those have to be the
    /// same object rather than two computations that agree by inspection — a
    /// second opinion about conflicts is how `status` came to call a file
    /// conflicted that a plain `dotsync` then merged without complaint.
    ///
    /// Computed once per run and remembered, because the wc commit and the head
    /// between them fix all three sides: the snapshot is the wc commit's tree,
    /// the mark is its parent, and every method here that moves either writes a
    /// new wc commit. A second call with the same pair could only recompute the
    /// same tree.
    pub(crate) async fn merge_with(
        &mut self,
        session: &mut Session,
        head: &Commit,
    ) -> Result<MergedTree, DotsyncError> {
        self.observe(session, head).await?;
        if let Some((wc_commit, merged_head, merged)) = &self.merged {
            if wc_commit == self.wc_commit.id() && merged_head == head.id() {
                return Ok(merged.clone());
            }
        }
        let mark = self.mark().await?;
        let merged = merge_trees(
            (self.snapshot_tree(), "local changes in home"),
            (mark.tree(), "what this machine last synced"),
            (head.tree(), &self.machine_scope),
        )
        .await?;
        self.merged = Some((
            self.wc_commit.id().clone(),
            head.id().clone(),
            merged.clone(),
        ));
        Ok(merged)
    }

    /// Moves home to `head`: rule 3. `merge(snapshot, mark, head)` in
    /// memory; resolved materializes whole (a new wc commit on `head`, local
    /// edits carried), conflicted touches nothing and hands back the merge.
    pub(crate) async fn materialize(
        &mut self,
        session: &mut Session,
        head: &Commit,
    ) -> Result<Materialized, DotsyncError> {
        let mark = self.mark().await?;
        if head.id() == mark.id() {
            return Ok(Materialized::AlreadyThere);
        }
        let merged = self.merge_with(session, head).await?;
        if merged.has_conflict() {
            return Ok(Materialized::Conflicted { merged });
        }
        self.switch_and_check_out(session, head.id().clone(), merged)
            .await?;
        Ok(Materialized::Applied)
    }

    /// Moves home to `head` taking home's own bytes as the resolution of every
    /// path the merge could not resolve.
    ///
    /// This is `continue` at the home boundary. "I have written the
    /// resolution" is a fact only the agent can state, so nothing about the
    /// conflict is stored between the run that presented it and the run that
    /// finishes it: the merge is recomputed from the same three trees, and
    /// home's side of the conflicted paths is taken as final. Every other path
    /// merges as it would have anyway, so the incoming changes the stop
    /// withheld arrive together with the resolution.
    ///
    /// The resolution stays an ordinary uncommitted local change: the mark
    /// moves to `head`, and home holding something `head` does not is exactly
    /// what a local change is.
    pub(crate) async fn resolve_with_home_bytes(
        &mut self,
        session: &mut Session,
        head: &Commit,
    ) -> Result<Resolved, DotsyncError> {
        let mark = self.mark().await?;
        if head.id() == mark.id() {
            return Ok(Resolved::NothingToResolve);
        }
        let merged = self.merge_with(session, head).await?;
        if !merged.has_conflict() {
            return Ok(Resolved::NothingToResolve);
        }

        // Taken after `merge_with`, whose observation may have widened the
        // snapshot — home's side of the resolution has to be the widened one.
        let snapshot = self.snapshot_tree();
        let conflicted = merged
            .conflicts()
            .map(|(path, value)| {
                value.map_err(|err| jj_error(format!("read conflicted {path:?}: {err}")))?;
                Ok(path)
            })
            .collect::<Result<Vec<_>, DotsyncError>>()?;
        let mut builder = MergedTreeBuilder::new(merged);
        for path in conflicted {
            let value = snapshot
                .path_value(path.as_ref())
                .map_err(|err| jj_error(format!("read home's {path:?}: {err}")))?;
            builder.set_or_remove(path, value);
        }
        let resolved = builder
            .write_tree()
            .await
            .map_err(|err| jj_error(format!("write the resolved tree: {err}")))?;

        // `check_out` refuses a tree that is still conflicted, which is what
        // keeps the no-markers rule true here: home's side of every conflicted
        // path was just written over the merge, so a conflict left in it would
        // be a bug in this method rather than a state to materialize.
        self.switch_and_check_out(session, head.id().clone(), resolved)
            .await?;
        Ok(Resolved::Applied)
    }

    /// Moves home to `head` with home's side of the merge dropped: the head's
    /// tree is materialized whole and every local change at a managed path is
    /// gone.
    ///
    /// That is the one question `--force` asks — "overwrite what is in home?" —
    /// and it cannot be answered by `materialize`, whose whole job is to carry
    /// local changes across. It is also the operation `discard <paths>` narrows
    /// to a path list (PLAN §2.3 step 7); the whole-home version is what the
    /// flag can express.
    pub(crate) async fn materialize_discarding_local(
        &mut self,
        session: &mut Session,
        head: &Commit,
    ) -> Result<Materialized, DotsyncError> {
        // Observed even though home's side is about to be dropped: check_out
        // diffs against the snapshot, so an unobserved path on disk would
        // read as absent and be blindly overwritten instead of counted as a
        // local change this discard is discarding.
        self.observe(session, head).await?;
        let mark = self.mark().await?;
        if head.id() == mark.id() && self.wc_commit.tree_ids() == head.tree().tree_ids() {
            return Ok(Materialized::AlreadyThere);
        }
        self.switch_and_check_out(session, head.id().clone(), head.tree())
            .await?;
        Ok(Materialized::Applied)
    }

    /// Ends the run at the home boundary: persists which operation home has
    /// seen. Call it after the last transaction, with the session's final
    /// operation.
    pub(crate) async fn finish(self, session: &Session) -> Result<(), DotsyncError> {
        let op_id: OperationId = session.repo().op_id().clone();
        Box::new(self.locked)
            .finish(op_id)
            .await
            .map_err(|err| jj_error(format!("persist working copy state: {err}")))?;
        Ok(())
    }

    /// Reads home. The probe set is every path any relevant tree knows: what
    /// was last materialized (already in the probe), what the wc commit
    /// holds, and what the mark holds — the mark's paths keep a
    /// deleted-then-recreated file visible until the deletion is committed.
    async fn snapshot_home(&mut self) -> Result<MergedTree, DotsyncError> {
        let mark = self.mark().await?;
        self.locked.probe_also(tree_paths(&self.wc_commit.tree())?);
        self.locked.probe_also(tree_paths(&mark.tree())?);

        // Asked before the snapshot opens anything, so a tracked path that is
        // now a fifo gets dotsync's explanation of what that is instead of a
        // failure from inside a read — and, before this module existed, instead
        // of blocking for ever.
        if let Some(path) = self.locked.irregular_home_paths().into_iter().next() {
            return Err(DotsyncError::NotARegularFile { path });
        }

        let options = SnapshotOptions {
            base_ignores: GitIgnoreFile::empty(),
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &EverythingMatcher,
            max_new_file_size: u64::MAX,
        };
        let (snapshot, _stats) = self
            .locked
            .snapshot(&options)
            .await
            .map_err(|err| jj_error(format!("snapshot home: {err}")))?;
        Ok(snapshot)
    }

    /// Amends the wc commit to the given tree, parent unchanged — the
    /// "snapshot amends, sync switches" half that keeps the wc commit a
    /// resolved snapshot of home.
    async fn amend_if_changed(
        &mut self,
        session: &mut Session,
        tree: MergedTree,
    ) -> Result<(), DotsyncError> {
        if tree.tree_ids() == self.wc_commit.tree_ids() {
            return Ok(());
        }
        let mut tx = session.repo().start_transaction();
        let amended = tx
            .repo_mut()
            .rewrite_commit(&self.wc_commit)
            .set_tree(tree)
            .set_author(machine_signature(&self.machine_scope))
            .write()
            .await
            .map_err(|err| jj_error(format!("record home's changes: {err}")))?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(|err| jj_error(format!("rebase descendants: {err}")))?;
        let repo = tx
            .commit("dotsync: snapshot home")
            .await
            .map_err(|err| jj_error(format!("commit snapshot: {err}")))?;
        session.advance_to(repo).await?;
        self.wc_commit = amended;
        Ok(())
    }

    /// The stale-working-copy repair. A run died between committing a sync
    /// (the view's wc commit moved to the sync's target) and materializing
    /// it (home did not move). Amending that target to home's bytes would be
    /// the silent-revert path — every incoming change the crashed run never
    /// delivered would read as a local edit undoing it — so this never
    /// amends first. It reconstructs the three honest trees and applies
    /// rule 3:
    ///
    /// - **home now** — a fresh snapshot (which may contain edits made since
    ///   the crash);
    /// - **what home derives from** — the last *materialized* tree, from the
    ///   working copy's own state file;
    /// - **the interrupted target** — the view's wc commit.
    async fn repair(
        &mut self,
        session: &mut Session,
        workspace: &WorkspaceNameBuf,
    ) -> Result<(), DotsyncError> {
        let target_tree = self.wc_commit.tree();
        let snapshot = self.snapshot_home().await?;

        // The crash was after materializing (between check_out and finish):
        // home already matches the target, and only the state file is
        // behind. finish() at the end of this run catches it up.
        if snapshot.tree_ids() == target_tree.tree_ids() {
            return Ok(());
        }

        let materialized_tree = self.locked.old_tree().clone();
        let merged = merge_trees(
            (snapshot.clone(), "local changes in home"),
            (materialized_tree, "what this machine last materialized"),
            (target_tree, "the interrupted sync"),
        )
        .await?;

        if !merged.has_conflict() {
            // Post-crash edits (if any) do not collide with the interrupted
            // sync: complete it, carrying them. Amend-then-check-out is the
            // ordinary materialization shape with the parent already right.
            self.amend_if_changed(session, merged).await?;
            self.check_out().await?;
            return Ok(());
        }

        // Post-crash edits collide with the interrupted sync. Home has never
        // seen the target, so make the wc commit say what is true — home is
        // an edit of the *old* mark — and let this run's ordinary sync leg
        // re-meet the head and present the conflict through the ordinary
        // channel. The old mark is recovered from the operation the state
        // file names: that operation's view holds the wc commit as it was
        // when home was last materialized, and its parent is the mark home
        // still derives from.
        let old_mark_id = self.last_materialized_mark(session, workspace).await?;
        self.switch_to(session, old_mark_id, snapshot).await?;
        Ok(())
    }

    /// The parent of the wc commit as of the operation the working copy last
    /// materialized — the commit home's bytes actually derive from.
    async fn last_materialized_mark(
        &self,
        session: &Session,
        workspace: &WorkspaceNameBuf,
    ) -> Result<CommitId, DotsyncError> {
        let op = session
            .repo()
            .loader()
            .load_operation(self.locked.old_operation_id())
            .await
            .map_err(|err| jj_error(format!("load the working copy's operation: {err}")))?;
        let view = op
            .view()
            .await
            .map_err(|err| jj_error(format!("load the working copy's view: {err}")))?;
        let wc_id = view
            .get_wc_commit_id(workspace.as_ref())
            .ok_or_else(|| jj_error("the working copy's operation has no working copy".into()))?;
        let old_wc = session
            .repo()
            .store()
            .get_commit(wc_id)
            .map_err(|err| jj_error(format!("load the old working copy commit: {err}")))?;
        Ok(old_wc.parent_ids()[0].clone())
    }

    /// The wc commit moves and home follows: the two halves of materializing a
    /// tree, which are only ever done together.
    ///
    /// Every command that moves home to a new head ends here — the ordinary
    /// merge, the resolution `continue` writes, and the head-wins tree
    /// `--force` asks for — because the difference between them is entirely in
    /// which tree they arrive with. Keeping the pair in one place is what makes
    /// "the wc commit describes home" hold by construction rather than by three
    /// call sites each remembering the second half.
    async fn switch_and_check_out(
        &mut self,
        session: &mut Session,
        parent: CommitId,
        tree: MergedTree,
    ) -> Result<(), DotsyncError> {
        self.switch_to(session, parent, tree).await?;
        self.check_out().await
    }

    /// Writes the wc commit's tree into home. Separate from the switch for the
    /// repair alone, which amends rather than switches because the parent is
    /// already the commit home is heading for.
    async fn check_out(&mut self) -> Result<(), DotsyncError> {
        self.locked
            .check_out(&self.wc_commit)
            .await
            .map_err(|err| jj_error(format!("materialize into home: {err}")))?;
        Ok(())
    }

    /// Switches the wc commit onto a new parent with a new (resolved) tree.
    ///
    /// The old wc commit is always abandoned: a wc commit is pure mechanism
    /// (no bookmark ever points at one), and everything it holds lives on in
    /// home and in the new wc commit's tree — in the ordinary sync switch the
    /// merge carried the local edits, and in the repair rewind the snapshot
    /// *is* home. Keeping it would litter the repo with an anonymous head per
    /// sync.
    async fn switch_to(
        &mut self,
        session: &mut Session,
        parent: CommitId,
        tree: MergedTree,
    ) -> Result<(), DotsyncError> {
        let old_wc = self.wc_commit.clone();
        let mut tx = session.repo().start_transaction();
        let new_wc = tx
            .repo_mut()
            .new_commit(vec![parent], tree)
            .set_description(WC_COMMIT_DESCRIPTION)
            .set_author(machine_signature(&self.machine_scope))
            .write()
            .await
            .map_err(|err| jj_error(format!("create the new working copy commit: {err}")))?;
        tx.repo_mut()
            .set_wc_commit(self.workspace_name(), new_wc.id().clone())
            .map_err(|err| jj_error(format!("point the working copy at its commit: {err}")))?;
        tx.repo_mut().record_abandoned_commit(&old_wc);
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(|err| jj_error(format!("rebase descendants: {err}")))?;
        let repo = tx
            .commit("dotsync: sync home")
            .await
            .map_err(|err| jj_error(format!("commit sync: {err}")))?;
        session.advance_to(repo).await?;
        self.wc_commit = new_wc;
        Ok(())
    }

    fn workspace_name(&self) -> WorkspaceNameBuf {
        self.machine_scope.as_str().into()
    }
}

const WC_COMMIT_DESCRIPTION: &str = "dotsync: working copy";

/// The wc commit for this machine's workspace, creating it if this repo has
/// never had one — which is both `init`'s first run and the migration of a
/// machine upgrading from the sync-state.json era. The new wc commit is an
/// empty-diff child of the machine scope's bookmark: differences between home
/// and that bookmark then appear as ordinary local changes on the very next
/// snapshot, which is the D5 lesson (report, never assume) applied to
/// migration. The old sync-state.json is not consulted at all.
async fn ensure_wc_commit(
    session: &mut Session,
    machine_scope: &str,
    workspace: &WorkspaceNameBuf,
) -> Result<Commit, DotsyncError> {
    if let Some(commit_id) = session.repo().view().get_wc_commit_id(workspace.as_ref()) {
        return session
            .repo()
            .store()
            .get_commit(commit_id)
            .map_err(|err| jj_error(format!("load the working copy commit: {err}")));
    }
    let scope_head = load_scope_commit(session.repo().as_ref(), machine_scope)?;
    let mut tx = session.repo().start_transaction();
    let wc_commit = tx
        .repo_mut()
        .new_commit(vec![scope_head.id().clone()], scope_head.tree())
        .set_description(WC_COMMIT_DESCRIPTION)
        .set_author(machine_signature(machine_scope))
        .write()
        .await
        .map_err(|err| jj_error(format!("create the working copy commit: {err}")))?;
    tx.repo_mut()
        .set_wc_commit(workspace.clone(), wc_commit.id().clone())
        .map_err(|err| jj_error(format!("point the working copy at its commit: {err}")))?;
    let repo = tx
        .commit("dotsync: create working copy")
        .await
        .map_err(|err| jj_error(format!("commit working copy creation: {err}")))?;
    session.advance_to(repo).await?;
    Ok(wc_commit)
}

/// Deletes the machine-local state file the release before this one kept.
///
/// Which commit this machine last materialized is a `wc_commit_ids` entry in
/// jj's own view, and it moves in the same operation as the bookmark it belongs
/// to. A file beside the repo saying the same thing is a second authority that
/// can disagree with the first, so an upgrading machine leaves it behind.
///
/// Failure is not worth reporting: the file is read by nothing, so the only
/// consequence of it surviving is that it is still there.
fn shed_the_previous_releases_state(paths: &DotsyncPaths) {
    let _ = std::fs::remove_file(paths.home_dir.join(SHED_SYNC_STATE_RELATIVE_PATH));
}

/// Three labeled trees into jj's merge: one base (the mark), two sides (home
/// and the head). The labels are what conflict presentation shows.
async fn merge_trees(
    ours: (MergedTree, &str),
    base: (MergedTree, &str),
    theirs: (MergedTree, &str),
) -> Result<MergedTree, DotsyncError> {
    let merge = Merge::from_removes_adds(
        [(base.0, base.1.to_string())],
        [
            (ours.0, ours.1.to_string()),
            (theirs.0, theirs.1.to_string()),
        ],
    );
    MergedTree::merge(merge)
        .await
        .map_err(|err| jj_error(format!("merge trees: {err}")))
}

/// A home-relative path as jj names it. Home-relative because that is how
/// dotsync records a path and how every machine on the scope reads it back.
pub(crate) fn repo_path_of(
    relative: &std::path::Path,
) -> Result<jj_lib::repo_path::RepoPathBuf, DotsyncError> {
    let relative_str = relative.to_str().ok_or_else(|| DotsyncError::NonUtf8Path {
        path: relative.to_path_buf(),
    })?;
    jj_lib::repo_path::RepoPathBuf::from_internal_string(relative_str)
        .map_err(|err| jj_error(format!("invalid repo path {}: {err}", relative.display())))
}

fn tree_paths(tree: &MergedTree) -> Result<Vec<jj_lib::repo_path::RepoPathBuf>, DotsyncError> {
    let mut paths = Vec::new();
    for (path, value) in tree.entries() {
        value.map_err(|err| jj_error(format!("read tree entry {path:?}: {err}")))?;
        paths.push(path);
    }
    Ok(paths)
}

fn state_exists(state_path: &std::path::Path) -> bool {
    state_path.join("dotsync-home.json").exists()
}
