use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct DotsyncPaths {
    pub repo_root: PathBuf,
    pub home_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SyncOptions {
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct CommitOptions {
    pub scope: String,
    pub message: String,
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct FileDrift {
    pub repo_path: PathBuf,
    pub system_path: PathBuf,
    pub diff: String,
}

#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub current_scope: String,
    pub synced_paths: Vec<PathBuf>,
    pub drifts: Vec<FileDrift>,
}

#[derive(Debug, Clone, Default)]
pub struct CommitReport {
    pub committed_scope: String,
    pub cascaded_scopes: Vec<String>,
    pub sync: SyncReport,
}

#[derive(Debug)]
pub enum DotsyncError {
    NotImplemented(&'static str),
}

impl std::fmt::Display for DotsyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotImplemented(message) => write!(f, "not implemented: {message}"),
        }
    }
}

impl std::error::Error for DotsyncError {}

pub async fn sync(_paths: &DotsyncPaths, _options: SyncOptions) -> Result<SyncReport, DotsyncError> {
    Err(DotsyncError::NotImplemented("sync"))
}

pub async fn commit_and_sync(
    _paths: &DotsyncPaths,
    _options: CommitOptions,
) -> Result<CommitReport, DotsyncError> {
    Err(DotsyncError::NotImplemented("commit_and_sync"))
}
