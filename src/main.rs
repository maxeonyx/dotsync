use clap::{Parser, Subcommand, ValueEnum};
use dotsync::{
    abort_paused_cascade, commit_and_sync, continue_after_conflict, create_scope, diff_home,
    discard, init, status, sync, view, CommitOptions, DiffReport, DotsyncError, DotsyncPaths,
    Explanation, MachineState, Resumed, Run, UnreachableRemote, ViewAnswer,
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
  1  it did not, or `dotsync diff` found changes. Which of those, and what kind
     of stop it was, is in `--output json`: `status` is \"error\" for a stop and
     \"ok\" for the changes `diff` found, and `error` names the kind — including
     `cascade_paused`, the one that means a merge is waiting for you

Examples:
  $ dotsync
  $ dotsync commit linux -m \"add bashrc\" .bashrc
  $ dotsync init <url>";

const INIT_ABOUT: &str = "Clone or join a dotsync remote";

const INIT_LONG_ABOUT: &str = "REMOTE_URL is the git remote that stores your dotsync repo.

`dotsync init` clones the repo into ~/.local/share/dotsync/repo, creates this machine's own scope, and syncs it into home.

Joining a remote that already has scopes means saying where this machine's config comes from: `--parent work-linux`. A hostname cannot tell a `home-linux` from a `work-linux`, and a scope is created where its parents are and never moved, so this is the one moment that answer can be given. Run `dotsync view` on another machine to see the scopes there are. Give `--parent` more than once for a machine that inherits from several scopes.

A remote with no scopes on it yet has nothing to choose from: this machine gets the root scope `all`, a scope for its OS, and its own scope under that.

If REMOTE_URL is omitted, dotsync asks for it.";

const CREATE_SCOPE_ABOUT: &str = "Create a scope for config that several machines share";

const CREATE_SCOPE_LONG_ABOUT: &str = "NAME is what the new scope is called. `--parent` is where it hangs: config on the parents reaches it, and config committed to it reaches every machine that hangs off it in turn.

Creating a scope is the only thing that can happen to the scope graph. Nothing renames, moves or deletes one, which is what lets dotsync read the graph off its own history instead of a file that can disagree with it.

Machines join a scope with `dotsync init <remote-url> --parent <name>`, so a scope created now is for the machines that join under it.

`-m` says what belongs on the scope, for whoever reads `dotsync view` later. A name that says it already — `hyprland`, `work` — needs nothing.";

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

A run reports both halves of what that came to: `newly_tracked` for the files it put on the scope for the first time, and `skipped_paths` for the files under a named directory it left alone. Both appear in `--output json` and as notes on stderr.";

const DISCARD_ABOUT: &str = "Throw away the local changes at the paths you name";

const DISCARD_LONG_ABOUT: &str = "PATHS are home-relative files whose local changes to throw away: dotsync writes the scope's version of each one into home instead, and syncs as usual.

This is the other way a local change ends. `dotsync commit` makes it everybody's; `dotsync discard` decides against it. Deleting the file yourself is neither — a deletion is a local change too, so home would come back empty rather than canonical.

Every path must be one of the changes `dotsync status` lists. Naming anything else is a stop rather than a run that discarded nothing, because discarding cannot be undone.";

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
}

#[derive(Debug, Clone)]
enum Action {
    Sync,
    Init {
        remote_url: InitRemote,
        parents: Vec<String>,
    },
    CreateScope {
        scope: String,
        parents: Vec<String>,
        description: Option<String>,
    },
    Commit {
        scope: String,
        message: String,
        paths: Vec<PathBuf>,
    },
    Discard {
        paths: Vec<PathBuf>,
    },
    Continue,
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

