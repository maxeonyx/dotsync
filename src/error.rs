use std::path::PathBuf;

use thiserror::Error;

use crate::drift::FileState;
use crate::sync::FileDrift;

#[derive(Debug, Clone)]
pub struct ErrorReport {
    pub code: &'static str,
    pub message: String,
    pub drifts: Vec<FileDrift>,
    pub current_state: Option<String>,
}

/// One path a commit named that dotsync will not record, and why. Kept
/// structured so that one run reports every bad path, and each line is
/// rendered at the edge rather than built into the error.
#[derive(Debug, Clone)]
pub struct RejectedCommitPath {
    pub path: PathBuf,
    pub problem: CommitPathProblem,
}

/// One path a commit named that names a real file dotsync could record, but
/// whose content is not this machine's to record. Structured so that one run
/// reports every such path, and so the explanation can name what actually
/// happened to the file.
#[derive(Debug, Clone)]
pub struct RefusedCommitPath {
    pub path: PathBuf,
    pub state: FileState,
}

impl RefusedCommitPath {
    pub(crate) fn explain(&self) -> String {
        let path = self.path.display();
        match self.state {
            FileState::StaleNotYours => format!(
                "`{path}` has not been edited here: home holds exactly what dotsync last synced, and the repo has changed it since. That change came from another machine, and committing home's copy would revert it."
            ),
            FileState::IncomingNew => format!(
                "`{path}` is not in home: the repo has just added it on another machine and this machine has not synced it yet, so there is nothing here to record."
            ),
            FileState::RemovedFromRepo => format!(
                "`{path}` was deleted on another machine, and home still holds the copy dotsync last synced. Committing it would put the file back."
            ),
            FileState::IncomingNewCollidesWithUntrackedHome => format!(
                "`{path}` has never been synced here, and the repo has just added a different file at the same path. Committing home's copy would discard the one that arrived."
            ),
            FileState::NoSyncRecord => format!(
                "`{path}` differs from the scope, and this machine has no sync record — so dotsync cannot tell whether you edited it here or another machine changed it. Committing home's copy would discard the other possibility."
            ),
            other => format!("`{path}` is {}.", other.reason()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum CommitPathProblem {
    Absolute,
    EscapesHome,
    /// Matched neither a file in home nor a file already on the target scope.
    Unmatched {
        home_path: PathBuf,
    },
    SyncState,
    /// The scope graph, named for a scope other than `all`.
    ScopeGraphOutsideAllScope,
    DotsyncRepoRoot {
        repo_root: PathBuf,
    },
    InsideDotsyncRepo {
        repo_root: PathBuf,
    },
}

impl RejectedCommitPath {
    pub(crate) fn explain(&self, scope: &str) -> String {
        let path = self.path.display();
        match &self.problem {
            CommitPathProblem::Absolute => format!(
                "`{path}` is an absolute path, and dotsync resolves every commit path against your home directory."
            ),
            CommitPathProblem::EscapesHome => format!(
                "`{path}` climbs out of your home directory with `..`, and dotsync records the path you name verbatim, so every machine on the scope would write it outside its own home."
            ),
            CommitPathProblem::Unmatched { home_path } => format!(
                "`{path}` matched nothing: no file exists at or under {}, and scope `{scope}` tracks no file at or under `{path}`.",
                home_path.display()
            ),
            CommitPathProblem::SyncState => format!(
                "`{path}` is this machine's dotsync sync state; it records which machine scope this home uses, so it has to stay machine-local."
            ),
            CommitPathProblem::ScopeGraphOutsideAllScope => format!(
                "`{path}` is the scope graph, and dotsync only reads it from `all`; a copy recorded on `{scope}` would configure nothing, and would still overwrite the real one in home on every machine using that scope."
            ),
            CommitPathProblem::DotsyncRepoRoot { repo_root } => format!(
                "`{path}` is dotsync's hidden repo itself, at {}, which is where dotsync stores every scope.",
                repo_root.display()
            ),
            CommitPathProblem::InsideDotsyncRepo { repo_root } => format!(
                "`{path}` is inside dotsync's hidden repo at {}, which is where dotsync stores every scope.",
                repo_root.display()
            ),
        }
    }

    /// Read by the binary's renderer to add advice about dotsync's own state.
    pub fn is_dotsync_state(&self) -> bool {
        matches!(
            self.problem,
            CommitPathProblem::SyncState
                | CommitPathProblem::DotsyncRepoRoot { .. }
                | CommitPathProblem::InsideDotsyncRepo { .. }
        )
    }

    /// Read by the binary's renderer to point at the one scope that owns the
    /// scope graph.
    pub fn is_scope_graph(&self) -> bool {
        matches!(self.problem, CommitPathProblem::ScopeGraphOutsideAllScope)
    }
}

#[derive(Debug, Error)]
pub enum DotsyncError {
    #[error(
        "HOME is not set, so dotsync cannot find your home directory. Set HOME to the home directory dotsync should manage, then rerun."
    )]
    HomeNotSet,
    #[error(
        "path {path:?} is not valid UTF-8; dotsync can only manage files whose paths are valid UTF-8"
    )]
    NonUtf8Path { path: PathBuf },
    #[error("{path} is a git submodule; dotsync manages regular files and symlinks only")]
    GitSubmodule { path: PathBuf },
    #[error("cannot commit {} of the paths you named", rejected.len())]
    UnusableCommitPaths {
        scope: String,
        rejected: Vec<RejectedCommitPath>,
    },
    #[error("cannot commit {} of the paths you named, because this machine did not change them", refused.len())]
    StaleCommitPaths {
        scope: String,
        refused: Vec<RefusedCommitPath>,
    },
    #[error("{} conflicted file(s) are unchanged since the cascade paused at scope `{scope}`", paths.len())]
    UnresolvedConflict { scope: String, paths: Vec<PathBuf> },
    #[error(
        "the cascade paused at scope `{scope}` recorded no contents to check a resolution against"
    )]
    PausePredatesResolutionCheck { scope: String },
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
    #[error("unable to determine current machine scope")]
    NoCurrentScope,
    #[error("scope `{scope}` does not exist in config")]
    InvalidScope { scope: String },
    #[error(
        "scope `{scope}` has diverged: this machine and the remote each have commits the other does not"
    )]
    ScopeDiverged {
        scope: String,
        local_target: String,
        remote_target: String,
    },
    #[error("scope `{scope}` does not have a local bookmark")]
    MissingScopeBookmark { scope: String },
    #[error("sync state error at {path}: {message}")]
    SyncState { path: PathBuf, message: String },
    #[error("detected drift in {count} file(s)")]
    DriftDetected {
        count: usize,
        drifts: Vec<FileDrift>,
    },
    #[error("cascade paused at scope `{scope}` with conflicts in {conflicted_files}")]
    CascadePaused {
        scope: String,
        conflicted_files: String,
    },
    #[error("paused cascade at scope `{scope}` must be resolved before starting another commit")]
    PausedCascadeInProgress { scope: String },
    #[error("no paused cascade to continue")]
    NoPausedCascade,
    #[error("repo already exists at {path}")]
    RepoAlreadyExists { path: PathBuf },
    #[error("not initialized")]
    NotInitialized { path: PathBuf },
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
            DotsyncError::InvalidScope { .. } => basic_error_report("invalid_scope", self),
            DotsyncError::ScopeDiverged { .. } => basic_error_report("scope_diverged", self),
            DotsyncError::NoCurrentScope => basic_error_report("no_current_scope", self),
            DotsyncError::MissingScopeBookmark { .. } => {
                basic_error_report("missing_scope_bookmark", self)
            }
            DotsyncError::MissingParent { .. } => basic_error_report("missing_parent", self),
            DotsyncError::ScopeCycle { .. } => basic_error_report("scope_cycle", self),
            DotsyncError::ConfigParse { .. } => basic_error_report("config_parse", self),
            DotsyncError::SyncState { .. } => basic_error_report("sync_state", self),
            DotsyncError::CascadePaused { .. } => basic_error_report("cascade_paused", self),
            DotsyncError::PausedCascadeInProgress { .. } => {
                basic_error_report("paused_cascade_in_progress", self)
            }
            DotsyncError::NoPausedCascade => basic_error_report("no_paused_cascade", self),
            DotsyncError::RepoAlreadyExists { .. } => basic_error_report("repo_exists", self),
            DotsyncError::NotInitialized { path } => ErrorReport {
                code: "not_initialized",
                message: format!(
                    "Dotsync could not find its hidden repo at {}. Run `dotsync init <remote-url>` from this home directory, then rerun `dotsync status`.",
                    path.display()
                ),
                drifts: Vec::new(),
                current_state: error_current_state(self),
            },
            DotsyncError::MissingHostname => basic_error_report("missing_hostname", self),
            DotsyncError::Io { .. } => basic_error_report("io", self),
            DotsyncError::Jj { .. } => basic_error_report("jj", self),
            DotsyncError::HomeNotSet => basic_error_report("home_not_set", self),
            DotsyncError::NonUtf8Path { .. } => basic_error_report("non_utf8_path", self),
            DotsyncError::GitSubmodule { .. } => basic_error_report("git_submodule", self),
            DotsyncError::UnusableCommitPaths { .. } => {
                basic_error_report("unusable_commit_paths", self)
            }
            DotsyncError::StaleCommitPaths { .. } => {
                basic_error_report("stale_commit_paths", self)
            }
            DotsyncError::UnresolvedConflict { .. } => {
                basic_error_report("unresolved_conflict", self)
            }
            DotsyncError::PausePredatesResolutionCheck { .. } => {
                basic_error_report("pause_predates_resolution_check", self)
            }
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
        DotsyncError::InvalidScope { scope } => Some(format!("requested scope: {scope}")),
        DotsyncError::SyncState { path, .. } => {
            Some(format!("sync state path: {}", path.display()))
        }
        DotsyncError::ScopeDiverged {
            scope,
            local_target,
            remote_target,
        } => Some(format!(
            "scope: {scope}; local target: {local_target}; remote target: {remote_target}"
        )),
        DotsyncError::CascadePaused { scope, .. } => Some(format!("paused scope: {scope}")),
        DotsyncError::UnusableCommitPaths { scope, rejected } => Some(
            rejected
                .iter()
                .map(|rejected| rejected.explain(scope))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        DotsyncError::StaleCommitPaths { refused, .. } => Some(
            refused
                .iter()
                .map(RefusedCommitPath::explain)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        DotsyncError::PausePredatesResolutionCheck { scope } => Some(format!(
            "paused scope: {scope}; the pause holds no record of what the conflicted files contained when it paused."
        )),
        DotsyncError::UnresolvedConflict { scope, paths } => Some(format!(
            "unchanged since the cascade paused at scope `{scope}`: {}",
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        DotsyncError::PausedCascadeInProgress { scope } => Some(format!("paused scope: {scope}")),
        DotsyncError::NotInitialized { path } => Some(format!(
            "expected repo path: {}; standard location: ~/.local/share/dotsync/repo",
            path.display()
        )),
        DotsyncError::HomeNotSet
        | DotsyncError::NonUtf8Path { .. }
        | DotsyncError::GitSubmodule { .. }
        | DotsyncError::NoPausedCascade
        | DotsyncError::Io { .. }
        | DotsyncError::ConfigParse { .. }
        | DotsyncError::MissingParent { .. }
        | DotsyncError::ScopeCycle { .. }
        | DotsyncError::NoCurrentScope
        | DotsyncError::MissingScopeBookmark { .. }
        | DotsyncError::DriftDetected { .. }
        | DotsyncError::RepoAlreadyExists { .. }
        | DotsyncError::MissingHostname
        | DotsyncError::Jj { .. } => None,
    }
}

pub(crate) fn jj_error(message: String) -> DotsyncError {
    DotsyncError::Jj { message }
}
