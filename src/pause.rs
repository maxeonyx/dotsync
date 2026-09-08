//! A paused convergence, and the two commands that end one.
//!
//! Convergence pauses when a scope's merge holds a file two sides changed
//! differently. Nothing records that: whether this machine is paused is
//! answered by running the pass again in a transaction nothing commits
//! (`converge::pending_pause`), because the same commits give the same merge
//! and the same conflict. So a resolution shows up the moment it is written, a
//! crash leaves nothing to be stale, and `status` cannot describe a machine
//! differently from the run that follows it.
//!
//! There are three paused states and each is derived from a different place:
//!
//! - a scope's merge, from the pass;
//! - home against this machine's own scope head, from home, the mark and the
//!   head;
//! - a `commit` whose own merge conflicted, from the file beside the repo,
//!   because that merge has home as one of its two sides — recomputing it
//!   after the answer is written finds nothing conflicted — and because the
//!   scope and the message it was carrying only ever existed in that run's
//!   arguments.
//!
//! That is also the whole of what the file holds, along with where the scopes
//! stood before the run wrote anything, which is where `abort` goes back to.
//! It is a record of a *run*, not of a pause.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use jj_lib::backend::CommitId;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::Repo as _;

use crate::converge::{self, Converged, Published};
use crate::drift::{changed_paths, FileState};
use crate::error::{ConflictedFile, DotsyncError};
use crate::home::{repo_path_of, Home, Resolved};
use crate::machine::machine_signature;
use crate::paths::DotsyncPaths;
use crate::repo::{collect_managed_tree_entries, read_entry_bytes, scope_head_commit, PushReport};
use crate::session::{in_session, Run, Session};
use crate::status::FileChange;
use crate::sync::{
    classify_home_against_head, conflicted_versions, finishing, LocalChanges, SyncReport,
};

/// What a run that stopped knew and the repo cannot say.
///
/// Written beside the repo, and deliberately not a record of the pause: the
/// pause is derived. Everything here is about the run — where it started, and
/// the commit it was making if that commit's own merge is what stopped it.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct PausedRun {
    /// Where every scope stood before the run wrote anything, which is where
    /// `abort` returns them to.
    ///
    /// Not derivable, and the near miss is worth naming: at a pause the scopes
    /// ahead of the remote are the ones this run moved *plus* any an earlier
    /// run committed and could not publish, and abort must not take those.
    /// "Where this run started" is a fact about the run.
    #[serde(default, alias = "original_scope_commit_ids")]
    pub(crate) checkpoint: BTreeMap<String, String>,
    /// The commit this run was making, when the conflict was in that commit's
    /// own merge rather than in the pass.
    #[serde(default)]
    pub(crate) paused_commit: Option<PausedCommit>,
}

/// A `commit` that stopped in its own merge — home's bytes against the scope
/// head, three-way against the version home started from.
///
/// The only paused state that has to be written down. The merge has home as
/// one of its sides, so recomputing it after the resolution is written finds
/// nothing conflicted; and the scope, the message and the paths came from the
/// command line, which no amount of reading the repo will recover.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct PausedCommit {
    pub(crate) machine_scope: String,
    pub(crate) scope: String,
    pub(crate) message: String,
    pub(crate) parent_commit_id: String,
    pub(crate) conflicted_paths: Vec<PathBuf>,
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
    Cascade {
        scope: String,
        /// This machine's own scope, when `scope` is neither it nor above it —
        /// so the conflicted files in home were another machine's config for
        /// the length of the pause, and this run handed them back. Said out
        /// loud because the sync reports discarding them, and a run that
        /// destroys home content without saying why reads as a bug.
        borrowed_from: Option<String>,
    },
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
    /// The scope the pass still stops at, when discarding this run's own work
    /// was not enough to end the conflict.
    ///
    /// `abort` takes back what this machine committed, and a conflict that
    /// came from the remote is not that: `all` moved on the shared remote,
    /// `linux` collides with it, and nothing local is in the way. So the run
    /// puts home back, says the conflict is still there, and exits with the
    /// code that means one is waiting — rather than reporting success and
    /// leaving the next command to disagree.
    pub still_paused: Option<String>,
}

