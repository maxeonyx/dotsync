//! `dotsync commit`: what a commit records, and where it puts it.
//!
//! The changes a commit *can* record are `diff(mark, snapshot)` — what home
//! holds against the commit home derives from — and which of those it does
//! record is `selection`'s question. This module answers the other two: which
//! tree those entries are written onto, and what the run then says it did.

use std::path::PathBuf;

use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;

use crate::converge;
use crate::error::{DotsyncError, SkippedCommitPath};
use crate::home::{repo_path_of, Home};
use crate::machine::machine_signature;
use crate::paths::DotsyncPaths;
use crate::pause::{
    conflicted_paths_of, converge_or_pause, present, publish_or_pause, reject_commit_if_paused,
    save_paused_run, PausedCommit, PausedRun,
};
use crate::repo::{scope_head_commit, PushReport};
use crate::selection::{load_scope_entries, select_changes_to_record, Selection};
use crate::session::{in_session, Run, Session};
use crate::sync::{finishing, SyncReport};

#[derive(Debug, Clone)]
pub struct CommitOptions {
    pub scope: String,
    pub message: String,
    /// Home wins for the paths this commit names, whatever the classification
    /// says about them — and for nothing else. Selection and
    /// authority ride the same argument list on purpose: naming a path says
    /// "include this", and forcing says "and home is right about it", so a
    /// forced commit cannot reach a file it never mentioned.
    pub force: bool,
    /// Empty means every managed file this machine has changed, which is the
    /// same set `dotsync status` reports as changes.
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct CommitReport {
    pub committed_scope: String,
    /// This machine's own scope. Known before the commit decides whether it has
    /// anything to record, so both outcomes can name it — the empty string a
    /// no-op commit used to report came from standing in a default sync report
    /// for the sync it never ran.
    pub machine_scope: String,
    /// Paths a named directory matched that this commit left alone. Empty for
    /// every other shape of commit: a bare commit selects what changed rather
    /// than filtering a list, and a path named exactly is refused out loud.
    pub skipped: Vec<SkippedCommitPath>,
    pub push: PushReport,
    /// What the commit recorded, or `None` when it found nothing to record.
    ///
    /// A commit with nothing to record writes no history, so it also runs no
    /// cascade and no home sync — and therefore has no synced files, no newly
    /// tracked files and no forced overwrites, rather than empty lists of them.
    pub recorded: Option<RecordedCommit>,
}

/// The half of a commit report that only exists when the commit recorded
/// something.
#[derive(Debug, Clone)]
pub struct RecordedCommit {
    /// Paths this commit put on the scope for the first time. Every machine
    /// sharing that scope will have them written into its home directory, so a
    /// run that adds files says which ones rather than reading like a run that
    /// changed a line.
    pub newly_tracked: Vec<PathBuf>,
    /// Paths recorded on the authority of `--force` rather than on the
    /// authority of a change made on this machine. Reported because a forced
    /// commit is the one shape of commit that can discard someone else's work,
    /// and a run that does that has to say so.
    pub forced_overwrites: Vec<PathBuf>,
    pub sync: SyncReport,
}

impl CommitReport {
    /// A commit that found nothing to add. It creates no history of its own,
    /// but it still names the scope it targeted and reports what it published
    /// on behalf of earlier runs.
    fn nothing_to_commit(
        scope: &str,
        machine_scope: &str,
        skipped: Vec<SkippedCommitPath>,
        push: PushReport,
    ) -> Self {
        Self {
            committed_scope: scope.to_string(),
            machine_scope: machine_scope.to_string(),
            skipped,
            push,
            recorded: None,
        }
    }
}

/// A commit that stopped part-way, and what it had already done when it did.
///
/// A forced overwrite is finished the moment the history carrying it is
/// written and pushed: nothing later in the run can take it back. Returning a
/// bare error would drop that fact exactly when it matters most — a run that
/// reverted another machine's change and then failed.
#[derive(Debug)]
pub struct CommitFailure {
    pub forced_overwrites: Vec<PathBuf>,
    /// Boxed because this struct rides in `Result::Err` through the whole
    /// commit path, and `DotsyncError` is a large enum — an unboxed copy of it
    /// here makes every `Result` on that path carry the worst-case variant.
    pub error: Box<DotsyncError>,
}

impl From<DotsyncError> for CommitFailure {
    /// Everything that can go wrong before any history exists.
    fn from(error: DotsyncError) -> Self {
        Self {
            forced_overwrites: Vec::new(),
            error: Box::new(error),
        }
    }
}

pub async fn commit_and_sync(
    paths: &DotsyncPaths,
    options: CommitOptions,
) -> Run<Result<CommitReport, CommitFailure>> {
    in_session(paths, async |session, paths| {
        reject_commit_if_paused(session, session.machine_scope()).await?;
        let mut home = Home::acquire(session, paths).await?;
        let outcome = commit_in_session(session, &mut home, options).await;
        finishing(home, session, outcome).await
    })
    .await
}

async fn commit_in_session(
    session: &mut Session,
    home: &mut Home,
    options: CommitOptions,
) -> Result<CommitReport, CommitFailure> {
    // Converge before looking at this commit at all: DESIGN's "commit is
    // converge, add the new commit, converge again". Building a commit on a
    // head that another machine has moved is how a change comes to be recorded
    // against a version of the scope that no longer exists.
    session.fetch().await?;
    let checkpoint = converge::checkpoint(session.repo().as_ref(), session.graph());
    converge_or_pause(session, home, &checkpoint).await?;
    // Publish what earlier runs left behind: this commit may turn out to add
    // nothing, and a machine with an interrupted push behind it must still
    // heal. Anything this run goes on to create is published by the push after
    // the second pass.
    let pending_push = publish_or_pause(session, home, &checkpoint).await?;
    let graph = session.graph().clone();

    if !graph.contains(&options.scope) {
        return Err(DotsyncError::InvalidScope {
            scope: options.scope.clone(),
        }
        .into());
    }

    let machine_scope = home.machine_scope().to_string();
    let target_entries = load_scope_entries(session.repo().as_ref(), &options.scope)?;

    let selection =
        select_changes_to_record(session, home, &machine_scope, &options, &target_entries).await?;
    let Selection {
        paths: selected_paths,
        newly_tracked,
        skipped,
        forced_paths,
        forced_overwrites,
    } = selection;

    if selected_paths.is_empty() {
        return Ok(CommitReport::nothing_to_commit(
            &options.scope,
            &machine_scope,
            skipped,
            pending_push,
        ));
    }

    // The commit home derives from, which is what makes a home edit an edit
    // *of something* rather than a bare assertion about bytes.
    let mark = home.mark().await?;

    // Whether this commit's bytes reach this machine at all. When the target
    // scope is an ancestor of the machine scope the cascade carries them down
    // into home, so home has a version of the target scope it started from —
    // the mark descends from it. When it is not — another machine's leaf, a
    // sibling branch of the DAG — that is not true, and the merge base below
    // falls back to the scope's own head. Whether such commits should be
    // allowed without an explicit per-path force is still open (PLAN.md §1.5,
    // D6).
    let cascades_into_home = graph
        .ancestors_and_self(&machine_scope)
        .iter()
        .any(|scope| scope.name == options.scope);

    let repo = session.repo().clone();
    let mut tx = repo.start_transaction();
    let base_commit = scope_head_commit(tx.repo_mut().base_repo().as_ref(), &options.scope)?;

    let merge_base_tree = commit_merge_base_tree(
        tx.repo_mut(),
        cascades_into_home,
        &options.scope,
        &base_commit,
        &mark,
    )
    .await?;
    let mut builder = MergedTreeBuilder::new(merge_base_tree.clone());
    for relative in &selected_paths {
        builder.set_or_remove(repo_path_of(relative)?, home.entry(relative)?);
    }
    let home_tree = builder.write_tree().await.map_err(|err| DotsyncError::Jj {
        message: format!("write commit tree for {}: {err}", options.scope),
    })?;

    // Three sides: what the target scope held when this machine last synced,
    // what it holds now, and that same base with this commit's home bytes laid
    // over it. When the scope has not moved the first two are equal and this
    // is a plain assignment; when another machine has moved it, this is the
    // merge that keeps their change instead of overwriting it.
    let merged_tree = MergedTree::merge(Merge::from_removes_adds(
        [(
            merge_base_tree,
            "the state this machine last synced".to_string(),
        )],
        [
            (base_commit.tree(), format!("scope `{}`", options.scope)),
            (home_tree, "your home edit".to_string()),
        ],
    ))
    .await
    .map_err(|err| DotsyncError::Jj {
        message: format!("merge home edit into {}: {err}", options.scope),
    })?;

    let mut builder = MergedTreeBuilder::new(merged_tree);
    for relative in &forced_paths {
        builder.set_or_remove(repo_path_of(relative)?, home.entry(relative)?);
    }
    let new_tree = builder.write_tree().await.map_err(|err| DotsyncError::Jj {
        message: format!("write forced commit tree for {}: {err}", options.scope),
    })?;

    if new_tree.has_conflict() {
        let conflicted_paths = conflicted_paths_of(&new_tree, &options.scope)?;
        // Nothing was written, so there is no transaction to keep: the pause
        // resolves against the scope head that is already there.
        drop(tx);
        save_paused_run(
            session.paths(),
            &PausedRun {
                checkpoint: checkpoint.clone(),
                paused_commit: Some(PausedCommit {
                    machine_scope: machine_scope.clone(),
                    scope: options.scope.clone(),
                    message: options.message.clone(),
                    parent_commit_id: base_commit.id().hex(),
                    conflicted_paths: conflicted_paths.clone(),
                }),
            },
        )?;
        // The same stop a convergence pause builds. A commit can only target
        // this machine's own scope or one above it, so the borrowing half of
        // it never applies — which falls out of the ancestry test rather than
        // being asserted here.
        return Err(present(session, &machine_scope, &new_tree, &options.scope)
            .await?
            .into());
    }

    if new_tree.tree_ids() == base_commit.tree().tree_ids() {
        return Ok(CommitReport::nothing_to_commit(
            &options.scope,
            &machine_scope,
            skipped,
            pending_push,
        ));
    }

    let new_commit = tx
        .repo_mut()
        .new_commit(vec![base_commit.id().clone()], new_tree)
        .set_description(&options.message)
        .set_author(machine_signature(&machine_scope))
        .write()
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!("write commit for {}: {err}", options.scope),
        })?;
    tx.repo_mut().set_local_bookmark_target(
        RefNameBuf::from(options.scope.as_str()).as_ref(),
        RefTarget::normal(new_commit.id().clone()),
    );

    session
        .advance_to(
            tx.commit("dotsync: commit scoped change")
                .await
                .map_err(|err| DotsyncError::Jj {
                    message: format!("commit scoped change for {}: {err}", options.scope),
                })?,
        )
        .await?;

    // From here the history exists, so every exit has to carry what it
    // overwrote — the forced overwrites are in it and nothing later can take
    // them back.
    let stopped = |error: DotsyncError| CommitFailure {
        forced_overwrites: forced_overwrites.clone(),
        error: Box::new(error),
    };
    // The second pass: this commit moved one scope, and every scope below it
    // now merges a parent that moved. It is the same operation the run opened
    // with, which is why there is no cascade to plan.
    converge_or_pause(session, home, &checkpoint)
        .await
        .map_err(stopped)?;
    // Push as soon as the history exists: the home sync below can legitimately
    // stop on a conflict, and a stop must never strand committed scope
    // history.
    let push = publish_or_pause(session, home, &checkpoint)
        .await
        .map_err(stopped)?;
    // The same home sync every other command ends with. It needs nothing said
    // about the paths this commit just recorded: they reached the scope from
    // home, so the merge that moves home onto the new head finds home's side
    // and the head's side agreeing, and a local change the commit did not name
    // is carried across rather than stopped on.
    let sync =
        crate::sync::sync_home_to_machine_scope(session, home, crate::sync::LocalChanges::Carry)
            .await
            .map_err(stopped)?;

    Ok(CommitReport {
        committed_scope: options.scope,
        machine_scope,
        skipped,
        push,
        recorded: Some(RecordedCommit {
            newly_tracked,
            forced_overwrites,
            sync,
        }),
    })
}

