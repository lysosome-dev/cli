//! `lysosome` — the disk cleaner for the agentic coding era.

mod progress;
mod ui;
mod worktrees;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, CommandFactory, Parser, Subcommand};

const ABOUT: &str = "Find the disk your dev tools and coding agents left behind.";

const LONG_ABOUT: &str = "\
Find the disk your dev tools and coding agents left behind.

lysosome finds dormant git worktrees (and soon build artifacts, package
caches, AI models, Docker and Xcode data) and labels every item by how safe
it is to delete. Scanning is strictly read-only: it never deletes, moves or
writes anything.";

const EXAMPLES: &str = "\
Examples:
  lysosome worktrees                       Scan your home folder
  lysosome worktrees ~/code ~/.worktrees   Scan specific folders (faster)
  lysosome worktrees --days 14             Call anything idle 14+ days dormant
  lysosome worktrees --json | jq .summary  Machine-readable output";

const WORKTREE_HELP: &str = "\
Statuses:
  REMOVABLE  dormant, clean, and merged into the default branch or fully pushed
  REVIEW     dormant but dirty, locked, or holding commits found nowhere else
  ORPHAN     its parent repository was moved or deleted
  ACTIVE     used within --days (listed with --all)

Examples:
  lysosome worktrees ~/code               Scan one folder
  lysosome worktrees --days 7 --all       Shorter window, list active ones too
  lysosome worktrees --json > scan.json   Versioned JSON (schema/lysosome.schema.json)
  lysosome worktrees --profile            Show walk / git / size timings on stderr

Nothing is ever changed. Suggested `git worktree remove` commands are printed
for you to review and run yourself.";

#[derive(Debug, Parser)]
#[command(
    name = "lysosome",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    after_help = EXAMPLES,
    propagate_version = true
)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// Disable colored output (NO_COLOR is honored too)
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Print per-phase timings (walk, git, size) to stderr
    #[arg(long, global = true)]
    pub profile: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Find dormant git worktrees and the disk they're holding
    #[command(after_help = WORKTREE_HELP)]
    Worktrees(WorktreesArgs),
}

#[derive(Debug, Clone, Args)]
pub struct WorktreesArgs {
    /// Folders to scan [default: your home folder]
    #[arg(value_name = "ROOTS")]
    pub roots: Vec<PathBuf>,

    /// Idle days before a worktree counts as dormant
    #[arg(long, value_name = "N", default_value_t = lysosome_core::worktree::DEFAULT_DORMANT_DAYS)]
    pub days: u64,

    /// Maximum folder depth to search for repositories
    #[arg(long, value_name = "N", default_value_t = lysosome_core::worktree::DEFAULT_MAX_DEPTH)]
    pub depth: usize,

    /// Also list active worktrees in the table
    #[arg(long)]
    pub all: bool,

    /// Rows to show in the table
    #[arg(long, value_name = "N", default_value_t = 30)]
    pub top: usize,

    /// Print the versioned JSON report instead of a table
    #[arg(long)]
    pub json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        None => {
            // Bare `lysosome`: concise help, success.
            let _ = Cli::command().print_help();
            return ExitCode::SUCCESS;
        }
        Some(Command::Worktrees(args)) => worktrees::run(args, &cli.global),
    };
    match result {
        Ok(output) => {
            let mut stdout = std::io::stdout().lock();
            match stdout
                .write_all(output.as_bytes())
                .and_then(|_| stdout.flush())
            {
                Ok(()) => ExitCode::SUCCESS,
                // `lysosome worktrees | head` is fine.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Err(e) => {
            let p = ui::Painter::new(ui::color_enabled(cli.global.no_color, ui::stderr_is_tty()));
            eprintln!(
                "{} {e:#}",
                p.paint("error:", owo_colors::Style::new().red().bold())
            );
            ExitCode::FAILURE
        }
    }
}
