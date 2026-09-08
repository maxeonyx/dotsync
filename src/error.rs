use std::path::PathBuf;

use thiserror::Error;

use crate::drift::FileState;
use crate::sync::FileDrift;

#[derive(Debug, Clone)]
pub struct ErrorReport {
    pub code: &'static str,
    pub message: String,
    pub drifts: Vec<FileDrift>,
    /// The scope a conflict is waiting to be resolved at, when that is what
    /// stopped the run. The same field every successful report carries, from
    /// the same place, so `status` and the run that stopped name it alike.
    pub paused_cascade: Option<String>,
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
    /// The files a stop could not merge, each with every version of it.
    pub conflicts: Vec<ConflictedFile>,
}

/// One file a merge could not resolve, with every version of it.
///
/// Carried structured and whole because resolving a conflict needs all of it:
/// the two sides *and* the version they both changed. An agent shown two sides
/// cannot tell an addition from a deletion, or which side changed which line —
/// Max, 2026-08-19: "Yes the base is supposed to be included."
///
/// The versions are not materialized into home. A config file full of
/// `<<<<<<<` is broken config for exactly as long as the conflict takes to
/// fix, so the application it configures breaks precisely while somebody is
/// fixing it (PLAN §2.3 step 6, settled 2026-08-19). This is the object that
/// carries them instead.
#[derive(Debug, Clone)]
pub struct ConflictedFile {
    pub path: PathBuf,
    /// Where the file stands across the three sides home knows about, so a
    /// conflict between home and this machine's scope is rendered with the
    /// same marker and the same reason `status` gives it. `None` when the two
    /// sides are two scopes rather than home and one: that vocabulary is about
    /// home, and a merge home is not part of has no answer in it.
    pub state: Option<FileState>,
    /// Base first, then the sides, in the order the merge holds them.
    pub versions: Vec<ConflictedVersion>,
}

