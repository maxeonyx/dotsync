use clap::{Parser, Subcommand, ValueEnum};
use dotsync::{
    commit_and_sync, continue_after_conflict, init, sync, CommandOutcome, CommitOptions,
    DotsyncError, DotsyncPaths, ErrorReport, FileDrift, SyncOptions,
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

#[derive(Debug, Clone)]
struct SuccessOutput {
    json: serde_json::Value,
    human: String,
    notes: Vec<String>,
}

#[derive(Debug, Clone)]
struct UsageError {
    message: &'static str,
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

    let outcome = match (cli.command, cli.scope, cli.message) {
        (Some(Command::Init { remote_url }), None, None) => run_init(remote_url).await,
        (Some(Command::Continue), None, None) => run_continue(cli.force).await,
        (None, None, None) => run_sync(cli.force).await,
        (None, Some(scope), Some(message)) => run_commit(scope, message, cli.force).await,
        (None, Some(_), None) => Ok(CliOutput::Usage(UsageError {
            message: "<scope> requires -m/--message",
        })),
        (Some(Command::Init { .. }), Some(_), _) | (Some(Command::Init { .. }), None, Some(_)) => {
            Ok(CliOutput::Usage(UsageError {
                message: "`init` does not take scope or message arguments",
            }))
        }
        (Some(Command::Continue), Some(_), _) | (Some(Command::Continue), None, Some(_)) => {
            Ok(CliOutput::Usage(UsageError {
                message: "`continue` does not take scope or message arguments",
            }))
        }
        (None, None, Some(_)) => unreachable!("clap requires scope when message is set"),
    };

    let exit_code = match outcome {
        Ok(output) => emit_output(&output_format, output),
        Err(error) => emit_output(&output_format, CliOutput::Error(error.to_error_report())),
    };
    std::process::exit(exit_code);
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
            "synced_files": report.sync.synced_paths.iter().map(|path| display_path(path)).collect::<Vec<_>>()
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
                "synced_files": report.sync.synced_paths.iter().map(|path| display_path(path)).collect::<Vec<_>>()
            }),
            human: format!(
                "dotsync: resumed cascade and synced {} file(s)",
                report.sync.synced_paths.len()
            ),
            notes: success_notes_for_drifts(&report.sync.drifts),
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
            "synced_files": report.synced_paths.iter().map(|path| display_path(path)).collect::<Vec<_>>()
        }),
        human: format!(
            "dotsync: synced {} file(s) for {}",
            report.synced_paths.len(),
            report.current_scope
        ),
        notes: success_notes_for_drifts(&report.drifts),
    }))
}

async fn run_commit(
    scope: String,
    message: String,
    force: bool,
) -> Result<CliOutput, DotsyncError> {
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
        CommandOutcome::Success(report) => Ok(CliOutput::Success(SuccessOutput {
            json: json!({
                "status": "ok",
                "command": "commit",
                "scope": report.committed_scope,
                "machine_scope": report.sync.current_scope,
                "synced_files": report.sync.synced_paths.iter().map(|path| display_path(path)).collect::<Vec<_>>()
            }),
            human: format!(
                "dotsync: committed {} and synced {} file(s)",
                report.committed_scope,
                report.sync.synced_paths.len()
            ),
            notes: success_notes_for_drifts(&report.sync.drifts),
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
    for drift in drifts {
        eprintln!("- {}", drift.repo_path.display());
        eprintln!("{}", drift.diff);
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
            eprintln!("{}", render_conflict_human(&conflict));
            if matches!(output_format, OutputFormat::Json) {
                println!("{}", render_conflict_json(&conflict));
            }
            3
        }
        CliOutput::Error(error) => {
            eprintln!("{}", render_error_human(&error));
            if !error.drifts.is_empty() {
                print_drifts(&error.drifts);
            }
            if matches!(output_format, OutputFormat::Json) {
                println!("{}", render_error_json(&error));
            }
            1
        }
        CliOutput::Usage(error) => {
            eprintln!("dotsync: {}", error.message);
            if matches!(output_format, OutputFormat::Json) {
                println!("{}", render_usage_error_json(&error));
            }
            2
        }
    }
}

fn render_conflict_json(conflict: &dotsync::CascadePause) -> serde_json::Value {
    json!({
        "status": "conflict",
        "scope": conflict.scope,
        "conflicted_files": conflict.conflicted_files,
        "scopes_done": conflict.scopes_done,
        "scopes_pending": conflict.scopes_pending,
        "original_scope": conflict.original_scope,
        "machine_scope": conflict.machine_scope,
        "parent_scopes": conflict.parent_scopes,
        "scope_dag": conflict.scope_dag,
    })
}

fn render_error_json(error: &ErrorReport) -> serde_json::Value {
    json!({
        "status": "error",
        "error": error.code,
        "message": error.message,
        "drifts": error.drifts.iter().map(render_drift_json).collect::<Vec<_>>()
    })
}

fn render_usage_error_json(error: &UsageError) -> serde_json::Value {
    json!({
        "status": "error",
        "error": "usage",
        "message": error.message,
    })
}

fn render_drift_json(drift: &FileDrift) -> serde_json::Value {
    json!({
        "path": display_path(&drift.repo_path),
        "system_path": display_path(&drift.system_path),
        "diff": drift.diff,
    })
}

fn render_error_human(error: &ErrorReport) -> String {
    match error.code {
        "drift_detected" => "dotsync: drift detected".to_string(),
        _ => format!("dotsync: {}", error.message),
    }
}

fn render_conflict_human(conflict: &dotsync::CascadePause) -> String {
    let conflicted_files = conflict
        .conflicted_files
        .iter()
        .map(|path| format!("- {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    let colliding_scopes = conflict.parent_scopes.join(", ");

    format!(
        "dotsync: cascade paused due to conflicts\n\nDotsync is propagating a config change through the scope branch DAG so shared changes reach every affected machine. Different scopes exist because some config is shared across all machines, some is shared by subsets like an OS or desktop environment, and some is machine-specific. This pause happened because the same file was changed differently on branches that now need to be merged.\n\nScope DAG:\n{}\n\nPaused at scope: {}\nMerging changes from {} into {}\nConflicted files:\n{}\n\nWhat to do next:\n- Edit the conflicted files in ~/dotfiles/ and remove the conflict markers, keeping the content you want in this paused scope.\n- Run `dotsync continue` to resume the cascade.\n- The cascade may pause again on a later scope; if it does, repeat this process.\n\nAgent notes:\n- The scope you are resolving may belong to another machine; that is expected because dotsync cascades through every affected descendant scope.\n- When the cascade finishes, dotsync returns you to your machine scope: {}.\n- Do not run other dotsync commands while this cascade is paused.\n- `dotsync abort` is planned but is not implemented yet in this build.",
        conflict.scope_dag,
        conflict.scope,
        colliding_scopes,
        conflict.scope,
        conflicted_files,
        conflict.machine_scope,
    )
}

fn success_notes_for_drifts(drifts: &[FileDrift]) -> Vec<String> {
    if drifts.is_empty() {
        return Vec::new();
    }
    let mut notes = vec![format!(
        "dotsync: overwrote {} drifted file(s)",
        drifts.len()
    )];
    notes.extend(drifts.iter().flat_map(|drift| {
        [
            format!("- {}", drift.repo_path.display()),
            drift.diff.clone(),
        ]
    }));
    notes
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
