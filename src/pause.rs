//! A paused cascade, and the two commands that end one.
//!
//! A cascade pauses when merging a scope into its child conflicts. That is
//! recorded in a file beside the repo — the one piece of dotsync-invented state
//! left, and PLAN §2.3 step 6 derives it from the conflicted commits instead —
//! and `continue` and `abort` are the two ways out of it.
//!
//! `continue` also ends the *other* paused state, the conflict between home and
//! this machine's own scope head. That one stores nothing: it is recomputed from
//! home, the mark and the head every run, so the pause file is what tells the
//! two apart, and there being neither is what "nothing is paused" means.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use jj_lib::backend::CommitId;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;

use crate::cascade::{
    execute_cascade_steps, CascadeCommand, CascadeOutcome, CascadeStep, ScopeHeads,
};
use crate::config::DotsyncPaths;
use crate::drift::{changed_paths, FileState};
use crate::error::DotsyncError;
use crate::home::{repo_path_of, Home, Resolved};
use crate::machine::machine_signature;
use crate::repo::{
    collect_managed_tree_entries, load_scope_commit, push_scope_updates, read_entry_bytes,
    PushReport,
};
use crate::session::{in_session, Run, Session};
use crate::status::FileChange;
use crate::sync::{classify_home_against_head, finishing, SyncReport};

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct PausedCascadeState {
    pub(crate) machine_scope: String,
    pub(crate) paused_scope: String,
    pub(crate) parent_commit_ids: Vec<String>,
    pub(crate) remaining_steps: Vec<PausedCascadeStep>,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) original_scope_commit_ids: BTreeMap<String, String>,
    /// The conflicted files, and what each held in home when the cascade
    /// paused. `continue` resolves exactly these paths, and refuses when they
    /// have not changed — see `unresolved_conflicted_files`. Defaulted rather
    /// than required so that a pause file written before this field existed
    /// still loads and can still be aborted; `continue` refuses such a pause
    /// outright. Deleted with this whole file when conflicts become commits
    /// (PLAN item 3), like the `WithheldPausedCascade` publish guard.
    #[serde(default)]
    pub(crate) paused_home_contents: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct PausedCascadeStep {
    pub(crate) scope: String,
    pub(crate) parent_scopes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ContinueReport {
    /// Which of the two paused states this run finished. The two syncs are the
    /// same operation, but a run that says it resumed a cascade when it
    /// resolved a home conflict is describing something that did not happen.
    pub resumed: Resumed,
    pub sync: SyncReport,
    pub push: PushReport,
}

/// What `continue` found paused.
#[derive(Debug, Clone)]
pub enum Resumed {
    /// A cascade, stopped part-way down the graph at this scope.
    Cascade { scope: String },
    /// Home against this machine's own scope head. It has no name of its own
    /// because it is not stored anywhere: it is recomputed from home, the mark
    /// and the head on every run.
    SyncConflict,
}

#[derive(Debug, Clone)]
pub struct AbortReport {
    /// The scope the cascade was paused at. It is not the scope that was
    /// aborted — the cascade was — and it is not the scope the discarded commit
    /// was made on either, which is why it says which of the three it is.
    pub paused_scope: String,
    pub sync: SyncReport,
}

/// Home contents of the conflicted files, recorded when a cascade pauses so
/// `continue` can tell a resolution from an untouched file.
///
/// A conflict can be about a file this machine has never held, so home is
/// widened to cover these paths before they are read.
pub(crate) async fn home_contents(
    session: &mut Session,
    home: &mut Home,
    relatives: &[PathBuf],
) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>, DotsyncError> {
    let repo_paths = relatives
        .iter()
        .map(|relative| repo_path_of(relative))
        .collect::<Result<Vec<_>, DotsyncError>>()?;
    home.observe_paths(session, repo_paths).await?;
    let mut contents = BTreeMap::new();
    for relative in relatives {
        let value = home.entry(relative)?.as_resolved().cloned().flatten();
        contents.insert(
            relative.clone(),
            read_entry_bytes(session.repo().store(), relative, value.as_ref()).await?,
        );
    }
    Ok(contents)
}

/// Conflicted files that hold exactly what they held when the cascade paused.
///
/// Today's pause never materializes conflict markers into home, so DESIGN's
/// "`continue` verifies the markers are gone" is vacuously true and `continue`
/// takes home's untouched content as the resolution — silently deleting the
/// losing side and reporting success. Until conflicts become commits and the
/// two sides really are written into home (PLAN item 3), an unchanged file is
/// proof that no resolution was made. Deleted with the pause file.
async fn unresolved_conflicted_files(
    session: &mut Session,
    home: &mut Home,
    state: &PausedCascadeState,
) -> Result<Vec<PathBuf>, DotsyncError> {
    let paused: Vec<PathBuf> = state.paused_home_contents.keys().cloned().collect();
    let now = home_contents(session, home, &paused).await?;
    Ok(paused
        .into_iter()
        .filter(|relative| now.get(relative) == state.paused_home_contents.get(relative))
        .collect())
}

pub async fn continue_after_conflict(
    paths: &DotsyncPaths,
    discard_local: bool,
) -> Run<Result<ContinueReport, DotsyncError>> {
    in_session(paths, async |session, paths| {
        let mut home = Home::acquire(session, paths).await?;
        let outcome = continue_in_session(session, &mut home, discard_local).await;
        finishing(home, session, outcome).await
    })
    .await
}

/// `continue` is the agent saying "the resolution is written". There are two
/// states that sentence can end, and which one this machine is in is a fact
/// about the machine rather than something the command has to be told: a
/// cascade pause is recorded in a file, and a home-against-head conflict is
/// recomputed from home, the mark and the head every run. So a pause file
/// decides which of the two this is, and there being neither is what "nothing
/// is paused" means.
async fn continue_in_session(
    session: &mut Session,
    home: &mut Home,
    discard_local: bool,
) -> Result<ContinueReport, DotsyncError> {
    let state = match load_paused_cascade_state(session.paths()) {
        Ok(state) => state,
        Err(DotsyncError::NoPausedCascade) => return complete_a_sync_conflict(session, home).await,
        Err(error) => return Err(error),
    };
    // A cascade pauses because at least one file conflicted, so an empty
    // record means the pause was written before this check existed rather than
    // that nothing conflicted. Skipping the check there would reopen the
    // silent discard exactly when a machine upgrades mid-pause; `abort` reads
    // nothing this pause lacks, so refusing does not wedge it.
    if state.paused_home_contents.is_empty() {
        return Err(DotsyncError::PausePredatesResolutionCheck {
            scope: state.paused_scope,
        });
    }
    let unresolved = unresolved_conflicted_files(session, home, &state).await?;
    if !unresolved.is_empty() {
        return Err(DotsyncError::UnresolvedConflict {
            scope: state.paused_scope,
            paths: unresolved,
        });
    }
    let graph = session.config().graph.clone();
    let repo = session.repo().clone();
    let mut tx = repo.start_transaction();
    let mut scope_heads = ScopeHeads::load_existing(tx.repo_mut().base_repo(), &graph)?;
    let parent_commits = state
        .parent_commit_ids
        .iter()
        .map(|id| load_commit_by_hex(tx.repo_mut(), id))
        .collect::<Result<Vec<_>, DotsyncError>>()?;
    if parent_commits.is_empty() {
        return Err(DotsyncError::Jj {
            message: "paused cascade has no parent commits".to_string(),
        });
    }
    let merged_tree = merge_commit_trees(tx.repo_mut(), &parent_commits)
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!(
                "merge paused cascade parents for {}: {err}",
                state.paused_scope
            ),
        })?;
    let mut builder = MergedTreeBuilder::new(merged_tree);
    for relative in state.paused_home_contents.keys() {
        builder.set_or_remove(repo_path_of(relative)?, home.entry(relative)?);
    }
    let resolved_tree = builder.write_tree().await.map_err(|err| DotsyncError::Jj {
        message: format!("write resolved tree for {}: {err}", state.paused_scope),
    })?;
    let resolved_commit = tx
        .repo_mut()
        .new_commit(
            parent_commits
                .iter()
                .map(|commit| commit.id().clone())
                .collect(),
            resolved_tree,
        )
        .set_description(&state.description)
        .set_author(machine_signature(&state.machine_scope))
        .write()
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!(
                "write resolved cascade commit for {}: {err}",
                state.paused_scope
            ),
        })?;
    tx.repo_mut().set_local_bookmark_target(
        RefNameBuf::from(state.paused_scope.as_str()).as_ref(),
        RefTarget::normal(resolved_commit.id().clone()),
    );
    scope_heads.update(state.paused_scope.clone(), resolved_commit);

    let command = CascadeCommand {
        root_scope: state.paused_scope.clone(),
        description: state.description.clone(),
        author: machine_signature(&state.machine_scope),
    };
    let remaining_plan = state
        .remaining_steps
        .iter()
        .map(|step| CascadeStep {
            scope: step.scope.clone(),
            parent_scopes: step.parent_scopes.clone(),
        })
        .collect::<Vec<_>>();
    match execute_cascade_steps(tx.repo_mut(), &mut scope_heads, &remaining_plan, &command).await? {
        CascadeOutcome::Completed => {}
        CascadeOutcome::Paused {
            scope,
            conflicted_files,
        } => {
            let paused_step = remaining_plan
                .iter()
                .find(|step| step.scope == scope)
                .ok_or_else(|| DotsyncError::Jj {
                    message: format!("paused cascade step `{scope}` was not in remaining plan"),
                })?;
            let parent_commit_ids = parent_commit_ids_for_step(&scope_heads, paused_step)?;
            let remaining_steps = remaining_steps_after_pause(&remaining_plan, &scope);
            session
                .advance_to(
                    tx.commit("dotsync: pause cascade again")
                        .await
                        .map_err(|err| DotsyncError::Jj {
                            message: format!("commit repeated paused cascade state: {err}"),
                        })?,
                )
                .await?;
            let conflicted_paths = conflicted_files
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            let paused_home_contents = home_contents(session, home, &conflicted_paths).await?;
            save_paused_cascade_state(
                session.paths(),
                &PausedCascadeState {
                    machine_scope: state.machine_scope,
                    paused_scope: scope.clone(),
                    parent_commit_ids,
                    remaining_steps,
                    description: state.description,
                    original_scope_commit_ids: state.original_scope_commit_ids,
                    paused_home_contents,
                },
            )?;
            return Err(DotsyncError::CascadePaused {
                scope,
                conflicted_files: conflicted_files.join(", "),
            });
        }
    }

    session
        .advance_to(
            tx.commit("dotsync: continue cascade")
                .await
                .map_err(|err| DotsyncError::Jj {
                    message: format!("commit continued cascade: {err}"),
                })?,
        )
        .await?;
    remove_paused_cascade_state(session.paths())?;
    let push = push_scope_updates(session).await?;
    let sync = crate::sync::sync_home_to_machine_scope(session, home, discard_local).await?;
    Ok(ContinueReport {
        resumed: Resumed::Cascade {
            scope: state.paused_scope,
        },
        sync,
        push,
    })
}

