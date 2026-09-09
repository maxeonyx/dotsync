use std::path::PathBuf;

use thiserror::Error;

use crate::drift::FileState;

/// Everything a stop has to say, read off the error itself.
///
/// One method builds this — `DotsyncError::explain` — so a variant cannot be
/// given a code in one place, facts in another and teaching text in a third,
/// which is how a variant's teaching text came to be unreachable once already.
/// A new variant answers every question here or it does not compile.
#[derive(Debug, Clone)]
pub struct Explanation {
    pub code: &'static str,
    pub message: String,
    /// The scope a merge is waiting to be resolved at, when that is what
    /// stopped the run. The same field every successful report carries, so
    /// `status` and the run that stopped name it alike.
    pub paused_cascade: Option<String>,
    /// What dotsync found, one fact per entry.
    ///
    /// A list rather than a paragraph because a run that refused three paths
    /// found three things: joining them for a person to read is a decision for
    /// whoever is rendering, and a reader that has to split them back apart on
    /// a newline is reading a rendering rather than an answer.
    pub current_state: Vec<String>,
    /// The files a stop could not merge, each with every version of it.
    pub conflicts: Vec<ConflictedFile>,
    /// What to teach the reader, for a stop with somewhere to go from here.
    /// `None` is a stop whose one line is the whole of it — a missing HOME, a
    /// failure inside jj — where there is no flow to explain.
    pub teaching: Option<Teaching>,
}

/// The teaching block: what dotsync was doing, what it expected, why it
/// stopped, and what to do about it.
#[derive(Debug, Clone)]
pub struct Teaching {
    pub summary: String,
    pub what_dotsync_does: String,
    pub this_flow: String,
    pub expected: String,
    pub why_stopped: String,
    /// One invocation or decision per entry, in the order to try them.
    pub next_steps: Vec<String>,
}

impl Teaching {
    fn new(
        summary: &str,
        what_dotsync_does: &str,
        this_flow: &str,
        expected: &str,
        why_stopped: &str,
        next_steps: &[&str],
    ) -> Self {
        Self {
            summary: summary.to_string(),
            what_dotsync_does: what_dotsync_does.to_string(),
            this_flow: this_flow.to_string(),
            expected: expected.to_string(),
            why_stopped: why_stopped.to_string(),
            next_steps: next_steps.iter().map(|step| step.to_string()).collect(),
        }
    }
}

impl Explanation {
    fn stop(code: &'static str, error: &DotsyncError) -> Self {
        Self {
            code,
            message: error.to_string(),
            paused_cascade: None,
            current_state: Vec::new(),
            conflicts: Vec::new(),
            teaching: None,
        }
    }

    fn state(mut self, facts: Vec<String>) -> Self {
        self.current_state = facts;
        self
    }

    fn conflicts(mut self, files: Vec<ConflictedFile>) -> Self {
        self.conflicts = files;
        self
    }

    fn paused_at(mut self, scope: &str) -> Self {
        self.paused_cascade = Some(scope.to_string());
        self
    }

    fn teaching(mut self, teaching: Teaching) -> Self {
        self.teaching = Some(teaching);
        self
    }
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

    /// Read where the advice about dotsync's own state is written.
    fn is_dotsync_state(&self) -> bool {
        matches!(
            self.problem,
            CommitPathProblem::DotsyncRepoRoot { .. } | CommitPathProblem::InsideDotsyncRepo { .. }
        )
    }

