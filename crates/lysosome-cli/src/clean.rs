//! `lysosome clean`: scan, plan, and (only with consent) clean up.
//!
//! Dry run unless `--yes` is given or the user answers `y` at the prompt
//! (offered only when stdin and stderr are terminals and `--json` is off).
//! Planning, re-checks and execution live in `lysosome_core::clean`; this
//! module only renders.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use clap::{Args, ValueEnum};
use lysosome_core::clean::{
    self, ActionKind, ActionLog, DirTrash, Executor, ItemResult, Outcome, Plan, PlanItem,
    PlanOptions, Scope, Selection, SystemTrash, Trasher,
};
use lysosome_core::detector::{Detector, ScanContext};
use lysosome_core::detectors;
use lysosome_core::model::{Category, Status, Tier};
use lysosome_core::progress::NoProgress;
use lysosome_core::system;
use owo_colors::Style;
use serde_json::{json, Value};

use crate::errors;
use crate::pick;
use crate::progress::Reporter;
use crate::ui::{self, human_bytes, pad_left, pad_right, plural, Painter, PathShortener};
use crate::worktrees;
use crate::GlobalArgs;

/// Which tiers to clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TierArg {
    /// Cheap to rebuild only (the default)
    #[value(name = "regenerable")]
    Regenerable,
    /// Also items that are costly to get back (dependencies, models, images)
    #[value(name = "regenerable_expensive", alias = "regenerable-expensive")]
    RegenerableExpensive,
}

#[derive(Debug, Clone, Args)]
pub struct CleanArgs {
    /// Folders to scan for worktrees and project artifacts [default: your home folder]
    #[arg(value_name = "ROOTS")]
    pub roots: Vec<PathBuf>,

    /// Clean dormant git worktrees
    #[arg(long)]
    pub worktrees: bool,

    /// Clean git-ignored build/dependency folders of idle projects
    #[arg(long)]
    pub artifacts: bool,

    /// Clean package caches, AI models, Docker, Xcode, Android and editor data
    #[arg(long)]
    pub caches: bool,

    /// Keep dormant worktrees; remove only the git-ignored artifact folders
    /// inside them (also in REVIEW worktrees). Implies --worktrees
    #[arg(long)]
    pub strip_artifacts: bool,

    /// Highest tier to clean: regenerable, or regenerable_expensive (both)
    #[arg(long, value_enum, value_name = "TIER", default_value = "regenerable")]
    pub tier: TierArg,

    /// Only items idle at least this long: 90d, 12w, 6mo, 1y (items of unknown age are left out)
    #[arg(long, value_name = "AGE", value_parser = parse_age)]
    pub older_than: Option<u64>,

    /// Only items whose id, kind, name or path matches (exact or glob, repeatable)
    #[arg(long, value_name = "ID|GLOB")]
    pub only: Vec<String>,

    /// Pick items in a terminal checklist, then confirm (needs a terminal)
    #[arg(long, short = 'i', conflicts_with = "json")]
    pub interactive: bool,

    /// Do it. Without --yes nothing is changed
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// Delete instead of moving to the Trash (needs --yes)
    #[arg(long)]
    pub permanent: bool,

    /// Print the plan (and results) as JSON (schema/clean.schema.json)
    #[arg(long)]
    pub json: bool,

    /// Idle days before a project or worktree counts as dormant
    #[arg(long, value_name = "N", default_value_t = lysosome_core::worktree::DEFAULT_DORMANT_DAYS)]
    pub days: u64,

    /// Maximum folder depth to search for repositories
    #[arg(long, value_name = "N", default_value_t = lysosome_core::worktree::DEFAULT_MAX_DEPTH)]
    pub depth: usize,

    #[command(flatten)]
    pub net: crate::NetArgs,
}

fn parse_age(s: &str) -> Result<u64, String> {
    clean::parse_age_days(s)
        .ok_or_else(|| format!("`{s}` is not an age; use e.g. 90d, 12w, 6mo, 1y"))
}

/// What `run` hands back to `main`.
pub struct CleanOutput {
    pub text: String,
    /// Some item failed (exit status 1).
    pub failed: bool,
}

