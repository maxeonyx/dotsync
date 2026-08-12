use clap::{Parser, Subcommand, ValueEnum};
use dotsync::{
    abort_paused_cascade, commit_and_sync, continue_after_conflict, diff_home, init, status, sync,
    view, CommitOptions, DiffReport, DotsyncError, DotsyncPaths, FileChange, FileDrift, FileState,
    ForceScope, Run, UnreachableRemote, ViewReport,
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

const TOP_LEVEL_AFTER_HELP: &str = "Examples:
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

dotsync compares three sides of every path: what it last synced to this machine, what is in home now, and what the scopes hold now. A path whose home content is simply older than the repo has not been changed here, so `commit` refuses it and points at plain `dotsync` instead — committing it would revert whoever published the change that is already there.

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
    human: String,
    notes: Vec<String>,
    stdout: Option<String>,
    exit_code: i32,
    /// Set when the run could not reach the remote, so every command says
    /// which state it is reporting against without each one remembering to.
    unreachable_remote: Option<UnreachableRemote>,
}

#[derive(Debug, Clone)]
struct UsageError {
    message: String,
}

#[derive(Debug)]
enum CliOutput {
    Success(SuccessOutput),
    Error(ErrorOutput),
    Usage(UsageError),
}

/// A run that stopped, plus anything it had already done that the error alone
/// would not say.
#[derive(Debug)]
struct ErrorOutput {
    error: DotsyncError,
    forced_overwrites: Vec<PathBuf>,
    unreachable_remote: Option<UnreachableRemote>,
}

impl From<DotsyncError> for ErrorOutput {
    fn from(error: DotsyncError) -> Self {
        Self {
            error,
            forced_overwrites: Vec::new(),
            unreachable_remote: None,
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
    let output_format = cli.output_format;
    let outcome = match Action::try_from_cli(cli, detect_cli_context()) {
        Ok(action) => dispatch(action).await,
        Err(error) => Ok(CliOutput::Usage(error)),
    };

    let exit_code = match outcome {
        Ok(output) => emit_output(&output_format, output),
        Err(error) => emit_output(&output_format, CliOutput::Error(error.into())),
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
            Err(error) => return Ok(CliOutput::Usage(error)),
        },
    };
    let paths = discover_paths()?;
    let Run {
        report,
        unreachable_remote,
    } = init(&paths, &remote_url).await?;
    Ok(CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": "init",
            "scope": report.sync.current_scope,
            "machine_scope": report.sync.current_scope,
            "synced_files": report.sync.synced_paths.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
            "unpushed_scopes": report.push.unpushed_scopes(),
        }),
        human: format!(
            "dotsync: initialized {} and synced {} file(s)",
            report.sync.current_scope,
            report.sync.synced_paths.len()
        ),
        notes: render::success_notes(&report.sync.drifts, Some(&report.push)),
        stdout: None,
        exit_code: 0,
        unreachable_remote,
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
    let Run {
        report,
        unreachable_remote,
    } = continue_after_conflict(&paths, blanket_force(force)).await?;
    Ok(CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": "continue",
            "scope": report.sync.current_scope,
            "machine_scope": report.sync.current_scope,
            "synced_files": report.sync.synced_paths.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
            "unpushed_scopes": report.push.unpushed_scopes(),
        }),
        human: format!(
            "dotsync: resumed cascade and synced {} file(s)",
            report.sync.synced_paths.len()
        ),
        notes: render::success_notes(&report.sync.drifts, Some(&report.push)),
        stdout: None,
        exit_code: 0,
        unreachable_remote,
    }))
}

async fn run_abort() -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let Run {
        report,
        unreachable_remote,
    } = abort_paused_cascade(&paths).await?;
    Ok(CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": "abort",
            "aborted_scope": report.aborted_scope,
            "scope": report.sync.current_scope,
            "machine_scope": report.sync.current_scope,
            "synced_files": report.sync.synced_paths.iter().map(|path| render::display_path(path)).collect::<Vec<_>>()
        }),
        human: format!(
            "dotsync: aborted cascade at {} and synced {} file(s)",
            report.aborted_scope,
            report.sync.synced_paths.len()
        ),
        notes: render::success_notes(&report.sync.drifts, None),
        stdout: None,
        exit_code: 0,
        unreachable_remote,
    }))
}