/// Converges every scope, and turns a merge that could not resolve into the
/// stop that presents it.
///
/// Every command that writes starts here, so a pause is the same object
/// however the conflict arose — two machines that both moved a scope, a change
/// cascading into a scope that had moved, or a parent scope somebody published
/// without cascading it. There is one merge and one way it can stop.
pub(crate) async fn converge_or_pause(
    session: &mut Session,
    home: &mut Home,
    checkpoint: &BTreeMap<String, String>,
) -> Result<(), DotsyncError> {
    match converge::converge(session, home.machine_scope()).await? {
        Converged::Completed => Ok(()),
        Converged::Paused(pause) => Err(pause_at(session, home, checkpoint, pause).await?),
    }
}

/// Publishes what this machine has, converging onto anything that arrives
/// while it tries.
pub(crate) async fn publish_or_pause(
    session: &mut Session,
    home: &mut Home,
    checkpoint: &BTreeMap<String, String>,
) -> Result<PushReport, DotsyncError> {
    match converge::publish(session, home.machine_scope()).await? {
        Published::Report(report) => Ok(report),
        Published::Paused(pause) => Err(pause_at(session, home, checkpoint, pause).await?),
    }
}

/// Records where this run started and builds the stop that presents the
/// conflict.
///
/// The pause itself is not recorded. The pass recomputes it — same commits,
/// same merge, same conflicted paths — so what the file needs from a
/// convergence pause is only `abort`'s checkpoint.
pub(crate) async fn pause_at(
    session: &mut Session,
    home: &mut Home,
    checkpoint: &BTreeMap<String, String>,
    pause: converge::Pause,
) -> Result<DotsyncError, DotsyncError> {
    let conflicted = conflicted_paths_of(&pause.merged, &pause.scope)?;

    // Home has to cover the conflicted paths before the run ends, because
    // `continue` reads the resolution back out of them and a conflict can be
    // about a file this machine has never held.
    observe(session, home, &conflicted).await?;
    save_paused_run(
        session.paths(),
        &PausedRun {
            checkpoint: checkpoint.clone(),
            paused_commit: None,
        },
    )?;
    present(session, home.machine_scope(), &pause.merged, &pause.scope).await
}

/// The stop that puts a conflicted merge in front of whoever has to resolve
/// it: every conflicted file, with the base and both sides, labeled by scope.
pub(crate) async fn present(
    session: &Session,
    machine_scope: &str,
    merged: &jj_lib::merged_tree::MergedTree,
    scope: &str,
) -> Result<DotsyncError, DotsyncError> {
    let mut files = Vec::new();
    for relative in conflicted_paths_of(merged, scope)? {
        let Some(versions) = conflicted_versions(session, merged, &relative, scope).await? else {
            continue;
        };
        files.push(ConflictedFile {
            path: relative,
            state: None,
            versions,
        });
    }
    Ok(DotsyncError::CascadePaused {
        borrowed_from: borrowed_from(session, machine_scope, scope),
        scope: scope.to_string(),
        files,
    })
}

pub(crate) fn conflicted_paths_of(
    merged: &jj_lib::merged_tree::MergedTree,
    scope: &str,
) -> Result<Vec<PathBuf>, DotsyncError> {
    merged
        .conflicts()
        .map(|(path, value)| {
            value.map_err(|err| DotsyncError::Jj {
                message: format!("read conflict for {scope}: {err}"),
            })?;
            Ok(PathBuf::from(path.as_internal_file_string()))
        })
        .collect()
}

