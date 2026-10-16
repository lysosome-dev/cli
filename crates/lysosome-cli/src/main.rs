//! `lysosome` — the disk cleaner for the agentic coding era.

mod caches;
mod clean;
mod completions;
mod errors;
mod hint;
mod pick;
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

lysosome finds dormant git worktrees, the git-ignored dependencies and build
output of idle projects, package caches, AI models, Docker and Xcode data, and
labels every item by how safe it is to delete. Scanning is strictly
read-only: it never deletes, moves or writes anything. `lysosome clean`
is the only command that changes anything: it is a dry run unless you pass
--yes, re-checks every item before acting, and uses the Trash by default.";

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
  lysosome scan                            Idle project artifacts + worktrees in ~
  lysosome scan ~/code ~/.worktrees        Scan specific folders (faster)
  lysosome worktrees                       Only dormant git worktrees
  lysosome caches                          Package caches, AI models, Docker, Xcode
  lysosome clean                           What a cleanup would do (dry run)
  lysosome clean -i                        Pick what to clean, then confirm
  lysosome clean --yes                     Clean REMOVABLE, cheap-to-rebuild items
  lysosome scan --days 90                  Call anything idle 90+ days dormant
  lysosome scan --json | jq .summary       Machine-readable output

After `scan` and `clean` on a terminal, one dim line mentions the desktop app;
set LYSOSOME_NO_HINTS=1 to hide it. It never appears in JSON or piped output.";

const SCAN_HELP: &str = "\
Sections:
  Project build & dependency output  git-ignored node_modules, target, .venv,
                                     dist, .next, Pods... in every repository
  Git worktrees                      same as `lysosome worktrees`
  AI models, Xcode, Package caches,  machine-wide data, same as `lysosome caches`;
  Containers, Android, Editor caches not limited by ROOTS (skip with --no-system)

Artifact folders are REMOVABLE when their project (last commit, checkout or
staging, whichever is newest) has been idle --days or more, ACTIVE otherwise.
Only folders `git check-ignore` confirms are counted; tracked files never are.
Tiers: dependencies (node_modules, .venv, venv, Pods) are regenerable_expensive
(re-download); build output is regenerable. Machine-wide items ignore --days:
caches and models are REMOVABLE by tier (review items are REVIEW) however
recently they were used; LAST USED / IDLE tells you how recently.

Examples:
  lysosome scan ~/code                    Scan one folder (plus machine-wide data)
  lysosome scan ~/code --no-system        Only projects and worktrees under ~/code
  lysosome scan --days 180 --top 50       Long-idle projects, more rows
  lysosome scan --all                     Also list active items
  lysosome scan --json > scan.json        Versioned JSON (schema/lysosome.schema.json)
  lysosome scan --profile                 Show phase and detector timings on stderr

Nothing is ever changed.";

const WORKTREE_HELP: &str = "\
Statuses:
  REMOVABLE  dormant, clean, and merged into the default branch, fully pushed,
             or its exact HEAD was squash-merged in a GitHub PR (needs `gh`)
  REVIEW     dormant but dirty, locked, or holding commits found nowhere else
  ORPHAN     its parent repository was moved or deleted
  ACTIVE     used within --days (listed with --all)

Examples:
  lysosome worktrees ~/code               Scan one folder
  lysosome worktrees --days 7 --all       Shorter window, list active ones too
  lysosome worktrees --json > scan.json   Versioned JSON (schema/lysosome.schema.json)
  lysosome worktrees --profile            Show walk / git / size timings on stderr

Squash merges: git can't tell that a branch was squash-merged on GitHub, so
when `gh` is installed and signed in, worktrees that are REVIEW only because
of unpushed commits are looked up (`gh pr list --state merged --head <branch>`).
If a merged PR's last commit is exactly the worktree's HEAD, it is REMOVABLE.
Skip with --no-gh or --offline.

Nothing is ever changed. Suggested `git worktree remove` commands are printed
for you to review and run yourself.";

const CLEAN_HELP: &str = "\
What gets cleaned (only REMOVABLE items, never REVIEW, ACTIVE or ORPHAN):
  worktrees  git -C <main> worktree remove <path> (never --force), then
             git worktree prune; the branch is kept
  artifacts  git-ignored node_modules, target, dist... of idle projects,
             moved to the Trash
  caches     the tool's own cleanup command (npm cache clean --force,
             docker builder prune, brew cleanup -s...), else the Trash

Defaults: all three groups, tier `regenerable` only (cheap to rebuild).
Pass --tier regenerable_expensive to include dependencies, AI models and
images. Tier `review` is never cleaned.

