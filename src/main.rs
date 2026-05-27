use clap::{Parser, Subcommand, ValueEnum};
use dotsync::{
    commit_and_sync, continue_after_conflict, init, sync, CommandOutcome, CommitOptions,
    CommitSelection, DotsyncError, DotsyncPaths, ErrorReport, FileDrift, SyncOptions,
};
mod render;
use serde_json::json;
use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug, Parser)]
#[command(author, version, about = "Agent-first dotfile sync", long_about = None)]
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

    /// Commit every working-copy change
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
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Clone or join a dotsync remote
    Init { remote_url: String },
    /// Continue a paused merge cascade after resolving conflicts
    Continue,
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

#[derive(Debug, Clone)]
enum CliOutput {
    Success(SuccessOutput),
    Conflict(dotsync::CascadePause),
    Error(ErrorReport),
    Usage(UsageError),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let output_format = cli.output_format.clone();
    let outcome = match Action::try_from(cli) {
        Ok(action) => dispatch(action).await,
        Err(error) => Ok(CliOutput::Usage(error)),
    };

    let exit_code = match outcome {
        Ok(output) => emit_output(&output_format, output),
        Err(error) => emit_output(&output_format, CliOutput::Error(error.to_error_report())),
    };
    std::process::exit(exit_code);
}

impl TryFrom<Cli> for Action {
    type Error = UsageError;

    fn try_from(cli: Cli) -> Result<Self, Self::Error> {
        match (cli.command, cli.scope, cli.message) {
            (Some(Command::Init { remote_url }), None, None) => Ok(Self::Init { remote_url }),
            (Some(Command::Continue), None, None) => Ok(Self::Continue { force: cli.force }),
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
                    (false, true) => {
                        return Err(usage_error(
                            "commit mode requires explicit file/directory paths or --all",
                        ));
                    }
                    (true, true) => CommitSelection::All,
                    (false, false) => CommitSelection::Paths(cli.paths),
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
        CommandOutcome::Conflict(conflict) => Ok(CliOutput::Conflict(conflict)),
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
        CommandOutcome::Conflict(conflict) => Ok(CliOutput::Conflict(conflict)),
    }
}

fn discover_paths() -> Result<DotsyncPaths, DotsyncError> {
    let home_dir = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(DotsyncError::NotImplemented("HOME is not set"))?;
    Ok(DotsyncPaths {
        repo_root: home_dir.join("dotfiles"),
        home_dir,
    })
}

fn print_drifts(drifts: &[FileDrift]) {
    for line in render::render_drifts_human(drifts) {
        eprintln!("{line}");
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
        CliOutput::Conflict(conflict) => {
            eprintln!("{}", render::render_conflict_human(&conflict));
            if matches!(output_format, OutputFormat::Json) {
                println!("{}", render::render_conflict_json(&conflict));
            }
            3
        }
        CliOutput::Error(error) => {
            eprintln!("{}", render::render_error_human(&error));
            if !error.drifts.is_empty() {
                print_drifts(&error.drifts);
            }
            if matches!(output_format, OutputFormat::Json) {
                println!("{}", render::render_error_json(&error));
            }
            1
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