/// Reads home's side of every one of these paths, so that whatever comes next
/// is looking at bytes rather than at an unobserved path, which reads as
/// absent — and absent is a deletion.
async fn observe(
    session: &mut Session,
    home: &mut Home,
    relatives: &[PathBuf],
) -> Result<(), DotsyncError> {
    home.observe_paths(
        session,
        relatives
            .iter()
            .map(|relative| repo_path_of(relative))
            .collect::<Result<Vec<_>, DotsyncError>>()?,
    )
    .await
}

/// This machine's own scope, when the merge waiting for a decision is not on
/// it or above it.
///
/// `commit` cannot target a scope this machine is not on, but a cascade from a
/// shared ancestor still merges into scopes it is not on — so this is reached
/// by the routine event rather than by a mistake.
pub(crate) fn borrowed_from(
    session: &Session,
    machine_scope: &str,
    paused_scope: &str,
) -> Option<String> {
    let in_ancestry = session
        .graph()
        .ancestors_and_self(machine_scope)
        .iter()
        .any(|scope| scope.name == paused_scope);
    match in_ancestry {
        true => None,
        false => Some(machine_scope.to_string()),
    }
}

/// Conflicted files whose home content is not a resolution, because it still
/// holds conflict markers.
///
/// "Resolved" is a property of the content, and this is the whole of it.
/// Nothing else can be: an unchanged file is a legitimate resolution — the
/// agent read both sides and kept this one — and treating it as unresolved is
/// silently wrong for exactly the agent that did the work properly (DESIGN,
/// "Whether `continue` survives"). What is never a resolution is a file with
/// markers in it: recorded as the merged contents, they cascade into every
/// descendant and every other machine then syncs `<<<<<<<` into its live
/// config.
async fn files_that_still_hold_markers(
    session: &Session,
    home: &Home,
    relatives: &[PathBuf],
) -> Result<Vec<PathBuf>, DotsyncError> {
    let mut unresolved = Vec::new();
    for relative in relatives {
        let value = home.entry(relative)?.as_resolved().cloned().flatten();
        let Some(bytes) =
            read_entry_bytes(session.repo().store(), relative, value.as_ref()).await?
        else {
            continue;
        };
        if holds_conflict_markers(&bytes) {
            unresolved.push(relative.clone());
        }
    }
    Ok(unresolved)
}

