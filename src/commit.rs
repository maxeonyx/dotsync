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
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;

use crate::converge::{self, Pin, Pins};
use crate::error::{DotsyncError, SkippedCommitPath};
use crate::home::{repo_path_of, Home};
use crate::paths::DotsyncPaths;
use crate::pause::{
    conflicted_files, conflicted_paths_of, converge_or_pause, present, publish_or_pause,
    reject_commit_if_paused, save_paused_run, PausedCommit, PausedRun,
};
use crate::place::{carry_out, plan, CarriedOut, Plan, Planned};
use crate::repo::{scope_head_commit, PushReport};
use crate::selection::{load_scope_entries, select_changes_to_record, Selection};
use crate::session::{in_session, Run, Session};
use crate::status::PausedCascade;
use crate::sync::{finishing, SyncReport};

#[derive(Debug, Clone)]
pub struct CommitOptions {
    pub scope: String,
    pub message: String,
    /// Empty means every managed file this machine has changed, which is the
    /// same set `dotsync status` reports as changes.
    pub paths: Vec<PathBuf>,
    /// Say what the commit would do, and do none of it.
    pub dry_run: bool,
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
    /// What this run published. `None` for a dry run, which publishes nothing.
    pub push: Option<PushReport>,
    /// What the commit recorded, or `None` when it found nothing to record.
    ///
    /// A commit with nothing to record writes no history, so it also runs no
    /// cascade and no home sync — and therefore has no synced files and no
    /// newly tracked files, rather than empty lists of them.
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
    /// What the commit changes on every machine, and — for a dry run — the
    /// merge it would stop at.
    pub planned: Planned,
    /// The home sync that followed. `None` for a dry run.
    pub sync: Option<SyncReport>,
}

