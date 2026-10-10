//! `lysosome` — the disk cleaner for the agentic coding era.

mod caches;
mod completions;
mod progress;
mod scan;
mod ui;
mod worktrees;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, CommandFactory, Parser, Subcommand};

const ABOUT: &str = "Find the disk your dev tools and coding agents left behind.";

const LONG_ABOUT: &str = "\
Find the disk your dev tools and coding agents left behind.

lysosome finds dormant git worktrees and the git-ignored dependencies and
build output of idle projects (and soon package caches, AI models, Docker and
Xcode data) and labels every item by how safe it is to delete. Scanning is strictly read-only: it never deletes, moves or
writes anything.";

/// `--version` adds the commit and target; `-V` stays `lysosome X.Y.Z`.
const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("LYSOSOME_GIT_SHA"),
    " ",
    env!("LYSOSOME_TARGET"),
    ")"
);

const EXAMPLES: &str = "\
Examples:
  lysosome scan                            Everything, across your home folder
  lysosome scan ~/code ~/.worktrees        Scan specific folders (faster)
  lysosome worktrees                       Only dormant git worktrees
  lysosome caches                          Package caches, AI models, Docker, Xcode
  lysosome scan --days 90                  Call anything idle 90+ days dormant
  lysosome scan --json | jq .summary       Machine-readable output";

const SCAN_HELP: &str = "\
Sections:
  Project build & dependency output  git-ignored node_modules, target, .venv,
                                     dist, .next, Pods... in every repository
  Git worktrees                      same as `lysosome worktrees`

Artifact folders are REMOVABLE when their project (last commit, checkout or
staging, whichever is newest) has been idle --days or more, ACTIVE otherwise.
Only folders `git check-ignore` confirms are counted; tracked files never are.
Tiers: dependencies (node_modules, .venv, venv, Pods) are regenerable_expensive
(re-download); build output is regenerable.

Examples:
  lysosome scan ~/code                    Scan one folder
  lysosome scan --days 180 --top 50       Long-idle projects, more rows
  lysosome scan --all                     Also list active items
  lysosome scan --json > scan.json        Versioned JSON (schema/lysosome.schema.json)
  lysosome scan --profile                 Show phase and detector timings on stderr

Nothing is ever changed.";

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

const CACHES_HELP: &str = "\
Tiers:
  cache    cheap to rebuild (package caches, DerivedData, editor caches)
  costly   rebuildable but slow to get back (AI models, Docker images,
           simulator runtimes, emulators); JSON adds details.redownload_bytes
  review   may hold something you can't rebuild (archives, Docker volumes)

Examples:
  lysosome caches                          Everything, grouped by category
  lysosome caches --category ai_models     Just local AI models
  lysosome caches --json | jq '.findings[] | {name: .details.name, size_bytes}'

Tools are only asked where their data lives (with timeouts); Docker is only
queried if it's already running. Nothing is ever changed.";

#[derive(Debug, Parser)]
#[command(
    name = "lysosome",
    version,
    long_version = LONG_VERSION,
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
    /// Find everything reclaimable: idle project artifacts and dormant worktrees
    #[command(after_help = SCAN_HELP)]
    Scan(ScanArgs),

    /// Find dormant git worktrees and the disk they're holding
    #[command(after_help = WORKTREE_HELP)]
    Worktrees(WorktreesArgs),

    /// Find package caches, AI models, Docker, Xcode, Android and editor data
    #[command(visible_alias = "system", after_help = CACHES_HELP)]
    Caches(caches::CachesArgs),

    /// Print a shell completion script (bash, zsh, fish, powershell, elvish)
    #[command(after_help = completions::COMPLETIONS_HELP)]
    Completions(completions::CompletionsArgs),
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

#[derive(Debug, Clone, Args)]
pub struct ScanArgs {
    /// Folders to scan [default: your home folder]
    #[arg(value_name = "ROOTS")]
    pub roots: Vec<PathBuf>,

    /// Idle days before a project or worktree counts as dormant
    #[arg(long, value_name = "N", default_value_t = lysosome_core::worktree::DEFAULT_DORMANT_DAYS)]
    pub days: u64,

    /// Maximum folder depth to search for repositories
    #[arg(long, value_name = "N", default_value_t = lysosome_core::worktree::DEFAULT_MAX_DEPTH)]
    pub depth: usize,

    /// Also list active items
    #[arg(long)]
    pub all: bool,

    /// Rows (or projects) to show per section
    #[arg(long, value_name = "N", default_value_t = 30)]
    pub top: usize,

    /// Print the versioned JSON report instead of a table
    #[arg(long)]
    pub json: bool,
}

fn main() -> ExitCode {
    // Release-only man page generator; kept outside clap so it never shows
    // up in --help or shell completions.
    if let Some(code) = completions::generate_man_entry(Cli::command) {
        return code;
    }
    let cli = Cli::parse();
    let result = match &cli.command {
        None => {
            // Bare `lysosome`: concise help, success.
            let _ = Cli::command().print_help();
            return ExitCode::SUCCESS;
        }
        Some(Command::Scan(args)) => scan::run(args, &cli.global),
        Some(Command::Worktrees(args)) => worktrees::run(args, &cli.global),
        Some(Command::Caches(args)) => caches::run(args, &cli.global),
        Some(Command::Completions(args)) => completions::completions(Cli::command(), args),
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