/// Whether these bytes are a conflict somebody stopped half way through
/// resolving.
///
/// Both ends, deliberately. jj parses six marker characters and a file that
/// starts a line with seven of any of them is a marker to it — which makes
/// `=======` under a heading, and a markdown rule of seven dashes, conflict
/// markers. Requiring a start line *and* an end line is what no config file
/// holds by accident, and it is what jj's own materialization always writes.
/// The length comes from jj so the number is not invented here; jj picks a
/// longer one when the content already contains markers, so this is a floor.
fn holds_conflict_markers(bytes: &[u8]) -> bool {
    let marker_line = |byte: u8| {
        bytes.split(|&b| b == b'\n').any(|line| {
            line.iter().take_while(|&&b| b == byte).count()
                >= jj_lib::conflicts::MIN_CONFLICT_MARKER_LEN
        })
    };
    marker_line(b'<') && marker_line(b'>')
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

/// `continue` is the agent saying "the resolution is written". Which paused
/// state that sentence ends is a fact about the machine rather than something
/// the command has to be told, and the three are looked for in the order the
/// agent met them: a `commit` that stopped in its own merge is the one that
/// leaves a record, then the pass, then home against this machine's own head.
/// None of the three is what "nothing is paused" means.
async fn continue_in_session(
    session: &mut Session,
    home: &mut Home,
    discard_local: bool,
) -> Result<ContinueReport, DotsyncError> {
    let recorded = load_paused_run(session.paths())?;
    if let Some(paused_commit) = recorded.as_ref().and_then(|run| run.paused_commit.clone()) {
        let checkpoint = recorded.map(|run| run.checkpoint).unwrap_or_default();
        return finish_the_paused_commit(session, home, paused_commit, checkpoint, discard_local)
            .await;
    }
    let Some(pause) =
        converge::pending_pause(session.repo(), session.graph(), session.machine_scope()).await?
    else {
        return complete_a_sync_conflict(session, home).await;
    };
    let checkpoint = recorded
        .map(|run| run.checkpoint)
        .unwrap_or_else(|| converge::checkpoint(session.repo().as_ref(), session.graph()));
    finish_the_paused_convergence(session, home, pause, checkpoint, discard_local).await
}

/// The pass again, with home's bytes as the answer at exactly the paths it
/// stops on.
///
/// Nothing is loaded: the merge, the scope it is on and the paths that
/// conflicted are all recomputed, so a machine that lost every local record of
/// the pause resolves it the same way, and a resolution the agent has written
/// is picked up simply by being there.
async fn finish_the_paused_convergence(
    session: &mut Session,
    home: &mut Home,
    pause: converge::Pause,
    checkpoint: BTreeMap<String, String>,
    discard_local: bool,
) -> Result<ContinueReport, DotsyncError> {
    let machine_scope = session.machine_scope().to_string();
    let conflicted = conflicted_paths_of(&pause.merged, &pause.scope)?;
    observe(session, home, &conflicted).await?;
    refuse_markers(session, home, &conflicted, &pause.scope).await?;

    let mut entries = Vec::new();
    for relative in &conflicted {
        entries.push((repo_path_of(relative)?, home.entry(relative)?));
    }
    let resolution = converge::Resolution {
        scope: pause.scope.clone(),
        entries,
    };
    // One pass writes the resolution and everything the resolved scope
    // cascades into: below it, every scope merges a parent that moved, which
    // is the ordinary convergence. There is no remaining cascade to remember,
    // because the pass finds what is left by looking.
    match converge::converge_with(session, &machine_scope, Some(&resolution)).await? {
        converge::Converged::Completed => {}
        // A second conflict, over something the agent was not shown. The
        // record of this run's start goes with it, so `abort` still has
        // somewhere to go back to.
        converge::Converged::Paused(next) => {
            return Err(pause_at(session, home, &checkpoint, next).await?)
        }
    }
    remove_paused_run(session.paths())?;
    let push = publish_or_pause(session, home, &checkpoint).await?;
    finished_resolving(session, home, pause.scope, conflicted, discard_local, push).await
}

/// The `commit` whose own merge stopped it, finished: merge its parent again,
/// lay home's bytes over the paths that conflicted, and record it under the
/// message the command was given.
///
/// This is the one resolution the pass cannot do, because the conflict was
/// never in the pass — it was between home and the scope head, three-way
/// against the version home started from.
async fn finish_the_paused_commit(
    session: &mut Session,
    home: &mut Home,
    paused: PausedCommit,
    checkpoint: BTreeMap<String, String>,
    discard_local: bool,
) -> Result<ContinueReport, DotsyncError> {
    let repo = session.repo().clone();
    let parent = load_commit_by_hex(repo.as_ref(), &paused.parent_commit_id)?;
    let conflicted = paused.conflicted_paths;
    observe(session, home, &conflicted).await?;
    refuse_markers(session, home, &conflicted, &paused.scope).await?;

    let mut builder = MergedTreeBuilder::new(parent.tree());
    for relative in &conflicted {
        builder.set_or_remove(repo_path_of(relative)?, home.entry(relative)?);
    }
    let resolved_tree = builder.write_tree().await.map_err(|err| DotsyncError::Jj {
        message: format!("write resolved tree for {}: {err}", paused.scope),
    })?;
    let mut tx = repo.start_transaction();
    let resolved_commit = tx
        .repo_mut()
        .new_commit(vec![parent.id().clone()], resolved_tree)
        .set_description(&paused.message)
        .set_author(machine_signature(&paused.machine_scope))
        .write()
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!("write the resolved commit for {}: {err}", paused.scope),
        })?;
    tx.repo_mut().set_local_bookmark_target(
        RefNameBuf::from(paused.scope.as_str()).as_ref(),
        RefTarget::normal(resolved_commit.id().clone()),
    );
    session
        .advance_to(
            tx.commit("dotsync: record the resolution")
                .await
                .map_err(|err| DotsyncError::Jj {
                    message: format!("commit the resolution: {err}"),
                })?,
        )
        .await?;
    // The record goes before the pass runs, so that a pass which stops on a
    // conflict of its own writes a record describing that run instead.
    remove_paused_run(session.paths())?;
    converge_or_pause(session, home, &checkpoint).await?;
    let push = publish_or_pause(session, home, &checkpoint).await?;
    finished_resolving(session, home, paused.scope, conflicted, discard_local, push).await
}

