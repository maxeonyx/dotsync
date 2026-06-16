use clap::{Parser, Subcommand, ValueEnum};
use dotsync::{
    commit_and_sync, continue_after_conflict, init, status, sync, ChangeStatus, CommandOutcome,
    CommitOptions, CommitSelection, DotsyncError, DotsyncPaths, FileDrift, SyncOptions,
};
mod render;
use serde_json::json;
use std::env;
use std::path::PathBuf;

const TOP_LEVEL_ABOUT: &str = "Agent-first dotfile sync";

const TOP_LEVEL_LONG_ABOUT: &str = "dotsync keeps a hidden repo at ~/.local/share/dotsync/repo and syncs the current machine scope into your home directory.

A scope is a branch in the dotsync DAG. Shared config lives on ancestor scopes such as `all` or `linux`; machine-specific config lives on leaf scopes such as your hostname.

Basic workflow:
  - plain `dotsync` syncs your current machine scope into home
  - edit files in home, then run `dotsync <scope> -m \"message\" <path>...` to record the change on the right scope
  - run `dotsync continue` if a cascade pauses for conflicts";

const TOP_LEVEL_AFTER_HELP: &str = "Examples:
  $ dotsync
  $ dotsync linux -m \"add bashrc\" .bashrc
  $ dotsync init <url>";

const INIT_ABOUT: &str = "Clone or join a dotsync remote";

const INIT_LONG_ABOUT: &str = "REMOTE_URL is the git remote that stores your dotsync repo.

`dotsync init` clones the repo into ~/.local/share/dotsync/repo, detects this machine, sets up any missing scope branches for its OS and machine, and syncs the resulting machine scope into home.";

const CONTINUE_ABOUT: &str = "Continue a paused merge cascade after resolving conflicts";

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
    #[arg(long = "output", value_enum, default_value = "human")]
    output_format: OutputFormat,

    /// Scope to commit changes to; omit for sync-only mode
    scope: Option<String>,

    /// Commit message (required when scope is provided)
    #[arg(short = 'm', long = "message", requires = "scope")]
    message: Option<String>,

    /// Proceed even when drift is detected
    #[arg(long)]
    force: bool,

    /// Commit every repo change
    #[arg(long)]
    all: bool,

    /// Repo-relative file or directory paths to commit
    paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
enum Action {
    Sync {
        force: bool,
    },
    Commit {
        scope: String,
        message: String,
        force: bool,
        selection: CommitSelection,
    },
    Init {
        remote_url: String,
    },
    Continue {
        force: bool,
    },
    Status {
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = INIT_ABOUT, long_about = INIT_LONG_ABOUT)]
    Init {
        /// Git remote URL or local path for the dotsync repo
        remote_url: String,
    },
    #[command(about = CONTINUE_ABOUT)]
    Continue,
    /// Show managed files that differ from the repo
    Status,
}

#[derive(Debug, Clone)]
struct SuccessOutput {
    json: serde_json::Value,
    human: String,
    notes: Vec<String>,
}

#[derive(Debug, Clone)]
struct UsageError {
    message: String,
}

#[derive(Debug)]
enum CliOutput {
    Success(SuccessOutput),
    Error(DotsyncError),
    Usage(UsageError),
}

#[tokio::main]
async fn main() {
    if try_handle_version_request() {
        return;
    }

    let cli = Cli::parse();
    let output_format = cli.output_format;
    let outcome = match Action::try_from(cli) {
        Ok(action) => dispatch(action).await,
        Err(error) => Ok(CliOutput::Usage(error)),
    };

    let exit_code = match outcome {
        Ok(output) => emit_output(&output_format, output),
        Err(error) => emit_output(&output_format, CliOutput::Error(error)),
    };
    std::process::exit(exit_code);
}

fn try_handle_version_request() -> bool {
    let args: Vec<String> = env::args().skip(1).collect();

    if is_version_json_request(&args) {
        println!(
            "{}",
            json!({
                "package": "dotsync",
                "binary": "dotsync",
                "version": env!("CARGO_PKG_VERSION"),
            })
        );
        return true;
    }

    if is_version_request(&args) {
        println!("dotsync {}", env!("CARGO_PKG_VERSION"));
        return true;
    }

    false
}

