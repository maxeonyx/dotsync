use clap::{Parser, Subcommand, ValueEnum};
use dotsync::{
    abort_paused_cascade, commit_and_sync, continue_after_conflict, diff_home, init, status, sync,
    view, CommitFailure, CommitOptions, DiffReport, DotsyncError, DotsyncPaths, ForceScope, Run,
    UnreachableRemote, ViewReport,
};
mod render;
use serde_json::json;
use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

const TOP_LEVEL_ABOUT: &str = "Agent-first dotfile sync";

const TOP_LEVEL_LONG_ABOUT: &str = "dotsync keeps a hidden repo at ~/.local/share/dotsync/repo and syncs the current machine scope into your home directory.

A scope is a branch in the dotsync DAG. Shared config lives on ancestor scopes such as `all` or `linux`; machine-specific config lives on leaf scopes such as your hostname.

Basic workflow:
  - plain `dotsync` syncs your current machine scope into home
  - edit files in home, then run `dotsync commit <scope> -m \"message\" <path>...` to record the change on the right scope
  - run `dotsync continue` if a cascade pauses for conflicts
  - run `dotsync abort` to discard a paused cascade";

const TOP_LEVEL_AFTER_HELP: &str = "Exit codes:
  0  the command did what it says
  1  dotsync stopped, or `dotsync diff` found changes — with `--output json`,
     `status` is \"error\" for a stop and \"ok\" for changes `diff` found
  2  the command line was wrong
  3  a paused cascade is waiting; resolve the conflicted files in home and run
     `dotsync continue`, or run `dotsync abort` to discard it

Examples:
  $ dotsync
  $ dotsync commit linux -m \"add bashrc\" .bashrc
  $ dotsync init <url>";

const INIT_ABOUT: &str = "Clone or join a dotsync remote";

const INIT_LONG_ABOUT: &str = "REMOTE_URL is the git remote that stores your dotsync repo.

`dotsync init` clones the repo into ~/.local/share/dotsync/repo, detects this machine, sets up any missing scope branches for its OS and machine, and syncs the resulting machine scope into home.

If REMOTE_URL is omitted, dotsync asks for it.";

const INIT_REMOTE_URL_USAGE: &str = "init needs the repo remote URL

Usage:
  dotsync init <remote-url>

The remote URL is the git remote that stores your dotsync repo.

Example:
  dotsync init git@github.com:maxeonyx/dotfiles.git";

const COMMIT_ABOUT: &str = "Commit selected home changes to a scope, cascade, sync, and push";

const COMMIT_LONG_ABOUT: &str = "PATHS are home-relative files or directories to record on SCOPE. Omit them to record every managed file this machine has changed, which is exactly the set `dotsync status` lists as changes.

dotsync compares three sides of every path: what it last synced to this machine, what is in home now, and what the scopes hold now. A path whose home content is simply older than the repo has not been changed here, so naming it is refused and pointed at plain `dotsync` instead — committing it would revert whoever published the change that is already there.

Naming a directory records what this machine changed under it, adds what is new under it, and steps around what another machine changed. Omitting the paths records only changes to files dotsync already tracks — it never adds anything, which is why a new file has to be opted into by naming it or the directory it is in.

A run reports both halves of what that came to: `newly_tracked` for the files it put on the scope for the first time, and `skipped_paths` for the files under a named directory it left alone. Both appear in `--output json` and as notes on stderr, alongside `forced_overwrites`.

`--force` means \"home wins anyway\", and on `commit` it applies only to the paths you name. That is deliberately different from `--force` on plain `dotsync` and on `continue`, which name no paths and so overwrite every drifted file. So `dotsync commit linux -m msg --force -- .bashrc` overwrites `.bashrc` and nothing else, while `dotsync --force` overwrites everything that drifted.

Forced paths are listed in the run's `--output json` under `forced_overwrites`.";

