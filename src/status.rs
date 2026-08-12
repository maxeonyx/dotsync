use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::config::DotsyncPaths;
use crate::drift::{classify_home_against_scope, FileState, RecordedFromHome};
use crate::error::DotsyncError;
use crate::session::{in_session, Run};
use crate::sync::{load_sync_state, resolve_current_scope};

/// What `status` found, split by whether anyone has to decide anything.
///
/// Both lists are the same three-way classification; only the reader's job
/// differs. Reporting them as one list is what used to make a routine remote
/// advance read exactly like a local edit — and acting on that reading is how
/// a machine that was merely behind reverted another machine's work.
#[derive(Debug, Clone)]
pub struct StatusReport {
    pub machine_scope: String,
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
    in_session(paths, async |session, _paths| {
        session.fetch().await?;
        let sync_state = load_sync_state(session.paths(), session.config())?;
        let machine_scope = resolve_current_scope(session.config(), sync_state.as_ref(), None)?;
        let classification = classify_home_against_scope(
            session,
            sync_state.as_ref(),
            &machine_scope,
            &BTreeSet::new(),
            &RecordedFromHome::default(),
        )
        .await?;

        let file_changes = |include: fn(FileState) -> bool| {
            classification
                .paths
                .iter()
                .filter(|(_, path)| include(path.state))
                .map(|(relative, path)| FileChange {
                    path: relative.clone(),
                    state: path.state,
                })
                .collect::<Vec<_>>()
        };

        Ok(StatusReport {
            machine_scope,
            changes: file_changes(FileState::is_drift),
            incoming: file_changes(FileState::is_incoming),
        })
    })
    .await
}
