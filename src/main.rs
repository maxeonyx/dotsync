use clap::Parser;

#[derive(Debug, Parser)]
#[command(author, version, about = "Agent-first dotfile sync", long_about = None)]
struct Cli {
    /// Scope to commit changes to; omit for sync-only mode
    scope: Option<String>,

    /// Commit message (required when scope is provided)
    #[arg(short = 'm', long = "message", requires = "scope")]
    message: Option<String>,

    /// Proceed even when drift is detected
    #[arg(long)]
    force: bool,
}

fn main() {
    let cli = Cli::parse();

    match (&cli.scope, &cli.message) {
        (None, None) => {
            println!("dotsync: not implemented yet (sync-only mode)");
        }
        (Some(_), Some(_)) => {
            println!("dotsync: not implemented yet (commit + cascade + sync + push mode)");
        }
        (None, Some(_)) => {
            eprintln!("dotsync: -m/--message requires <scope>");
            std::process::exit(2);
        }
        (Some(_), None) => {
            eprintln!("dotsync: <scope> requires -m/--message");
            std::process::exit(2);
        }
    }
}
