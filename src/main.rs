use clap::{Parser, Subcommand, ValueEnum};
use dotsync::{
    commit_and_sync, continue_after_conflict, init, sync, CommandOutcome, CommitOptions,
    DotsyncError, DotsyncPaths, SyncOptions,
};
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
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Clone or join a dotsync remote
    Init { remote_url: String },
    /// Continue a paused merge cascade after resolving conflicts
    Continue,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let output_format = cli.output_format.clone();

    let result = match (cli.command, cli.scope, cli.message) {
        (Some(Command::Init { remote_url }), None, None) => {
            run_init(remote_url, &output_format).await
        }
        (Some(Command::Continue), None, None) => run_continue(cli.force, &output_format).await,
        (None, None, None) => run_sync(cli.force, &output_format).await,
        (None, Some(scope), Some(message)) => {
            run_commit(scope, message, cli.force, &output_format).await
        }
        (None, Some(_), None) => {
            eprintln!("dotsync: <scope> requires -m/--message");
            std::process::exit(2);
        }
        (Some(Command::Init { .. }), Some(_), _) | (Some(Command::Init { .. }), None, Some(_)) => {
            eprintln!("dotsync: `init` does not take scope or message arguments");
            std::process::exit(2);
        }
        (Some(Command::Continue), Some(_), _) | (Some(Command::Continue), None, Some(_)) => {
            eprintln!("dotsync: `continue` does not take scope or message arguments");
            std::process::exit(2);
        }
        (None, None, Some(_)) => unreachable!("clap requires scope when message is set"),
    };

    if let Err(error) = result {
        print_error(&error, &output_format);
        std::process::exit(1);
    }
}

async fn run_init(remote_url: String, output_format: &OutputFormat) -> Result<(), DotsyncError> {
    let paths = discover_paths()?;
    let report = init(&paths, &remote_url).await?;
    emit_success(
        &output_format,
        json!({
            "status": "ok",
            "command": "init",
            "scope": report.current_scope,
            "synced_files": report.sync.synced_paths.iter().map(|path| display_path(path)).collect::<Vec<_>>()
        }),
        format!(
            "dotsync: initialized {} and synced {} file(s)",
            report.current_scope,
            report.sync.synced_paths.len()
        ),
    );
    Ok(())
}

async fn run_continue(force: bool, output_format: &OutputFormat) -> Result<(), DotsyncError> {
    let paths = discover_paths()?;
    match continue_after_conflict(&paths, SyncOptions { force }).await? {
        CommandOutcome::Success(report) => {
            emit_success(
                &output_format,
                json!({
                    "status": "ok",
                    "command": "continue",
                    "synced_files": report.sync.synced_paths.iter().map(|path| display_path(path)).collect::<Vec<_>>()
                }),
                format!(
                    "dotsync: resumed cascade and synced {} file(s)",
                    report.sync.synced_paths.len()
                ),
            );
            Ok(())
        }
        CommandOutcome::Conflict(conflict) => exit_conflict(&output_format, conflict),
    }
}

async fn run_sync(force: bool, output_format: &OutputFormat) -> Result<(), DotsyncError> {
    let paths = discover_paths()?;
    let report = sync(&paths, SyncOptions { force }).await?;
    if !report.drifts.is_empty() {
        eprintln!("dotsync: overwrote {} drifted file(s)", report.drifts.len());
        print_drifts(&report.drifts);
    }
    emit_success(
        &output_format,
        json!({
            "status": "ok",
            "command": "sync",
            "scope": report.current_scope,
            "synced_files": report.synced_paths.iter().map(|path| display_path(path)).collect::<Vec<_>>()
        }),
        format!(
            "dotsync: synced {} file(s) for {}",
            report.synced_paths.len(),
            report.current_scope
        ),
    );
    Ok(())
}

async fn run_commit(
    scope: String,
    message: String,
    force: bool,
    output_format: &OutputFormat,
) -> Result<(), DotsyncError> {
    let paths = discover_paths()?;
    match commit_and_sync(
        &paths,
        CommitOptions {
            scope,
            message,
            force,
        },
    )
    .await?
    {
        CommandOutcome::Success(report) => {
            if !report.sync.drifts.is_empty() {
                eprintln!(
                    "dotsync: overwrote {} drifted file(s)",
                    report.sync.drifts.len()
                );
                print_drifts(&report.sync.drifts);
            }
            emit_success(
                &output_format,
                json!({
                    "status": "ok",
                    "command": "commit",
                    "scope": report.committed_scope,
                    "synced_files": report.sync.synced_paths.iter().map(|path| display_path(path)).collect::<Vec<_>>()
                }),
                format!(
                    "dotsync: committed {} and synced {} file(s)",
                    report.committed_scope,
                    report.sync.synced_paths.len()
                ),
            );
            Ok(())
        }
        CommandOutcome::Conflict(conflict) => exit_conflict(&output_format, conflict),
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

fn print_error(error: &DotsyncError, output_format: &OutputFormat) {
    match error {
        DotsyncError::DriftDetected { drifts, .. } => {
            eprintln!("dotsync: drift detected");
            print_drifts(drifts);
        }
        _ => eprintln!("dotsync: {error}"),
    }
    if matches!(output_format, OutputFormat::Json) {
        println!(
            "{}",
            json!({"status": "error", "message": error.to_string()})
        );
    }
}

fn print_drifts(drifts: &[dotsync::FileDrift]) {
    for drift in drifts {
        eprintln!("- {}", drift.repo_path.display());
        eprintln!("{}", drift.diff);
    }
}

fn emit_success(output_format: &OutputFormat, json_value: serde_json::Value, human: String) {
    match output_format {
        OutputFormat::Human => println!("{human}"),
        OutputFormat::Json => println!("{json_value}"),
    }
}

fn exit_conflict(
    output_format: &OutputFormat,
    conflict: dotsync::CascadePause,
) -> Result<(), DotsyncError> {
    let json_value = json!({
        "status": "conflict",
        "scope": conflict.scope,
        "conflicted_files": conflict.conflicted_files,
        "scopes_done": conflict.scopes_done,
        "scopes_pending": conflict.scopes_pending,
        "original_scope": conflict.original_scope,
        "machine_scope": conflict.machine_scope,
    });
    match output_format {
        OutputFormat::Human => eprintln!("dotsync: conflict detected"),
        OutputFormat::Json => println!("{json_value}"),
    }
    std::process::exit(3);
}

fn display_path(path: &std::path::Path) -> String {
    path.display().to_string()
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