const CONTINUE_ABOUT: &str = "Continue a paused merge cascade after resolving conflicts";
const ABORT_ABOUT: &str = "Abort a paused merge cascade and restore the pre-pause state";

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = TOP_LEVEL_ABOUT,
    long_about = TOP_LEVEL_LONG_ABOUT,
    after_help = TOP_LEVEL_AFTER_HELP,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Output format
    #[arg(long = "output", value_enum, default_value = "human", global = true)]
    output_format: OutputFormat,

    /// Overwrite drifted home files: every one on plain `dotsync` and
    /// `continue`, only the paths you name on `commit`
    #[arg(long, global = true)]
    force: bool,
}

#[derive(Debug, Clone)]
enum Action {
    Sync {
        force: bool,
    },
    Init {
        remote_url: InitRemote,
    },
    Commit {
        scope: String,
        message: String,
        force: bool,
        paths: Vec<PathBuf>,
    },
    Continue {
        force: bool,
    },
    Abort,
    Status,
    Diff,
    View {
        scope: Option<String>,
        file: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = INIT_ABOUT, long_about = INIT_LONG_ABOUT)]
    Init {
        /// Git remote URL or local path for the dotsync repo
        remote_url: Option<String>,
    },
    #[command(about = COMMIT_ABOUT, long_about = COMMIT_LONG_ABOUT)]
    Commit {
        /// Scope to commit changes to
        scope: String,

        /// Commit message
        #[arg(short = 'm', long = "message")]
        message: String,

        /// Home-relative file or directory paths to commit; omit to commit
        /// every managed file this machine has changed
        paths: Vec<PathBuf>,
    },
    #[command(about = CONTINUE_ABOUT)]
    Continue,
    #[command(about = ABORT_ABOUT)]
    Abort,
    /// Show managed files that differ from the repo
    Status,
    /// Show line-oriented diffs for managed home files that differ from the repo
    Diff,
    /// Show checked-in scope and file state
    View {
        /// Scope to inspect
        #[arg(long)]
        scope: Option<String>,

        /// Repo-relative file path to inspect
        #[arg(long)]
        file: Option<PathBuf>,
    },
    #[command(external_subcommand)]
    Unknown(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InitRemote {
    Provided(String),
    Prompt,
}

#[derive(Debug, Clone, Copy)]
struct CliContext {
    interactive_terminal: bool,
}

#[derive(Debug, Clone)]
struct SuccessOutput {
    json: serde_json::Value,
    /// The same answer for a person, on the stream it belongs on. One field,
    /// so a command cannot fill in two and silently lose one.
    human: HumanOutput,
    /// Said alongside the answer on stderr, in every output format: what the
    /// run overwrote, published, or could not reach.
    notes: Vec<String>,
    /// 0 for every command but `dotsync diff`, which exits 1 when it found
    /// changes so a script can tell clean from dirty without parsing. That is
    /// why exit 1 means "dotsync stopped, or `diff` found changes", and why
    /// `status` in the payload is what separates the two. Documented in
    /// `--help`.
    exit_code: i32,
}

/// Where a command's human-readable answer goes, and why those are not the
/// same stream.
#[derive(Debug, Clone)]
enum HumanOutput {
    /// The answer *is* the output: `view` prints a file's contents, a scope's
    /// file list, the scope graph. A caller may pipe it into something.
    Stdout(String),
    /// A report about what the run did, which belongs beside a caller's data
    /// rather than in it.
    Message(String),
}

impl SuccessOutput {
    /// A run that reports what it did. The common case: everything but `view`.
    fn message(json: serde_json::Value, message: String) -> Self {
        Self {
            json,
            human: HumanOutput::Message(message),
            notes: Vec::new(),
            exit_code: 0,
        }
    }

    /// A run whose answer is its output.
    fn stdout(json: serde_json::Value, stdout: String) -> Self {
        Self {
            json,
            human: HumanOutput::Stdout(stdout),
            notes: Vec::new(),
            exit_code: 0,
        }
    }

    fn with_notes(mut self, notes: Vec<String>) -> Self {
        self.notes = notes;
        self
    }
}

#[derive(Debug, Clone)]
struct UsageError {
    message: String,
}

/// What to print, and what the run that produced it could not do.
///
/// The remote state sits out here rather than inside either arm, because it is
/// as true of a run that stopped as of one that finished — and putting it in
/// both arms is how it came to be reported on only one of them.
#[derive(Debug)]
struct CliOutput {
    kind: OutputKind,
    unreachable_remote: Option<UnreachableRemote>,
}

#[derive(Debug)]
enum OutputKind {
    Success(SuccessOutput),
    Error(ErrorOutput),
    Usage(UsageError),
}

impl CliOutput {
    /// Output from something that never became a run: a bad command line, or
    /// an environment dotsync cannot work in.
    fn without_run(kind: OutputKind) -> Self {
        Self {
            kind,
            unreachable_remote: None,
        }
    }
}

/// Turns a finished run into output, carrying what the run could not do onto
/// whichever arm it ended in. The one place that decision is made.
fn output_of<T, E: Into<ErrorOutput>>(
    run: Run<Result<T, E>>,
    render: impl FnOnce(T) -> SuccessOutput,
) -> CliOutput {
    let Run {
        report,
        unreachable_remote,
    } = run;
    CliOutput {
        kind: match report {
            Ok(report) => OutputKind::Success(render(report)),
            Err(error) => OutputKind::Error(error.into()),
        },
        unreachable_remote,
    }
}

/// A run that stopped, plus anything it had already done that the error alone
/// would not say.
#[derive(Debug)]
struct ErrorOutput {
    error: DotsyncError,
    forced_overwrites: Vec<PathBuf>,
}

impl From<DotsyncError> for ErrorOutput {
    fn from(error: DotsyncError) -> Self {
        Self {
            error,
            forced_overwrites: Vec::new(),
        }
    }
}

impl From<CommitFailure> for ErrorOutput {
    /// A commit that stopped after writing history has to say what it
    /// overwrote on the way past.
    fn from(failure: CommitFailure) -> Self {
        Self {
            error: failure.error,
            forced_overwrites: failure.forced_overwrites,
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if try_handle_version_json_request() {
        return;
    }

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => std::process::exit(emit_clap_error(error)),
    };
    let output_format = output_format_of(&cli);
    let outcome = match Action::try_from_cli(cli, detect_cli_context()) {
        Ok(action) => dispatch(action).await,
        Err(error) => Ok(CliOutput::without_run(OutputKind::Usage(error))),
    };

    let exit_code = match outcome {
        Ok(output) => emit_output(&output_format, output),
        Err(error) => emit_output(
            &output_format,
            CliOutput::without_run(OutputKind::Error(error.into())),
        ),
    };
    std::process::exit(exit_code);
}

fn detect_cli_context() -> CliContext {
    CliContext {
        interactive_terminal: io::stdin().is_terminal() && io::stderr().is_terminal(),
    }
}

/// `<tool> --version --json` is the agent-tools workspace contract for
/// machine-readable version reporting, enforced across every tool by the
/// `version-artifacts` concern. Clap cannot express it: `--version` prints and
/// exits inside clap, so `--json` never reaches a handler. Plain `--version`
/// is clap's own.
fn try_handle_version_json_request() -> bool {
    let args: Vec<String> = env::args().skip(1).collect();
    let is_version_json_request = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--version" | "-V"))
        && args.iter().any(|arg| arg == "--json")
        && args
            .iter()
            .all(|arg| matches!(arg.as_str(), "--version" | "-V" | "--json"));
    if !is_version_json_request {
        return false;
    }

    println!(
        "{}",
        json!({
            "package": "dotsync",
            "binary": "dotsync",
            "version": env!("CARGO_PKG_VERSION"),
        })
    );
    true
}

/// Clap exits the process itself on a parse failure, which used to happen
/// before `main` had read `--output` — so every clap-generated usage error
/// broke the documented JSON contract. Reading the format straight out of
/// argv is the only way to honor it: there is no parsed `Cli` to ask.
fn emit_clap_error(error: clap::Error) -> i32 {
    if !error.use_stderr() {
        // `--help` and `--version` arrive here as errors; they render on
        // stdout and exit 0.
        let _ = error.print();
        return error.exit_code();
    }

    let message = error.render().to_string();
    eprint!("{message}");
    if matches!(output_format_from_args(), OutputFormat::Json) {
        println!(
            "{}",
            render::render_usage_error_json(&usage_error(message.trim_end()))
        );
    }
    error.exit_code()
}

/// Which format this run answers in, including when clap could not tell.
///
/// `external_subcommand` — the arm that catches an unknown command so dotsync
/// can say so in its own words — swallows every argument after it, `--output`
/// included. So `dotsync bogus --output json` parsed the flag into the unknown
/// command's arguments and left `output_format` at its default, and the run
/// printed nothing at all on stdout: an empty stdout with exit 2, which is
/// what a crash looks like. Reading argv is the same fallback clap's own parse
/// failures already need, for the same reason.
fn output_format_of(cli: &Cli) -> OutputFormat {
    match cli.command {
        Some(Command::Unknown(_)) => output_format_from_args(),
        _ => cli.output_format,
    }
}

fn output_format_from_args() -> OutputFormat {
    let args: Vec<String> = env::args().skip(1).collect();
    let requested_json = args.iter().enumerate().any(|(index, arg)| {
        arg == "--output=json"
            || (arg == "--output" && args.get(index + 1).is_some_and(|value| value == "json"))
    });
    if requested_json {
        OutputFormat::Json
    } else {
        OutputFormat::Human
    }
}

impl Action {
    fn try_from_cli(cli: Cli, context: CliContext) -> Result<Self, UsageError> {
        match cli.command {
            Some(Command::Init { remote_url }) => {
                reject_force_before(cli.force, "init")?;
                let remote_url = init_remote_from_args(remote_url, context)?;
                Ok(Self::Init { remote_url })
            }
            Some(Command::Continue) => Ok(Self::Continue { force: cli.force }),
            Some(Command::Abort) => {
                reject_force_before(cli.force, "abort")?;
                Ok(Self::Abort)
            }
            Some(Command::Status) => {
                reject_force_before(cli.force, "status")?;
                Ok(Self::Status)
            }
            Some(Command::Diff) => {
                reject_force_before(cli.force, "diff")?;
                Ok(Self::Diff)
            }
            Some(Command::View { scope, file }) => {
                reject_force_before(cli.force, "view")?;
                Ok(Self::View { scope, file })
            }
            Some(Command::Commit {
                scope,
                message,
                paths,
            }) => Ok(Self::Commit {
                scope,
                message,
                force: cli.force,
                paths,
            }),
            Some(Command::Unknown(args)) => {
                // `--force` is checked per command below, and an unknown
                // command has no behavior to force.
                let command = args.first().map(String::as_str).unwrap_or("<empty>");
                Err(usage_error(&format!(
                    "unknown command `{command}`; run `dotsync --help` for supported commands"
                )))
            }
            None => Ok(Self::Sync { force: cli.force }),
        }
    }
}

fn init_remote_from_args(
    remote_url: Option<String>,
    context: CliContext,
) -> Result<InitRemote, UsageError> {
    if let Some(remote_url) = remote_url {
        return Ok(InitRemote::Provided(remote_url));
    }

    if context.interactive_terminal {
        return Ok(InitRemote::Prompt);
    }

    Err(usage_error(INIT_REMOTE_URL_USAGE))
}

async fn dispatch(action: Action) -> Result<CliOutput, DotsyncError> {
    match action {
        Action::Sync { force } => run_sync(force).await,
        Action::Commit {
            scope,
            message,
            force,
            paths,
        } => run_commit(scope, message, force, paths).await,
        Action::Init { remote_url } => run_init(remote_url).await,
        Action::Continue { force } => run_continue(force).await,
        Action::Abort => run_abort().await,
        Action::Status => run_status().await,
        Action::Diff => run_diff().await,
        Action::View { scope, file } => run_view(scope, file).await,
    }
}

/// `--force` is global, like `--output`, so it parses in either position and
/// one message explains it wherever it means nothing. Declaring it per command
/// instead would hand the commands that reject it clap's generic 'unexpected
/// argument' - and on `init`, clap's 'to pass --force as a value' tip, which
/// would make the flag the remote URL. A command that never chooses whether to
/// overwrite drifted home files has no meaning for it, and silently accepting
/// it there would teach an agent that retrying with `--force` could change the
/// answer. `init` and `abort` write home but never make that choice: `init`
/// has nothing of yours to overwrite, and `abort` exists precisely to discard
/// the home edit that started the cascade.
fn reject_force_before(force: bool, command: &str) -> Result<(), UsageError> {
    if !force {
        return Ok(());
    }
    Err(usage_error(&format!(
        "`--force` has no meaning for `{command}`; it only decides whether to overwrite drifted files in your home directory, which is a choice made by plain `dotsync`, `commit`, and `continue`"
    )))
}

/// `--force` on the commands that name no paths for it to scope to.
fn blanket_force(force: bool) -> ForceScope {
    if force {
        ForceScope::Everything
    } else {
        ForceScope::Nothing
    }
}

fn usage_error(message: &str) -> UsageError {
    UsageError {
        message: message.to_string(),
    }
}

async fn run_init(remote_url: InitRemote) -> Result<CliOutput, DotsyncError> {
    let remote_url = match remote_url {
        InitRemote::Provided(remote_url) => remote_url,
        InitRemote::Prompt => match prompt_init_remote_url() {
            Ok(remote_url) => remote_url,
            Err(error) => return Ok(CliOutput::without_run(OutputKind::Usage(error))),
        },
    };
    let paths = discover_paths()?;
    let run = init(&paths, &remote_url).await;
    Ok(output_of(run, |report| {
        render::synced_output(
            "init",
            format!(
                "dotsync: initialized {} and synced {} file(s)",
                report.sync.current_scope,
                report.sync.synced_paths.len()
            ),
            &report.sync,
            Some(&report.push),
        )
    }))
}

fn prompt_init_remote_url() -> Result<String, UsageError> {
    eprint!("dotsync init remote URL: ");
    io::stderr()
        .flush()
        .map_err(|err| usage_error(&format!("init could not write prompt: {err}")))?;

    let mut remote_url = String::new();
    io::stdin()
        .read_line(&mut remote_url)
        .map_err(|err| usage_error(&format!("init could not read remote URL: {err}")))?;
    let remote_url = remote_url.trim().to_string();
    if remote_url.is_empty() {
        return Err(usage_error(INIT_REMOTE_URL_USAGE));
    }
    Ok(remote_url)
}

async fn run_continue(force: bool) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = continue_after_conflict(&paths, blanket_force(force)).await;
    Ok(output_of(run, |report| {
        render::synced_output(
            "continue",
            format!(
                "dotsync: resumed cascade and synced {} file(s)",
                report.sync.synced_paths.len()
            ),
            &report.sync,
            Some(&report.push),
        )
    }))
}

async fn run_abort() -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = abort_paused_cascade(&paths).await;
    Ok(output_of(run, |report| {
        // `abort` publishes nothing, so it has no push to report — and the one
        // thing it knows that the other syncing commands do not is where the
        // cascade it discarded had stopped.
        let mut output = render::synced_output(
            "abort",
            format!(
                "dotsync: aborted the cascade paused at {} and synced {} file(s)",
                report.paused_scope,
                report.sync.synced_paths.len()
            ),
            &report.sync,
            None,
        );
        output.json["paused_scope"] = json!(report.paused_scope);
        output
    }))
}