async fn run_sync(force: bool) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let Run {
        report,
        unreachable_remote,
    } = sync(&paths, blanket_force(force)).await?;
    Ok(CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": "sync",
            "scope": report.sync.current_scope,
            "machine_scope": report.sync.current_scope,
            "synced_files": report.sync.synced_paths.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
            "unpushed_scopes": report.push.unpushed_scopes(),
        }),
        human: format!(
            "dotsync: synced {} file(s) for {}",
            report.sync.synced_paths.len(),
            report.sync.current_scope
        ),
        notes: render::success_notes(&report.sync.drifts, Some(&report.push)),
        stdout: None,
        exit_code: 0,
        unreachable_remote,
    }))
}

async fn run_status() -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let Run {
        report,
        unreachable_remote,
    } = status(&paths).await?;
    let files = report
        .changes
        .iter()
        .map(|change| render_change_json(change, true))
        .chain(
            report
                .incoming
                .iter()
                .map(|change| render_change_json(change, false)),
        )
        .collect::<Vec<_>>();

    Ok(CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": "status",
            "machine_scope": report.machine_scope,
            "changed_count": report.changes.len(),
            "incoming_count": report.incoming.len(),
            "groups": [{
                "scope": serde_json::Value::Null,
                "files": files,
            }],
        }),
        human: render_status_human(&report),
        notes: Vec::new(),
        stdout: None,
        exit_code: 0,
        unreachable_remote,
    }))
}

async fn run_diff() -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let Run {
        report,
        unreachable_remote,
    } = diff_home(&paths).await?;
    let changed_count = report.drifts.len();
    let drifts = report
        .drifts
        .iter()
        .map(render::render_drift_json)
        .collect::<Vec<_>>();
    let exit_code = if report.drifts.is_empty() { 0 } else { 1 };

    Ok(CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": "diff",
            "machine_scope": report.machine_scope,
            "changed_count": changed_count,
            "drifts": drifts,
        }),
        human: render_diff_human(&report),
        notes: Vec::new(),
        stdout: None,
        exit_code,
        unreachable_remote,
    }))
}

async fn run_view(scope: Option<String>, file: Option<PathBuf>) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let Run {
        report,
        unreachable_remote,
    } = view(&paths, scope.as_deref(), file.as_deref()).await?;
    Ok(CliOutput::Success(match report {
        ViewReport::FileContents {
            scope,
            file,
            contents,
        } => SuccessOutput {
            json: json!({
                "status": "ok",
                "command": "view",
                "scope": scope,
                "path": render::display_path(&file),
                "contents": String::from_utf8_lossy(&contents),
            }),
            human: String::new(),
            notes: Vec::new(),
            stdout: Some(String::from_utf8_lossy(&contents).into_owned()),
            exit_code: 0,
            unreachable_remote,
        },
        ViewReport::Scope { scope, files } => SuccessOutput {
            json: json!({
                "status": "ok",
                "command": "view",
                "scope": scope,
                "files": files.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
            }),
            human: String::new(),
            notes: Vec::new(),
            stdout: Some(render_view_scope_stdout(&scope, &files)),
            exit_code: 0,
            unreachable_remote,
        },
        ViewReport::FileScopes { file, scopes } => SuccessOutput {
            json: json!({
                "status": "ok",
                "command": "view",
                "file": render::display_path(&file),
                "scopes": scopes,
            }),
            human: String::new(),
            notes: Vec::new(),
            stdout: Some(render_view_file_scopes_stdout(&file, &scopes)),
            exit_code: 0,
            unreachable_remote,
        },
        ViewReport::Overview { scopes, files } => SuccessOutput {
            json: json!({
                "status": "ok",
                "command": "view",
                "scopes": scopes.iter().map(|scope| json!({
                    "name": scope.name,
                    "parents": scope.parents,
                })).collect::<Vec<_>>(),
                "files": files.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
            }),
            human: String::new(),
            notes: Vec::new(),
            stdout: Some(render_view_overview_stdout(&scopes, &files)),
            exit_code: 0,
            unreachable_remote,
        },
    }))
}

async fn run_commit(
    scope: String,
    message: String,
    force: bool,
    commit_paths: Vec<PathBuf>,
) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = match commit_and_sync(
        &paths,
        CommitOptions {
            scope,
            message,
            force,
            paths: commit_paths,
        },
    )
    .await
    {
        Ok(run) => run,
        Err(failure) => {
            return Ok(CliOutput::Error(ErrorOutput {
                error: failure.error,
                forced_overwrites: failure.forced_overwrites,
                unreachable_remote: failure.unreachable_remote,
            }))
        }
    };
    let Run {
        report,
        unreachable_remote,
    } = run;
    Ok(CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": "commit",
            "scope": report.committed_scope,
            "machine_scope": report.sync.current_scope,
            "synced_files": report.sync.synced_paths.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
            "forced_overwrites": report.forced_overwrites.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
            "unpushed_scopes": report.push.unpushed_scopes(),
        }),
        human: format!(
            "dotsync: committed {} and synced {} file(s)",
            report.committed_scope,
            report.sync.synced_paths.len()
        ),
        notes: render::forced_overwrite_notes(&report.forced_overwrites)
            .into_iter()
            .chain(render::success_notes(
                &report.sync.drifts,
                Some(&report.push),
            ))
            .collect(),
        stdout: None,
        exit_code: 0,
        unreachable_remote,
    }))
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

