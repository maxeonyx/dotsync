use std::path::PathBuf;

use thiserror::Error;

use crate::sync::FileDrift;

#[derive(Debug, Clone)]
pub struct ErrorReport {
    pub code: &'static str,
    pub message: String,
    pub drifts: Vec<FileDrift>,
    pub current_state: Option<String>,
}

#[derive(Debug, Error)]
pub enum DotsyncError {
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("scope `{scope}` references missing parent `{parent}`")]
    MissingParent { scope: String, parent: String },
    #[error("scope graph contains a cycle involving `{scope}`")]
    ScopeCycle { scope: String },
    #[error("no scope bookmark points at the working copy commit")]
    NoCurrentScope,
    #[error("scope `{scope}` does not exist in config")]
    InvalidScope { scope: String },
    #[error("commit mode requires explicit file/directory paths or --all")]
    CommitSelectionRequired,
    #[error("commit mode accepts explicit paths or --all, not both")]
    ConflictingCommitSelection,
    #[error("commit path `{path}` is outside the repo root")]
    CommitPathOutsideRepo { path: PathBuf },
    #[error("commit path `{path}` does not exist in the working copy or selected deletion set")]
    CommitPathMissing { path: PathBuf },
    #[error("commit selection did not match any working-copy changes")]
    CommitSelectionEmpty,
    #[error("{config_path} may only be committed to scope `all`; requested scope `{scope}`")]
    ConfigOnlyAllowedOnBaseScope { config_path: String, scope: String },
    #[error(
        "fetch would overwrite local bookmark `{bookmark}` by moving it from {local_target} to {remote_target}"
    )]
    FetchWouldOverwriteLocalBookmark {
        bookmark: String,
        local_target: String,
        remote_target: String,
    },
    #[error("scope `{scope}` is not an ancestor of `{current_scope}`")]
    ScopeNotAncestor {
        scope: String,
        current_scope: String,
    },
    #[error("scope `{scope}` does not have a local bookmark")]
    MissingScopeBookmark { scope: String },
    #[error("cascade already in progress on `{scope}`")]
    CascadeInProgress { scope: String },
    #[error("no cascade is currently paused")]
    NoPausedCascade,
    #[error("cascade state error at {path}: {source}")]
    CascadeState {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("sync state error at {path}: {message}")]
    SyncState { path: PathBuf, message: String },
    #[error("detected drift in {count} file(s)")]
    DriftDetected {
        count: usize,
        drifts: Vec<FileDrift>,
    },
    #[error(
        "working copy has uncommitted changes in {count} path(s); plain `dotsync` requires a clean working copy. Use `dotsync <scope> -m \"message\"` instead"
    )]
    DirtyWorkingCopy { count: usize },
    #[error("repo already exists at {path}")]
    RepoAlreadyExists { path: PathBuf },
    #[error("unable to determine machine hostname")]
    MissingHostname,
    #[error("jj operation failed: {message}")]
    Jj { message: String },
}

impl DotsyncError {
    pub fn to_error_report(&self) -> ErrorReport {
        match self {
            DotsyncError::DriftDetected { drifts, .. } => ErrorReport {
                code: "drift_detected",
                message: self.to_string(),
                drifts: drifts.clone(),
                current_state: Some(
                    "managed files in home differ from the repo version for this machine scope"
                        .to_string(),
                ),
            },
            DotsyncError::DirtyWorkingCopy { .. } => basic_error_report("dirty_working_copy", self),
            DotsyncError::NoPausedCascade => basic_error_report("no_paused_cascade", self),
            DotsyncError::InvalidScope { .. } => basic_error_report("invalid_scope", self),
            DotsyncError::ScopeNotAncestor { .. } => basic_error_report("scope_not_ancestor", self),
            DotsyncError::CascadeInProgress { .. } => {
                basic_error_report("cascade_in_progress", self)
            }
            DotsyncError::CommitSelectionRequired => {
                basic_error_report("commit_selection_required", self)
            }
            DotsyncError::ConflictingCommitSelection => {
                basic_error_report("conflicting_commit_selection", self)
            }
            DotsyncError::CommitPathOutsideRepo { .. } => {
                basic_error_report("commit_path_outside_repo", self)
            }
            DotsyncError::CommitPathMissing { .. } => {
                basic_error_report("commit_path_missing", self)
            }
            DotsyncError::CommitSelectionEmpty => {
                basic_error_report("commit_selection_empty", self)
            }
            DotsyncError::ConfigOnlyAllowedOnBaseScope { .. } => {
                basic_error_report("config_base_scope_only", self)
            }
            DotsyncError::FetchWouldOverwriteLocalBookmark { .. } => {
                basic_error_report("fetch_would_overwrite_local_bookmark", self)
            }
            DotsyncError::NoCurrentScope => basic_error_report("no_current_scope", self),
            DotsyncError::MissingScopeBookmark { .. } => {
                basic_error_report("missing_scope_bookmark", self)
            }
            DotsyncError::MissingParent { .. } => basic_error_report("missing_parent", self),
            DotsyncError::ScopeCycle { .. } => basic_error_report("scope_cycle", self),
            DotsyncError::ConfigParse { .. } => basic_error_report("config_parse", self),
            DotsyncError::CascadeState { .. } => basic_error_report("cascade_state", self),
            DotsyncError::SyncState { .. } => basic_error_report("sync_state", self),
            DotsyncError::RepoAlreadyExists { .. } => basic_error_report("repo_exists", self),
            DotsyncError::MissingHostname => basic_error_report("missing_hostname", self),
            DotsyncError::Io { .. } => basic_error_report("io", self),
            DotsyncError::Jj { .. } => basic_error_report("jj", self),
            DotsyncError::NotImplemented(_) => basic_error_report("not_implemented", self),
        }
    }
}

pub(crate) fn basic_error_report(code: &'static str, error: &DotsyncError) -> ErrorReport {
    ErrorReport {
        code,
        message: error.to_string(),
        drifts: Vec::new(),
        current_state: error_current_state(error),
    }
}

pub(crate) fn error_current_state(error: &DotsyncError) -> Option<String> {
    match error {
        DotsyncError::CascadeInProgress { scope } => Some(format!("paused cascade scope: {scope}")),
        DotsyncError::InvalidScope { scope } => Some(format!("requested scope: {scope}")),
        DotsyncError::ScopeNotAncestor {
            scope,
            current_scope,
        } => Some(format!(
            "requested scope: {scope}; current machine scope: {current_scope}"
        )),
        DotsyncError::SyncState { path, .. } => {
            Some(format!("sync state path: {}", path.display()))
        }
        DotsyncError::CommitPathOutsideRepo { path } | DotsyncError::CommitPathMissing { path } => {
            Some(format!("requested path: {}", path.display()))
        }
        DotsyncError::ConfigOnlyAllowedOnBaseScope { config_path, scope } => Some(format!(
            "requested scope: {scope}; restricted path: {config_path}"
        )),
        DotsyncError::FetchWouldOverwriteLocalBookmark {
            bookmark,
            local_target,
            remote_target,
        } => Some(format!(
            "bookmark: {bookmark}; local target: {local_target}; remote target: {remote_target}"
        )),
        DotsyncError::DirtyWorkingCopy { count } => Some(format!(
            "working copy has uncommitted changes in {count} path(s)"
        )),
        DotsyncError::NoPausedCascade => Some("no cascade is currently paused".to_string()),
        _ => None,
    }
}

pub(crate) fn jj_error(message: String) -> DotsyncError {
    DotsyncError::Jj { message }
}