async fn run_sync(force: bool) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = sync(&paths, blanket_force(force)).await;
    Ok(output_of(run, |report| {
        render::synced_output(
            "sync",
            format!(
                "dotsync: synced {} file(s) for {}",
                report.sync.synced_paths.len(),
                report.sync.current_scope
            ),
            &report.sync,
            Some(&report.push),
        )
    }))
}

async fn run_status() -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = status(&paths).await;
    Ok(output_of(run, |report| {
        SuccessOutput::message(
            with_paused_cascade(
                json!({
                    "status": "ok",
                    "command": "status",
                    "machine_scope": report.machine_scope,
                    "changes": render::changes_json(&report.changes),
                    "incoming": render::changes_json(&report.incoming),
                }),
                report.paused_cascade.as_ref(),
            ),
            render_status_human(&report),
        )
        .with_notes(render::paused_cascade_notes(report.paused_cascade.as_ref()))
    }))
}

async fn run_diff() -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = diff_home(&paths).await;
    Ok(output_of(run, |report| SuccessOutput {
        // Drift is what `diff` exists to report, so it is not an error — but
        // scripts and agents need to tell clean from dirty without parsing.
        exit_code: if report.drifts.is_empty() { 0 } else { 1 },
        // The same changes `status` lists, under the same name, with the diffs
        // shown. That is the whole difference between the two commands.
        ..SuccessOutput::message(
            with_paused_cascade(
                json!({
                    "status": "ok",
                    "command": "diff",
                    "machine_scope": report.machine_scope,
                    "changes": report
                        .drifts
                        .iter()
                        .map(render::render_drift_json)
                        .collect::<Vec<_>>(),
                }),
                report.paused_cascade.as_ref(),
            ),
            render_diff_human(&report),
        )
        .with_notes(render::paused_cascade_notes(report.paused_cascade.as_ref()))
    }))
}