/// `continue` with no cascade paused: the conflict is between home and this
/// machine's own scope head, and the agent has resolved it in home.
///
/// Nothing was stored when that conflict was presented, so nothing is loaded
/// here — the merge is recomputed from the same three trees and home's side of
/// every conflicted path is taken as the answer. The resolution reaches no
/// scope: it stays an ordinary uncommitted change in home, for the agent to
/// commit to whichever scope should own it.
async fn complete_a_sync_conflict(
    session: &mut Session,
    home: &mut Home,
) -> Result<ContinueReport, DotsyncError> {
    let machine_scope = home.machine_scope().to_string();
    let head = load_scope_commit(session.repo().as_ref(), &machine_scope)?;
    match home.resolve_with_home_bytes(session, &head).await? {
        // Home and the head merge cleanly, so there is nothing here that only
        // the agent could have decided.
        Resolved::NothingToResolve => return Err(DotsyncError::NoPausedCascade),
        Resolved::Applied => {}
    }

    let push = push_scope_updates(session).await?;
    let classified = classify_home_against_head(session, home, &head).await?;
    Ok(ContinueReport {
        resumed: Resumed::SyncConflict,
        sync: SyncReport {
            current_scope: machine_scope,
            synced_paths: collect_managed_tree_entries(&head.tree())?
                .into_keys()
                .collect(),
            drifts: Vec::new(),
            carried_changes: changed_paths(&classified, FileState::is_drift)
                .into_iter()
                .map(|(path, classified)| FileChange {
                    path,
                    state: classified.state,
                })
                .collect(),
        },
        push,
    })
}