/// The end both resolutions share: home stops holding the answer, because the
/// scope holds it now.
///
/// The conflicted paths were borrowed to write the resolution into, so this
/// machine's own scope decides what home holds there again. In ancestry that
/// is the resolution itself — it has just cascaded down — and out of ancestry
/// it is this machine's own config coming back, which is the whole of DESIGN's
/// mode switch and needs no branch to say which case this is.
async fn finished_resolving(
    session: &mut Session,
    home: &mut Home,
    scope: String,
    conflicted: Vec<PathBuf>,
    discard_local: bool,
    push: PushReport,
) -> Result<ContinueReport, DotsyncError> {
    let borrowed_from = borrowed_from(session, home.machine_scope(), &scope);
    let local = match discard_local {
        true => LocalChanges::Discard,
        false => LocalChanges::DiscardAt(conflicted),
    };
    let sync = crate::sync::sync_home_to_machine_scope(session, home, local).await?;
    Ok(ContinueReport {
        resumed: Resumed::Cascade {
            borrowed_from,
            scope,
        },
        sync,
        push,
    })
}

async fn refuse_markers(
    session: &Session,
    home: &Home,
    conflicted: &[PathBuf],
    scope: &str,
) -> Result<(), DotsyncError> {
    let unresolved = files_that_still_hold_markers(session, home, conflicted).await?;
    match unresolved.is_empty() {
        true => Ok(()),
        false => Err(DotsyncError::UnresolvedConflict {
            scope: scope.to_string(),
            paths: unresolved,
        }),
    }
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
    let head = scope_head_commit(session.repo().as_ref(), &machine_scope)?;
    match home.resolve_with_home_bytes(session, &head).await? {
        // Home and the head merge cleanly, so there is nothing here that only
        // the agent could have decided.
        Resolved::NothingToResolve => return Err(DotsyncError::NoPausedCascade),
        Resolved::Applied => {}
    }

    let checkpoint = converge::checkpoint(session.repo().as_ref(), session.graph());
    let push = publish_or_pause(session, home, &checkpoint).await?;
    let classified = classify_home_against_head(session, home, &head.tree()).await?;
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
    let machine_scope = session.machine_scope().to_string();
    let recorded = load_paused_run(session.paths())?;
    let Some(stopped_at) = paused_scope(session, &machine_scope, recorded.as_ref()).await? else {
        return Err(DotsyncError::NoPausedCascade);
    };

    // Every scope this run moved goes back where it was. There may be nothing
    // to move: a conflict that came from the remote is not something this
    // machine committed, and abort discards this machine's work rather than
    // anybody else's.
    let checkpoint = recorded.map(|run| run.checkpoint).unwrap_or_default();
    if !checkpoint.is_empty() {
        let repo = session.repo().clone();
        let mut tx = repo.start_transaction();
        for (scope, commit_id) in &checkpoint {
            let commit = load_commit_by_hex(tx.repo_mut(), commit_id)?;
            tx.repo_mut().set_local_bookmark_target(
                RefNameBuf::from(scope.as_str()).as_ref(),
                RefTarget::normal(commit.id().clone()),
            );
        }
        session
            .advance_to(tx.commit("dotsync: abort cascade").await.map_err(|err| {
                DotsyncError::Jj {
                    message: format!("commit aborted cascade: {err}"),
                }
            })?)
            .await?;
    }
    remove_paused_run(session.paths())?;

    // Abort is a full sync of home back to the machine scope's pre-pause tip,
    // not a selective restore: the home edit that started the cascade is
    // exactly what abort exists to discard, so it cannot also be a reason to
    // refuse. Drift outside the paused selection goes the same way, which is
    // what DESIGN.md's "reverts all the config files" says and what the old
    // selective restore quietly did not do. That is the same discarding sync
    // `dotsync --force` runs, which is why `abort` refuses the flag: it has
    // already made that choice.
    let sync =
        crate::sync::sync_home_to_machine_scope(session, home, LocalChanges::Discard).await?;

    Ok(AbortReport {
        paused_scope: stopped_at,
        sync,
        still_paused: paused_scope(session, &machine_scope, None).await?,
    })
}