async fn run_view(scope: Option<String>, file: Option<PathBuf>) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = view(&paths, scope.as_deref(), file.as_deref()).await;
    Ok(output_of(run, |report| match report {
        ViewReport::FileContents {
            scope,
            file,
            contents,
        } => SuccessOutput::stdout(
            json!({
                "status": "ok",
                "command": "view",
                "scope": scope,
                "path": render::display_path(&file),
                "contents": String::from_utf8_lossy(&contents),
            }),
            String::from_utf8_lossy(&contents).into_owned(),
        ),
        ViewReport::Scope { scope, files } => SuccessOutput::stdout(
            json!({
                "status": "ok",
                "command": "view",
                "scope": scope,
                "files": files.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
            }),
            render_view_scope_stdout(&scope, &files),
        ),
        ViewReport::FileScopes { file, scopes } => SuccessOutput::stdout(
            json!({
                "status": "ok",
                "command": "view",
                "file": render::display_path(&file),
                "scopes": scopes,
            }),
            render_view_file_scopes_stdout(&file, &scopes),
        ),
        ViewReport::Overview { scopes, files } => SuccessOutput::stdout(
            json!({
                "status": "ok",
                "command": "view",
                "scopes": scopes.iter().map(|scope| json!({
                    "name": scope.name,
                    "parents": scope.parents,
                })).collect::<Vec<_>>(),
                "files": files.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
            }),
            render_view_overview_stdout(&scopes, &files),
        ),
    }))
}

