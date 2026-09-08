//! The convergence pass: one operation over the whole scope graph.
//!
//! DESIGN, "The convergence model": for each scope in topological order, the
//! new head is the merge of {local head, remote head, updated parent-scope
//! heads}. That single rule is all four of the things dotsync used to do with
//! separate machinery:
//!
//! - the remote moved and this machine did not — the merge is the remote's
//!   head, so the bookmark moves onto it and no commit is written;
//! - this machine moved and the remote did not — the merge is the local head,
//!   so nothing happens and the push publishes it;
//! - both moved — a real merge commit, which is what a contested head is
//!   waiting for;
//! - a parent scope moved — the cascade, which is the same merge with the
//!   parent's head as the other input.
//!
//! Which case a scope is in is not a question the pass asks. It builds the
//! input set, drops the inputs that are ancestors of other inputs, and looks
//! at how many are left: one means there is nothing to merge, more than one
//! means a merge commit. So there is no branch to get wrong, and "skip
//! no-ops" is the same fact rather than a rule applied on top — a run with
//! nothing to converge leaves every bookmark where it found it and writes no
//! operation at all.
//!
//! Every state a machine can be in is an input to the next pass, which is why
//! there is no recovery path: a run killed mid-cascade, a push that was
//! refused, a scope another machine published without cascading it, and an
//! edit made offline all converge by being converged.

use std::collections::BTreeMap;

use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::{RefNameBuf, RemoteRefSymbol};
use jj_lib::repo::{MutableRepo, Repo};
use jj_lib::rewrite::merge_commit_trees;

use crate::error::{jj_error, DotsyncError};
use crate::machine::machine_signature;
use crate::repo::{push_scope_updates, scope_head, PushReport, Rejection, ORIGIN};
use crate::scope_graph::ScopeGraph;
use crate::session::Session;

/// How far the pass got.
pub(crate) enum Converged {
    /// Every scope's head is now the merge of everything that reaches it.
    Completed,
    /// One scope's merge held a file two sides changed differently, which only
    /// the person at this machine can decide. Everything above it in the
    /// graph converged; that scope and everything below it did not.
    Paused(Pause),
}

/// A merge that stopped, and everything the run that presents it and the run
/// that finishes it need.
pub(crate) struct Pause {
    pub(crate) scope: String,
    /// The commits the merge was of. `continue` merges exactly these again and
    /// lays home's bytes over the paths that conflicted.
    pub(crate) parents: Vec<CommitId>,
    pub(crate) description: String,
    /// The conflicted merge itself, which holds the base and both sides of
    /// every file that did not resolve. Nothing is stored: the stop reads the
    /// versions out of this to present them, and a rerun recomputes it.
    pub(crate) merged: MergedTree,
}

/// Where every scope stood before this run wrote anything — what `abort`
/// returns to.
///
/// Single-commit heads only. A contested head is two positions and neither is
/// the one to go back to; a scope with no head has nothing to restore.
pub(crate) fn checkpoint(repo: &dyn Repo, graph: &ScopeGraph) -> BTreeMap<String, String> {
    graph
        .names()
        .filter_map(|scope| {
            let head = scope_head(repo, scope).as_normal()?;
            Some((scope.to_string(), head.hex()))
        })
        .collect()
}

/// One thing a scope's new head has to account for, and what to call it when
/// it collides with another.
struct Input {
    commit: Commit,
    label: String,
}

