use std::path::PathBuf;

use crate::config::DotsyncPaths;
use crate::drift::{changed_paths, FileState};
use crate::error::DotsyncError;
use crate::home::Home;
use crate::repo::load_scope_commit;
use crate::session::{in_session, Run};
use crate::sync::{classify_home_against_head, finishing};

/// What `status` found, split by whether anyone has to decide anything.
///
/// Both lists are the same three-way classification; only the reader's job
/// differs. Reporting them as one list is what used to make a routine remote
/// advance read exactly like a local edit — and acting on that reading is how
/// a machine that was merely behind reverted another machine's work.
#[derive(Debug, Clone)]
pub struct StatusReport {
    pub machine_scope: String,
    /// The scope a cascade is paused at, if one is.
    ///
    /// Reported by the read-only commands because a paused cascade is the one
    /// state where a machine that looks completely clean cannot commit
    /// anything at all, and the message that said so scrolled away one command
    /// ago. `status` is the reflex diagnostic; answering "no changes" here is
    /// answering a different question than the one being asked.
    pub paused_cascade: Option<String>,
    /// Home holds something dotsync did not put there. Someone has to choose.
    pub changes: Vec<FileChange>,
    /// The repo moved and home did not. Plain `dotsync` applies these.
    pub incoming: Vec<FileChange>,
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
    session: &mut crate::session::Session,
    home: &mut Home,
) -> Result<StatusReport, DotsyncError> {
    session.fetch().await?;
    let machine_scope = home.machine_scope().to_string();
    let head = load_scope_commit(session.repo().as_ref(), &machine_scope)?;
    home.observe(session, &head).await?;

    let classified = classify_home_against_head(session, home, &head).await?;
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
        paused_cascade: crate::commit::paused_cascade_scope(session.paths())?,
        changes: file_changes(FileState::is_drift),
        incoming: file_changes(FileState::is_incoming),
    })
}