async fn run_commit(
    scope: String,
    message: String,
    force: bool,
    commit_paths: Vec<PathBuf>,
) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = commit_and_sync(
        &paths,
        CommitOptions {
            scope,
            message,
            force,
            paths: commit_paths,
        },
    )
    .await;
    Ok(output_of(run, render_commit_success))
}

/// A commit has two outcomes and says which one it had, because they are not
/// the same event: one wrote history and synced home, the other did neither.
/// The fields that only one of them can honestly fill are only on that one.
fn render_commit_success(report: dotsync::CommitReport) -> SuccessOutput {
    let mut json = json!({
        "status": "ok",
        "command": "commit",
        "outcome": if report.recorded.is_some() { "committed" } else { "nothing_to_commit" },
        "scope": report.committed_scope,
        "machine_scope": report.machine_scope,
        "skipped_paths": render::skipped_paths_json(&report.skipped),
        "unpushed_scopes": report.push.unpushed_scopes(),
    });
    let skipped = render::skipped_path_notes(&report.skipped);

    let Some(recorded) = report.recorded else {
        return SuccessOutput::message(
            json,
            format!(
                "dotsync: nothing to record on `{}`; no commit was made and home was not synced",
                report.committed_scope
            ),
        )
        .with_notes(
            skipped
                .into_iter()
                .chain(render::push_notes(&report.push))
                .collect(),
        );
    };

    json["synced_files"] = json!(render::display_paths(&recorded.sync.synced_paths));
    json["newly_tracked"] = json!(render::display_paths(&recorded.newly_tracked));
    json["forced_overwrites"] = json!(render::display_paths(&recorded.forced_overwrites));
    SuccessOutput::message(
        json,
        format!(
            "dotsync: committed {} and synced {} file(s)",
            report.committed_scope,
            recorded.sync.synced_paths.len()
        ),
    )
    .with_notes(
        render::newly_tracked_notes(&recorded.newly_tracked)
            .into_iter()
            .chain(skipped)
            .chain(render::forced_overwrite_notes(&recorded.forced_overwrites))
            .chain(render::success_notes(
                &recorded.sync.drifts,
                Some(&report.push),
            ))
            .collect(),
    )
}

