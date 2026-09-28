use clap::{Parser, Subcommand, ValueEnum};
use dotsync::{
    abort_paused_cascade, commit_and_sync, compare, continue_after_conflict, create_scope,
    delete_scope, diff_home, discard, files, init, place, scopes, show, status, sync,
    CommitOptions, DiffReport, DotsyncError, DotsyncPaths, Explanation, FileRow, FilesQuery,
    InitOptions, MachineEffect, MachineState, PlacementOptions, Planned, Resumed, Run, ScopeInfo,
    Standing, UnreachableRemote,
};
mod render;
use serde_json::json;
use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

const TOP_LEVEL_ABOUT: &str = "Agent-first dotfile sync";

const TOP_LEVEL_LONG_ABOUT: &str = "dotsync keeps your dotfiles in a hidden repo of scopes at ~/.local/share/dotsync/repo, and syncs this machine's scope into your home directory. You never touch that repo: every question about it and every change to it is a dotsync command.

A scope is a layer of config. Shared config lives on scopes such as `all` or `linux`; every scope inherits what the scopes above it hold, and a machine's own scope is the one at the bottom. A scope can add a file, override one it inherits with its own version, or remove one. Any scope can be read from any machine.

Everyday:
  - plain `dotsync` syncs your current machine scope into home, with whatever other machines published
  - edit files in home, then run `dotsync commit <scope> -m \"message\" <path>...` to record the change on this machine's scope or one above it
  - `dotsync status` lists what you changed here and what is incoming

Reading the fleet:
  - `dotsync scopes` shows the scopes, where each hangs, and the machines each reaches
  - `dotsync files [--own] [--scope <scope>] [<path>...]` shows what each scope holds, and whether it inherited, added, overrode or removed it
  - `dotsync show <scope> <path>` prints a file as one scope holds it
  - `dotsync diff <scope>` shows what a scope changes over what it inherits; `dotsync diff <scope> <scope>` compares two

Rearranging config, on any scope, with content taken from the repo:
  - `dotsync move <path>... --from <scope> --to <scope> -m \"message\"` gives the second scope the first one's version
  - `dotsync drop <path>... --from <scope> -m \"message\"` makes a scope take what it inherits
  - add `--dry-run` to any write to see what it would change on each machine first

When a merge stops, resolve it in home and run `dotsync continue`, or run `dotsync abort`.";

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

Joining a remote that already has scopes means saying where this machine's config comes from: `--parent work-linux`. A hostname cannot tell a `home-linux` from a `work-linux`, so this is the one moment that answer can be given. Run `dotsync init <remote-url>` without `--parent` to clone the fleet and stop: `dotsync scopes`, `dotsync files` and `dotsync show` then read it, and `dotsync init --parent <scope>` finishes joining. Give `--parent` more than once for a machine that inherits from several scopes.

A file home already holds that the scope holds differently is never overwritten: the first sync stops and shows both versions. Keep yours with `dotsync continue`, or take the scope's with `dotsync discard <path>`.

This machine's own scope is named after its hostname unless `--name` says otherwise, and whichever it is, the machine keeps that name: later runs read it from the machine's own record, not from the hostname.

A remote with no scopes on it yet has nothing to choose from: this machine gets the root scope `all`, a scope for its OS, and its own scope under that.

If REMOTE_URL is omitted, dotsync asks for it.";

const CREATE_SCOPE_ABOUT: &str = "Create a scope for config that several machines share";

const CREATE_SCOPE_LONG_ABOUT: &str = "NAME is what the new scope is called. `--parent` is where it hangs: config on the parents reaches it, and config committed to it reaches every machine that hangs off it in turn.

Creating and deleting are the only things that can happen to the scope graph. Nothing renames or moves a scope, which is what lets dotsync read the graph off its own history instead of a file that can disagree with it.

Machines join a scope with `dotsync init <remote-url> --parent <name>`, so a scope created now is for the machines that join under it.

`-m` says what belongs on the scope, for whoever reads `dotsync scopes` later. A name that says it already — `hyprland`, `work` — needs nothing.";

const DELETE_SCOPE_ABOUT: &str = "Delete a scope whose machine is gone";

const DELETE_SCOPE_LONG_ABOUT: &str = "NAME is the scope to delete. Its branch goes from the remote and from this machine, and the files only it had go with it — the run says which those were. Nothing else moves, so no machine's home changes.