/// The tree a home edit is a change *against*: the target scope as it stood
/// when this machine last materialized it.
///
/// Home derives from the mark, and the mark descends from the target scope's
/// head as it was at that moment — so the common ancestor of the target
/// scope's head now and the mark is exactly the version of the target scope
/// home was derived from. When nobody else has moved the scope, that ancestor
/// *is* the current head and the merge below degenerates into the plain
/// assignment dotsync has always done. When somebody has, it is what turns
/// their change into a three-way merge instead of a silent overwrite.
///
/// Falls back to the scope's own head when the commit does not cascade into
/// this home: home was never derived from such a scope, so there is no version
/// of it this machine can claim to have started from.
async fn commit_merge_base_tree(
    mut_repo: &mut jj_lib::repo::MutableRepo,
    cascades_into_home: bool,
    target_scope: &str,
    target_head: &jj_lib::commit::Commit,
    mark: &jj_lib::commit::Commit,
) -> Result<jj_lib::merged_tree::MergedTree, DotsyncError> {
    if !cascades_into_home {
        return Ok(target_head.tree());
    }

    let base_ids = mut_repo
        .index()
        .common_ancestors(&[target_head.id().clone()], &[mark.id().clone()])
        .map_err(|err| DotsyncError::Jj {
            message: format!("find the base of the home edit for {target_scope}: {err}"),
        })?;
    if base_ids.is_empty() {
        return Ok(target_head.tree());
    }

    let base_commits = base_ids
        .iter()
        .map(|id| {
            mut_repo
                .store()
                .get_commit(id)
                .map_err(|err| DotsyncError::Jj {
                    message: format!("load the base of the home edit for {target_scope}: {err}"),
                })
        })
        .collect::<Result<Vec<_>, DotsyncError>>()?;
    merge_commit_trees(mut_repo, &base_commits)
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!("merge the bases of the home edit for {target_scope}: {err}"),
        })
}
