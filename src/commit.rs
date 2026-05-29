use std::path::PathBuf;

use crate::config::DotsyncPaths;
use crate::error::DotsyncError;
use crate::sync::{SyncOptions, SyncReport};

#[derive(Debug, Clone)]
pub struct CommitOptions {
    pub scope: String,
    pub message: String,
    pub force: bool,
    pub selection: CommitSelection,
}

#[derive(Debug, Clone)]
pub enum CommitSelection {
    All,
    Paths(Vec<PathBuf>),
}

#[derive(Debug, Clone, Default)]
pub struct CommitReport {
    pub committed_scope: String,
    pub cascaded_scopes: Vec<String>,
    pub sync: SyncReport,
}

#[derive(Debug, Clone, Default)]
pub struct ContinueReport {
    pub cascaded_scopes: Vec<String>,
    pub sync: SyncReport,
}

#[derive(Debug, Clone)]
pub enum CommandOutcome<T> {
    Success(T),
}

pub async fn commit_and_sync(
    _paths: &DotsyncPaths,
    _options: CommitOptions,
) -> Result<CommandOutcome<CommitReport>, DotsyncError> {
    Err(DotsyncError::NotImplemented(
        "scoped commit is not available until home-diff commit flow lands",
    ))
}

pub async fn continue_after_conflict(
    _paths: &DotsyncPaths,
    _options: SyncOptions,
) -> Result<CommandOutcome<ContinueReport>, DotsyncError> {
    Err(DotsyncError::NotImplemented(
        "continue is not available until home-diff commit flow lands",
    ))
}