pub async fn abort_paused_cascade(paths: &DotsyncPaths) -> Run<Result<AbortReport, DotsyncError>> {
    in_session(paths, async |session, paths| {
        let mut home = Home::acquire(session, paths).await?;
        let outcome = abort_in_session(session, &mut home).await;
        finishing(home, session, outcome).await
    })
    .await
}

async fn abort_in_session(
    session: &mut Session,
    home: &mut Home,
) -> Result<AbortReport, DotsyncError> {
    let state = load_paused_cascade_state(session.paths())?;
    if state.original_scope_commit_ids.is_empty() {
        return Err(DotsyncError::Jj {
            message: "paused cascade state does not include an abort checkpoint; resolve the conflict and run `dotsync continue` instead".to_string(),
        });
    }

    let repo = session.repo().clone();
    let mut tx = repo.start_transaction();
    for (scope, commit_id) in &state.original_scope_commit_ids {
        let commit = load_commit_by_hex(tx.repo_mut(), commit_id)?;
        tx.repo_mut().set_local_bookmark_target(
            RefNameBuf::from(scope.as_str()).as_ref(),
            RefTarget::normal(commit.id().clone()),
        );
    }
    session
        .advance_to(
            tx.commit("dotsync: abort cascade")
                .await
                .map_err(|err| DotsyncError::Jj {
                    message: format!("commit aborted cascade: {err}"),
                })?,
        )
        .await?;
    remove_paused_cascade_state(session.paths())?;

    // Abort is a full sync of home back to the machine scope's pre-pause tip,
    // not a selective restore: the home edit that started the cascade is
    // exactly what abort exists to discard, so it cannot also be a reason to
    // refuse. Drift outside the paused selection goes the same way, which is
    // what DESIGN.md's "reverts all the config files" says and what the old
    // selective restore quietly did not do. That is the same discarding sync
    // `dotsync --force` runs, which is why `abort` refuses the flag: it has
    // already made that choice.
    let sync = crate::sync::sync_home_to_machine_scope(session, home, true).await?;

    Ok(AbortReport {
        paused_scope: state.paused_scope,
        sync,
    })
}