Only a scope nothing hangs off can be deleted. A scope other scopes hang off is refused, because the machines under it would silently start taking their config from the scopes above while keeping everything it had already merged into their history. This machine's own scope is refused too: home is materialized from it. Delete a machine's scope from another machine, once that machine is gone.

Deleting needs the remote, the way `dotsync init` does, because the deletion is the remote not having the branch. A remote that will not take it is a stop that changed nothing, so the answer is to run the command again.

The other machines are not told anything and do not need to be: the next time each of them runs dotsync, the scope is gone.";

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

const MOVE_ABOUT: &str =
    "Give a scope another scope's version of a file, and take it off the first";

const MOVE_LONG_ABOUT: &str = "PATHS are repo paths whose version on `--from` should live on `--to` instead. `--to` holds that version afterwards, and `--from` holds nothing of its own there: it inherits.

Promote config to a scope several machines share (`--to` above `--from`), narrow it to the scope it belongs on (`--to` below `--from`), or hand it sideways. Any two scopes work, whichever machine runs it, because the content comes from the repo rather than from home.

Every other scope that holds its own version keeps it. So the only machines whose config changes are the ones that took the file from `--from` or will now take it from `--to` — and the run lists them, file by file. `--dry-run` lists them without changing anything.";

const DROP_ABOUT: &str = "Make a scope take what it inherits instead of its own version";

const DROP_LONG_ABOUT: &str = "PATHS are repo paths where `--from` holds a version of its own: a file it added, overrode, or removed. Afterwards it holds whatever the scopes above it hold there — the shared version for an override, nothing for a file it added, the inherited file again for one it removed.

Works on any scope, whichever machine runs it. Every other scope that holds its own version keeps it, and the run lists the machines whose config changed. `--dry-run` lists them without changing anything.";

const SCOPES_ABOUT: &str = "Show the scopes, where each hangs, and the machines each reaches";

const FILES_ABOUT: &str = "Show what each scope holds, and how it relates to what it inherits";

const FILES_LONG_ABOUT: &str = "One row per scope and path. Each says whether the scope inherited it, added it, overrode what it inherits with its own version, or removed it; where an inherited version comes from; and a content id that is equal exactly when two versions are identical.

`--own` keeps only what each scope adds, overrides or removes — what is local to each machine, and what could be shared. `--scope` narrows to named scopes; PATHS narrow to those paths and whatever is under them.";

const SHOW_ABOUT: &str = "Print a file as one scope holds it";

const DIFF_ABOUT: &str = "Show your local changes, what a scope changes, or how two scopes differ";

const DIFF_LONG_ABOUT: &str = "With no scope: the diffs of the managed files you changed in home, the same list `dotsync status` shows. Exits 1 when there are any.

With one scope: what that scope changes over what it inherits. With two: how the second differs from the first. PATHS after `--` narrow either to those paths and whatever is under them. Exits 1 when there are differences.";

const STATUS_ABOUT: &str =
    "Show what you changed here, what is incoming, and untracked files under named directories";

const STATUS_LONG_ABOUT: &str = "Lists the managed files changed on this machine, and separately the files another machine changed that home has not caught up to. A file dotsync does not track is not a change to anything, so it is not listed — unless you name directories: `dotsync status .config .local/bin` also lists every file under them that this machine's scope does not hold, and which scopes hold their own version of it.