/// Converges every scope, in one transaction.
///
/// `machine_scope` is the author every merge this run writes carries: a
/// convergence commit is history this machine made, and it ends up being the
/// commit every other machine's bookmark points at.
pub(crate) async fn converge(
    session: &mut Session,
    machine_scope: &str,
) -> Result<Converged, DotsyncError> {
    let graph = session.graph().clone();
    let repo = session.repo().clone();
    let mut tx = repo.start_transaction();
    let mut moved = false;
    let mut paused = None;

    for scope in graph.in_cascade_order() {
        let inputs = convergence_inputs(tx.repo_mut(), &graph, &scope.name)?;
        let Some((first, rest)) = inputs.split_first() else {
            // No head at all. Unreachable for a scope the graph names, since
            // the graph is derived from the bookmarks that exist — and a skip
            // rather than a stop because a scope dotsync cannot see is not a
            // scope it should be writing to.
            continue;
        };
        if rest.is_empty() {
            // One input left after dropping the ancestors: there is nothing to
            // merge, and the head is either already there or a fast-forward.
            if scope_head(tx.repo_mut(), &scope.name).as_normal() != Some(first.commit.id()) {
                set_head(tx.repo_mut(), &scope.name, first.commit.id().clone());
                moved = true;
            }
            continue;
        }

        let merged = merge_inputs(tx.repo_mut(), &inputs).await?;
        let description = format!("dotsync: converge {}", scope.name);
        if merged.has_conflict() {
            paused = Some(Pause {
                scope: scope.name.clone(),
                parents: inputs
                    .iter()
                    .map(|input| input.commit.id().clone())
                    .collect(),
                description,
                merged,
            });
            break;
        }

        let commit = tx
            .repo_mut()
            .new_commit(
                inputs
                    .iter()
                    .map(|input| input.commit.id().clone())
                    .collect(),
                merged,
            )
            .set_description(&description)
            .set_author(machine_signature(machine_scope))
            .write()
            .await
            .map_err(|err| jj_error(format!("write the merge for {}: {err}", scope.name)))?;
        set_head(tx.repo_mut(), &scope.name, commit.id().clone());
        moved = true;
    }

    match (moved, paused) {
        // Nothing to converge. The transaction is dropped rather than
        // committed, so a steady-state run writes no operation and leaves
        // every bookmark exactly where it found it.
        (false, None) => Ok(Converged::Completed),
        (_, paused) => {
            let description = match &paused {
                Some(pause) => format!("dotsync: converge, stopping at {}", pause.scope),
                None => "dotsync: converge".to_string(),
            };
            session
                .advance_to(
                    tx.commit(&description)
                        .await
                        .map_err(|err| jj_error(format!("commit the convergence: {err}")))?,
                )
                .await?;
            Ok(match paused {
                Some(pause) => Converged::Paused(pause),
                None => Converged::Completed,
            })
        }
    }
}

/// Everything a scope's new head has to account for: its own head, and its
/// parents' heads as this pass has just left them.
///
/// A head contributes each of its sides, so a contested one contributes two —
/// which is the whole of what "diverged" needs. The remote's position is not
/// read separately: jj's import already merged it into the local head using
/// the position the remote was last seen at, so a scope that is merely behind
/// arrives as one commit and only a genuine divergence arrives as two.
///
/// Inputs that are ancestors of other inputs are dropped, because merging a
/// commit with its own descendant is the descendant. That is what turns four
/// cases into one: whatever is left is what genuinely has to be reconciled.
fn convergence_inputs(
    repo: &dyn Repo,
    graph: &ScopeGraph,
    scope: &str,
) -> Result<Vec<Input>, DotsyncError> {
    let head = scope_head(repo, scope);
    let published = repo
        .view()
        .get_remote_bookmark(RemoteRefSymbol {
            name: RefNameBuf::from(scope).as_ref(),
            remote: ORIGIN.as_ref(),
        })
        .target
        .clone();

    let mut candidates: Vec<(CommitId, String)> = Vec::new();
    for id in head.added_ids() {
        let label = if head.has_conflict() {
            match published.added_ids().any(|side| side == id) {
                true => format!("`{scope}` as it was published"),
                false => format!("`{scope}` as this machine has it"),
            }
        } else {
            format!("scope `{scope}`")
        };
        candidates.push((id.clone(), label));
    }
    if let Some(scope) = graph.get(scope) {
        for parent in &scope.parents {
            for id in scope_head(repo, parent).added_ids() {
                candidates.push((id.clone(), format!("scope `{parent}`")));
            }
        }
    }
    candidates.dedup_by(|a, b| a.0 == b.0);

    let heads = repo
        .index()
        .heads(&mut candidates.iter().map(|(id, _)| id))
        .map_err(|err| jj_error(format!("reduce the inputs for {scope}: {err}")))?;

    candidates
        .into_iter()
        .filter(|(id, _)| heads.contains(id))
        .map(|(id, label)| {
            Ok(Input {
                commit: repo
                    .store()
                    .get_commit(&id)
                    .map_err(|err| jj_error(format!("load {} for {scope}: {err}", id.hex())))?,
                label,
            })
        })
        .collect()
}

