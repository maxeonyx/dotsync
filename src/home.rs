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
//! The materialization rule (spike.ignore/README.md, settled 2026-08-19) is
//! rule 3: `merge(snapshot, mark, head)` in memory; if it resolves, home
//! moves whole and non-conflicting local edits are carried across; if it
//! conflicts, home is not touched and the conflict is presented from the
//! in-memory merge. Partial materialization is unrepresentable here because
//! the only write path is `check_out` of one resolved tree.
//!
//! The wc commit is always a resolved snapshot of home: snapshot amends it
//! (parent unchanged), sync switches it (new parent, merged tree). It never
//! holds a conflict — jj's own convention allows that only because jj
//! materializes conflict markers and re-parses them, which the no-markers
//! rule forbids. The one state that can break "the wc commit describes home"
//! is a run dying between committing a sync and materializing it; `acquire`
//! repairs exactly that, and the repair is the subtlest code in this module —
//! its comments carry the reasoning.

#![allow(dead_code)]

use jj_lib::backend::{CommitId, Signature, Timestamp};
use jj_lib::commit::Commit;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::working_copy::{
    CheckoutStats, LockedWorkingCopy as _, SnapshotOptions, WorkingCopyFreshness,
};

use crate::config::DotsyncPaths;
use crate::error::{jj_error, DotsyncError};
use crate::machine::detect_machine;
use crate::repo::{load_repo_direct, load_scope_commit};
use crate::session::Session;
use crate::working_copy::{HomeLockedWorkingCopy, HomeWorkingCopy};

/// The run's handle on home: the locked working copy plus the wc commit, kept
/// in step with each other by construction — every method that moves one
/// moves the other.
pub(crate) struct Home {
    locked: HomeLockedWorkingCopy,
    wc_commit: Commit,
    machine_scope: String,
}

/// What materializing a head did. There is no partial case to represent.
pub(crate) enum Materialized {
    /// Home already matched: the head is the mark and nothing had changed.
    AlreadyThere,
    /// The merge resolved; home moved whole, local edits carried across.
    Applied { stats: CheckoutStats },
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
        session.advance_to(load_repo_direct(paths).await?).await?;

        let wc_commit = ensure_wc_commit(session, &machine_scope, &workspace).await?;
        shed_sync_state_file(session, paths);

        let freshness = WorkingCopyFreshness::check_stale(&locked, &wc_commit, session.repo())
            .map_err(|err| jj_error(format!("check working copy freshness: {err}")))?;

        let mut home = Self {
            locked,
            wc_commit,
            machine_scope,
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

    pub(crate) fn wc_commit(&self) -> &Commit {
        &self.wc_commit
    }

    /// Moves home to `head`: rule 3. `merge(snapshot, mark, head)` in
    /// memory; resolved materializes whole (a new wc commit on `head`, local
    /// edits carried), conflicted touches nothing and hands back the merge.
    pub(crate) async fn materialize(
        &mut self,
        session: &mut Session,
        head: &Commit,
        head_label: &str,
    ) -> Result<Materialized, DotsyncError> {
        let mark = self.mark().await?;
        if head.id() == mark.id() {
            return Ok(Materialized::AlreadyThere);
        }
        let merged = merge_trees(
            (self.snapshot_tree(), "local changes in home"),
            (mark.tree(), "what this machine last synced"),
            (head.tree(), head_label),
        )
        .await?;
        if merged.has_conflict() {
            return Ok(Materialized::Conflicted { merged });
        }
        self.switch_to(session, head.id().clone(), merged).await?;
        let stats = self
            .locked
            .check_out(&self.wc_commit)
            .await
            .map_err(|err| jj_error(format!("materialize into home: {err}")))?;
        Ok(Materialized::Applied { stats })
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
            self.locked
                .check_out(&self.wc_commit)
                .await
                .map_err(|err| jj_error(format!("materialize into home: {err}")))?;
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

/// Commits made by a machine carry the machine's name, so history can say
/// which machine made a change — the `author: ""` oversight PLAN §2.3 step 2
/// retires.
pub(crate) fn machine_signature(machine_scope: &str) -> Signature {
    Signature {
        name: machine_scope.to_string(),
        email: format!("{machine_scope}@dotsync"),
        timestamp: Timestamp::now(),
    }
}

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

/// The sync-state.json era ends the first time a step-2 binary runs: the
/// file's two facts (machine scope, last synced revision) live in the view's
/// wc commit now, so the file is deleted wherever the config said it lived.
/// Best-effort — a missing file is the normal case forever after.
fn shed_sync_state_file(session: &Session, paths: &DotsyncPaths) {
    let state_file = paths
        .home_dir
        .join(&session.config().sync_state_relative_path);
    let _ = std::fs::remove_file(state_file);
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