fn is_version_request(args: &[String]) -> bool {
    args.len() == 1 && matches!(args[0].as_str(), "--version" | "-V")
}

fn is_version_json_request(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "--version" | "-V"))
        && args.iter().any(|arg| arg == "--json")
        && args
            .iter()
            .all(|arg| matches!(arg.as_str(), "--version" | "-V" | "--json"))
}

impl TryFrom<Cli> for Action {
    type Error = UsageError;

    fn try_from(cli: Cli) -> Result<Self, Self::Error> {
        match (cli.command, cli.scope, cli.message) {
            (Some(Command::Init { remote_url }), None, None) => Ok(Self::Init { remote_url }),
            (Some(Command::Continue), None, None) => Ok(Self::Continue { force: cli.force }),
            (Some(Command::Status), None, None) => Ok(Self::Status { force: false }),
            (None, None, None) if !cli.all && cli.paths.is_empty() => {
                Ok(Self::Sync { force: cli.force })
            }
            (None, Some(scope), Some(message)) => {
                let selection = match (cli.all, cli.paths.is_empty()) {
                    (true, false) => {
                        return Err(usage_error(
                            "commit mode accepts explicit paths or --all, not both",
                        ));
                    }
                    (true, true) => CommitSelection::All,
                    (false, _) => CommitSelection::Paths(cli.paths),
                };

                Ok(Self::Commit {
                    scope,
                    message,
                    force: cli.force,
                    selection,
                })
            }
            (None, Some(_), None) => Err(usage_error("<scope> requires -m/--message")),
            (None, None, None) => Err(usage_error(
                "sync mode does not accept commit path arguments or --all",
            )),
            (Some(Command::Init { .. }), Some(_), _)
            | (Some(Command::Init { .. }), None, Some(_)) => Err(usage_error(
                "`init` does not take scope or message arguments",
            )),
            (Some(Command::Status), Some(_), _) | (Some(Command::Status), None, Some(_)) => Err(
                usage_error("`status` does not take scope or message arguments"),
            ),
            (Some(Command::Continue), Some(_), _) | (Some(Command::Continue), None, Some(_)) => {
                Err(usage_error(
                    "`continue` does not take scope or message arguments",
                ))
            }
            (None, None, Some(_)) => unreachable!("clap requires scope when message is set"),
        }
    }
}

async fn dispatch(action: Action) -> Result<CliOutput, DotsyncError> {
    match action {
        Action::Sync { force } => run_sync(force).await,
        Action::Commit {
            scope,
            message,
            force,
            selection,
        } => run_commit(scope, message, force, selection).await,
        Action::Init { remote_url } => run_init(remote_url).await,
        Action::Continue { force } => run_continue(force).await,
        Action::Status { force } => run_status(force).await,
    }
}

fn usage_error(message: &str) -> UsageError {
    UsageError {
        message: message.to_string(),
    }
}

async fn run_init(remote_url: String) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let report = init(&paths, &remote_url).await?;
    Ok(CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": "init",
            "scope": report.current_scope,
            "machine_scope": report.current_scope,
            "synced_files": report.sync.synced_paths.iter().map(|path| render::display_path(path)).collect::<Vec<_>>()
        }),
        human: format!(
            "dotsync: initialized {} and synced {} file(s)",
            report.current_scope,
            report.sync.synced_paths.len()
        ),
        notes: Vec::new(),
    }))
}

async fn run_continue(force: bool) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    match continue_after_conflict(&paths, SyncOptions { force }).await? {
        CommandOutcome::Success(report) => Ok(CliOutput::Success(SuccessOutput {
            json: json!({
                "status": "ok",
                "command": "continue",
                "scope": report.sync.current_scope,
                "machine_scope": report.sync.current_scope,
                "synced_files": report.sync.synced_paths.iter().map(|path| render::display_path(path)).collect::<Vec<_>>()
            }),
            human: format!(
                "dotsync: resumed cascade and synced {} file(s)",
                report.sync.synced_paths.len()
            ),
            notes: render::success_notes_for_drifts(&report.sync.drifts),
        })),
    }
}