Safety: dry run unless --yes (or `y` at the prompt on a terminal). Every
item is re-checked right before acting and skipped if anything changed.
Files go to the Trash unless --permanent (which needs --yes). Every action
is appended to an action log (~/Library/Logs/lysosome/actions.log on macOS,
$XDG_STATE_HOME/lysosome/actions.log elsewhere).

Interactive (-i): pick items in a checklist grouped by category. Cheap items
start ticked, costly ones (dependencies, models, images) unticked unless you
pass --tier regenerable_expensive; REVIEW
items are listed greyed out with the reason they are kept. Keys: ↑/↓ move,
space toggle, a all/none, enter continue, q cancel. You then see the final
plan and confirm with y before anything happens.

Examples:
  lysosome clean                              Show what would be cleaned
  lysosome clean -i ~/code                    Choose items, then confirm
  lysosome clean ~/code --worktrees --yes     Remove removable worktrees
  lysosome clean --strip-artifacts --tier regenerable_expensive ~/.worktrees
                                              Free node_modules etc. inside
                                              dormant worktrees, keep them
  lysosome clean --caches --only 'npm*'       Just the npm cache
  lysosome clean --older-than 90d --json      Machine-readable plan";

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

    /// Progress on stderr: auto (a spinner on a terminal), json (newline-delimited
    /// events for programs, schema/progress.schema.json) or off
    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "MODE",
        default_value = "auto"
    )]
    pub progress: progress::ProgressMode,

    /// Leave this folder out: never listed or entered (repeatable). `clean`
    /// needs the same --skip as the scan that found its items
    #[arg(long = "skip", global = true, value_name = "PATH")]
    pub skip: Vec<std::path::PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Find idle project artifacts, dormant worktrees, caches, AI models, Docker and Xcode data
    #[command(after_help = SCAN_HELP)]
    Scan(ScanCmdArgs),

    /// Find dormant git worktrees and the disk they're holding
    #[command(after_help = WORKTREE_HELP)]
    Worktrees(WorktreesArgs),

    /// Find package caches, AI models, Docker, Xcode, Android and editor data
    #[command(visible_alias = "system", after_help = CACHES_HELP)]
    Caches(caches::CachesArgs),

    /// Clean up safely: dry run by default, --yes to act
    #[command(after_help = CLEAN_HELP)]
    Clean(clean::CleanArgs),

    /// Print a shell completion script (bash, zsh, fish, powershell, elvish)
    #[command(after_help = completions::COMPLETIONS_HELP)]
    Completions(completions::CompletionsArgs),
}

/// `lysosome worktrees` takes the same options as `lysosome scan`.
pub type WorktreesArgs = ScanArgs;

/// `lysosome scan` = the shared arguments plus scan-only switches.
#[derive(Debug, Clone, Args)]
pub struct ScanCmdArgs {
    #[command(flatten)]
    pub common: ScanArgs,

    /// Skip machine-wide caches, AI models, Docker, Xcode, Android and editor data
    #[arg(long)]
    pub no_system: bool,
}

/// Options shared by `scan` and `worktrees`.
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

    /// Rows to show per section (in `scan`, projects count as one row)
    #[arg(long, value_name = "N", default_value_t = 30)]
    pub top: usize,

    /// Print the versioned JSON report instead of a table
    #[arg(long)]
    pub json: bool,

    #[command(flatten)]
    pub net: NetArgs,
}

/// Network switches shared by `scan`, `worktrees` and `clean`.
#[derive(Debug, Clone, Default, Args)]
pub struct NetArgs {
    /// Don't ask GitHub (via `gh`) whether REVIEW worktrees were squash-merged
    #[arg(long)]
    pub no_gh: bool,

    /// Never use the network (today this only skips the `gh` lookups)
    #[arg(long)]
    pub offline: bool,
}

impl NetArgs {
    pub fn github(&self) -> bool {
        !(self.no_gh || self.offline)
    }
}