impl CommitReport {
    /// A commit that found nothing to add. It creates no history of its own,
    /// but it still names the scope it targeted and reports what it published
    /// on behalf of earlier runs.
    fn nothing_to_commit(
        scope: &str,
        machine_scope: &str,
        skipped: Vec<SkippedCommitPath>,
        push: Option<PushReport>,
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

pub async fn commit_and_sync(
    paths: &DotsyncPaths,
    options: CommitOptions,
) -> Run<Result<CommitReport, DotsyncError>> {
    in_session(paths, async |session, paths| {
        // Before the fetch, because it is a fact about the command line and
        // nothing about the repo can change the answer.
        if options.message.trim().is_empty() {
            return Err(DotsyncError::EmptyCommitMessage {
                scope: options.scope.clone(),
            });
        }
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
) -> Result<CommitReport, DotsyncError> {
    // Converge before looking at this commit at all: DESIGN's "commit is
    // converge, add the new commit, converge again". Building a commit on a
    // head that another machine has moved is how a change comes to be recorded
    // against a version of the scope that no longer exists. A dry run records
    // none of it, so it converges inside the transaction its plan runs in.
    session.fetch().await?;
    let checkpoint = converge::checkpoint(session.repo().as_ref(), session.graph());
    // Publish what earlier runs left behind: this commit may turn out to add
    // nothing, and a machine with an interrupted push behind it must still
    // heal. Anything this run goes on to create is published by the push after
    // the second pass.
    let pending_push = match options.dry_run {
        true => None,
        false => {
            converge_or_pause(session, home, &checkpoint).await?;
            Some(publish_or_pause(session, home, &checkpoint).await?)
        }
    };
    let graph = session.graph().clone();

    if !graph.contains(&options.scope) {
        return Err(DotsyncError::InvalidScope {
            scope: options.scope.clone(),
        });
    }

    let machine_scope = home.machine_scope().to_string();
    let target_entries = load_scope_entries(session.repo().as_ref(), &options.scope)?;

    let selection =
        select_changes_to_record(session, home, &machine_scope, &options, &target_entries).await?;
    let Selection {
        paths: selected_paths,
        newly_tracked,
        skipped,
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

    // The target has to be a scope this machine holds (Max, 2026-08-13). What
    // a commit records is home, and home was built from this machine's own
    // scopes — so for anything else there is no version of the target this
    // machine can claim to have started from, and what it wrote there would
    // overwrite rather than build on whatever that machine has. Refusing it
    // is also what makes the merge base below unconditional.
    if !graph
        .ancestors_and_self(&machine_scope)
        .iter()
        .any(|scope| scope.name == options.scope)
    {
        return Err(DotsyncError::CommitOutsideAncestry {
            shared_ancestor: graph
                .nearest_shared_ancestor(&machine_scope, &options.scope)
                .map(str::to_string),
            scope: options.scope.clone(),
            machine_scope,
        });
    }

    let repo = session.repo().clone();
    let mut tx = repo.start_transaction();
    let mut converged = false;
    if options.dry_run {
        let (moved, stopped) = converge::pass(&mut tx, &graph, &machine_scope, None, None).await?;
        if let Some(pause) = stopped {
            return Err(present(session, &machine_scope, &pause.merged, &pause.scope).await?);
        }
        converged = moved;
    }
    let base_commit = scope_head_commit(tx.repo(), &options.scope)?;

    let target_base =
        commit_merge_base_tree(tx.repo_mut(), &options.scope, &base_commit, &mark).await?;
    // Where the target holds a file, the edit is from what home was derived
    // from — this machine's version — to what home holds. Where no scope
    // between the target and this machine holds its own version, that is the
    // target's version as this machine last synced it. Where one does, the
    // edit is still only the edit: the part only true below the target stays
    // there, and an edit inside that part is not an edit of anything the
    // target holds, so it conflicts rather than publishes. Where the target
    // holds nothing, there is nothing of its own to keep, and it gains the
    // file as home holds it.
    let mark_tree = mark.tree();
    let mut base_builder = MergedTreeBuilder::new(target_base.clone());
    let mut builder = MergedTreeBuilder::new(target_base.clone());
    for relative in &selected_paths {
        let path = repo_path_of(relative)?;
        let read = |tree: &MergedTree, what: &str| {
            tree.path_value(path.as_ref())
                .map_err(|err| DotsyncError::Jj {
                    message: format!("read {} as {what}: {err}", relative.display()),
                })
        };
        if !read(&target_base, "the target last held it")?.is_absent() {
            base_builder.set_or_remove(
                path.clone(),
                read(&mark_tree, "this machine last synced it")?,
            );
        }
        builder.set_or_remove(path, home.entry(relative)?);
    }
    let merge_base_tree = base_builder
        .write_tree()
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!(
                "write the base of the home edit for {}: {err}",
                options.scope
            ),
        })?;
    let home_tree = builder.write_tree().await.map_err(|err| DotsyncError::Jj {
        message: format!("write commit tree for {}: {err}", options.scope),
    })?;

    // Three sides: what the target scope held when this machine last synced,
    // what it holds now, and that same base with this commit's home bytes laid
    // over it. When the scope has not moved the first two are equal and this
    // is a plain assignment; when another machine has moved it, this is the
    // merge that keeps their change instead of overwriting it.
    let new_tree = MergedTree::merge(Merge::from_removes_adds(
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

    if new_tree.has_conflict() {
        if options.dry_run {
            return Ok(CommitReport {
                committed_scope: options.scope.clone(),
                machine_scope,
                skipped,
                push: None,
                recorded: Some(RecordedCommit {
                    newly_tracked,
                    planned: Planned {
                        effect: Vec::new(),
                        stops_at: Some(PausedCascade {
                            conflicts: conflicted_files(session, &new_tree, &options.scope).await?,
                            scope: options.scope.clone(),
                        }),
                    },
                    sync: None,
                }),
            });
        }
        let conflicted_paths = conflicted_paths_of(&new_tree, &options.scope)?;
        // Nothing was written, so there is no transaction to keep: the pause
        // resolves against the scope head that is already there.
        drop(tx);
        save_paused_run(
            session.paths(),
            &PausedRun {
                checkpoint: checkpoint.clone(),
                paused_commit: Some(PausedCommit {
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
        return Err(present(session, &machine_scope, &new_tree, &options.scope).await?);
    }

    if new_tree.tree_ids() == base_commit.tree().tree_ids() {
        return Ok(CommitReport::nothing_to_commit(
            &options.scope,
            &machine_scope,
            skipped,
            pending_push,
        ));
    }

    // The commit is a pin on the target scope, laid down by the same pass that
    // then carries it through every scope below — so the cascade is not a
    // second step, and what the run reports it changed is what the pass did.
    let mut pinned = Vec::new();
    for relative in &selected_paths {
        let path = repo_path_of(relative)?;
        let value = new_tree.path_value(&path).map_err(|err| DotsyncError::Jj {
            message: format!("read {} from the commit tree: {err}", relative.display()),
        })?;
        pinned.push((path, Pin::Holds(value)));
    }
    let pins = Pins {
        by_scope: [(options.scope.clone(), pinned)].into_iter().collect(),
        description: options.message.clone(),
    };
    let planned = plan(session, home, &mut tx, &pins).await?;

    if options.dry_run {
        drop(tx);
        return Ok(CommitReport {
            committed_scope: options.scope,
            machine_scope,
            skipped,
            push: None,
            recorded: Some(RecordedCommit {
                newly_tracked,
                planned: planned.report(session).await?,
                sync: None,
            }),
        });
    }

    let Plan {
        moved,
        effect,
        stop,
    } = planned;
    let CarriedOut { push, sync } = carry_out(
        session,
        home,
        &checkpoint,
        tx,
        converged || moved,
        stop,
        "dotsync: commit scoped change",
    )
    .await?;

    Ok(CommitReport {
        committed_scope: options.scope,
        machine_scope,
        skipped,
        push: Some(push),
        recorded: Some(RecordedCommit {
            newly_tracked,
            planned: Planned {
                effect,
                stops_at: None,
            },
            sync: Some(sync),
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
    target_scope: &str,
    target_head: &jj_lib::commit::Commit,
    mark: &jj_lib::commit::Commit,
) -> Result<jj_lib::merged_tree::MergedTree, DotsyncError> {
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