/// Adds the pause to a read-only command's payload, and only when there is
/// one — the same shape as `remote_unreachable`, and for the same reason: a
/// run with nothing to say about a pause and a run on a machine with no pause
/// are the same answer to whoever reads this field.
fn with_paused_cascade(
    mut json: serde_json::Value,
    paused_cascade: Option<&String>,
) -> serde_json::Value {
    if let Some(scope) = paused_cascade {
        json["paused_cascade"] = json!(scope);
    }
    json
}

fn discover_paths() -> Result<DotsyncPaths, DotsyncError> {
    let home_dir = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(DotsyncError::HomeNotSet)?;
    Ok(DotsyncPaths {
        repo_root: home_dir.join(".local/share/dotsync/repo"),
        home_dir,
    })
}

/// The header `status` and `diff` share: same count, same population, same
/// words. They are two views of one answer, and reading one after the other
/// must not look like reading about two different machines.
fn changed_files_header(count: usize, machine_scope: &str) -> String {
    format!("dotsync: {count} changed managed file(s) for {machine_scope}")
}

fn render_status_human(report: &dotsync::StatusReport) -> String {
    let mut lines = Vec::new();

    if !report.changes.is_empty() {
        lines.push(changed_files_header(
            report.changes.len(),
            &report.machine_scope,
        ));
        lines.extend(
            report
                .changes
                .iter()
                .map(|change| render::render_change_line(&change.path, change.state)),
        );
    }
    if !report.incoming.is_empty() {
        lines.push(format!(
            "dotsync: {} incoming file(s) for {} — plain `dotsync` applies these",
            report.incoming.len(),
            report.machine_scope
        ));
        lines.extend(
            report
                .incoming
                .iter()
                .map(|change| render::render_change_line(&change.path, change.state)),
        );
    }

    if lines.is_empty() {
        return format!("dotsync: no changes for {}", report.machine_scope);
    }
    lines.join("\n")
}