fn print_drifts(drifts: &[FileDrift]) {
    for line in render::render_drifts_human(drifts) {
        eprintln!("{line}");
    }
}

fn render_status_human(report: &dotsync::StatusReport) -> String {
    let mut lines = Vec::new();

    if !report.changes.is_empty() {
        lines.push(format!(
            "dotsync: {} changed managed file(s) for {}",
            report.changes.len(),
            report.machine_scope
        ));
        lines.extend(report.changes.iter().map(render_change_human));
    }
    if !report.incoming.is_empty() {
        lines.push(format!(
            "dotsync: {} incoming file(s) for {} — plain `dotsync` applies these",
            report.incoming.len(),
            report.machine_scope
        ));
        lines.extend(report.incoming.iter().map(render_change_human));
    }

    if lines.is_empty() {
        return format!("dotsync: no changes for {}", report.machine_scope);
    }
    lines.join("\n")
}

fn render_diff_human(report: &DiffReport) -> String {
    if report.drifts.is_empty() {
        return format!("dotsync: no changes for {}", report.machine_scope);
    }

    let mut lines = Vec::new();
    lines.push(format!(
        "dotsync: {} drifted managed file(s) for {}",
        report.drifts.len(),
        report.machine_scope
    ));
    lines.extend(render::render_drifts_human(&report.drifts));
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

/// One status line: a marker an agent can scan for, then the reason in words
/// so it never has to guess what the marker meant.
fn render_change_human(change: &FileChange) -> String {
    format!(
        "  {} {} ({})",
        change_marker(change.state),
        render::display_path(&change.path),
        change.state.reason()
    )
}

fn render_change_json(change: &FileChange, action_required: bool) -> serde_json::Value {
    json!({
        "path": render::display_path(&change.path),
        "status": change.state.code(),
        "action_required": action_required,
    })
}

fn change_marker(state: FileState) -> &'static str {
    match state {
        FileState::EditedInHome | FileState::EditedInHomeButRemovedFromRepo => "M",
        FileState::DeletedInHome | FileState::DeletedInHomeTipAlsoChanged => "D",
        FileState::DivergedEdit
        | FileState::IncomingNewCollidesWithUntrackedHome
        | FileState::NoSyncRecord => "C",
        FileState::IncomingNew => "A",
        FileState::StaleNotYours => "U",
        FileState::RemovedFromRepo => "R",
        // Not reported: `status` only ever renders drift and incoming changes.
        FileState::UntrackedInHome
        | FileState::IncomingNewAlreadyMatchesHome
        | FileState::AlreadyApplied
        | FileState::InSync
        | FileState::RemovedEverywhere
        | FileState::AbsentEverywhere => " ",
    }
}

fn emit_output(output_format: &OutputFormat, output: CliOutput) -> i32 {
    match output {
        CliOutput::Success(success) => {
            for note in render::unreachable_remote_notes(success.unreachable_remote.as_ref()) {
                eprintln!("{note}");
            }
            for note in success.notes {
                eprintln!("{note}");
            }
            if matches!(output_format, OutputFormat::Json) {
                println!(
                    "{}",
                    render::with_remote_state(success.json, success.unreachable_remote.as_ref())
                );
            } else if let Some(stdout) = success.stdout {
                print!("{stdout}");
            } else {
                eprintln!("{}", success.human);
            }
            success.exit_code
        }
        CliOutput::Error(ErrorOutput {
            error,
            forced_overwrites,
            unreachable_remote,
        }) => {
            let exit_code = if matches!(error, DotsyncError::CascadePaused { .. }) {
                3
            } else {
                1
            };
            for note in render::unreachable_remote_notes(unreachable_remote.as_ref()) {
                eprintln!("{note}");
            }
            for note in render::forced_overwrite_notes(&forced_overwrites) {
                eprintln!("{note}");
            }
            eprintln!("{}", render::render_error_human(&error));
            let mut error_report = error.to_error_report();
            error_report.forced_overwrites = forced_overwrites;
            if !error_report.drifts.is_empty() {
                print_drifts(&error_report.drifts);
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
        CliOutput::Usage(error) => {
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