        /// Scope this machine's config comes from; repeat for several
        #[arg(long = "parent")]
        parents: Vec<String>,
    },
    #[command(name = "create-scope", about = CREATE_SCOPE_ABOUT, long_about = CREATE_SCOPE_LONG_ABOUT)]
    CreateScope {
        /// Name for the new scope
        scope: String,

        /// Scope the new one hangs off; repeat for several
        #[arg(long = "parent")]
        parents: Vec<String>,

        /// What belongs on this scope
        #[arg(short = 'm', long = "message")]
        description: Option<String>,
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
    #[command(about = DISCARD_ABOUT, long_about = DISCARD_LONG_ABOUT)]
    Discard {
        /// Home-relative paths whose local changes to throw away
        #[arg(required = true)]
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

    /// Adds to what this run has to say rather than replacing it: a run that
    /// overwrote a file and also has something to explain about why owes the
    /// reader both.
    fn with_notes(mut self, notes: Vec<String>) -> Self {
        self.notes.extend(notes);
        self
    }
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
    /// How the user invoked dotsync, when they invoked something it
    /// recognises: the words to type to run this command again.
    ///
    /// Carried so that a stop can finish its advice with the command to rerun.
    /// The errors that need it are raised far from any knowledge of it — "your
    /// machine is not initialized" comes out of opening the repo — and the
    /// alternative was what dotsync used to do: tell everybody to rerun
    /// `dotsync status`, including the agent who ran `dotsync commit`.
    ///
    /// An invocation, deliberately, and not the name the JSON payload uses for
    /// the same command. For plain sync those differ — the payload says
    /// `"sync"` and the invocation is bare `dotsync`, because `dotsync sync`
    /// is not a subcommand — and advice that names something dotsync does not
    /// recognise is the defect this field exists to prevent.
    invocation: Option<&'static str>,
}

#[derive(Debug)]
enum OutputKind {
    Success(SuccessOutput),
    Error(DotsyncError),
    /// The command line was wrong, so there was never a run. It reports in the
    /// same shape as everything else: `Explanation` is what a stop is, and a
    /// caller that has learned to read one has learned to read the first error
    /// it is ever likely to meet.
    Usage(Explanation),
}

impl CliOutput {
    /// Output from something that never became a run: a bad command line, or
    /// an environment dotsync cannot work in.
    fn without_run(kind: OutputKind) -> Self {
        Self {
            kind,
            unreachable_remote: None,
            invocation: None,
        }
    }
}

/// Turns a finished run into output, carrying what the run could not do onto
/// whichever arm it ended in. The one place that decision is made.
fn output_of<T, E: Into<DotsyncError>>(
    invocation: &'static str,
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
        invocation: Some(invocation),
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
        Err(message) => Ok(CliOutput::without_run(OutputKind::Usage(usage_error(
            &message,
        )))),
    };

    let exit_code = match outcome {
        Ok(output) => emit_output(&output_format, output),
        Err(error) => emit_output(
            &output_format,
            CliOutput::without_run(OutputKind::Error(error)),
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
            render::render_error_json(&usage_error(message.trim_end()))
        );
    }
    1
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
    fn try_from_cli(cli: Cli, context: CliContext) -> Result<Self, String> {
        match cli.command {
            Some(Command::Init {
                remote_url,
                parents,
            }) => {
                let remote_url = init_remote_from_args(remote_url, context)?;
                Ok(Self::Init {
                    remote_url,
                    parents,
                })
            }
            Some(Command::CreateScope {
                scope,
                parents,
                description,
            }) => Ok(Self::CreateScope {
                scope,
                parents,
                description,
            }),
            Some(Command::Discard { paths }) => Ok(Self::Discard { paths }),
            Some(Command::Continue) => Ok(Self::Continue),
            Some(Command::Abort) => Ok(Self::Abort),
            Some(Command::Status) => Ok(Self::Status),
            Some(Command::Diff) => Ok(Self::Diff),
            Some(Command::View { scope, file }) => Ok(Self::View { scope, file }),
            Some(Command::Commit {
                scope,
                message,
                paths,
            }) => Ok(Self::Commit {
                scope,
                message,
                paths,
            }),
            Some(Command::Unknown(args)) => {
                let command = args.first().map(String::as_str).unwrap_or("<empty>");
                Err(format!(
                    "unknown command `{command}`; run `dotsync --help` for supported commands"
                ))
            }
            None => Ok(Self::Sync),
        }
    }
}

fn init_remote_from_args(
    remote_url: Option<String>,
    context: CliContext,
) -> Result<InitRemote, String> {
    if let Some(remote_url) = remote_url {
        return Ok(InitRemote::Provided(remote_url));
    }

    if context.interactive_terminal {
        return Ok(InitRemote::Prompt);
    }

    Err(INIT_REMOTE_URL_USAGE.to_string())
}