/// `status`'s changed list, with each file's two sides shown under it.
fn render_diff_human(report: &DiffReport) -> String {
    if report.drifts.is_empty() {
        return format!("dotsync: no changes for {}", report.machine_scope);
    }

    let mut lines = vec![changed_files_header(
        report.drifts.len(),
        &report.machine_scope,
    )];
    for drift in &report.drifts {
        lines.push(render::render_change_line(&drift.repo_path, drift.state));
        lines.push(render::render_drift_diff(drift));
    }
    lines.join("\n")
}

fn render_view_overview_stdout(scopes: &[dotsync::ScopeInfo], files: &[PathBuf]) -> String {
    render_lines(
        std::iter::once("Scopes".to_string())
            .chain(scopes.iter().map(render_scope_line))
            .chain([String::new(), "Files".to_string()])
            .chain(files.iter().map(|path| render::display_path(path))),
    )
}

fn render_view_scope_stdout(scope: &str, files: &[PathBuf]) -> String {
    render_lines(
        std::iter::once(format!("Scope {scope}"))
            .chain(files.iter().map(|path| render::display_path(path))),
    )
}

fn render_view_file_scopes_stdout(path: &std::path::Path, scopes: &[String]) -> String {
    render_lines(
        [
            format!("File {}", render::display_path(path)),
            "Scopes".to_string(),
        ]
        .into_iter()
        .chain(scopes.iter().cloned()),
    )
}

