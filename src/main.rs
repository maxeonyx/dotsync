use clap::{Parser, Subcommand};
use dotsync::{
    commit_and_sync, init, sync, CommitOptions, DotsyncError, DotsyncPaths, SyncOptions,
};
use std::env;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(author, version, about = "Agent-first dotfile sync", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match (cli.command, cli.scope, cli.message) {
        (Some(Command::Init { remote_url }), None, None) => run_init(remote_url).await,
        (None, None, None) => run_sync(cli.force).await,
        (None, Some(scope), Some(message)) => run_commit(scope, message, cli.force).await,
        (None, Some(_), None) => {
            eprintln!("dotsync: <scope> requires -m/--message");
            std::process::exit(2);
        }
        (Some(Command::Init { .. }), Some(_), _) | (Some(Command::Init { .. }), None, Some(_)) => {
            eprintln!("dotsync: `init` does not take scope or message arguments");
            std::process::exit(2);
        }
        (None, None, Some(_)) => unreachable!("clap requires scope when message is set"),
    };

    if let Err(error) = result {
        print_error(&error);
        std::process::exit(1);
    }
}

async fn run_init(remote_url: String) -> Result<(), DotsyncError> {
    let paths = discover_paths()?;
    let report = init(&paths, &remote_url).await?;
    println!(
        "dotsync: initialized {} and synced {} file(s)",
        report.current_scope,
        report.sync.synced_paths.len()
    );
    Ok(())
}

async fn run_sync(force: bool) -> Result<(), DotsyncError> {
    let paths = discover_paths()?;
    let report = sync(&paths, SyncOptions { force }).await?;
    if !report.drifts.is_empty() {
        eprintln!("dotsync: overwrote {} drifted file(s)", report.drifts.len());
        print_drifts(&report.drifts);
    }
    println!(
        "dotsync: synced {} file(s) for {}",
        report.synced_paths.len(),
        report.current_scope
    );
    Ok(())
}

async fn run_commit(scope: String, message: String, force: bool) -> Result<(), DotsyncError> {
    let paths = discover_paths()?;
    let report = commit_and_sync(
        &paths,
        CommitOptions {
            scope,
            message,
            force,
        },
    )
    .await?;
    if !report.sync.drifts.is_empty() {
        eprintln!(
            "dotsync: overwrote {} drifted file(s)",
            report.sync.drifts.len()
        );
        print_drifts(&report.sync.drifts);
    }
    println!(
        "dotsync: committed {} and synced {} file(s)",
        report.committed_scope,
        report.sync.synced_paths.len()
    );
    Ok(())
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

fn print_error(error: &DotsyncError) {
    match error {
        DotsyncError::DriftDetected { drifts, .. } => {
            eprintln!("dotsync: drift detected");
            print_drifts(drifts);
        }
        _ => eprintln!("dotsync: {error}"),
    }
}

fn print_drifts(drifts: &[dotsync::FileDrift]) {
    for drift in drifts {
        eprintln!("- {}", drift.repo_path.display());
        eprintln!("{}", drift.diff);
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