/// One version of a conflicted file: which part of the merge it is, what to
/// call it, and what it holds.
#[derive(Debug, Clone)]
pub struct ConflictedVersion {
    pub role: ConflictRole,
    /// What this version is, in words — for a side of a sync conflict, the
    /// scope it came from or the fact that it is home's own bytes. jj carries
    /// these on the merge itself, so they are the labels the merge was built
    /// with rather than a second naming of the same thing.
    pub label: String,
    /// `None` when this version does not hold the file at all, which is a
    /// version of it: one side added the file, or one side deleted it.
    pub contents: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictRole {
    /// The version both sides changed.
    Base,
    /// A version that changed it.
    Side,
}

impl ConflictRole {
    pub fn code(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Side => "side",
        }
    }
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

/// One path a named directory matched that the commit left out.
///
/// Not an error and not a refusal: the run succeeds, and this is what it has
/// to say about what it did not do — so it is reported alongside the result
/// rather than instead of one.
#[derive(Debug, Clone)]
pub struct SkippedCommitPath {
    pub path: PathBuf,
    pub reason: SkipReason,
}

/// Why a bulk selection left a path alone.
#[derive(Debug, Clone)]
pub enum SkipReason {
    /// Home holds no change of this machine's own at the path; the state says
    /// which of the ways that happened.
    NotChangedHere(FileState),
    /// A socket, a device, a fifo: something with no file content to record.
    NotARegularFile,
}

impl SkipReason {
    /// The code an agent branches on, in the same field as a file's state
    /// everywhere else — because "why is this file not in the commit" is one
    /// question whether the answer is about content or about the path.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotChangedHere(state) => state.code(),
            Self::NotARegularFile => "not_a_regular_file",
        }
    }

    /// The same thing in words, as a phrase that reads after a path.
    pub fn explain(&self) -> String {
        match self {
            Self::NotChangedHere(state) => state.reason().to_string(),
            Self::NotARegularFile => {
                "not a regular file, so there is no content to record".to_string()
            }
        }
    }
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
            other => format!("`{path}` is {}.", other.reason()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum CommitPathProblem {
    /// Your whole home directory, however it was named.
    HomeRoot,
    Absolute,
    EscapesHome,
    /// Matched neither a file in home nor a file already on the target scope.
    Unmatched {
        home_path: PathBuf,
    },
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
            CommitPathProblem::DotsyncRepoRoot { .. } | CommitPathProblem::InsideDotsyncRepo { .. }
        )
    }

    /// Read by the binary's renderer to say what to name instead of home.
    pub fn is_home_root(&self) -> bool {
        matches!(self.problem, CommitPathProblem::HomeRoot)
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
    /// A path in home that exists and is not a regular file: a fifo, a socket,
    /// a device. Raised before anything opens it, because opening one can
    /// never return.
    #[error("{} is not a regular file, so dotsync cannot record what it holds", path.display())]
    NotARegularFile { path: PathBuf },
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
    #[error("{} conflicted file(s) still hold conflict markers", paths.len())]
    UnresolvedConflict { scope: String, paths: Vec<PathBuf> },
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A scope named as a parent that the repo has no scope for. Carries the
    /// scopes it does have, because the answer to "which ones are there" is
    /// the whole of what the reader needs and dotsync has it in hand.
    #[error("there is no scope called `{parent}`")]
    NoSuchParentScope { parent: String, scopes: Vec<String> },
    /// Creating a scope without saying where it hangs. The graph is
    /// append-only, so this is the only moment that answer can be given.
    #[error("scope `{scope}` needs at least one parent scope")]
    ParentScopeRequired { scope: String, scopes: Vec<String> },
    /// A machine trying to adopt a scope other scopes already hang off.
    /// Config on a scope reaches every machine below it, so such a scope
    /// cannot be one machine's own.
    #[error("scope `{scope}` is shared with {}", children.join(", "))]
    MachineScopeIsShared {
        scope: String,
        children: Vec<String>,
    },
    /// `init --parent` on a machine whose scope the fleet already has. Where
    /// it hangs was decided when it was created and cannot be changed.
    #[error("scope `{scope}` already exists")]
    MachineScopeAlreadyPlaced { scope: String, parents: Vec<String> },
    /// This machine's hostname names no scope in the repo — nothing has been
    /// created for it, or something outside dotsync moved the branch it was
    /// on.
    #[error("this machine has no scope of its own: nothing in the repo is called `{scope}`")]
    MachineScopeMissing {
        scope: String,
        scopes: Vec<String>,
        /// A scope with no parents, when the repo has one. The stop needs a
        /// parent it can name outright, and a root is the answer that assumes
        /// least about where this machine belongs.
        root: Option<String>,
    },
    /// Creating a scope over a name the repo already uses. A scope is created
    /// once, and a branch that is not a scope belongs to whoever pushed it.
    #[error("`{scope}` already exists on the remote")]
    ScopeNameTaken { scope: String },
    /// A scope created under parents that hold different versions of the same
    /// file. Its first commit would be a conflict nobody asked for.
    #[error("scope `{scope}` cannot be created while its parents disagree about {}", files.join(", "))]
    ScopeCreationConflict { scope: String, files: Vec<String> },
    #[error("scope `{scope}` does not exist")]
    InvalidScope { scope: String },
    /// Asked for a file on a scope that does not hold it. An ordinary answer
    /// to an ordinary question — a file exists on the scope that added it and
    /// on every scope below — so it is its own error rather than an internal
    /// failure with a jj message.
    #[error("`{}` is not on scope `{scope}`", path.display())]
    FileNotOnScope { scope: String, path: PathBuf },
    /// The scope graph names a scope this machine's repo has no history for.
    /// Says what it means rather than which of jj's objects is missing:
    /// "bookmark" is a concept dotsync exists to keep out of the user's way.
    #[error("scope `{scope}` is configured, but this machine's repo has no history for it")]
    ScopeNotInRepo { scope: String },
    /// Home and the scope changed the same file differently, so the merge a
    /// sync is could not resolve. The sync stops whole and home is not
    /// touched: home is derived from one commit, so a home built partly from
    /// the old head and partly from the new one makes that single parent a
    /// lie, and an unapplied incoming change then reads as a local edit
    /// undoing it (`spike.ignore/README.md`, rule 3).
    #[error("{} home file(s) conflict with what scope `{scope}` holds", files.len())]
    SyncConflict {
        scope: String,
        files: Vec<ConflictedFile>,
    },
    /// Converging a scope met a file two sides changed differently. The merge
    /// is not written and the scope's head does not move, so nothing about the
    /// pause is history — a rerun computes the same merge from the same
    /// commits and presents the same conflict.
    #[error("converging scope `{scope}` stopped on {} conflicted file(s)", files.len())]
    CascadePaused {
        scope: String,
        files: Vec<ConflictedFile>,
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
    /// Something inside dotsync's own repository handling went wrong. The
    /// detail is jj's and is kept, because it is what a bug report needs — but
    /// the headline is dotsync's, because the reader cannot act on jj's.
    #[error("dotsync could not complete an internal repository operation: {message}")]
    Jj { message: String },
}

impl DotsyncError {
    /// The scope this stop is paused at, when the stop is "a conflict is
    /// waiting for a decision" — the one state with a remedy of its own:
    /// resolve the conflicted files and run `dotsync continue`, or discard the
    /// merge with `dotsync abort`.
    ///
    /// Both the exit code 3 the binary uses and the `paused_cascade` every
    /// payload carries are read off this, so an agent cannot be told the state
    /// through one channel and not the other. It is a property of the state
    /// rather than of which command met it, because it used to be neither: the
    /// run that created the pause exited 3 and the next run that ran into it
    /// exited 1, so an agent that had learned "3 means go and resolve" was told
    /// its very next command had failed for some other reason. Exhaustive on
    /// purpose — a new variant describing this state has to answer the
    /// question rather than inherit a default.
    pub fn paused_scope(&self) -> Option<&str> {
        match self {
            DotsyncError::CascadePaused { scope, .. }
            | DotsyncError::PausedCascadeInProgress { scope }
            | DotsyncError::UnresolvedConflict { scope, .. } => Some(scope),
            DotsyncError::HomeNotSet
            | DotsyncError::NonUtf8Path { .. }
            | DotsyncError::GitSubmodule { .. }
            | DotsyncError::NotARegularFile { .. }
            | DotsyncError::UnusableCommitPaths { .. }
            | DotsyncError::StaleCommitPaths { .. }
            | DotsyncError::Io { .. }
            | DotsyncError::NoSuchParentScope { .. }
            | DotsyncError::ParentScopeRequired { .. }
            | DotsyncError::MachineScopeIsShared { .. }
            | DotsyncError::MachineScopeAlreadyPlaced { .. }
            | DotsyncError::MachineScopeMissing { .. }
            | DotsyncError::ScopeNameTaken { .. }
            | DotsyncError::ScopeCreationConflict { .. }
            | DotsyncError::InvalidScope { .. }
            | DotsyncError::ScopeNotInRepo { .. }
            | DotsyncError::FileNotOnScope { .. }
            | DotsyncError::SyncConflict { .. }
            | DotsyncError::NoPausedCascade
            | DotsyncError::RepoAlreadyExists { .. }
            | DotsyncError::NotInitialized { .. }
            | DotsyncError::MissingHostname
            | DotsyncError::RemoteUnreachable { .. }
            | DotsyncError::Jj { .. } => None,
            // Whatever stopped the init is what the reader has to act on, and
            // an init cannot meet a paused merge — but saying so through the
            // wrapped error keeps that true by construction.
            DotsyncError::PartialInitLeftBehind { original, .. } => original.paused_scope(),
        }
    }

    pub fn to_error_report(&self) -> ErrorReport {
        match self {
            DotsyncError::SyncConflict { files, .. } => ErrorReport {
                code: "sync_conflict",
                message: self.to_string(),
                drifts: Vec::new(),
                paused_cascade: self.paused_scope().map(str::to_string),
                current_state: error_current_state(self),
                forced_overwrites: Vec::new(),
                conflicts: files.clone(),
            },
            DotsyncError::CascadePaused { files, .. } => ErrorReport {
                code: "cascade_paused",
                message: self.to_string(),
                drifts: Vec::new(),
                paused_cascade: self.paused_scope().map(str::to_string),
                current_state: error_current_state(self),
                forced_overwrites: Vec::new(),
                conflicts: files.clone(),
            },
            DotsyncError::InvalidScope { .. } => basic_error_report("invalid_scope", self),
            DotsyncError::ScopeNotInRepo { .. } => basic_error_report("scope_not_in_repo", self),
            DotsyncError::FileNotOnScope { .. } => basic_error_report("file_not_on_scope", self),
            DotsyncError::NoSuchParentScope { .. } => {
                basic_error_report("no_such_parent_scope", self)
            }
            DotsyncError::ParentScopeRequired { .. } => {
                basic_error_report("parent_scope_required", self)
            }
            DotsyncError::MachineScopeIsShared { .. } => {
                basic_error_report("machine_scope_is_shared", self)
            }
            DotsyncError::MachineScopeAlreadyPlaced { .. } => {
                basic_error_report("machine_scope_already_placed", self)
            }
            DotsyncError::MachineScopeMissing { .. } => {
                basic_error_report("machine_scope_missing", self)
            }
            DotsyncError::ScopeNameTaken { .. } => basic_error_report("scope_name_taken", self),
            DotsyncError::ScopeCreationConflict { .. } => {
                basic_error_report("scope_creation_conflict", self)
            }
            DotsyncError::PausedCascadeInProgress { .. } => {
                basic_error_report("paused_cascade_in_progress", self)
            }
            DotsyncError::NoPausedCascade => basic_error_report("no_paused_cascade", self),
            DotsyncError::RepoAlreadyExists { .. } => basic_error_report("repo_exists", self),
            DotsyncError::NotInitialized { path } => ErrorReport {
                code: "not_initialized",
                // Command-neutral: the human rendering names the command
                // that was run, and this message is read by whatever ran it.
                message: format!(
                    "Dotsync could not find its hidden repo at {}. Run `dotsync init <remote-url>` from this home directory first.",
                    path.display()
                ),
                drifts: Vec::new(),
                paused_cascade: self.paused_scope().map(str::to_string),
                current_state: error_current_state(self),
                forced_overwrites: Vec::new(),
                conflicts: Vec::new(),
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
            DotsyncError::Jj { .. } => basic_error_report("internal", self),
            DotsyncError::HomeNotSet => basic_error_report("home_not_set", self),
            DotsyncError::NonUtf8Path { .. } => basic_error_report("non_utf8_path", self),
            DotsyncError::GitSubmodule { .. } => basic_error_report("git_submodule", self),
            DotsyncError::NotARegularFile { .. } => {
                basic_error_report("not_a_regular_file", self)
            }
            DotsyncError::UnusableCommitPaths { .. } => {
                basic_error_report("unusable_commit_paths", self)
            }
            DotsyncError::StaleCommitPaths { .. } => {
                basic_error_report("stale_commit_paths", self)
            }
            DotsyncError::UnresolvedConflict { .. } => {
                basic_error_report("unresolved_conflict", self)
            }
        }
    }
}

pub(crate) fn basic_error_report(code: &'static str, error: &DotsyncError) -> ErrorReport {
    ErrorReport {
        code,
        message: error.to_string(),
        drifts: Vec::new(),
        paused_cascade: error.paused_scope().map(str::to_string),
        current_state: error_current_state(error),
        forced_overwrites: Vec::new(),
        conflicts: Vec::new(),
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
        // One entry per file, for the reason `SyncConflict` has one: one file
        // is one decision to make, and every version of it is printed below.
        DotsyncError::CascadePaused { scope, files } => files
            .iter()
            .map(|file| {
                format!(
                    "`{}` was changed differently by two of the histories merging into `{scope}`",
                    file.path.display()
                )
            })
            .collect(),
        DotsyncError::UnusableCommitPaths { scope, rejected } => rejected
            .iter()
            .map(|rejected| rejected.explain(scope))
            .collect(),
        DotsyncError::StaleCommitPaths { refused, .. } => {
            refused.iter().map(RefusedCommitPath::explain).collect()
        }
        DotsyncError::UnresolvedConflict { scope, paths } => vec![format!(
            "still holding conflict markers, for the merge paused at scope `{scope}`: {}",
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )],
        DotsyncError::PausedCascadeInProgress { scope } => vec![format!("paused scope: {scope}")],
        // One entry per file, because one file is one thing to resolve. Every
        // version of it is printed in full below the teaching block and
        // carried in the payload; this is the list of decisions to make.
        DotsyncError::SyncConflict { scope, files } => files
            .iter()
            .map(|file| {
                format!(
                    "`{}` was changed in home and on `{scope}` since this machine last synced it",
                    file.path.display()
                )
            })
            .collect(),
        DotsyncError::NotInitialized { path } => vec![format!(
            "expected repo path: {}; standard location: ~/.local/share/dotsync/repo",
            path.display()
        )],
        DotsyncError::NoSuchParentScope { parent, scopes }
        | DotsyncError::MachineScopeMissing {
            scope: parent,
            scopes,
            ..
        } => vec![scopes_in_the_repo(parent, scopes)],
        DotsyncError::ParentScopeRequired { scope, scopes } => vec![format!(
            "creating scope: {scope}; {}",
            if scopes.is_empty() {
                "this remote has no scopes yet".to_string()
            } else {
                format!("scopes in the repo: {}", scopes.join(", "))
            }
        )],
        DotsyncError::MachineScopeIsShared { scope, children } => vec![format!(
            "scope: {scope}; scopes hanging off it: {}",
            children.join(", ")
        )],
        DotsyncError::MachineScopeAlreadyPlaced { scope, parents } => vec![format!(
            "scope: {scope}; it already hangs off: {}",
            parents.join(", ")
        )],
        DotsyncError::ScopeCreationConflict { scope, files } => vec![format!(
            "scope: {scope}; its parents hold different versions of: {}",
            files.join(", ")
        )],
        DotsyncError::HomeNotSet
        | DotsyncError::NonUtf8Path { .. }
        | DotsyncError::GitSubmodule { .. }
        | DotsyncError::NotARegularFile { .. }
        | DotsyncError::NoPausedCascade
        | DotsyncError::Io { .. }
        | DotsyncError::ScopeNameTaken { .. }
        | DotsyncError::ScopeNotInRepo { .. }
        | DotsyncError::FileNotOnScope { .. }
        | DotsyncError::RepoAlreadyExists { .. }
        | DotsyncError::MissingHostname
        | DotsyncError::RemoteUnreachable { .. }
        | DotsyncError::Jj { .. } => Vec::new(),
        DotsyncError::PartialInitLeftBehind { original, .. } => error_current_state(original),
    }
}

/// What the repo does have, for a stop about a scope name it does not.
fn scopes_in_the_repo(named: &str, scopes: &[String]) -> String {
    if scopes.is_empty() {
        format!("named scope: {named}; this remote has no scopes yet")
    } else {
        format!(
            "named scope: {named}; scopes in the repo: {}",
            scopes.join(", ")
        )
    }
}

pub(crate) fn jj_error(message: String) -> DotsyncError {
    DotsyncError::Jj { message }
}
