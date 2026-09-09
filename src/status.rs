use std::path::PathBuf;

use crate::drift::{changed_paths, FileState};
use crate::error::{ConflictedFile, DotsyncError};
use crate::home::Home;
use crate::paths::DotsyncPaths;
use crate::repo::{diverged_scopes, unpushed_scopes};
use crate::session::{in_session, Run, Session};
use crate::sync::{classify_home_against_machine_scope, finishing};

/// What is true of the machine, whatever the command was asked.
///
/// Three facts that qualify every answer `status`, `diff` and `view` give:
/// each of them describes a machine that is not doing what an agent reading
/// "no changes" would assume. They are read in one place and reported in one
/// place, because a fact carried by two of the three commands and not the
/// third is the incoherence that made `status` answer "no changes" on a
/// machine that could not commit at all.
#[derive(Debug, Clone)]
pub struct MachineState {
    /// The merge waiting for a decision, if one is.
    ///
    /// A paused cascade is the one state where a machine that looks completely
    /// clean cannot commit anything at all, and the message that said so
    /// scrolled away one command ago.
    pub paused_cascade: Option<PausedCascade>,
    /// The scopes this machine and the remote have each moved. Reported
    /// because it is the state the next writing run will merge, and this
    /// answer describes the state before that merge.
    pub diverged_scopes: Vec<String>,
    /// The scopes this machine has committed and the remote has never seen.
    ///
    /// Under the same name the publishing commands report it, because it is
    /// the same fact: a refused push is otherwise reported by the run that hit
    /// it and nowhere else, so once that output has scrolled away a machine
    /// holding unpublished commits reads as completely clean. That is how the
    /// 2026-07-27 machine sat unnoticed for sixteen days.
    pub unpushed_scopes: Vec<String>,
}

impl MachineState {
    pub(crate) async fn read(session: &Session) -> Result<Self, DotsyncError> {
        let recorded = crate::pause::load_paused_run(session.paths())?;
        Ok(Self {
            paused_cascade: crate::pause::pending_pause(
                session,
                session.machine_scope(),
                recorded.as_ref(),
            )
            .await?,
            diverged_scopes: diverged_scopes(session.repo().as_ref(), session.graph()),
            unpushed_scopes: unpushed_scopes(session.repo().as_ref(), session.graph()),
        })
    }
}

/// What `status` found, split by whether anyone has to decide anything.
///
/// Both lists are the same three-way classification; only the reader's job
/// differs. Reporting them as one list is what used to make a routine remote
/// advance read exactly like a local edit — and acting on that reading is how
/// a machine that was merely behind reverted another machine's work.
#[derive(Debug, Clone)]
pub struct StatusReport {
    pub machine_scope: String,
    pub machine: MachineState,
    /// Home holds something dotsync did not put there. Someone has to choose.
    pub changes: Vec<FileChange>,
    /// The repo moved and home did not. Plain `dotsync` applies these.
    pub incoming: Vec<FileChange>,
}

/// The merge a machine is waiting on, and every version of every file it could
/// not resolve.
///
/// The versions are the whole reason this carries more than a scope name: they
/// exist nowhere else — nothing is written into home, and neither side is on a
/// scope this machine syncs from — so an agent that lost the pause message has
/// to be able to ask for them again.
#[derive(Debug, Clone)]
pub struct PausedCascade {
    pub scope: String,
    /// Empty when the merge that stopped is a `commit`'s own. That one has
    /// home as a side, so nothing repo-side recomputes it, and it is the same
    /// reason the run that made it had to write down what it was doing.
    pub conflicts: Vec<ConflictedFile>,
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    pub state: FileState,
}

pub async fn status(paths: &DotsyncPaths) -> Run<Result<StatusReport, DotsyncError>> {
    in_session(paths, async |session, paths| {
        // `status` acquires home for the same reason a sync does: home's own
        // bytes are one of the three sides of every answer it gives, and the
        // working copy is what reads them. Acquiring writes snapshot
        // operations to the op log, which is jj's own convention and is not a
        // change to anything a caller can see — no scope bookmark moves and
        // nothing is written into home.
        let mut home = Home::acquire(session, paths).await?;
        let outcome = status_report(session, &mut home).await;
        finishing(home, session, outcome).await
    })
    .await
}

async fn status_report(
    session: &mut Session,
    home: &mut Home,
) -> Result<StatusReport, DotsyncError> {
    session.fetch().await?;
    let machine_scope = home.machine_scope().to_string();
    let classified = classify_home_against_machine_scope(session, home).await?;
    let file_changes = |include: fn(FileState) -> bool| {
        changed_paths(&classified, include)
            .into_iter()
            .map(|(path, classified)| FileChange {
                path,
                state: classified.state,
            })
            .collect::<Vec<_>>()
    };

    Ok(StatusReport {
        machine_scope,
        machine: MachineState::read(session).await?,
        changes: file_changes(FileState::is_drift),
        incoming: file_changes(FileState::is_incoming),
    })
}