fn main() -> ExitCode {
    // Release-only man page generator; kept outside clap so it never shows
    // up in --help or shell completions.
    if let Some(code) = completions::generate_man_entry(Cli::command) {
        return code;
    }
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => return usage_error(e),
    };
    lysosome_core::walk::set_skip_paths(&cli.global.skip);
    let json = cli.command.as_ref().is_some_and(Command::json);
    let mut failed = false;
    let result = match &cli.command {
        None => {
            // Bare `lysosome`: a quick start, then the help; success.
            let p = ui::Painter::new(ui::color_enabled(cli.global.no_color, ui::stdout_is_tty()));
            print!("{}", quick_start(&p));
            let _ = Cli::command().print_help();
            return ExitCode::SUCCESS;
        }
        Some(Command::Scan(args)) => scan::run(args, &cli.global),
        Some(Command::Worktrees(args)) => worktrees::run(args, &cli.global),
        Some(Command::Caches(args)) => caches::run(args, &cli.global),
        Some(Command::Completions(args)) => completions::completions(Cli::command(), args),
        Some(Command::Clean(args)) => match clean::run(args, &cli.global) {
            Ok(out) => {
                failed = out.failed;
                Ok(out.text)
            }
            Err(e) => Err(e),
        },
    };
    match result {
        Ok(output) => {
            let mut stdout = std::io::stdout().lock();
            match stdout
                .write_all(output.as_bytes())
                .and_then(|_| stdout.flush())
            {
                Ok(()) if failed => ExitCode::FAILURE,
                Ok(()) => ExitCode::SUCCESS,
                // `lysosome worktrees | head` is fine.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe && !failed => {
                    ExitCode::SUCCESS
                }
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::FAILURE,
                Err(e) => {
                    // stdout itself failed, so even a --json error goes to stderr.
                    eprintln!("error: could not write the output: {e}");
                    eprintln!(
                        "  hint: check free disk space and the file or pipe you are writing to"
                    );
                    ExitCode::FAILURE
                }
            }
        }
        Err(e) => {
            let err = errors::describe(&e);
            if json {
                // Machine-readable on stdout (schema/error.schema.json).
                let _ = std::io::stdout().write_all(errors::json(&err).as_bytes());
            } else {
                let p =
                    ui::Painter::new(ui::color_enabled(cli.global.no_color, ui::stderr_is_tty()));
                eprintln!(
                    "{} {}",
                    p.paint("error:", owo_colors::Style::new().red().bold()),
                    err.message
                );
                if let Some(h) = &err.hint {
                    eprintln!(
                        "  {} {h}",
                        p.paint("hint:", owo_colors::Style::new().cyan())
                    );
                }
            }
            ExitCode::FAILURE
        }
    }
}

impl Command {
    /// The command was asked for JSON output.
    fn json(&self) -> bool {
        match self {
            Command::Scan(a) => a.common.json,
            Command::Worktrees(a) => a.json,
            Command::Caches(a) => a.json,
            Command::Clean(a) => a.json,
            Command::Completions(_) => false,
        }
    }
}

/// Three lines to get going, shown above the help of a bare `lysosome`.
fn quick_start(p: &ui::Painter) -> String {
    let rows = [
        (
            "lysosome scan ~/code",
            "what's reclaimable under ~/code (read-only)",
        ),
        (
            "lysosome clean -i ~/code",
            "pick what to clean, then confirm",
        ),
        (
            "lysosome caches",
            "package caches, AI models, Docker, Xcode",
        ),
    ];
    let mut o = String::new();
    for (n, (cmd, what)) in rows.iter().enumerate() {
        o.push_str(&format!(
            "{}  {}  {}\n",
            if n == 0 {
                p.bold("Try:")
            } else {
                "    ".into()
            },
            p.paint(ui::pad_right(cmd, 25), owo_colors::Style::new().cyan()),
            p.dim(what)
        ));
    }
    o.push('\n');
    o
}

/// A command-line mistake. With `--json` anywhere in the arguments the
/// error is printed as JSON on stdout (exit 2); help and version requests
/// print as usual.
fn usage_error(e: clap::Error) -> ExitCode {
    use clap::error::ErrorKind;
    // `--json` before any `--`, for a subcommand that has a JSON mode.
    let args: Vec<std::ffi::OsString> = std::env::args_os()
        .skip(1)
        .take_while(|a| a != "--")
        .collect();
    let has_json_mode = args
        .iter()
        .find(|a| !a.to_string_lossy().starts_with('-'))
        .is_some_and(|c| {
            matches!(
                c.to_str(),
                Some("scan" | "worktrees" | "caches" | "system" | "clean")
            )
        });
    let wants_json = has_json_mode && args.iter().any(|a| a == "--json");
    let informational = matches!(
        e.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    if !wants_json || informational {
        e.exit();
    }
    // clap's text: "error: ...", detail lines, a "tip: ..." line, usage.
    let text = e.render().to_string();
    let mut message = Vec::new();
    let mut tip = None;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("Usage:") || l.starts_with("For more information") {
            break;
        }
        if let Some(t) = l.strip_prefix("tip:") {
            tip = Some(t.trim().to_string());
        } else if !l.is_empty() {
            message.push(l.trim_start_matches("error: ").to_string());
        }
    }
    let message = if message.is_empty() {
        "invalid arguments".to_string()
    } else {
        message.join(" ")
    };
    let err = errors::CliError {
        code: "usage",
        message,
        hint: Some(
            tip.unwrap_or_else(|| "run `lysosome help <command>` to see the options".into()),
        ),
    };
    let _ = std::io::stdout().write_all(errors::json(&err).as_bytes());
    ExitCode::from(2)
}