async fn run_sync(force: bool) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let report = sync(&paths, SyncOptions { force }).await?;
    Ok(CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": "sync",
            "scope": report.current_scope,
            "machine_scope": report.current_scope,
            "synced_files": report.synced_paths.iter().map(|path| render::display_path(path)).collect::<Vec<_>>()
        }),
        human: format!(
            "dotsync: synced {} file(s) for {}",
            report.synced_paths.len(),
            report.current_scope
        ),
        notes: render::success_notes_for_drifts(&report.drifts),
    }))
}

async fn run_status(_force: bool) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let report = status(&paths).await?;
    let files = report
        .changes
        .iter()
        .map(|change| {
            json!({
                "path": render::display_path(&change.path),
                "status": render_change_status_json(change.status),
            })
        })
        .collect::<Vec<_>>();

    Ok(CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": "status",
            "machine_scope": report.machine_scope,
            "changed_count": files.len(),
            "groups": [{
                "scope": serde_json::Value::Null,
                "files": files,
            }],
        }),
        human: render_status_human(&report),
        notes: Vec::new(),
    }))
}

async fn run_commit(
    scope: String,
    message: String,
    force: bool,
    selection: CommitSelection,
) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    match commit_and_sync(
        &paths,
        CommitOptions {
            scope,
            message,
            force,
            selection,
        },
    )
    .await?
    {
        CommandOutcome::Success(report) => Ok(CliOutput::Success(SuccessOutput {
            json: json!({
                "status": "ok",
                "command": "commit",
                "scope": report.committed_scope,
                "machine_scope": report.sync.current_scope,
                "synced_files": report.sync.synced_paths.iter().map(|path| render::display_path(path)).collect::<Vec<_>>()
            }),
            human: format!(
                "dotsync: committed {} and synced {} file(s)",
                report.committed_scope,
                report.sync.synced_paths.len()
            ),
            notes: render::success_notes_for_drifts(&report.sync.drifts),
        })),
    }
}

fn discover_paths() -> Result<DotsyncPaths, DotsyncError> {
    let home_dir = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(DotsyncError::NotImplemented("HOME is not set"))?;
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
    if report.changes.is_empty() {
        return format!("dotsync: no changes for {}", report.machine_scope);
    }

    let mut lines = Vec::with_capacity(report.changes.len() + 1);
    lines.push(format!(
        "dotsync: {} changed managed file(s) for {}",
        report.changes.len(),
        report.machine_scope
    ));
    lines.extend(report.changes.iter().map(|change| {
        format!(
            "  {} {}",
            render_change_status_human(change.status),
            render::display_path(&change.path)
        )
    }));
    lines.join("\n")
}

fn render_change_status_human(status: ChangeStatus) -> &'static str {
    match status {
        ChangeStatus::Modified => "M",
        ChangeStatus::Deleted => "D",
    }
}

fn render_change_status_json(status: ChangeStatus) -> &'static str {
    match status {
        ChangeStatus::Modified => "modified",
        ChangeStatus::Deleted => "deleted",
    }
}

fn emit_output(output_format: &OutputFormat, output: CliOutput) -> i32 {
    match output {
        CliOutput::Success(success) => {
            for note in success.notes {
                eprintln!("{note}");
            }
            eprintln!("{}", success.human);
            if matches!(output_format, OutputFormat::Json) {
                println!("{}", success.json);
            }
            0
        }
        CliOutput::Error(error) => {
            let exit_code = if matches!(error, DotsyncError::CascadePaused { .. }) {
                3
            } else {
                1
            };
            eprintln!("{}", render::render_error_human(&error));
            let error_report = error.to_error_report();
            if !error_report.drifts.is_empty() {
                print_drifts(&error_report.drifts);
            }
            if matches!(output_format, OutputFormat::Json) {
                println!("{}", render::render_error_json(&error_report));
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
