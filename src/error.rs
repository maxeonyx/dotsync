use std::path::PathBuf;

use thiserror::Error;

use crate::drift::FileState;
use crate::sync::FileDrift;

#[derive(Debug, Clone)]
pub struct ErrorReport {
    pub code: &'static str,
    pub message: String,
    pub drifts: Vec<FileDrift>,
    /// What dotsync found, one fact per entry.
    ///
    /// A list rather than a paragraph because a run that refused three paths
    /// found three things: joining them for a person to read is a decision for
    /// whoever is rendering, and a reader that has to split them back apart on
    /// a newline is reading a rendering rather than an answer.
    pub current_state: Vec<String>,
    /// What the run had already overwritten under `--force` when it stopped.
    /// Empty for every error raised before a run can overwrite anything, which
    /// is all of them except a commit that failed after writing its history.
    pub forced_overwrites: Vec<PathBuf>,
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
///
/// Refused, not skipped: this is a path the command named exactly, so dotsync
/// stops and argues rather than quietly deciding for the user. The paths a
/// bulk selection steps around are `SkippedCommitPath`, and the difference
/// between the two is the whole of what naming a path exactly buys you.
#[derive(Debug, Clone)]
pub struct RefusedCommitPath {
    pub path: PathBuf,
    pub state: FileState,
}

/// One path a named directory matched that the commit left out, because home
/// holds no change of this machine's own at it.
///
/// Not an error and not a refusal: the run succeeds, and this is what it has
/// to say about what it did not do — so it is reported alongside the result
/// rather than instead of one.
#[derive(Debug, Clone)]
pub struct SkippedCommitPath {
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
    /// Your whole home directory, however it was named.
    HomeRoot,
    /// The path is a symlink, or reaches its file through one.
    Symlink {
        resolves_to: PathBuf,
    },
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
            CommitPathProblem::HomeRoot => format!(
                "`{path}` is your whole home directory. Dotsync would walk all of it and put every file it found on scope `{scope}` — ssh keys, credentials, browser profiles — and every machine sharing that scope would then have them written into its own home."
            ),
            CommitPathProblem::Symlink { resolves_to } => format!(
                "`{path}` is a symlink, or reaches its file through one: it resolves to {}. Dotsync records the content it finds at the path you name, and every machine on scope `{scope}` writes that content back to that same path — so a link is either somebody else's file being published under your path, or a later sync writing through the link to somewhere dotsync does not manage.",
                resolves_to.display()
            ),
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

    /// Read by the binary's renderer to say what to name instead of home.
    pub fn is_home_root(&self) -> bool {
        matches!(self.problem, CommitPathProblem::HomeRoot)
    }

    /// Read by the binary's renderer to explain what dotsync does with links.
    pub fn is_symlink(&self) -> bool {
        matches!(self.problem, CommitPathProblem::Symlink { .. })
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
    #[error("{}", one_or_many(rejected.len(), "cannot commit the path you named", "cannot commit {n} of the paths you named"))]
    UnusableCommitPaths {
        scope: String,
        rejected: Vec<RejectedCommitPath>,
    },
    #[error("{}", one_or_many(refused.len(), "cannot commit the path you named, because this machine did not change it", "cannot commit {n} of the paths you named, because this machine did not change them"))]
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
    /// Raised by `continue` and by `abort`, so it says what is not there
    /// rather than what the caller wanted to do with it.
    #[error("there is no paused cascade on this machine")]
    NoPausedCascade,
    #[error("repo already exists at {path}")]
    RepoAlreadyExists { path: PathBuf },
    #[error("not initialized")]
    NotInitialized { path: PathBuf },
    #[error("unable to determine machine hostname")]
    MissingHostname,
    /// Reaching the remote failed. Raised only where reaching it is the point
    /// of the command: everywhere else a run degrades to the last state it did
    /// fetch and says so — see `Session::fetch`.
    #[error("could not reach the remote: {reason}")]
    RemoteUnreachable { reason: String },
    /// An `init` that stopped and could not take its own leavings with it.
    /// Carries the failure that stopped it, because that is still the thing to
    /// fix; the half-made repo is what stops the retry from starting.
    #[error(
        "{original}\n\nDotsync could also not remove the partly created repo at {path}: {source}. Delete that directory before running `dotsync init` again."
    )]
    PartialInitLeftBehind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
        original: Box<DotsyncError>,
    },
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
                current_state: vec![
                    "managed files in home differ from the repo version for this machine scope"
                        .to_string(),
                ],
                forced_overwrites: Vec::new(),
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
                forced_overwrites: Vec::new(),
            },
            DotsyncError::MissingHostname => basic_error_report("missing_hostname", self),
            DotsyncError::RemoteUnreachable { .. } => {
                basic_error_report("remote_unreachable", self)
            }
            // Classified as whatever stopped the init, because that is what
            // the reader has to act on; the message carries both halves.
            DotsyncError::PartialInitLeftBehind { original, .. } => ErrorReport {
                message: self.to_string(),
                ..original.to_error_report()
            },
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
        forced_overwrites: Vec::new(),
    }
}

/// One message when there is one of something, another when there are several.
/// `{n}` in the plural form is the count.
fn one_or_many(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        one.to_string()
    } else {
        many.replace("{n}", &count.to_string())
    }
}

pub(crate) fn error_current_state(error: &DotsyncError) -> Vec<String> {
    match error {
        DotsyncError::InvalidScope { scope } => vec![format!("requested scope: {scope}")],
        DotsyncError::SyncState { path, .. } => {
            vec![format!("sync state path: {}", path.display())]
        }
        DotsyncError::ScopeDiverged {
            scope,
            local_target,
            remote_target,
        } => vec![format!(
            "scope: {scope}; local target: {local_target}; remote target: {remote_target}"
        )],
        DotsyncError::CascadePaused { scope, .. } => vec![format!("paused scope: {scope}")],
        DotsyncError::UnusableCommitPaths { scope, rejected } => rejected
            .iter()
            .map(|rejected| rejected.explain(scope))
            .collect(),
        DotsyncError::StaleCommitPaths { refused, .. } => {
            refused.iter().map(RefusedCommitPath::explain).collect()
        }
        DotsyncError::PausePredatesResolutionCheck { scope } => vec![format!(
            "paused scope: {scope}; the pause holds no record of what the conflicted files contained when it paused."
        )],
        DotsyncError::UnresolvedConflict { scope, paths } => vec![format!(
            "unchanged since the cascade paused at scope `{scope}`: {}",
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )],
        DotsyncError::PausedCascadeInProgress { scope } => vec![format!("paused scope: {scope}")],
        DotsyncError::NotInitialized { path } => vec![format!(
            "expected repo path: {}; standard location: ~/.local/share/dotsync/repo",
            path.display()
        )],
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
        | DotsyncError::RemoteUnreachable { .. }
        | DotsyncError::Jj { .. } => Vec::new(),
        DotsyncError::PartialInitLeftBehind { original, .. } => error_current_state(original),
    }
}

pub(crate) fn jj_error(message: String) -> DotsyncError {
    DotsyncError::Jj { message }
}