async fn dispatch(action: Action) -> Result<CliOutput, DotsyncError> {
    match action {
        Action::Sync => run_sync().await,
        Action::Commit {
            scope,
            message,
            paths,
        } => run_commit(scope, message, paths).await,
        Action::Discard { paths } => run_discard(paths).await,
        Action::Init {
            remote_url,
            parents,
        } => run_init(remote_url, parents).await,
        Action::CreateScope {
            scope,
            parents,
            description,
        } => run_create_scope(scope, parents, description).await,
        Action::Continue => run_continue().await,
        Action::Abort => run_abort().await,
        Action::Status => run_status().await,
        Action::Diff => run_diff().await,
        Action::View { scope, file } => run_view(scope, file).await,
    }
}

/// A command line that never started a run is still a stop, and it reports in
/// the shape every other stop has — so a caller that has learned to read one
/// payload has learned to read the first error it is ever likely to meet.
fn usage_error(message: &str) -> Explanation {
    Explanation {
        code: "usage",
        message: message.to_string(),
        paused_cascade: None,
        current_state: Vec::new(),
        conflicts: Vec::new(),
        teaching: None,
    }
}

async fn run_init(remote_url: InitRemote, parents: Vec<String>) -> Result<CliOutput, DotsyncError> {
    let remote_url = match remote_url {
        InitRemote::Provided(remote_url) => remote_url,
        InitRemote::Prompt => match prompt_init_remote_url() {
            Ok(remote_url) => remote_url,
            Err(message) => {
                return Ok(CliOutput::without_run(OutputKind::Usage(usage_error(
                    &message,
                ))))
            }
        },
    };
    let paths = discover_paths()?;
    let run = init(&paths, &remote_url, &parents).await;
    Ok(output_of("dotsync init", run, |report| {
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

async fn run_create_scope(
    scope: String,
    parents: Vec<String>,
    description: Option<String>,
) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = create_scope(&paths, &scope, &parents, description.as_deref()).await;
    Ok(output_of("dotsync create-scope", run, |report| {
        SuccessOutput::message(
            json!({
                "status": "ok",
                "command": "create-scope",
                "scope": report.scope,
                "parents": report.parents,
            }),
            format!(
                "dotsync: created scope {} under {}",
                report.scope,
                report.parents.join(", ")
            ),
        )
        .with_notes(render::push_notes(&report.push))
    }))
}

fn prompt_init_remote_url() -> Result<String, String> {
    eprint!("dotsync init remote URL: ");
    io::stderr()
        .flush()
        .map_err(|err| format!("init could not write prompt: {err}"))?;

    let mut remote_url = String::new();
    io::stdin()
        .read_line(&mut remote_url)
        .map_err(|err| format!("init could not read remote URL: {err}"))?;
    let remote_url = remote_url.trim().to_string();
    if remote_url.is_empty() {
        return Err(INIT_REMOTE_URL_USAGE.to_string());
    }
    Ok(remote_url)
}

async fn run_continue() -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = continue_after_conflict(&paths).await;
    Ok(output_of("dotsync continue", run, |report| {
        let synced = report.sync.synced_paths.len();
        let output = render::synced_output(
            "continue",
            match &report.resumed {
                Resumed::Cascade { scope, .. } => format!(
                    "dotsync: recorded your version on `{scope}` and synced {synced} file(s)"
                ),
                Resumed::SyncConflict => format!(
                    "dotsync: took your version of the conflicted file(s) and synced {synced} file(s)"
                ),
            },
            &report.sync,
            Some(&report.push),
        );
        // The sync reports discarding the resolution out of home, which is
        // correct and reads exactly like losing work. What makes it not that
        // is the scope it went to, so the run says so here rather than leaving
        // the reader to reconcile two of its own lines.
        let Resumed::Cascade {
            scope,
            borrowed_from: Some(machine_scope),
        } = &report.resumed
        else {
            return output;
        };
        output.with_notes(vec![format!(
            "dotsync: the conflicted file(s) held `{scope}`'s merge while you resolved it; `{machine_scope}`'s own version is back in home now, and `{scope}` has your resolution."
        )])
    }))
}

async fn run_abort() -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = abort_paused_cascade(&paths).await;
    Ok(output_of("dotsync abort", run, |report| {
        // `abort` publishes nothing, so it has no push to report — and the one
        // thing it knows that the other syncing commands do not is where the
        // cascade it discarded had stopped.
        let synced = report.sync.synced_paths.len();
        let mut output = render::synced_output(
            "abort",
            match &report.still_paused {
                None => format!(
                    "dotsync: discarded the merge paused at `{}` and synced {synced} file(s)",
                    report.paused_scope
                ),
                // Home went back, and that is all that happened: saying the
                // merge was discarded would be describing the pause as over.
                Some(scope) => format!(
                    "dotsync: put home back and synced {synced} file(s); the merge at `{scope}` is still waiting"
                ),
            },
            &report.sync,
            None,
        );
        output.json["paused_scope"] = json!(report.paused_scope);
        // Abort discards what this machine committed, so a conflict that came
        // from the remote is still there afterwards. Reported under the name
        // every other command uses, with the code that means a pause is
        // waiting: a run that says it succeeded and leaves the next command
        // stopping on the same merge is the disagreement worth avoiding.
        let Some(scope) = &report.still_paused else {
            return output;
        };
        output.json["paused_cascade"] = json!(scope);
        // The run did not do what it says on the tin: home went back, and the
        // merge it was asked to end is still waiting.
        output.exit_code = 1;
        output.notes.extend([
            format!("dotsync: the conflict at `{scope}` came from the remote rather than from anything this machine committed, so there was nothing to take back and it is still waiting"),
            "dotsync: aborting again will not clear it — edit the conflicted file(s) in home to the merged contents you want and run `dotsync continue`.".to_string(),
        ]);
        output
    }))
}

async fn run_sync() -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = sync(&paths).await;
    Ok(output_of("dotsync", run, |report| {
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

async fn run_discard(discard_paths: Vec<PathBuf>) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = discard(&paths, &discard_paths).await;
    Ok(output_of("dotsync discard", run, |report| {
        render::synced_output(
            "discard",
            format!(
                "dotsync: discarded {} local change(s) and synced {} file(s) for {}",
                report.sync.drifts.len(),
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
    Ok(output_of("dotsync status", run, |report| {
        with_machine_state(
            SuccessOutput::message(
                json!({
                    "status": "ok",
                    "command": "status",
                    "changes": render::changes_json(&report.changes),
                    "incoming": render::changes_json(&report.incoming),
                }),
                render_status_human(&report),
            ),
            &report.machine,
        )
    }))
}

async fn run_diff() -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = diff_home(&paths).await;
    Ok(output_of("dotsync diff", run, |report| SuccessOutput {
        // Drift is what `diff` exists to report, so it is not an error — but
        // scripts and agents need to tell clean from dirty without parsing.
        exit_code: if report.drifts.is_empty() { 0 } else { 1 },
        // The same changes `status` lists, under the same name, with the diffs
        // shown. That is the whole difference between the two commands.
        ..with_machine_state(
            SuccessOutput::message(
                json!({
                    "status": "ok",
                    "command": "diff",
                    "changes": report
                        .drifts
                        .iter()
                        .map(render::render_drift_json)
                        .collect::<Vec<_>>(),
                }),
                render_diff_human(&report),
            ),
            &report.machine,
        )
    }))
}

async fn run_view(scope: Option<String>, file: Option<PathBuf>) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = view(&paths, scope.as_deref(), file.as_deref()).await;
    Ok(output_of("dotsync view", run, |report| {
        // What is true of the machine is not true of the question asked, so it
        // is added once here rather than in each of the four shapes.
        let machine = report.machine;
        let answer = match report.found {
            ViewAnswer::FileContents {
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
            ViewAnswer::Scope { scope, files } => SuccessOutput::stdout(
                json!({
                    "status": "ok",
                    "command": "view",
                    "scope": scope,
                    "files": files.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
                }),
                render_view_scope_stdout(&scope, &files),
            ),
            ViewAnswer::FileScopes {
                file,
                scopes,
                owner,
            } => SuccessOutput::stdout(
                json!({
                    "status": "ok",
                    "command": "view",
                    "file": render::display_path(&file),
                    "scopes": scopes,
                    "owner": owner,
                }),
                render_view_file_scopes_stdout(&file, &scopes, owner.as_deref()),
            ),
            ViewAnswer::Overview { scopes, files } => SuccessOutput::stdout(
                json!({
                    "status": "ok",
                    "command": "view",
                    "scopes": scopes.iter().map(|scope| json!({
                        "name": scope.name,
                        "parents": scope.parents,
                        "description": scope.description,
                    })).collect::<Vec<_>>(),
                    "files": files.iter().map(|path| render::display_path(path)).collect::<Vec<_>>(),
                }),
                render_view_overview_stdout(&scopes, &files, &machine.machine_scope),
            ),
        };

        with_machine_state(reprinting_any_conflict(answer, &machine), &machine)
    }))
}

async fn run_commit(
    scope: String,
    message: String,
    commit_paths: Vec<PathBuf>,
) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    // Whether the caller named anything, which decides what a commit that
    // recorded nothing has to explain.
    let named_paths = !commit_paths.is_empty();
    let run = commit_and_sync(
        &paths,
        CommitOptions {
            scope,
            message,
            paths: commit_paths,
        },
    )
    .await;
    Ok(output_of("dotsync commit", run, |report| {
        render_commit_success(report, named_paths)
    }))
}

/// A commit has two outcomes and says which one it had, because they are not
/// the same event: one wrote history and synced home, the other did neither.
/// The fields that only one of them can honestly fill are only on that one.
fn render_commit_success(report: dotsync::CommitReport, named_paths: bool) -> SuccessOutput {
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
        // A commit that named nothing records only changes to files dotsync
        // already tracks, so this is the answer an agent gets after writing a
        // config file dotsync has never seen — and `status` did not list it
        // either, for the same reason. Saying so here is the only place that
        // reaches them.
        let new_file_advice = match named_paths {
            true => Vec::new(),
            false => vec![
                "dotsync: a file dotsync does not track yet is not a change to it, so a commit naming no paths never adds one. Name the file, or the directory it is in, to start tracking it."
                    .to_string(),
            ],
        };
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
                .chain(new_file_advice)
                .chain(render::push_notes(&report.push))
                .collect(),
        );
    };

    json["synced_files"] = json!(render::display_paths(&recorded.sync.synced_paths));
    json["newly_tracked"] = json!(render::display_paths(&recorded.newly_tracked));
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
            .chain(render::success_notes(
                &recorded.sync.drifts,
                Some(&report.push),
            ))
            .collect(),
    )
}

/// The conflict a machine is paused on, printed again by `view`.
///
/// Every version of every conflicted file exists nowhere else: dotsync writes
/// no markers into home, and neither side is on a scope this machine syncs
/// from. So the pause message is the only copy, and an agent that lost it — a
/// new session, a scrolled terminal — needs somewhere to ask. `view` is that
/// somewhere, because it already answers "what is checked in" and the pause is
/// derived, so it is correct whenever it is asked.
///
/// `status` and `diff` name the pause and stop there. `status` is the concise
/// one, and three versions of every conflicted file is not concise.
fn reprinting_any_conflict(answer: SuccessOutput, machine: &MachineState) -> SuccessOutput {
    let Some(paused) = &machine.paused_cascade else {
        return answer;
    };
    if paused.conflicts.is_empty() {
        return answer;
    }
    let mut json = answer.json;
    json["conflicts"] = json!(paused
        .conflicts
        .iter()
        .map(render::render_conflict_json)
        .collect::<Vec<_>>());
    let mut human = vec![format!(
        "dotsync: the merge at `{}` is waiting on these file(s)",
        paused.scope
    )];
    human.extend(render::render_conflicts_human(&paused.conflicts));
    SuccessOutput { json, ..answer }.with_notes(human)
}

/// What `status`, `diff` and `view` say about the machine, whatever they were
/// asked, in both channels.
///
/// One function for all three commands and all four of `view`'s shapes,
/// because these facts are true of the machine rather than of the question:
/// adding them per command per shape is how one of them came to carry a fact
/// the other two did not. `paused_cascade` is present only when there is one,
/// the same shape as `remote_unreachable` and for the same reason — a machine
/// with no pause and a run with nothing to say about one are the same answer
/// to whoever reads that field.
fn with_machine_state(answer: SuccessOutput, machine: &MachineState) -> SuccessOutput {
    let mut json = answer.json;
    json["machine_scope"] = json!(machine.machine_scope);
    if let Some(paused) = &machine.paused_cascade {
        json["paused_cascade"] = json!(paused.scope);
    }
    json["diverged_scopes"] = json!(machine.diverged_scopes);
    json["unpushed_scopes"] = json!(machine.unpushed_scopes);
    SuccessOutput { json, ..answer }.with_notes(
        render::paused_cascade_notes(machine.paused_cascade.as_ref().map(|paused| &paused.scope))
            .into_iter()
            .chain(render::diverged_scope_notes(&machine.diverged_scopes))
            .chain(render::unpushed_scope_notes(&machine.unpushed_scopes))
            .collect(),
    )
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
            &report.machine.machine_scope,
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
            report.machine.machine_scope
        ));
        lines.extend(
            report
                .incoming
                .iter()
                .map(|change| render::render_change_line(&change.path, change.state)),
        );
    }

    if lines.is_empty() {
        return format!("dotsync: no changes for {}", report.machine.machine_scope);
    }
    lines.join("\n")
}