fn selection(args: &CleanArgs) -> Selection {
    let mut scopes = BTreeSet::new();
    if args.worktrees || args.strip_artifacts {
        scopes.insert(Scope::Worktrees);
    }
    if args.artifacts {
        scopes.insert(Scope::Artifacts);
    }
    if args.caches {
        scopes.insert(Scope::Caches);
    }
    if scopes.is_empty() {
        scopes = Scope::ALL.into_iter().collect();
    }
    let tiers = match args.tier {
        TierArg::Regenerable => vec![Tier::Regenerable],
        TierArg::RegenerableExpensive => vec![Tier::Regenerable, Tier::RegenerableExpensive],
    };
    Selection {
        scopes,
        tiers: tiers.into_iter().collect(),
        older_than_days: args.older_than,
        only: args.only.clone(),
        strip_artifacts: args.strip_artifacts,
    }
}

pub fn run(args: &CleanArgs, global: &GlobalArgs) -> Result<CleanOutput> {
    if args.permanent && !args.yes {
        return Err(errors::fail(
            "needs_yes",
            "--permanent deletes for good, so it needs --yes as well (it is never the default)",
            "add --yes to delete permanently, or drop --permanent to use the Trash",
        ));
    }
    if args.interactive && !(std::io::stdin().is_terminal() && ui::stderr_is_tty()) {
        return Err(errors::fail(
            "needs_terminal",
            "--interactive needs a terminal (stdin and stderr must be a TTY)",
            "run `lysosome clean` without -i for a dry run, then pick items with --only",
        ));
    }
    let home = dirs::home_dir().ok_or(lysosome_core::Error::NoHomeDir)?;
    let shortener = PathShortener::new(Some(home.clone()));
    let sel = selection(args);
    let warn = Painter::new(ui::color_enabled(global.no_color, ui::stderr_is_tty()));

    // --- scan ---------------------------------------------------------------
    let project_scopes =
        sel.scopes.contains(&Scope::Worktrees) || sel.scopes.contains(&Scope::Artifacts);
    let roots = if project_scopes {
        worktrees::resolve_roots(&args.roots, Some(&home))?
    } else {
        Vec::new()
    };
    let mut dets: Vec<Box<dyn Detector>> = Vec::new();
    if project_scopes {
        dets.extend(
            detectors::project_detectors()
                .into_iter()
                .filter(|d| sel.scopes.contains(&Scope::of(d.category()))),
        );
    }
    if sel.scopes.contains(&Scope::Caches) {
        dets.extend(system::detectors());
    }
    let spinner = Reporter::new(
        global.progress,
        roots
            .iter()
            .map(|r| shortener.short(r))
            .collect::<Vec<_>>()
            .join(", "),
    );
    if let Some(s) = &spinner {
        if !project_scopes {
            s.set_message("Checking caches, AI models, Docker, Xcode and editors");
        }
    }
    let mut ctx = ScanContext::new(roots, home.clone());
    ctx.dormant_after_days = args.days;
    ctx.max_depth = args.depth;
    ctx.github = args.net.github();
    ctx.progress = match &spinner {
        Some(s) => s,
        None => &NoProgress,
    };
    let mut scan = detectors::run(&dets, &ctx);
    if let Some(s) = &spinner {
        s.clear();
    }
    if scan.all_failed() {
        let err = scan.runs.iter_mut().find_map(|r| r.error.take());
        return Err(err.map(anyhow::Error::from).unwrap_or_else(|| {
            errors::fail(
                "all_detectors_failed",
                "every detector failed",
                "check that the folders exist and that `git --version` works; `lysosome caches` checks the machine-wide part on its own",
            )
        }));
    }
    for r in &scan.runs {
        if let Some(e) = &r.error {
            Reporter::warn(
                spinner.as_ref(),
                &warn,
                &format!("{} scan failed: {e:#}", r.id),
            );
        }
    }
    for m in &scan.report.skipped.missing_roots {
        Reporter::warn(
            spinner.as_ref(),
            &warn,
            &format!("skipping {} (not a folder)", m.display()),
        );
    }

    // --- plan ---------------------------------------------------------------
    // Interactive mode lists costly items too (unticked), so they can be
    // opted into without re-running with --tier.
    let widened;
    let plan_sel = if args.interactive {
        let mut s = sel.clone();
        s.tiers.insert(Tier::RegenerableExpensive);
        widened = s;
        &widened
    } else {
        &sel
    };
    let mut plan = clean::plan(
        &scan.report.findings,
        plan_sel,
        PlanOptions {
            permanent: args.permanent,
            now: Some(ctx.now),
            // Resolved after picking (Plan::subset).
            keep_nested: args.interactive,
        },
    );
    let p = Painter::new(ui::color_enabled(global.no_color, ui::stdout_is_tty()));
    let nothing_changed = |lead: &str| CleanOutput {
        text: format!(
            "\n  {}{}\n",
            if lead.is_empty() {
                String::new()
            } else {
                format!("{} ", p.bold(lead))
            },
            p.dim("Nothing was changed.")
        ),
        failed: false,
    };

    if args.interactive {
        let held = clean::held_for_review(&scan.report.findings, &sel, ctx.now);
        if plan.items.is_empty() {
            // Nothing to pick: say so, and show why REVIEW items are kept.
            let mut text = render_plan(&plan, args, &sel, &p, &shortener, false);
            text.push_str(&pick::held_lines(&held, &p, &shortener));
            return Ok(CleanOutput {
                text,
                failed: false,
            });
        }
        let picker = pick::Picker::new(&plan, &held, &shortener, &sel);
        let Some(picked) = pick::run(picker, &warn).map_err(|e| {
            errors::fail(
                "picker",
                format!("the interactive picker failed: {e}"),
                "run without -i and choose items with --only",
            )
        })?
        else {
            return Ok(nothing_changed(""));
        };
        plan = plan.subset(&picked, &sel.tiers);
        if plan.items.is_empty() {
            return Ok(nothing_changed("Nothing selected."));
        }
    }

    // Ask before acting: always after picking with -i, otherwise on a
    // terminal without --yes.
    let prompting = !plan.items.is_empty()
        && (args.interactive
            || (!args.yes && !args.json && std::io::stdin().is_terminal() && ui::stderr_is_tty()));
    let mut execute = args.yes && !plan.items.is_empty();
    if prompting {
        // The plan must be seen before the question: if stdout is redirected,
        // show it on stderr (the terminal the question is asked on).
        if ui::stdout_is_tty() {
            print_now(&render_plan(&plan, args, &sel, &p, &shortener, true));
        } else {
            eprint!(
                "{}",
                render_plan(&plan, args, &sel, &warn, &shortener, true)
            );
        }
        execute = confirm(&warn)?;
        if !execute {
            return Ok(nothing_changed(""));
        }
    }

    if !execute {
        let text = if args.json {
            json_doc(&scan.report.scanned_roots, args, &sel, &plan, None, None)?
        } else {
            render_plan(&plan, args, &sel, &p, &shortener, false)
        };
        return Ok(CleanOutput {
            text,
            failed: false,
        });
    }

    // --- execute ------------------------------------------------------------
    let log_path = clean::default_log_path(
        &home,
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .as_deref(),
    );
    let log = ActionLog::open(&log_path)
        .map_err(|e| {
            errors::fail(
                "action_log",
                format!(
                    "could not open the action log {}: {e}; nothing was changed",
                    log_path.display()
                ),
                "make sure that folder is writable (or set XDG_STATE_HOME to another folder)",
            )
        })?
        // Integrations (the Claude Code plugin, schedulers) say who they are.
        .with_actor(std::env::var("LYSOSOME_ACTOR").ok().as_deref());
    let dir_trash = std::env::var_os("LYSOSOME_TRASH_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(DirTrash::new);
    let trasher: &dyn Trasher = match &dir_trash {
        Some(d) => d,
        None => &SystemTrash,
    };
    let mut ex = Executor::new(trasher, home.clone(), Some(&log));
    ex.dormant_after_days = args.days;
    // Worktrees planned because their PR was squash-merged are re-verified
    // with gh (and skipped without it).
    ex.gh = args
        .net
        .github()
        .then(lysosome_core::worktree::Gh::from_process)
        .flatten();
    if let Some(secs) = std::env::var("LYSOSOME_CLEAN_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        ex.command_timeout = std::time::Duration::from_secs(secs);
    }

    if !args.json && !prompting {
        print_now(&render_header(&plan, &p, true));
    } else if prompting {
        print_now("\n");
    }
    // Ctrl-C: finish (and log) the current item, then stop. A second Ctrl-C
    // exits at once.
    install_sigint();
    ex.stop = Some(&STOP);
    let stream = !args.json;
    let events = spinner.as_ref().and_then(Reporter::json);
    if let Some(j) = events {
        j.plan(plan.items.len(), plan.total_bytes);
    }
    let total = plan.items.len();
    let results = ex.execute(&plan, &mut |i, item, r| {
        if stream {
            print_now(&result_line(item, r, &p, &shortener));
        }
        if let Some(j) = events {
            j.item(
                i,
                total,
                &item.id,
                serde_json::to_value(r).unwrap_or(Value::Null),
            );
        }
    })?;
    let interrupted = results
        .iter()
        .any(|r| r.message.starts_with(clean::INTERRUPTED));
    if interrupted {
        Reporter::warn(
            spinner.as_ref(),
            &warn,
            "interrupted: stopped after the current item",
        );
    }
    let failed = interrupted || results.iter().any(|r| r.outcome == Outcome::Failed);
    let text = if args.json {
        json_doc(
            &scan.report.scanned_roots,
            args,
            &sel,
            &plan,
            Some(&results),
            Some(&log_path),
        )?
    } else {
        let mut text = render_summary(
            &plan,
            &results,
            dir_trash.as_ref().map(|_| ()),
            &log_path,
            &p,
            &shortener,
        );
        let freed: u64 = results.iter().map(|r| r.freed_bytes).sum();
        if !failed && freed > 0 && crate::hint::enabled_now(false, global.progress) {
            text.push_str(&crate::hint::line(&p));
        }
        text
    };
    Ok(CleanOutput { text, failed })
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    if STOP.swap(true, Ordering::SeqCst) {
        // Second Ctrl-C means "now": stop the running cleanup command too
        // (it has its own process group), then exit. Both async-signal-safe.
        clean::kill_active_command();
        // SAFETY: async-signal-safe.
        unsafe { libc::_exit(130) };
    }
}

fn install_sigint() {
    // SAFETY: installs a handler that only touches an atomic (or _exits).
    unsafe {
        libc::signal(
            libc::SIGINT,
            on_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
}

/// Undo `install_sigint` for a process that outlives one cleanup (`lysosome
/// mcp`): Ctrl-C ends the process again and the next cleanup starts unstopped.
pub fn reset_sigint() {
    // SAFETY: restores the default disposition.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
    STOP.store(false, Ordering::SeqCst);
}

fn print_now(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// `Proceed? [y/N]` on stderr; default No.
fn confirm(p: &Painter) -> Result<bool> {
    eprint!("\n  {} ", p.bold("Proceed? [y/N]"));
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

pub(crate) fn tier_badge(t: Tier, p: &Painter) -> String {
    let label = match t {
        Tier::Regenerable => "cheap",
        Tier::RegenerableExpensive => "costly",
        Tier::Review => "review",
    };
    p.paint(pad_right(label, 6), ui::tier_style(t))
}

fn action_label(i: &PlanItem) -> &'static str {
    match i.action {
        ActionKind::WorktreeRemove => "remove worktree",
        ActionKind::Command => "run",
        ActionKind::Trash => "move to Trash",
        ActionKind::Delete => "delete forever",
    }
}

pub(crate) fn size_cell(i: &PlanItem) -> String {
    if i.size_is_upper_bound {
        format!("≤{}", human_bytes(i.size_bytes))
    } else {
        human_bytes(i.size_bytes)
    }
}

fn argv_display(argv: &[String], paths: &PathShortener) -> String {
    argv.iter()
        .map(|a| {
            let short = if a.starts_with('/') {
                paths.short(Path::new(a))
            } else {
                a.clone()
            };
            if short.contains(' ') || short.contains('"') || short.contains('$') {
                ui::shell_quote(&short)
            } else {
                short
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the TARGET column shows.
pub(crate) fn target_of(i: &PlanItem, paths: &PathShortener) -> String {
    match i.action {
        ActionKind::Command => argv_display(&i.command, paths),
        ActionKind::WorktreeRemove => paths.short(&i.path),
        ActionKind::Trash | ActionKind::Delete => i
            .paths
            .iter()
            .map(|x| paths.short(x))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

fn render_header(plan: &Plan, p: &Painter, executing: bool) -> String {
    let mut o = String::new();
    writeln!(o).ok();
    let how = if plan.permanent {
        "deleting permanently"
    } else {
        "moving files to the Trash"
    };
    if executing {
        writeln!(
            o,
            "  {} {}",
            p.paint("Cleaning", Style::new().bold()),
            p.dim(format!(
                "{} · {} · re-checking each item first",
                plural(plan.items.len() as u64, "item", "items"),
                how
            ))
        )
        .ok();
    }
    o
}

fn render_plan(
    plan: &Plan,
    args: &CleanArgs,
    sel: &Selection,
    p: &Painter,
    paths: &PathShortener,
    prompting: bool,
) -> String {
    let mut o = String::new();
    writeln!(o).ok();
    if !prompting {
        writeln!(
            o,
            "  {} {}",
            p.paint("Dry run.", Style::new().bold()),
            p.dim("Nothing will be changed.")
        )
        .ok();
    }
    if plan.items.is_empty() {
        writeln!(
            o,
            "  {} {}",
            p.paint("✓", Style::new().green().bold()),
            p.bold("Nothing to clean with these options.")
        )
        .ok();
        held_back(&mut o, plan, args, sel, p);
        return o;
    }

    let cells: Vec<[String; 3]> = plan
        .items
        .iter()
        .map(|i| {
            [
                size_cell(i),
                action_label(i).to_string(),
                target_of(i, paths),
            ]
        })
        .collect();
    let ws = cells
        .iter()
        .map(|c| c[0].chars().count())
        .max()
        .unwrap_or(4);
    let wa = cells.iter().map(|c| c[1].len()).max().unwrap_or(6);
    for cat in Category::ALL {
        let rows: Vec<(&PlanItem, &[String; 3])> = plan
            .items
            .iter()
            .zip(&cells)
            .filter(|(i, _)| i.category == cat)
            .collect();
        if rows.is_empty() {
            continue;
        }
        let bytes: u64 = rows.iter().map(|(i, _)| i.size_bytes).sum();
        let title = if cat == Category::Worktrees && sel.strip_artifacts {
            "Artifacts inside dormant worktrees (worktrees kept)"
        } else {
            cat.display_name()
        };
        writeln!(o).ok();
        writeln!(
            o,
            "  {} {}  {}",
            p.dim("──"),
            p.paint(title, Style::new().bold().underline()),
            p.dim(format!(
                "{} · {}",
                plural(rows.len() as u64, "item", "items"),
                human_bytes(bytes)
            ))
        )
        .ok();
        if rows
            .iter()
            .any(|(i, _)| i.action == ActionKind::WorktreeRemove)
        {
            writeln!(
                o,
                "    {}",
                p.dim("each: git -C <main repo> worktree remove <path> (no --force), then git worktree prune; branches are kept")
            )
            .ok();
        }
        for (i, c) in rows {
            writeln!(
                o,
                "    {}  {}  {}  {}",
                tier_badge(i.tier, p),
                pad_left(&c[0], ws),
                pad_right(&c[1], wa),
                worktrees::path_cell(&c[2], p, false),
            )
            .ok();
            let note = match i.action {
                _ if i.status == Status::Review => Some(format!(
                    "inside a REVIEW worktree ({}); only this ignored folder goes",
                    i.project
                        .as_deref()
                        .map(|x| paths.short(x))
                        .unwrap_or_default()
                )),
                _ => None,
            };
            if let Some(n) = note {
                writeln!(
                    o,
                    "    {}  {}",
                    " ".repeat(6 + 2 + ws + 2),
                    p.dim(format!("└ {n}"))
                )
                .ok();
            }
        }
    }

    writeln!(o).ok();
    let upper = plan.items.iter().any(|i| i.size_is_upper_bound);
    writeln!(
        o,
        "  {} {} {}",
        p.bold(if prompting {
            "Will reclaim"
        } else {
            "Would reclaim"
        }),
        p.paint(
            format!(
                "{}{}",
                if upper { "up to " } else { "" },
                human_bytes(plan.total_bytes)
            ),
            Style::new().bold().underline()
        ),
        p.dim(format!(
            "({}){}",
            plural(plan.items.len() as u64, "item", "items"),
            match (
                plan.permanent,
                plan.items
                    .iter()
                    .any(|i| matches!(i.action, ActionKind::Trash | ActionKind::Delete)),
            ) {
                (_, false) => "",
                (true, true) => " · files are deleted permanently",
                (false, true) => " · files go to the Trash",
            }
        )),
    )
    .ok();
    held_back(&mut o, plan, args, sel, p);
    if !prompting {
        writeln!(o).ok();
        writeln!(o, "  {}", p.bold("Run again with --yes to do it.")).ok();
    }
    o
}

/// "Held back: ..." line explaining what was left out.
fn held_back(o: &mut String, plan: &Plan, args: &CleanArgs, sel: &Selection, p: &Painter) {
    let ex = &plan.excluded;
    let mut bits = Vec::new();
    if ex.tier.items > 0 {
        let hint = if args.tier == TierArg::Regenerable {
            " (add --tier regenerable_expensive)"
        } else {
            ""
        };
        bits.push(format!(
            "{} costly to get back{hint}",
            human_bytes(ex.tier.bytes)
        ));
    }
    if ex.review.items > 0 {
        bits.push(format!(
            "{} ({}) need review and are never cleaned",
            plural(ex.review.items, "REVIEW item", "REVIEW items"),
            human_bytes(ex.review.bytes)
        ));
    }
    if ex.orphan.items > 0 {
        bits.push(format!(
            "{} never cleaned",
            plural(ex.orphan.items, "ORPHAN", "ORPHANs")
        ));
    }
    if ex.older_than.items > 0 {
        bits.push(format!(
            "{} newer than --older-than or of unknown age",
            plural(ex.older_than.items, "item", "items")
        ));
    }
    if !ex.deselected.is_zero() {
        bits.push(format!(
            "{} ({}) not selected",
            plural(ex.deselected.items, "item", "items"),
            human_bytes(ex.deselected.bytes)
        ));
    }
    if ex.only.items > 0 && !sel.only.is_empty() {
        bits.push(format!(
            "{} not matched by --only",
            plural(ex.only.items, "item", "items")
        ));
    }
    if !bits.is_empty() {
        writeln!(o, "  {} {}", p.dim("Held back:"), p.dim(bits.join(" · "))).ok();
    }
}

fn result_line(item: &PlanItem, r: &ItemResult, p: &Painter, paths: &PathShortener) -> String {
    let target = target_of(item, paths);
    match r.outcome {
        Outcome::Done => format!(
            "    {}  {}  {}  {}\n",
            p.paint("✓", Style::new().green().bold()),
            pad_left(&human_bytes(r.freed_bytes), 8),
            pad_right(
                match item.action {
                    ActionKind::WorktreeRemove => "removed",
                    ActionKind::Command => "ran",
                    ActionKind::Trash => "trashed",
                    ActionKind::Delete => "deleted",
                },
                7
            ),
            worktrees::path_cell(&target, p, false),
        ),
        Outcome::Skipped => format!(
            "    {}  {}  {}  {}\n    {}  {}\n",
            p.paint("↷", Style::new().yellow().bold()),
            pad_left("", 8),
            pad_right("skipped", 7),
            p.dim(&target),
            " ".repeat(1 + 2 + 8 + 2 + 7),
            p.paint(format!("└ {}", r.message), Style::new().yellow()),
        ),
        Outcome::Failed => format!(
            "    {}  {}  {}  {}\n    {}  {}\n",
            p.paint("✗", Style::new().red().bold()),
            pad_left("", 8),
            pad_right("failed", 7),
            target,
            " ".repeat(1 + 2 + 8 + 2 + 7),
            p.paint(format!("└ {}", r.message), Style::new().red()),
        ),
    }
}

fn render_summary(
    plan: &Plan,
    results: &[ItemResult],
    dir_trash: Option<()>,
    log_path: &Path,
    p: &Painter,
    paths: &PathShortener,
) -> String {
    let done: Vec<&ItemResult> = results
        .iter()
        .filter(|r| r.outcome == Outcome::Done)
        .collect();
    let skipped: Vec<&ItemResult> = results
        .iter()
        .filter(|r| r.outcome == Outcome::Skipped)
        .collect();
    let failed = results
        .iter()
        .filter(|r| r.outcome == Outcome::Failed)
        .count() as u64;
    let freed: u64 = results.iter().map(|r| r.freed_bytes).sum();
    let mut o = String::new();
    writeln!(o).ok();
    let mut line = format!(
        "  {} {} {}",
        p.bold("Reclaimed"),
        p.paint(human_bytes(freed), Style::new().bold().underline().green()),
        p.dim(format!("({})", plural(done.len() as u64, "item", "items")))
    );
    if !skipped.is_empty() {
        let changed = skipped
            .iter()
            .all(|r| r.message.starts_with("changed since scan"));
        let why = if changed {
            ": changed since scan"
        } else {
            " (reasons above)"
        };
        write!(
            line,
            " {} {}",
            p.dim("·"),
            p.paint(
                format!("{} skipped{why}", skipped.len()),
                Style::new().yellow()
            )
        )
        .ok();
    }
    if failed > 0 {
        write!(
            line,
            " {} {}",
            p.dim("·"),
            p.paint(format!("{failed} failed"), Style::new().red().bold())
        )
        .ok();
    }
    writeln!(o, "{line}").ok();
    let trashed = plan
        .items
        .iter()
        .zip(results)
        .any(|(i, r)| i.action == ActionKind::Trash && r.outcome == Outcome::Done);
    if trashed {
        let where_ = if dir_trash.is_some() {
            "the LYSOSOME_TRASH_DIR folder"
        } else {
            "the Trash"
        };
        writeln!(
            o,
            "  {}",
            p.dim(format!(
                "Trashed items are in {where_}; empty it to free the space."
            ))
        )
        .ok();
    }
    writeln!(
        o,
        "  {}",
        p.dim(format!("Action log: {}", paths.short(log_path)))
    )
    .ok();
    o
}

/// The `--json` document (schema/clean.schema.json).
fn json_doc(
    roots: &[PathBuf],
    args: &CleanArgs,
    sel: &Selection,
    plan: &Plan,
    results: Option<&[ItemResult]>,
    log_path: Option<&Path>,
) -> Result<String> {
    let mut items = Vec::with_capacity(plan.items.len());
    for (n, i) in plan.items.iter().enumerate() {
        let mut v = serde_json::to_value(i)?;
        v["result"] = match results {
            Some(r) => serde_json::to_value(&r[n])?,
            None => Value::Null,
        };
        items.push(v);
    }
    let count = |o: Outcome| results.map_or(0, |r| r.iter().filter(|x| x.outcome == o).count());
    let doc = json!({
        "schema_version": lysosome_core::SCHEMA_VERSION,
        "tool_version": lysosome_core::TOOL_VERSION,
        "mode": if results.is_some() { "execute" } else { "dry_run" },
        "permanent": plan.permanent,
        "scanned_roots": roots.iter().map(|r| r.to_string_lossy()).collect::<Vec<_>>(),
        "dormant_after_days": args.days,
        "selection": sel,
        "items": items,
        "excluded": plan.excluded,
        "summary": {
            "planned_items": plan.items.len(),
            "planned_bytes": plan.total_bytes,
            "done": count(Outcome::Done),
            "skipped": count(Outcome::Skipped),
            "failed": count(Outcome::Failed),
            "freed_bytes": results.map_or(0, |r| r.iter().map(|x| x.freed_bytes).sum::<u64>()),
        },
        "log_path": log_path.map(|p| p.to_string_lossy().into_owned()),
    });
    let mut s = serde_json::to_string_pretty(&doc)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        a: CleanArgs,
    }

    fn sel(argv: &[&str]) -> Selection {
        let mut v = vec!["t"];
        v.extend_from_slice(argv);
        selection(&T::parse_from(v).a)
    }

    #[test]
    fn default_selection_is_everything_but_cheap_only() {
        let s = sel(&[]);
        assert_eq!(s.scopes.len(), 3);
        assert_eq!(s.tiers, [Tier::Regenerable].into_iter().collect());
        let s = sel(&["--tier", "regenerable_expensive", "--caches"]);
        assert_eq!(s.scopes, [Scope::Caches].into_iter().collect());
        assert_eq!(s.tiers.len(), 2);
        let s = sel(&["--strip-artifacts"]);
        assert_eq!(s.scopes, [Scope::Worktrees].into_iter().collect());
        assert!(s.strip_artifacts);
        let s = sel(&["--older-than", "90d", "--only", "npm*", "--only", "x"]);
        assert_eq!(s.older_than_days, Some(90));
        assert_eq!(s.only, ["npm*", "x"]);
        assert!(T::try_parse_from(["t", "--tier", "review"]).is_err());
        assert!(T::try_parse_from(["t", "--older-than", "soon"]).is_err());
    }
}