fn paused_cascade_state_path(paths: &DotsyncPaths) -> PathBuf {
    paths.repo_root.join(".dotsync-paused-cascade.json")
}

pub(crate) fn save_paused_cascade_state(
    paths: &DotsyncPaths,
    state: &PausedCascadeState,
) -> Result<(), DotsyncError> {
    let path = paused_cascade_state_path(paths);
    let contents = serde_json::to_vec_pretty(state).map_err(|err| DotsyncError::Jj {
        message: format!("serialize paused cascade state: {err}"),
    })?;
    fs::write(&path, contents).map_err(|source| DotsyncError::Io { path, source })
}

fn load_paused_cascade_state(paths: &DotsyncPaths) -> Result<PausedCascadeState, DotsyncError> {
    let path = paused_cascade_state_path(paths);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(DotsyncError::NoPausedCascade);
        }
        Err(source) => return Err(DotsyncError::Io { path, source }),
    };
    serde_json::from_str(&contents).map_err(|err| DotsyncError::Jj {
        message: format!("parse paused cascade state {}: {err}", path.display()),
    })
}

fn remove_paused_cascade_state(paths: &DotsyncPaths) -> Result<(), DotsyncError> {
    let path = paused_cascade_state_path(paths);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DotsyncError::Io { path, source }),
    }
}

/// The scope a paused cascade stopped at, if one is paused. Reads the same
/// state file `continue` and `abort` read, and disappears with it when
/// conflicts become commits.
pub(crate) fn paused_cascade_scope(paths: &DotsyncPaths) -> Result<Option<String>, DotsyncError> {
    match load_paused_cascade_state(paths) {
        Ok(state) => Ok(Some(state.paused_scope)),
        Err(DotsyncError::NoPausedCascade) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn reject_commit_if_cascade_paused(paths: &DotsyncPaths) -> Result<(), DotsyncError> {
    match load_paused_cascade_state(paths) {
        Ok(state) => Err(DotsyncError::PausedCascadeInProgress {
            scope: state.paused_scope,
        }),
        Err(DotsyncError::NoPausedCascade) => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn parent_commit_ids_for_step(
    scope_heads: &ScopeHeads,
    step: &CascadeStep,
) -> Result<Vec<String>, DotsyncError> {
    let mut ids = Vec::with_capacity(step.parent_scopes.len() + 1);
    ids.push(scope_heads.require(&step.scope)?.id().hex());
    for parent_scope in &step.parent_scopes {
        ids.push(scope_heads.require(parent_scope)?.id().hex());
    }
    Ok(ids)
}

pub(crate) fn remaining_steps_after_pause(
    steps: &[CascadeStep],
    paused_scope: &str,
) -> Vec<PausedCascadeStep> {
    steps
        .iter()
        .skip_while(|step| step.scope != paused_scope)
        .skip(1)
        .map(|step| PausedCascadeStep {
            scope: step.scope.clone(),
            parent_scopes: step.parent_scopes.clone(),
        })
        .collect()
}

fn load_commit_by_hex(
    repo: &dyn jj_lib::repo::Repo,
    id: &str,
) -> Result<jj_lib::commit::Commit, DotsyncError> {
    let commit_id = CommitId::try_from_hex(id).ok_or_else(|| DotsyncError::Jj {
        message: format!("paused cascade commit id `{id}` is not valid hex"),
    })?;
    repo.store()
        .get_commit(&commit_id)
        .map_err(|err| DotsyncError::Jj {
            message: format!("load paused cascade commit `{id}`: {err}"),
        })
}