/// The merge of every input, labeled so that a conflict can say which version
/// came from where.
///
/// Merged one input at a time against everything merged so far, with the base
/// being their common ancestors — which is what an n-way merge is. jj's own
/// `merge_commit_trees` would compute the same tree, and labels each side with
/// a change id and a commit id: dotsync's user interface does not have those
/// nouns in it, and a conflict nobody can read is the one thing this stop
/// cannot afford.
async fn merge_inputs(repo: &dyn Repo, inputs: &[Input]) -> Result<MergedTree, DotsyncError> {
    let mut merged = inputs[0].commit.tree();
    let mut label = inputs[0].label.clone();
    let mut merged_ids = vec![inputs[0].commit.id().clone()];

    for input in &inputs[1..] {
        let base_ids = repo
            .index()
            .common_ancestors(&merged_ids, &[input.commit.id().clone()])
            .map_err(|err| jj_error(format!("find what the two sides agreed on: {err}")))?;
        let mut base_commits = Vec::new();
        for id in &base_ids {
            base_commits.push(
                repo.store()
                    .get_commit(id)
                    .map_err(|err| jj_error(format!("load {}: {err}", id.hex())))?,
            );
        }
        let base = merge_commit_trees(repo, &base_commits)
            .await
            .map_err(|err| jj_error(format!("merge what the two sides agreed on: {err}")))?;

        merged = MergedTree::merge(Merge::from_removes_adds(
            [(base, "the version they last agreed on".to_string())],
            [(merged, label), (input.commit.tree(), input.label.clone())],
        ))
        .await
        .map_err(|err| jj_error(format!("merge {}: {err}", input.label)))?;
        label = "the merge so far".to_string();
        merged_ids.push(input.commit.id().clone());
    }

    Ok(merged)
}

fn set_head(mut_repo: &mut MutableRepo, scope: &str, commit: CommitId) {
    mut_repo.set_local_bookmark_target(RefNameBuf::from(scope).as_ref(), RefTarget::normal(commit));
}

/// How many times one run offers its scopes to a remote that keeps moving
/// under it.
///
/// A bound rather than a loop that runs until it wins: every attempt costs a
/// fetch and a round trip, and a machine that loses three races in a row is on
/// a remote busy enough that the scopes are better left for the next run —
/// which is an ordinary state, not a failure.
const PUSH_ATTEMPTS: usize = 3;

/// What publishing came to.
pub(crate) enum Published {
    Report(PushReport),
    /// Converging onto what the race winner published held a conflict.
    Paused(Pause),
}

/// Publishes this machine's scope commits, converging onto whatever arrived
/// while the run was working and trying again.
///
/// DESIGN, "The convergence model": "**Push is a loop, not a step.** A
/// rejected push isn't an error; it means another machine pushed first.
/// Fetch, converge, push again." The two ways a remote can say no are not the
/// same thing and only one of them is that race: a lease failure means the
/// remote moved, which the next attempt has an answer for, and a remote that
/// refuses the write itself will refuse it again however many times it is
/// asked. Retrying the second is a run that never returns.
pub(crate) async fn publish(
    session: &mut Session,
    machine_scope: &str,
) -> Result<Published, DotsyncError> {
    for attempt in 1..=PUSH_ATTEMPTS {
        let report = push_scope_updates(session).await?;
        let PushReport::Refused {
            rejection: Rejection::RemoteMoved,
            ..
        } = &report
        else {
            return Ok(Published::Report(report));
        };
        if attempt == PUSH_ATTEMPTS {
            return Ok(Published::Report(report));
        }
        session.fetch().await?;
        match converge(session, machine_scope).await? {
            Converged::Completed => {}
            Converged::Paused(pause) => return Ok(Published::Paused(pause)),
        }
    }
    unreachable!("the last attempt returns rather than looping")
}