    /// Read where the advice about naming something other than home is written.
    fn is_home_root(&self) -> bool {
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
    /// `discard` naming a path that holds no change of this machine's own.
    /// There is nothing at it to decide against — the path is a typo, or the
    /// file is already whatever the scope says it is — and a run that answered
    /// "discarded 0 file(s)" would read as having done the job.
    #[error("{}", one_or_many(paths.len(), "there is no change of yours to discard at the path you named", "there is no change of yours to discard at {n} of the paths you named"))]
    NothingToDiscard { paths: Vec<PathBuf> },
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
        /// This machine's own scope, when `scope` is neither it nor above it.
        ///
        /// A cascade from a shared ancestor merges into scopes this machine is
        /// not on, so the merge waiting for a decision can be another
        /// machine's. The file in home is then a resolution buffer holding
        /// somebody else's config, and that has to be said out loud: without
        /// it the agent reads another machine's settings as its own. `None`
        /// when the merge is on this machine's own path, where the resolution
        /// *is* its config and there is nothing to warn about.
        borrowed_from: Option<String>,
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
    /// Command-neutral on purpose: the human rendering names the command that
    /// was run, and this message is read by whatever ran it.
    #[error(
        "Dotsync could not find its hidden repo at {}. Run `dotsync init <remote-url>` from this home directory first.",
        path.display()
    )]
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
    /// What this stop is, what it found, and where to go from here.
    ///
    /// `invocation` is what the user typed, when they typed something dotsync
    /// recognises — the words, not the name the payload uses for the command.
    /// A stop that ends by naming the command to rerun has to name theirs, and
    /// has to name one that runs: before this, every one of them said `dotsync
    /// status`, including to the agent who ran `dotsync commit`.
    pub fn explain(&self, invocation: Option<&str>) -> Explanation {
        match self {
        Self::SyncConflict { scope, files } => Explanation::stop("sync_conflict", self)
            .conflicts(files.clone())
            .state(files.iter().map(|file| format!("`{}` was changed in home and on `{scope}` since this machine last synced it", file.path.display())).collect())
            .teaching(Teaching::new(
            if files.len() == 1 {
                "home and this machine's scope both changed the same file"
            } else {
                "home and this machine's scope both changed the same files"
            },
            "Dotsync keeps its hidden repo as the source of truth for your home-directory config, and a sync merges what the scopes hold now with whatever you have edited in home since the last one. An edit dotsync can merge around is carried across the sync; nothing has to be committed first.",
            "This sync flow merged three versions of every managed file: the version this machine last synced, the version in home now, and the version the scope holds now.",
            "It expects at most one of home and the scope to have changed each file — or, where both did, to have changed different lines of it.",
            "Both sides changed the same part of the same file, so there is no merged version dotsync can work out on its own. Nothing was written: home is untouched, and the incoming changes to every other file are held back with it, because home is derived from one commit and a home built half from each side would make the next run read those incoming changes as edits of yours undoing them.",
            &[
                "read the three versions of each file below, decide what the file should hold, and write that into the file at its real path in home.",
                &format!(
                    "then record your decision on a scope: `dotsync commit {scope} -m \"message\" -- <path>`. That is what makes it everybody's version, and it leaves this sync nothing left to merge."
                ),
                "or, if the version the scope already holds is the one you want, run `dotsync discard <path>` and let the sync finish. That throws away what is in home at the paths you name and nothing else.",
            ])),
        Self::CascadePaused {
            scope,
            borrowed_from,
            files,
        } => Explanation::stop("cascade_paused", self)
            .conflicts(files.clone())
            .paused_at(scope)
            .state(files.iter().map(|file| format!("`{}` was changed differently by two of the histories merging into `{scope}`", file.path.display())).collect())
            .teaching(Teaching::new(
            &format!(
                "paused at scope `{scope}`: two histories changed the same {} differently",
                if files.len() == 1 { "file" } else { "files" }
            ),
            "Dotsync layers scopes down to each machine: a change recorded on one scope is merged into the scopes below it, and a change another machine published is merged into what this one holds. Both of those are the same merge, and it runs over the whole scope graph on every command that writes.",
            &format!("This run was merging everything that reaches `{scope}` — what this machine has, what other machines have published, and what its parent scopes now hold — into one new version of it."),
            "It expects at most one of those histories to have changed each file, or, where more than one did, to have changed different lines of it.",
            &match borrowed_from {
                None => "More than one of them changed the same part of the same file, so there is no merged version dotsync can work out on its own. Nothing was written: the scope's head has not moved and no other machine can see this state.".to_string(),
                // The mode switch, stated where the reader cannot miss it and
                // before it starts editing: the file in home is about to stop
                // being this machine's config.
                Some(machine_scope) => format!("More than one of them changed the same part of the same file, so there is no merged version dotsync can work out on its own. Nothing was written: the scope's head has not moved and no other machine can see this state.\n\nThis machine is `{machine_scope}`, which does not descend from `{scope}`, so what you are resolving is not this machine's config — the file in home is a scratch buffer for `{scope}`'s merge. `{machine_scope}`'s own version comes back after `dotsync continue` or `dotsync abort`."),
            },
            &[
                "read the versions of each file below, decide what it should hold, and write that into the file at its real path in home; take out any marker lines you paste in.",
                "run `dotsync continue` from the same machine to record your decision and finish converging. Leaving a file exactly as it is says you decided on the version already there.",
                "or run `dotsync abort` from the same machine to discard it; that reverts the conflicted files in home to this machine's scope state, so save anything you want to keep outside home first.",
            ])),
        Self::UnresolvedConflict { scope, paths } => Explanation::stop("unresolved_conflict", self)
            .paused_at(scope)
            .state(vec![format!("still holding conflict markers, for the merge paused at scope `{scope}`: {}", display_paths(paths))])
            .teaching(Teaching::new(
            "conflict markers left in the resolution",
            "Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config. Where two branches changed one file differently, the cascade pauses and asks you for the merged contents.",
            "This continue flow reads each conflicted file back out of your home directory and records what it finds there as the resolution.",
            "It expects to find config: whatever you decided the file should hold.",
            &format!(
                "The file below still has conflict markers in it, so it is a resolution somebody stopped half way through. Recorded as the merged contents they would cascade into every scope below `{scope}` and every other machine would then sync `<<<<<<<` into its live config."
            ),
            &[
                &format!(
                    "the versions to choose between are the ones the pause printed; `dotsync view --scope {scope} --file {}` prints the one on the scope again.",
                    paths
                        .first()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default()
                ),
                "take the marker lines out, leave the contents you want, then run `dotsync continue`.",
                "or run `dotsync abort` to discard the merge; that reverts the conflicted files in home to this machine's scope state, so save anything you want to keep outside home first.",
                "if the file is genuinely meant to contain lines of `<<<<<<<` and `>>>>>>>`, `continue` cannot record it: abort, and commit it to the scope directly instead.",
            ])),
        Self::PausedCascadeInProgress { scope } => Explanation::stop("paused_cascade_in_progress", self)
            .paused_at(scope)
            .state(vec![format!("paused scope: {scope}")])
            .teaching(Teaching::new(
            "paused cascade in progress",
            "Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config.",
            "This commit flow was about to start a new scoped commit, but a previous cascade is still paused for conflict resolution.",
            "It expects exactly one cascade to be active at a time so commit history, conflict resolution, and home sync state stay aligned.",
            "Dotsync stopped before fetching, committing, or syncing because starting another commit would hide the real paused-cascade task and may mutate unrelated scope state.",
            &[
                "edit each conflicted file at its real path in home so it holds the merged contents you want; take out any marker lines you paste in.",
                "run `dotsync continue` to finish the paused cascade.",
                "or run `dotsync abort` to discard the paused cascade; that reverts the conflicted files in home to this machine's scope state.",
                "after `dotsync continue` succeeds, rerun the new commit if it is still needed.",
            ])),
        Self::UnusableCommitPaths { scope, rejected } => {
            let mut steps = vec![format!(
                "name paths relative to your home directory: `dotsync commit {scope} -m \"message\" -- .config/fish/config.fish`."
            )];
            steps.push(
                "do not use `~/`, absolute paths, or `..`; dotsync resolves every path against your home directory already, and records it verbatim.".to_string(),
            );
            if rejected.iter().any(|rejected| rejected.is_home_root()) {
                steps.push(
                    "name the directories or files you actually mean: `dotsync commit <scope> -m \"message\" -- .config/fish/ .bashrc`. Dotsync will not sweep a whole home directory onto a scope."
                        .to_string(),
                );
            }
            if rejected.iter().any(|rejected| rejected.is_dotsync_state()) {
                steps.push(
                    "commit the config files you edited instead; dotsync's hidden repo is not config and cannot travel on a scope."
                        .to_string(),
                );
                steps.push(
                    "to add a scope, run `dotsync create-scope <name> --parent <scope>`; scopes are branches in dotsync's own repo, not files in home."
                        .to_string(),
                );
            }
            steps.push("run `dotsync status` to see which managed files changed.".to_string());

            Explanation::stop("unusable_commit_paths", self)
                .state(rejected.iter().map(|rejected| rejected.explain(scope)).collect())
                .teaching(Teaching::new(
                if rejected.len() == 1 {
                    "cannot commit that path"
                } else {
                    "cannot commit those paths"
                },
                "Dotsync records the home files you name onto a scope branch, then cascades that scope so every machine sharing it receives the change. Every file on a scope is written back into home on each of those machines.",
                "This commit flow resolves each path you name against your home directory, checks that it is a config file dotsync may record, and commits the ones that changed.",
                "It expects every path you name to be a config file inside your home directory, named relative to it, and to exist either in home or on the target scope already.",
                "Dotsync stopped before recording anything. A commit records every path you named or none of them, so fixing the paths above and rerunning the same command is safe.",
                &steps.iter().map(String::as_str).collect::<Vec<_>>()))
        }
        Self::StaleCommitPaths { scope, refused } => Explanation::stop("stale_commit_paths", self)
            .state(refused.iter().map(RefusedCommitPath::explain).collect())
            .teaching(Teaching::new(
            if refused.len() == 1 {
                "cannot commit a file this machine has not changed"
            } else {
                "cannot commit files this machine has not changed"
            },
            "Dotsync records the home files you name onto a scope branch and cascades them to every machine sharing it. Plain `dotsync` goes the other way, writing what the scopes hold back into home.",
            "This commit flow reads each path you named across three sides: what dotsync last synced to this machine, what is in home now, and what the scopes hold now.",
            "It expects the paths you name to hold a change you made in home since the last sync.",
            "Recording these would put older bytes back on the scope and cascade them, silently reverting whoever published the change that is already there.",
            &[
                "run `dotsync` to bring this machine up to date; the incoming change is written into home, and an incoming deletion removes the file.",
                "then edit the file in home if you still want a change of your own, and commit it. To bring back a file another machine deleted, recreate it in home after syncing and commit that.",
                &format!(
                    "there is no way to skip the middle step: a commit of home's older bytes onto `{scope}` is the revert, so making it deliberately means syncing, writing what you want, and committing that."
                ),
            ])),
        Self::NothingToDiscard { paths } => Explanation::stop("nothing_to_discard", self)
            .state(paths.iter().map(|path| format!("`{}` holds no change of this machine's own: dotsync either does not manage it, or home already holds what the scope says it should.", path.display())).collect())
            .teaching(Teaching::new(
            if paths.len() == 1 {
                "there is nothing of yours to discard there"
            } else {
                "there is nothing of yours to discard at those paths"
            },
            "Dotsync writes what the scopes hold into your home directory, and carries a file you have edited since across each sync rather than overwriting it. That edit stays yours until you commit it to a scope — or decide against it.",
            "This discard flow throws away what home holds at the paths you name and writes the scope's version of them instead.",
            "It expects each path you name to be one of the changes `dotsync status` lists.",
            "Dotsync wrote nothing. Discarding is the one thing it does that cannot be undone, so a path it has nothing to discard at is a mistyped path far more often than it is a change of mind.",
            &[
                "run `dotsync status` to see the changes there are, and name one of those.",
                "to record a change instead of discarding it, run `dotsync commit <scope> -m \"message\" -- <paths...>`.",
            ])),
        // Raised by every command that takes a scope name, so it teaches about
        // scopes rather than about whichever command the reader happened to be
        // running: `view --scope` used to get a bare one-liner for the mistake
        // `commit` explained in full.
        Self::InvalidScope { .. } => Explanation::stop("invalid_scope", self)
            .teaching(Teaching::new(
            "invalid scope",
            "Dotsync stores dotfiles in a scope DAG so shared config can live on shared ancestor scopes and machine-specific config can stay isolated on leaf scopes.",
            "This flow resolves the scope you named against the scope graph, which dotsync reads off its own repo: every scope is a branch, created once and never moved.",
            "It expects the scope you name to be one of them.",
            "Dotsync stopped because there is no such scope: it can neither place a change on one nor show you what one holds.",
            &[
                "run `dotsync view` to list the scopes that do exist.",
                "then name one of those. For a commit, pick the root-est appropriate ancestor scope that should own the change.",
            ])),
        Self::NotARegularFile { .. } => Explanation::stop("not_a_regular_file", self)
            .teaching(Teaching::new(
            "that is not a regular file",
            "Dotsync records the bytes it finds at a path and writes those same bytes back to that path on every machine sharing the scope.",
            "This flow read your home directory to see what each managed path holds now.",
            "It expects every path it reads to be a regular file, or a link to one.",
            "There are no bytes to read: a fifo, a socket or a device is a thing to talk to, not a thing to copy. Dotsync stops rather than opening it, because opening one waits forever for something that is never going to write to it.",
            &[
                "leave it out of the commit, or move it out of the directory you named. A directory selection steps around one on its own and says so.",
                "if this path is one dotsync already tracks, put the real file back — or commit the deletion once it is gone.",
            ])),
        Self::FileNotOnScope { .. } => Explanation::stop("file_not_on_scope", self)
            .teaching(Teaching::new(
            "that file is not on that scope",
            "Dotsync stores dotfiles in a scope DAG, and a file lives on the scope it was committed to. Every scope below that one inherits it through the cascade, so the same file is visible on many scopes and absent from the ones above it.",
            "This view flow reads the file out of the tree that one scope holds.",
            "It expects that scope to hold the file — the scope it was committed to, or one below it.",
            "Dotsync stopped rather than printing nothing: empty output would read exactly like an empty file.",
            &[
                "run `dotsync view --file <path>` to see which scopes hold it.",
                "run `dotsync view --scope <scope>` to see what that scope does hold.",
            ])),
        // Every other command carries on against the last state it fetched
        // and says so in a note. `init` is the one whose whole job is to reach
        // the remote, so for it this really is a stop.
        Self::RemoteUnreachable { reason } => Explanation::stop("remote_unreachable", self)
            // Git's own words, in the payload as well as in the rendering:
            // what stopped the clone is the whole of what there is to act on.
            .state(vec![reason.clone()])
            .teaching(Teaching::new(
            "could not reach the remote",
            "Dotsync keeps your config in a hidden repo and shares it between your machines through a git remote, so every machine starts from what the others have already published.",
            "This init flow clones that remote into the hidden repo, works out which scopes this machine belongs to, and syncs them into home.",
            "It expects the remote URL you gave it to be reachable from this machine now.",
            "Dotsync stopped rather than starting from an empty history: scopes created here would collide with the ones already on the remote the first time this machine reached it.",
            &[
                "check the remote URL, this machine's network, and your credentials for that remote.",
                "then run `dotsync init <remote-url>` again.",
            ])),
        // The original failure is what there is to fix, so it renders in full;
        // the leftover directory is the extra step the retry now needs.
        // Classified and taught as whatever stopped the init, because that is
        // still the thing to fix; the leftover directory is an extra step the
        // retry needs, and it rides in the message.
        Self::PartialInitLeftBehind { original, .. } => Explanation {
            message: self.to_string(),
            ..original.explain(invocation)
        },
        Self::NotInitialized { path } => Explanation::stop("not_initialized", self)
            .state(vec![format!("expected repo path: {}; standard location: ~/.local/share/dotsync/repo", path.display())])
            .teaching(Teaching::new(
            "not initialized",
            "Dotsync keeps your config in a hidden repo at ~/.local/share/dotsync/repo and syncs the scopes this machine belongs to into your home directory. Every command works against that repo.",
            "This flow opened that repo to find out what this machine's scopes hold.",
            "It expects `dotsync init <remote-url>` to have been run in this home directory already, which is what creates the repo.",
            "There is nothing to compare your home directory against, so dotsync cannot answer for it.",
            &[
                "run `dotsync init <remote-url>` from this home directory. The remote URL is the git remote that stores your dotsync repo.",
                &format!("then rerun `{}`.", invocation.unwrap_or("dotsync")),
            ])),
        // Naming the repo path in the summary would be pointing an agent at
        // the one directory it is told never to touch; what it needs is that
        // this machine is already set up, and which command does the thing it
        // was reaching for.
        Self::RepoAlreadyExists { .. } => Explanation::stop("repo_exists", self)
            .teaching(Teaching::new(
            "already initialized",
            "Dotsync keeps your config in a hidden repo at ~/.local/share/dotsync/repo, created once per machine by `dotsync init` and used by every command after that.",
            "This init flow clones the remote into that repo and works out which scopes this machine belongs to.",
            "It expects to be the thing that creates the repo, so it refuses to run over one that exists.",
            "Cloning over an existing repo would discard whatever this machine has committed but not published.",
            &[
                "run `dotsync` to sync this machine, which is what `init` would have finished by doing.",
                "run `dotsync status` to see what this machine has changed, and `dotsync view` to see the scopes it knows about.",
                "to point this machine at a different remote, move the existing repo aside by hand first — dotsync has no command for that yet.",
            ])),
        Self::NoSuchParentScope { parent, scopes } => Explanation::stop("no_such_parent_scope", self)
            .state(vec![scopes_in_the_repo(parent, scopes)])
            .teaching(Teaching::new(
            "that scope is not in the repo",
            THE_SCOPE_GRAPH,
            "This flow was about to create a scope, hanging it off the scopes you named.",
            "It expects every parent you name to be a scope this repo already has, because a scope is created where its parents are and cannot be moved afterwards.",
            "A scope hung off a name nothing answers to would receive nothing and reach nothing.",
            &[
                "run `dotsync view` to see the scopes there are.",
                "then name the one this config should come from: the root-est scope whose machines should all share it.",
                "to create the parent itself first, run `dotsync create-scope <name> --parent <scope>`.",
            ])),
        Self::ParentScopeRequired { scope, scopes } => Explanation::stop("parent_scope_required", self)
            .state(vec![format!("creating scope: {scope}; {}", if scopes.is_empty() { "this remote has no scopes yet".to_string() } else { format!("scopes in the repo: {}", scopes.join(", ")) })])
            .teaching(Teaching::new(
            "say where this scope hangs",
            THE_SCOPE_GRAPH,
            "This flow was about to create a scope, and a scope is created where its parents are.",
            "It expects at least one `--parent`, because that is what decides which config this scope receives and which machines a change on it reaches.",
            "Nothing else says it. A hostname cannot tell a `home-linux` from a `work-linux`, and the graph is append-only, so a scope put in the wrong place cannot be moved afterwards — this is the only moment the answer can be given.",
            &[
                "run `dotsync view` on a machine that is already set up to see the scopes there are.",
                &format!(
                    "then name the one this config should come from: `--parent <scope>`, or several for a `{scope}` that inherits from more than one."
                ),
            ])),
        Self::MachineScopeIsShared { scope, children } => Explanation::stop("machine_scope_is_shared", self)
            .state(vec![format!("scope: {scope}; scopes hanging off it: {}", children.join(", "))])
            .teaching(Teaching::new(
            "that scope is shared with other machines",
            THE_SCOPE_GRAPH,
            "This init flow was about to adopt the scope named after this machine's hostname as the scope only this machine holds.",
            "It expects that scope to be a leaf: nothing else hanging off it, so nothing it holds reaches anywhere else.",
            &format!(
                "`{scope}` is what `{}` inherit from, so config committed to it would reach them as well — which is the opposite of what a machine's own scope is for.",
                children.join("` and `")
            ),
            &[
                "set DOTSYNC_HOSTNAME to a name that is this machine's alone, then run `dotsync init <remote-url> --parent <scope>` again.",
                "run `dotsync view` to see which scopes exist and what hangs off them.",
            ])),
        Self::MachineScopeAlreadyPlaced { scope, parents } => Explanation::stop("machine_scope_already_placed", self)
            .state(vec![format!("scope: {scope}; it already hangs off: {}", parents.join(", "))])
            .teaching(Teaching::new(
            "this machine's scope already exists",
            THE_SCOPE_GRAPH,
            "This init flow looked for the scope named after this machine's hostname, and found it.",
            "It expects to be told where to hang a scope it is creating, and nothing when it is adopting one that exists.",
            "Where a scope hangs was decided when it was created, and the graph is append-only, so `--parent` here could only be ignored or wrong.",
            &[
                "run `dotsync init <remote-url>` without `--parent` to adopt this machine's scope as it stands.",
                "run `dotsync view` to see where it hangs.",
            ])),
        Self::MachineScopeMissing { scope, scopes, root } => Explanation::stop("machine_scope_missing", self)
            .state(vec![scopes_in_the_repo(scope, scopes)])
            .teaching(Teaching::new(
            "this machine has no scope",
            THE_SCOPE_GRAPH,
            "This flow looked for the scope named after this machine's hostname, which is the one this machine syncs into home.",
            "It expects that scope to be in the repo: `dotsync init` creates it when the machine joins.",
            "Without it there is nothing that says what belongs on this machine, so there is nothing to sync, and nowhere to record a change of its own.",
            &[
                "run `dotsync view` to see the scopes there are.",
                &format!(
                    "give this machine a scope again with `dotsync create-scope {scope} --parent <the scope its config should come from>`."
                ),
                &match root {
                    Some(root) => format!(
                        "if you do not know which, `dotsync create-scope {scope} --parent {root}` hangs it off the root scope: everything every machine shares, and nothing else."
                    ),
                    None => "this repo has no scopes at all, so there is nothing to hang one off — `dotsync init <remote-url>` against the remote that has them.".to_string(),
                },
                "if this machine is meant to be called something else, set DOTSYNC_HOSTNAME and rerun.",
            ])),
        Self::ScopeNameTaken { scope } => Explanation::stop("scope_name_taken", self)
            .teaching(Teaching::new(
            "that name is taken",
            THE_SCOPE_GRAPH,
            "This flow was about to create a scope, which means creating a branch of that name in dotsync's repo.",
            "It expects the name to be free, on this machine and on the remote.",
            &format!(
                "Something already answers to `{scope}` — a scope somebody created, or a branch pushed by something that is not dotsync. Writing over it would take it away from whoever is using it."
            ),
            &[
                "run `dotsync view` to see whether it is already a scope, in which case there is nothing to create.",
                "otherwise pick another name.",
            ])),
        Self::ScopeCreationConflict { scope, files } => Explanation::stop("scope_creation_conflict", self)
            .state(vec![format!("scope: {scope}; its parents hold different versions of: {}", files.join(", "))])
            .teaching(Teaching::new(
            "those parent scopes disagree",
            THE_SCOPE_GRAPH,
            "This flow was about to create a scope holding everything its parents hold, which for more than one parent means merging what they hold.",
            "It expects the parents to agree about every file they share, or to have changed different lines of it.",
            &format!(
                "The new scope's first commit would be a conflict in {} that nobody asked for and no command is waiting to resolve.",
                files.join(", ")
            ),
            &[
                "run `dotsync view --file <path>` to see which scopes hold the file and what each of them says.",
                "commit one agreed version to a scope both parents inherit from, let it cascade, then create the scope.",
                "or create the scope under one parent for now.",
            ])),
        // Stops with nowhere to go: an environment dotsync cannot work in, a
        // failure inside jj, a question asked of a machine that is not in that
        // state. Their one line is the whole of what there is to say, and a
        // teaching block would be filler around it.
        Self::HomeNotSet => Explanation::stop("home_not_set", self),
        Self::NonUtf8Path { .. } => Explanation::stop("non_utf8_path", self),
        Self::GitSubmodule { .. } => Explanation::stop("git_submodule", self),
        Self::NoPausedCascade => Explanation::stop("no_paused_cascade", self),
        Self::Io { .. } => Explanation::stop("io", self),
        Self::ScopeNotInRepo { .. } => Explanation::stop("scope_not_in_repo", self),
        Self::MissingHostname => Explanation::stop("missing_hostname", self),
        Self::Jj { .. } => Explanation::stop("internal", self),
        }
    }
}

/// What dotsync does, for every stop about the shape of the graph.
const THE_SCOPE_GRAPH: &str = "Dotsync stores dotfiles in a DAG of scopes, and a machine holds everything its own scope holds plus everything the scopes above it hold. Every scope is a branch in dotsync's hidden repo, created once, where its parents are.";

/// One message when there is one of something, another when there are several.
/// `{n}` in the plural form is the count.
fn one_or_many(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        one.to_string()
    } else {
        many.replace("{n}", &count.to_string())
    }
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
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
