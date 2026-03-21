use clap::{Parser, Subcommand};

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

fn main() {
    let cli = Cli::parse();

    match (cli.command, cli.scope, cli.message) {
        (Some(Command::Init { .. }), None, None) => {
            eprintln!("dotsync: init not implemented yet");
            std::process::exit(1);
        }
        (None, None, None) => {
            eprintln!("dotsync: sync not implemented yet");
            std::process::exit(1);
        }
        (None, Some(_), Some(_)) => {
            eprintln!("dotsync: commit not implemented yet");
            std::process::exit(1);
        }
        (None, Some(_), None) => {
            eprintln!("dotsync: <scope> requires -m/--message");
            std::process::exit(2);
        }
        (Some(Command::Init { .. }), Some(_), _) | (Some(Command::Init { .. }), None, Some(_)) => {
            eprintln!("dotsync: `init` does not take scope or message arguments");
            std::process::exit(2);
        }
        (None, None, Some(_)) => unreachable!("clap requires scope when message is set"),
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