/// The scope this machine is paused at, if it is paused at all.
///
/// Derived, and this is where "derived" is decided: the pass says whether a
/// scope's merge stops, and the record of a stopped `commit` says whether one
/// of those is waiting. Nothing else is consulted, so removing every file
/// dotsync keeps beside its repo cannot change the answer for a convergence
/// pause, and `dotsync abort` cannot make one look resolved by deleting a
/// file.
pub(crate) async fn paused_scope(
    session: &Session,
    machine_scope: &str,
    recorded: Option<&PausedRun>,
) -> Result<Option<String>, DotsyncError> {
    if let Some(paused_commit) = recorded.and_then(|run| run.paused_commit.as_ref()) {
        return Ok(Some(paused_commit.scope.clone()));
    }
    Ok(
        converge::pending_pause(session.repo(), session.graph(), machine_scope)
            .await?
            .map(|pause| pause.scope),
    )
}

fn paused_run_path(paths: &DotsyncPaths) -> PathBuf {
    paths.repo_root.join(".dotsync-paused-cascade.json")
}

pub(crate) fn save_paused_run(paths: &DotsyncPaths, run: &PausedRun) -> Result<(), DotsyncError> {
    let path = paused_run_path(paths);
    let contents = serde_json::to_vec_pretty(run).map_err(|err| DotsyncError::Jj {
        message: format!("serialize the paused run: {err}"),
    })?;
    fs::write(&path, contents).map_err(|source| DotsyncError::Io { path, source })
}

pub(crate) fn load_paused_run(paths: &DotsyncPaths) -> Result<Option<PausedRun>, DotsyncError> {
    let path = paused_run_path(paths);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(DotsyncError::Io { path, source }),
    };
    serde_json::from_str(&contents)
        .map(Some)
        .map_err(|err| DotsyncError::Jj {
            message: format!("parse the paused run at {}: {err}", path.display()),
        })
}

fn remove_paused_run(paths: &DotsyncPaths) -> Result<(), DotsyncError> {
    let path = paused_run_path(paths);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DotsyncError::Io { path, source }),
    }
}

/// Refuses a new commit while one is waiting to be resolved, and says which
/// scope is waiting.
///
/// Derived like every other reader, so it holds on a machine that has lost the
/// record of the run that paused: the reason a commit is refused is that the
/// pass cannot get past a merge, and building new history on top of that is
/// how the pause stops being the thing anybody is working on.
pub(crate) async fn reject_commit_if_paused(
    session: &Session,
    machine_scope: &str,
) -> Result<(), DotsyncError> {
    let recorded = load_paused_run(session.paths())?;
    match paused_scope(session, machine_scope, recorded.as_ref()).await? {
        Some(scope) => Err(DotsyncError::PausedCascadeInProgress { scope }),
        None => Ok(()),
    }
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