/// `status`'s changed list, with each file's two sides shown under it.
fn render_diff_human(report: &DiffReport) -> String {
    if report.drifts.is_empty() {
        return format!("dotsync: no changes for {}", report.machine.machine_scope);
    }

    let mut lines = vec![changed_files_header(
        report.drifts.len(),
        &report.machine.machine_scope,
    )];
    for drift in &report.drifts {
        lines.push(render::render_change_line(&drift.repo_path, drift.state));
        lines.push(render::render_drift_diff(drift));
    }
    lines.join("\n")
}

fn render_view_overview_stdout(
    scopes: &[dotsync::ScopeInfo],
    files: &[PathBuf],
    machine_scope: &str,
) -> String {
    render_lines(
        std::iter::once("Scopes".to_string())
            .chain(
                scopes
                    .iter()
                    .map(|scope| render_scope_line(scope, machine_scope)),
            )
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

/// Which scopes hold a file — including none of them, which used to print two
/// headings with nothing between them. That is the answer to the commonest
/// reason for asking, a typo, and it read like a bug instead.
///
/// Still exit 0: "no scope holds this" is an answer to the question asked.
/// Asking for the *contents* of a file on a named scope is a different
/// question, and having none to print is a stop.
fn render_view_file_scopes_stdout(
    path: &std::path::Path,
    scopes: &[String],
    owner: Option<&str>,
) -> String {
    let path = render::display_path(path);
    let Some(owner) = owner else {
        return render_lines([
            format!("File {path}"),
            format!("No scope holds {path}."),
            "Run `dotsync view` to see every file the scopes do hold.".to_string(),
        ]);
    };
    render_lines(
        [
            format!("File {path}"),
            format!("Owned by {owner}; every other scope below has it from the cascade."),
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

/// One scope, its parents, and whether it is the machine reading this.
///
/// The marker is the answer to "where am I?", which is the question `view`
/// exists for and the one thing the list could not say: two machine scopes
/// rendered identically apart from their names.
fn render_scope_line(scope: &dotsync::ScopeInfo, machine_scope: &str) -> String {
    let here = match scope.name == machine_scope {
        true => "* ",
        false => "  ",
    };
    let mut line = match scope.parents.as_slice() {
        [] => format!("{here}{}", scope.name),
        parents => format!("{here}{} <- {}", scope.name, parents.join(", ")),
    };
    // Only the scopes whose creator said what they are for carry this, so a
    // graph of self-evident names reads as a graph and nothing else.
    if let Some(description) = &scope.description {
        line.push_str(&format!("  # {description}"));
    }
    line
}

fn emit_output(output_format: &OutputFormat, output: CliOutput) -> i32 {
    let CliOutput {
        kind,
        unreachable_remote,
        invocation,
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
        OutputKind::Error(error) => emit_stop(
            output_format,
            error.explain(invocation),
            unreachable_remote.as_ref(),
        ),
        OutputKind::Usage(explanation) => {
            emit_stop(output_format, explanation, unreachable_remote.as_ref())
        }
    }
}

/// A run that stopped, or a command line that never started one: the teaching
/// block, the conflict if there is one, the payload, and 1.
fn emit_stop(
    output_format: &OutputFormat,
    explanation: Explanation,
    unreachable_remote: Option<&UnreachableRemote>,
) -> i32 {
    eprintln!("{}", render::render_error_human(&explanation));
    // The conflict itself, after the teaching block and apart from it,
    // because it is the material to work from rather than more
    // instructions — and because it is what dotsync hands over
    // *instead* of writing markers into the file.
    if !explanation.conflicts.is_empty() {
        eprintln!("\nConflicted files:");
        for line in render::render_conflicts_human(&explanation.conflicts) {
            eprintln!("{line}");
        }
    }
    if matches!(output_format, OutputFormat::Json) {
        println!(
            "{}",
            render::with_remote_state(render::render_error_json(&explanation), unreachable_remote)
        );
    }
    1
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