While a merge is waiting, it prints every version of every file that merge could not resolve.";

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

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = INIT_ABOUT, long_about = INIT_LONG_ABOUT)]
    Init {
        /// Git remote URL or local path for the dotsync repo
        remote_url: Option<String>,

        /// Scope this machine's config comes from; repeat for several
        #[arg(long = "parent")]
        parents: Vec<String>,

        /// Name this machine joins under, and keeps; the hostname when omitted
        #[arg(long)]
        name: Option<String>,
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
    #[command(name = "delete-scope", about = DELETE_SCOPE_ABOUT, long_about = DELETE_SCOPE_LONG_ABOUT)]
    DeleteScope {
        /// Scope to delete
        scope: String,
    },
    #[command(about = COMMIT_ABOUT, long_about = COMMIT_LONG_ABOUT)]
    Commit {
        /// Scope to commit changes to
        scope: String,

        /// Commit message
        #[arg(short = 'm', long = "message")]
        message: String,

        /// Say what the commit would change on each machine, and change nothing
        #[arg(long)]
        dry_run: bool,

        /// Home-relative file or directory paths to commit; omit to commit
        /// every managed file this machine has changed
        paths: Vec<PathBuf>,
    },
    #[command(name = "move", about = MOVE_ABOUT, long_about = MOVE_LONG_ABOUT)]
    Move {
        /// Repo paths to move
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Scope whose own version moves
        #[arg(long)]
        from: String,

        /// Scope that holds it afterwards
        #[arg(long)]
        to: String,

        /// Commit message
        #[arg(short = 'm', long = "message")]
        message: String,

        /// Say what the move would change on each machine, and change nothing
        #[arg(long)]
        dry_run: bool,
    },
    #[command(about = DROP_ABOUT, long_about = DROP_LONG_ABOUT)]
    Drop {
        /// Repo paths where the scope holds its own version
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Scope that stops holding a version of its own
        #[arg(long)]
        from: String,

        /// Commit message
        #[arg(short = 'm', long = "message")]
        message: String,

        /// Say what the drop would change on each machine, and change nothing
        #[arg(long)]
        dry_run: bool,
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
    #[command(about = STATUS_ABOUT, long_about = STATUS_LONG_ABOUT)]
    Status {
        /// Home directories to also list untracked files under
        directories: Vec<PathBuf>,
    },
    #[command(about = DIFF_ABOUT, long_about = DIFF_LONG_ABOUT)]
    Diff {
        /// No scope for your local changes; one scope for what it changes; two
        /// to compare them
        #[arg(num_args = 0..=2)]
        scopes: Vec<String>,

        /// Repo paths to narrow the comparison to
        #[arg(last = true)]
        paths: Vec<PathBuf>,
    },
    #[command(about = SCOPES_ABOUT)]
    Scopes,
    #[command(about = FILES_ABOUT, long_about = FILES_LONG_ABOUT)]
    Files {
        /// Repo paths to narrow to, with whatever is under them
        paths: Vec<PathBuf>,

        /// Only this scope; repeat for several
        #[arg(long = "scope")]
        scopes: Vec<String>,

        /// Only what each scope adds, overrides or removes
        #[arg(long)]
        own: bool,
    },
    #[command(about = SHOW_ABOUT)]
    Show {
        /// Scope to read the file from
        scope: String,

        /// Repo path of the file
        path: PathBuf,
    },
    #[command(external_subcommand)]
    Unknown(Vec<String>),
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
    /// The answer *is* the output: `show` prints a file's contents, `files` the
    /// standing table, `scopes` the graph, a scope `diff` its diffs. A caller may pipe it into something.
    Stdout(String),
    /// A report about what the run did, which belongs beside a caller's data
    /// rather than in it.
    Message(String),
}

impl SuccessOutput {
    /// A run that reports what it did. The common case: everything but the fleet reads.
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
    /// The command line was wrong, so there was never a run to explain.
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
    let outcome = dispatch(cli.command, detect_cli_context()).await;

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

/// Bare `dotsync` is the sync, so the command is optional; every other arm
/// hands clap's own parse straight to the run that answers it.
async fn dispatch(
    command: Option<Command>,
    context: CliContext,
) -> Result<CliOutput, DotsyncError> {
    match command {
        None => run_sync().await,
        Some(Command::Init {
            remote_url,
            parents,
            name,
        }) => run_init(remote_url, parents, name, context).await,
        Some(Command::CreateScope {
            scope,
            parents,
            description,
        }) => run_create_scope(scope, parents, description).await,
        Some(Command::DeleteScope { scope }) => run_delete_scope(scope).await,
        Some(Command::Commit {
            scope,
            message,
            dry_run,
            paths,
        }) => run_commit(scope, message, paths, dry_run).await,
        Some(Command::Move {
            paths,
            from,
            to,
            message,
            dry_run,
        }) => run_place("move", paths, from, Some(to), message, dry_run).await,
        Some(Command::Drop {
            paths,
            from,
            message,
            dry_run,
        }) => run_place("drop", paths, from, None, message, dry_run).await,
        Some(Command::Discard { paths }) => run_discard(paths).await,
        Some(Command::Continue) => run_continue().await,
        Some(Command::Abort) => run_abort().await,
        Some(Command::Status { directories }) => run_status(directories).await,
        Some(Command::Diff { scopes, paths }) => match scopes.as_slice() {
            [] => run_diff().await,
            [one] => run_compare(one.clone(), None, paths).await,
            [left, right, ..] => run_compare(left.clone(), Some(right.clone()), paths).await,
        },
        Some(Command::Scopes) => run_scopes().await,
        Some(Command::Files { paths, scopes, own }) => run_files(paths, scopes, own).await,
        Some(Command::Show { scope, path }) => run_show(scope, path).await,
        // Clap's `external_subcommand`, so that an unknown command is refused
        // in dotsync's words and in the output format that was asked for.
        Some(Command::Unknown(args)) => {
            let command = args.first().map(String::as_str).unwrap_or("<empty>");
            Ok(usage_output(&format!(
                "unknown command `{command}`; run `dotsync --help` for supported commands"
            )))
        }
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

fn usage_output(message: &str) -> CliOutput {
    CliOutput::without_run(OutputKind::Usage(usage_error(message)))
}

async fn run_init(
    remote_url: Option<String>,
    parents: Vec<String>,
    name: Option<String>,
    context: CliContext,
) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let remote_url = match remote_url {
        Some(remote_url) => Some(remote_url),
        // A clone an earlier `init` kept is what this one joins from, so it
        // needs no URL.
        None if paths.repo_root.exists() => None,
        // A terminal can be asked for the URL. A script cannot, so it gets
        // the usage text instead — before anything opens the repo.
        None if context.interactive_terminal => match prompt_init_remote_url() {
            Ok(remote_url) => Some(remote_url),
            Err(message) => return Ok(usage_output(&message)),
        },
        None => return Ok(usage_output(INIT_REMOTE_URL_USAGE)),
    };
    let run = init(
        &paths,
        &InitOptions {
            remote_url,
            parents,
            name,
        },
    )
    .await;
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

async fn run_delete_scope(scope: String) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = delete_scope(&paths, &scope).await;
    Ok(output_of("dotsync delete-scope", run, |report| {
        SuccessOutput::message(
            json!({
                "status": "ok",
                "command": "delete-scope",
                "scope": report.scope,
                "files_gone": render::display_paths(&report.files_gone),
            }),
            // What went with it is the note above this line, listed path by
            // path, so a count here would be the same fact twice.
            format!("dotsync: deleted scope {}", report.scope),
        )
        .with_notes(render::files_gone_notes(&report.scope, &report.files_gone))
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

async fn run_status(directories: Vec<PathBuf>) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = status(&paths, &directories).await;
    Ok(output_of("dotsync status", run, |report| {
        let answer = reprinting_any_conflict(
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
        );
        let mut answer = with_machine_state(answer, &report.machine);
        if let Some(untracked) = &report.untracked {
            answer.json["untracked"] = json!(untracked
                .iter()
                .map(|file| json!({
                    "path": render::display_path(&file.path),
                    "elsewhere": file.elsewhere.iter().map(|held| json!({
                        "scope": held.scope,
                        "same": held.same,
                    })).collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>());
        }
        answer
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

async fn run_scopes() -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = scopes(&paths).await;
    Ok(output_of("dotsync scopes", run, |report| {
        let machine = report.machine;
        let answer = SuccessOutput::stdout(
            json!({
                "status": "ok",
                "command": "scopes",
                "scopes": report.scopes.iter().map(|scope| json!({
                    "name": scope.name,
                    "parents": scope.parents,
                    "children": scope.children,
                    "machine": scope.is_machine(),
                    "machines": scope.machines,
                    "description": scope.description,
                })).collect::<Vec<_>>(),
            }),
            render_lines(
                report
                    .scopes
                    .iter()
                    .map(|scope| render_scope_line(scope, &machine.machine_scope)),
            ),
        );
        with_machine_state(answer, &machine)
    }))
}

async fn run_files(
    only: Vec<PathBuf>,
    scopes: Vec<String>,
    own: bool,
) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = files(
        &paths,
        FilesQuery {
            scopes,
            own,
            paths: only.clone(),
        },
    )
    .await;
    Ok(output_of("dotsync files", run, |report| {
        // An empty table reads exactly like a bug, and the commonest reason it
        // is empty is a typo — so it says it is an answer.
        let nothing_there = match report.rows.is_empty() && !only.is_empty() {
            true => vec![format!(
                "dotsync: no scope holds anything at {}",
                only.iter()
                    .map(|path| render::display_path(path))
                    .collect::<Vec<_>>()
                    .join(", ")
            )],
            false => Vec::new(),
        };
        let answer = SuccessOutput::stdout(
            json!({
                "status": "ok",
                "command": "files",
                "files": report.rows.iter().map(|row| json!({
                    "scope": row.scope,
                    "path": render::display_path(&row.path),
                    "standing": row.standing.code(),
                    "kind": row.kind.map(|kind| kind.code()),
                    "content": row.content,
                    "origin": row.origin,
                })).collect::<Vec<_>>(),
            }),
            render_files_human(&report.rows),
        )
        .with_notes(nothing_there);
        with_machine_state(answer, &report.machine)
    }))
}

async fn run_show(scope: String, path: PathBuf) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = show(&paths, &scope, &path).await;
    Ok(output_of("dotsync show", run, |report| {
        let text = String::from_utf8(report.contents.clone());
        let answer = SuccessOutput::stdout(
            json!({
                "status": "ok",
                "command": "show",
                "scope": report.row.scope,
                "path": render::display_path(&report.row.path),
                "standing": report.row.standing.code(),
                "kind": report.row.kind.map(|kind| kind.code()),
                "origin": report.row.origin,
                "content": report.row.content,
                "utf8": text.is_ok(),
                "contents": String::from_utf8_lossy(&report.contents),
            }),
            String::from_utf8_lossy(&report.contents).into_owned(),
        );
        with_machine_state(answer, &report.machine)
    }))
}

async fn run_compare(
    left: String,
    right: Option<String>,
    only: Vec<PathBuf>,
) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = compare(&paths, &left, right.as_deref(), &only).await;
    Ok(output_of("dotsync diff", run, |report| {
        let diffs: Vec<(String, String)> = report
            .changes
            .iter()
            .map(|change| {
                (
                    render::display_path(&change.path),
                    render::unified_diff(
                        &report.left,
                        change.left.as_deref(),
                        &report.right,
                        change.right.as_deref(),
                    ),
                )
            })
            .collect();
        let human = match diffs.is_empty() {
            true => String::new(),
            false => render_lines(
                diffs
                    .iter()
                    .flat_map(|(path, diff)| [path.clone(), diff.clone()]),
            ),
        };
        let mut answer = SuccessOutput::stdout(
            json!({
                "status": "ok",
                "command": "diff",
                "left": report.left,
                "right": report.right,
                "changes": report.changes.iter().zip(&diffs).map(|(change, (path, diff))| json!({
                    "path": path,
                    "left_kind": change.left_kind.map(|kind| kind.code()),
                    "right_kind": change.right_kind.map(|kind| kind.code()),
                    "diff": diff,
                })).collect::<Vec<_>>(),
            }),
            human,
        );
        if report.changes.is_empty() {
            answer.notes.push(format!(
                "dotsync: no differences between {} and {}",
                report.left, report.right
            ));
        }
        // The same contract as `dotsync diff` with no scope: 1 means it found
        // differences, and the payload's `status` says it was not a stop.
        answer.exit_code = if report.changes.is_empty() { 0 } else { 1 };
        with_machine_state(answer, &report.machine)
    }))
}

async fn run_place(
    command: &'static str,
    place_paths: Vec<PathBuf>,
    from: String,
    to: Option<String>,
    message: String,
    dry_run: bool,
) -> Result<CliOutput, DotsyncError> {
    let paths = discover_paths()?;
    let run = place(
        &paths,
        PlacementOptions {
            paths: place_paths,
            from: from.clone(),
            to: to.clone(),
            message,
            dry_run,
        },
    )
    .await;
    let invocation = match command {
        "move" => "dotsync move",
        _ => "dotsync drop",
    };
    Ok(output_of(invocation, run, |report| {
        let headline = match (&to, dry_run) {
            (Some(to), true) => format!("dotsync: moving from `{from}` to `{to}` would change what follows; nothing was changed"),
            (Some(to), false) => format!("dotsync: moved from `{from}` to `{to}`"),
            (None, true) => format!("dotsync: dropping `{from}`'s own version would change what follows; nothing was changed"),
            (None, false) => format!("dotsync: `{from}` now takes what it inherits"),
        };
        let mut output = match &report.carried_out {
            Some(done) => {
                let mut output =
                    render::synced_output(command, headline, &done.sync, Some(&done.push));
                output.json["dry_run"] = json!(false);
                output
            }
            None => SuccessOutput::message(
                json!({
                    "status": "ok",
                    "command": command,
                    "machine_scope": report.machine_scope,
                    "dry_run": true,
                }),
                headline,
            ),
        };
        output.json["from"] = json!(from);
        if let Some(to) = &to {
            output.json["to"] = json!(to);
        }
        output.json["paths"] = json!(render::display_paths(&report.paths));
        with_plan(output, &report.planned)
    }))
}

/// What a write changes on each machine — and, for a dry run, where it would
/// stop — in both channels. One function for every write, because "which
/// machines get this untested" is the same question whichever write asks it.
fn with_plan(mut output: SuccessOutput, planned: &Planned) -> SuccessOutput {
    output.json["effect"] = json!(planned
        .effect
        .iter()
        .map(|machine| json!({
            "machine": machine.machine,
            "changes": machine.changes.iter().map(|change| json!({
                "path": render::display_path(&change.path),
                "change": change.change.code(),
            })).collect::<Vec<_>>(),
        }))
        .collect::<Vec<_>>());
    output.notes.extend(render_effect_notes(&planned.effect));
    if let Some(stop) = &planned.stops_at {
        output.json["stops_at"] = json!({
            "scope": stop.scope,
            "conflicts": stop.conflicts.iter().map(render::render_conflict_json).collect::<Vec<_>>(),
        });
        output.notes.push(format!(
            "dotsync: it would stop at `{}`, where these file(s) would not merge",
            stop.scope
        ));
        output
            .notes
            .extend(render::render_conflicts_human(&stop.conflicts));
    }
    output
}

fn render_effect_notes(effect: &[MachineEffect]) -> Vec<String> {
    if effect.is_empty() {
        return vec!["dotsync: no machine's config changes".to_string()];
    }
    let mut notes = vec![format!(
        "dotsync: config changes on {} machine(s):",
        effect.len()
    )];
    for machine in effect {
        notes.push(format!("  {}", machine.machine));
        for change in &machine.changes {
            let marker = match change.change {
                dotsync::Change::Added => "+",
                dotsync::Change::Modified => "M",
                dotsync::Change::Removed => "-",
            };
            notes.push(format!(
                "    {marker} {}",
                render::display_path(&change.path)
            ));
        }
    }
    notes
}

async fn run_commit(
    scope: String,
    message: String,
    commit_paths: Vec<PathBuf>,
    dry_run: bool,
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
            dry_run,
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
    let dry_run =
        report.push.is_none() && report.recorded.as_ref().is_some_and(|r| r.sync.is_none());
    let mut json = json!({
        "status": "ok",
        "command": "commit",
        "outcome": if report.recorded.is_some() { "committed" } else { "nothing_to_commit" },
        "scope": report.committed_scope,
        "machine_scope": report.machine_scope,
        "skipped_paths": render::skipped_paths_json(&report.skipped),
    });
    if let Some(push) = &report.push {
        json["unpushed_scopes"] = json!(push.unpushed_scopes());
    }
    let skipped = render::skipped_path_notes(&report.skipped);
    let push_notes = report
        .push
        .as_ref()
        .map(render::push_notes)
        .unwrap_or_default();

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
                .chain(push_notes)
                .collect(),
        );
    };

    json["dry_run"] = json!(dry_run);
    json["newly_tracked"] = json!(render::display_paths(&recorded.newly_tracked));
    let Some(sync) = &recorded.sync else {
        let output = SuccessOutput::message(
            json,
            format!(
                "dotsync: committing to {} would change what follows; nothing was changed",
                report.committed_scope
            ),
        )
        .with_notes(
            render::newly_tracked_notes(&recorded.newly_tracked)
                .into_iter()
                .chain(skipped)
                .collect(),
        );
        return with_plan(output, &recorded.planned);
    };
    json["synced_files"] = json!(render::display_paths(&sync.synced_paths));
    let output = SuccessOutput::message(
        json,
        format!(
            "dotsync: committed {} and synced {} file(s)",
            report.committed_scope,
            sync.synced_paths.len()
        ),
    )
    .with_notes(
        render::newly_tracked_notes(&recorded.newly_tracked)
            .into_iter()
            .chain(skipped)
            .chain(render::success_notes(&sync.drifts, report.push.as_ref()))
            .collect(),
    );
    with_plan(output, &recorded.planned)
}

/// The fleet table for a person: scope by scope, one marker per path.
///
/// An own version that another scope also holds, byte for byte, says so —
/// that pair is the commonest promotion candidate there is.
fn render_files_human(rows: &[FileRow]) -> String {
    let mut lines = Vec::new();
    let mut current: Option<&str> = None;
    for row in rows {
        if current != Some(row.scope.as_str()) {
            lines.push(row.scope.clone());
            current = Some(row.scope.as_str());
        }
        let marker = match row.standing {
            Standing::Inherited => "=",
            Standing::Added => "A",
            Standing::Overridden => "M",
            Standing::Removed => "D",
        };
        let mut line = format!("  {marker} {}", render::display_path(&row.path));
        match row.standing {
            Standing::Inherited => line.push_str(&format!("  (from {})", row.origin.join(", "))),
            Standing::Removed => {}
            _ => {
                let twins: Vec<&str> = rows
                    .iter()
                    .filter(|other| {
                        other.scope != row.scope
                            && other.path == row.path
                            && other.standing.is_own()
                            && other.content.is_some()
                            && other.content == row.content
                    })
                    .map(|other| other.scope.as_str())
                    .collect();
                if !twins.is_empty() {
                    line.push_str(&format!("  (same as {})", twins.join(", ")));
                }
            }
        }
        lines.push(line);
    }
    if lines.is_empty() {
        return String::new();
    }
    render_lines(lines)
}

/// The conflict a machine is paused on, printed again by `status`.
///
/// Every version of every conflicted file exists nowhere else: dotsync writes
/// no markers into home, and neither side is on a scope this machine syncs
/// from. So the pause message is the only copy, and an agent that lost it — a
/// new session, a scrolled terminal — needs somewhere to ask. `status` is that
/// somewhere because it is the command an agent runs by reflex, and the pause
/// is derived, so it is correct whenever it is asked.
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

/// What `status`, `diff` and the fleet reads say about the machine, whatever they were
/// asked, in both channels.
///
/// One function for every read, because these facts are true of the machine
/// rather than of the question: adding them per command is how one command
/// came to carry a fact the others did not. `paused_cascade` is present only when there is one,
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

    if let Some(untracked) = report.untracked.as_ref().filter(|files| !files.is_empty()) {
        lines.push(format!(
            "dotsync: {} untracked file(s) under the directories named — commit one to start tracking it",
            untracked.len()
        ));
        for file in untracked {
            let mut line = format!("  ? {}", render::display_path(&file.path));
            if !file.elsewhere.is_empty() {
                let held: Vec<String> = file
                    .elsewhere
                    .iter()
                    .map(|held| match held.same {
                        true => format!("{} (identical)", held.scope),
                        false => format!("{} (different)", held.scope),
                    })
                    .collect();
                line.push_str(&format!("  — also on {}", held.join(", ")));
            }
            lines.push(line);
        }
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

fn render_lines(lines: impl IntoIterator<Item = String>) -> String {
    let mut lines = lines.into_iter().collect::<Vec<_>>();
    lines.push(String::new());
    lines.join("\n")
}

/// One scope, its parents, and whether it is the machine reading this.
///
/// The marker is the answer to "where am I?". A shared scope says which
/// machines a change to it reaches, which is the question every commit to it
/// raises; a machine's own scope says it is one.
fn render_scope_line(scope: &ScopeInfo, machine_scope: &str) -> String {
    let here = match scope.name == machine_scope {
        true => "* ",
        false => "  ",
    };
    let mut line = match scope.parents.as_slice() {
        [] => format!("{here}{}", scope.name),
        parents => format!("{here}{} <- {}", scope.name, parents.join(", ")),
    };
    match scope.is_machine() {
        true => line.push_str("  (machine)"),
        false => line.push_str(&format!("  (reaches {})", scope.machines.join(", "))),
    }
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