fn render_lines(lines: impl IntoIterator<Item = String>) -> String {
    let mut lines = lines.into_iter().collect::<Vec<_>>();
    lines.push(String::new());
    lines.join("\n")
}

fn render_scope_line(scope: &dotsync::ScopeInfo) -> String {
    if scope.parents.is_empty() {
        scope.name.clone()
    } else {
        format!("{} <- {}", scope.name, scope.parents.join(", "))
    }
}

fn emit_output(output_format: &OutputFormat, output: CliOutput) -> i32 {
    let CliOutput {
        kind,
        unreachable_remote,
    } = output;
    // Before anything else the run has to say: it is the frame for all of it.
    for note in render::unreachable_remote_notes(unreachable_remote.as_ref()) {
        eprintln!("{note}");
    }

    match kind {
        OutputKind::Success(success) => {
            for note in success.notes {
                eprintln!("{note}");
            }
            if matches!(output_format, OutputFormat::Json) {
                println!(
                    "{}",
                    render::with_remote_state(success.json, unreachable_remote.as_ref())
                );
            } else {
                match success.human {
                    HumanOutput::Stdout(stdout) => print!("{stdout}"),
                    HumanOutput::Message(message) => eprintln!("{message}"),
                }
            }
            success.exit_code
        }
        OutputKind::Error(ErrorOutput {
            error,
            forced_overwrites,
        }) => {
            let exit_code = if error.is_paused_cascade() { 3 } else { 1 };
            for note in render::forced_overwrite_notes(&forced_overwrites) {
                eprintln!("{note}");
            }
            eprintln!("{}", render::render_error_human(&error));
            let mut error_report = error.to_error_report();
            error_report.forced_overwrites = forced_overwrites;
            // After the teaching message and set apart from it: these are the
            // files the run stopped on, not more instructions.
            if !error_report.drifts.is_empty() {
                eprintln!("\nChanged files:");
                for line in render::render_drifts_human(&error_report.drifts) {
                    eprintln!("{line}");
                }
            }
            if matches!(output_format, OutputFormat::Json) {
                println!(
                    "{}",
                    render::with_remote_state(
                        render::render_error_json(&error_report),
                        unreachable_remote.as_ref()
                    )
                );
            }
            exit_code
        }
        OutputKind::Usage(error) => {
            eprintln!("dotsync: {}", error.message);
            if matches!(output_format, OutputFormat::Json) {
                println!("{}", render::render_usage_error_json(&error));
            }
            2
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tdd_ratchet_gatekeeper() {
        if std::env::var("TDD_RATCHET").is_err() {
            panic!("Run tdd-ratchet instead of cargo test.");
        }
    }
}
