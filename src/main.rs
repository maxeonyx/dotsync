use clap::{Parser, Subcommand, ValueEnum};
use dotsync::{
    abort_paused_cascade, commit_and_sync, continue_after_conflict, diff_home, init,
    list_scope_tree, list_scopes, read_scope_file, status, sync, ChangeStatus, CommandOutcome,
    CommitOptions, CommitSelection, DiffReport, DotsyncError, DotsyncPaths, FileDrift,
    ScopeListReport, SyncOptions, TreeReport,
};
mod render;
use serde_json::json;
use std::collections::BTreeSet;
use std::env;
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

`dotsync init` clones the repo into ~/.local/share/dotsync/repo, detects this machine, sets up any missing scope branches for its OS and machine, and syncs the resulting machine scope into home.";

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
    #[arg(long = "output", value_enum, default_value = "human")]
    output_format: OutputFormat,

    /// Proceed even when drift is detected
    #[arg(long, global = true)]
    force: bool,
}

#[derive(Debug, Clone)]
enum Action {
    Sync {
        force: bool,
    },
    Init {
        remote_url: String,
    },
    Commit {
        scope: String,
        message: String,
        force: bool,
        selection: CommitSelection,
    },
    Continue {
        force: bool,
    },
    Abort {
        force: bool,
    },
    Status {
        force: bool,
    },
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
        remote_url: String,
    },
    /// Commit selected home changes to a scope, cascade, sync, and push
    Commit {
        /// Scope to commit changes to
        scope: String,

        /// Commit message
        #[arg(short = 'm', long = "message")]
        message: String,

        /// Commit every managed file that differs from the repo
        #[arg(long)]
        all: bool,

        /// Repo-relative file or directory paths to commit
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

#[derive(Debug, Clone)]
struct SuccessOutput {
    json: serde_json::Value,
    human: String,
    notes: Vec<String>,
    stdout: Option<String>,
    exit_code: i32,
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
        match cli.command {
            Some(Command::Init { remote_url }) => Ok(Self::Init { remote_url }),
            Some(Command::Continue) => Ok(Self::Continue { force: cli.force }),
            Some(Command::Abort) => Ok(Self::Abort { force: cli.force }),
            Some(Command::Status) => Ok(Self::Status { force: false }),
            Some(Command::Diff) => Ok(Self::Diff),
            Some(Command::View { scope, file }) => Ok(Self::View { scope, file }),
            Some(Command::Commit {
                scope,
                message,
                all,
                paths,
            }) => {
                let selection = match (all, paths.is_empty()) {
                    (true, false) => {
                        return Err(usage_error(
                            "commit mode accepts explicit paths or --all, not both",
                        ));
                    }
                    (true, true) => CommitSelection::All,
                    (false, _) => CommitSelection::Paths(paths),
                };

                Ok(Self::Commit {
                    scope,
                    message,
                    force: cli.force,
                    selection,
                })
            }
            Some(Command::Unknown(args)) => {
                let command = args.first().map(String::as_str).unwrap_or("<empty>");
                Err(usage_error(&format!(
                    "unknown command `{command}`; run `dotsync --help` for supported commands"
                )))
            }
            None => Ok(Self::Sync { force: cli.force }),
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
        Action::Abort { force } => run_abort(force).await,
        Action::Status { force } => run_status(force).await,
        Action::Diff => run_diff().await,
        Action::View { scope, file } => run_view(scope, file).await,
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
        stdout: None,
        exit_code: 0,
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
            stdout: None,
            exit_code: 0,
        })),
    }
}

async fn run_abort(force: bool) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    match abort_paused_cascade(&paths, SyncOptions { force }).await? {
        CommandOutcome::Success(report) => Ok(CliOutput::Success(SuccessOutput {
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
            notes: render::success_notes_for_drifts(&report.sync.drifts),
            stdout: None,
            exit_code: 0,
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
        stdout: None,
        exit_code: 0,
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
        stdout: None,
        exit_code: 0,
    }))
}

async fn run_diff() -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let report = diff_home(&paths).await?;
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
    }))
}

async fn run_view(scope: Option<String>, file: Option<PathBuf>) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    match (scope, file) {
        (Some(scope), Some(file)) => {
            let report = read_scope_file(&paths, &scope, &file).await?;
            Ok(file_success_output(
                "view",
                &report.scope,
                &report.path,
                report.contents,
            ))
        }
        (Some(scope), None) => {
            let report = list_scope_tree(&paths, &scope).await?;
            Ok(CliOutput::Success(SuccessOutput {
                json: json!({
                    "status": "ok",
                    "command": "view",
                    "scope": report.scope,
                    "files": report.paths.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
                }),
                human: String::new(),
                notes: Vec::new(),
                stdout: Some(render_view_scope_stdout(&report)),
                exit_code: 0,
            }))
        }
        (None, Some(file)) => run_view_file_scopes(&paths, file).await,
        (None, None) => run_view_overview(&paths).await,
    }
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
            stdout: None,
            exit_code: 0,
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

async fn run_view_overview(paths: &DotsyncPaths) -> Result<CliOutput, DotsyncError> {
    let scopes = list_scopes(paths).await?;
    let mut files = BTreeSet::new();
    for scope in &scopes.scopes {
        for path in list_scope_tree(paths, &scope.name).await?.paths {
            files.insert(path);
        }
    }

    Ok(CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": "view",
            "scopes": scopes.scopes.iter().map(|scope| json!({
                "name": scope.name,
                "parents": scope.parents,
            })).collect::<Vec<_>>(),
            "files": files.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
        }),
        human: String::new(),
        notes: Vec::new(),
        stdout: Some(render_view_overview_stdout(&scopes, &files)),
        exit_code: 0,
    }))
}

async fn run_view_file_scopes(
    paths: &DotsyncPaths,
    file: PathBuf,
) -> Result<CliOutput, DotsyncError> {
    let scopes = list_scopes(paths).await?;
    let mut matching_scopes = Vec::new();
    for scope in &scopes.scopes {
        let tree = list_scope_tree(paths, &scope.name).await?;
        if tree.paths.iter().any(|path| path == &file) {
            matching_scopes.push(scope.name.clone());
        }
    }

    Ok(CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": "view",
            "file": render::display_path(&file),
            "scopes": matching_scopes,
        }),
        human: String::new(),
        notes: Vec::new(),
        stdout: Some(render_view_file_scopes_stdout(&file, &matching_scopes)),
        exit_code: 0,
    }))
}

fn render_view_overview_stdout(report: &ScopeListReport, files: &BTreeSet<PathBuf>) -> String {
    let mut lines = Vec::new();
    lines.push("Scopes".to_string());
    lines.extend(report.scopes.iter().map(render_scope_line));
    lines.push(String::new());
    lines.push("Files".to_string());
    lines.extend(files.iter().map(|path| render::display_path(path)));
    lines.push(String::new());
    lines.join("\n")
}

fn render_view_scope_stdout(report: &TreeReport) -> String {
    let mut lines = vec![format!("Scope {}", report.scope)];
    lines.extend(report.paths.iter().map(|path| render::display_path(path)));
    lines.push(String::new());
    lines.join("\n")
}

fn render_view_file_scopes_stdout(path: &std::path::Path, scopes: &[String]) -> String {
    let mut lines = vec![
        format!("File {}", render::display_path(path)),
        "Scopes".to_string(),
    ];
    lines.extend(scopes.iter().cloned());
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

fn file_success_output(
    command: &str,
    scope: &str,
    path: &std::path::Path,
    contents: Vec<u8>,
) -> CliOutput {
    CliOutput::Success(SuccessOutput {
        json: json!({
            "status": "ok",
            "command": command,
            "scope": scope,
            "path": render::display_path(path),
            "contents": String::from_utf8_lossy(&contents),
        }),
        human: String::new(),
        notes: Vec::new(),
        stdout: Some(String::from_utf8_lossy(&contents).into_owned()),
        exit_code: 0,
    })
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
            if matches!(output_format, OutputFormat::Json) {
                println!("{}", success.json);
            } else if let Some(stdout) = success.stdout {
                print!("{stdout}");
            } else {
                eprintln!("{}", success.human);
            }
            success.exit_code
        }
        CliOutput::Error(error) => {
            let exit_code = if matches!(
                error,
                DotsyncError::CascadePaused { .. } | DotsyncError::ConcurrentScopeConflict { .. }
            ) {
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
