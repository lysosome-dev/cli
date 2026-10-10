//! `lysosome worktrees`: scan, then render a table or the JSON contract.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use lysosome_core::model::Status;
use lysosome_core::progress::{NoProgress, Phase};
use lysosome_core::worktree::{self, ScanOptions, Worktree, WorktreeScan};
use owo_colors::Style;

use crate::progress::Spinner;
use crate::ui::{
    self, human_bytes, pad_left, pad_right, plural, thousands, Painter, PathShortener,
};
use crate::{GlobalArgs, WorktreesArgs};

const BRANCH_MAX: usize = 28;
const MAX_STALE_SHOWN: usize = 10;
const MAX_COMMANDS_SHOWN: usize = 15;

pub fn run(args: &WorktreesArgs, global: &GlobalArgs) -> Result<String> {
    let home = dirs::home_dir();
    let shortener = PathShortener::new(home.clone());
    let roots = resolve_roots(&args.roots, home.as_deref())?;

    let mut opts = ScanOptions::new(roots.clone());
    opts.dormant_after_days = args.days;
    opts.max_depth = args.depth;
    opts.github = args.net.github();

    let err_color = ui::color_enabled(global.no_color, ui::stderr_is_tty());
    let warn = Painter::new(err_color);

    let scan = if ui::stderr_is_tty() {
        let label = roots
            .iter()
            .map(|r| shortener.short(r))
            .collect::<Vec<_>>()
            .join(", ");
        let spinner = Spinner::new(label);
        let res = worktree::scan(&opts, &spinner);
        spinner.clear();
        res?
    } else {
        worktree::scan(&opts, &NoProgress)?
    };

    for m in &scan.missing_roots {
        eprintln!(
            "{} skipping {} (not a folder)",
            warn.paint("warning:", Style::new().yellow().bold()),
            m.display()
        );
    }
    if global.profile {
        eprint!("{}", render_profile(&scan, &warn));
    }

    if args.json {
        let mut s = serde_json::to_string_pretty(&scan.to_report())?;
        s.push('\n');
        return Ok(s);
    }
    let p = Painter::new(ui::color_enabled(global.no_color, ui::stdout_is_tty()));
    Ok(render_table(&scan, &View::from(args), &p, &shortener))
}

/// Table options shared by `worktrees` and the worktree section of `scan`.
#[derive(Debug, Clone, Copy)]
pub struct View {
    pub days: u64,
    pub all: bool,
    pub top: usize,
}

impl From<&WorktreesArgs> for View {
    fn from(a: &WorktreesArgs) -> Self {
        View {
            days: a.days,
            all: a.all,
            top: a.top,
        }
    }
}

/// Expand `~`, default to the home directory.
pub fn resolve_roots(raw: &[PathBuf], home: Option<&Path>) -> Result<Vec<PathBuf>> {
    if raw.is_empty() {
        return home
            .map(|h| vec![h.to_path_buf()])
            .ok_or_else(|| lysosome_core::Error::NoHomeDir.into());
    }
    Ok(raw
        .iter()
        .map(|r| {
            let s = r.to_string_lossy();
            match (s.as_ref(), home) {
                ("~", Some(h)) => h.to_path_buf(),
                (s, Some(h)) if s.starts_with("~/") => h.join(&s[2..]),
                _ => std::path::absolute(r).unwrap_or_else(|_| r.clone()),
            }
        })
        .collect())
}

fn render_profile(scan: &WorktreeScan, p: &Painter) -> String {
    // git and size run concurrently; show them joined with "∥".
    let mut phases = String::new();
    let mut prev = None;
    for t in &scan.stats.timings {
        if prev.is_some() {
            let sep = if prev == Some(Phase::Git) && t.phase == Phase::Size {
                " ∥ "
            } else {
                " · "
            };
            phases.push_str(&p.dim(sep));
        }
        phases.push_str(&format!(
            "{} {}",
            t.phase,
            p.bold(format!("{:.2}s", t.elapsed.as_secs_f64()))
        ));
        prev = Some(t.phase);
    }
    format!(
        "{} {} {} total {}\n{}\n",
        p.paint("profile", Style::new().magenta().bold()),
        phases,
        p.dim("·"),
        p.bold(format!("{:.2}s", scan.stats.total.as_secs_f64())),
        p.dim(format!(
            "        {} · {} · {} git calls on {} threads · {} gh calls · {} size read errors",
            plural(scan.stats.dirs_visited, "folder", "folders"),
            plural(scan.worktrees.len() as u64, "worktree", "worktrees"),
            thousands(scan.stats.git_calls),
            lysosome_core::git::default_concurrency(),
            thousands(scan.stats.gh_calls),
            thousands(scan.stats.size_errors),
        )),
    )
}

pub fn status_style(s: Status) -> Style {
    match s {
        // Same stains as the tiers these statuses map to.
        Status::Removable => crate::ui::tier_style(lysosome_core::model::Tier::Regenerable).bold(),
        Status::Review => crate::ui::tier_style(lysosome_core::model::Tier::Review).bold(),
        Status::Orphan => Style::new().magenta().bold(),
        Status::Active => Style::new().dimmed(),
    }
}

fn render_table(scan: &WorktreeScan, args: &View, p: &Painter, paths: &PathShortener) -> String {
    let mut out = String::new();
    let o = &mut out;
    let roots = scan
        .roots
        .iter()
        .map(|r| paths.short(r))
        .collect::<Vec<_>>()
        .join(", ");

    if scan.worktrees.is_empty() {
        writeln!(o).ok();
        writeln!(o, "  No git worktrees found under {}.", p.bold(&roots)).ok();
        writeln!(
            o,
            "  {}",
            p.dim("Try pointing it at your code folder:  lysosome worktrees ~/code")
        )
        .ok();
    } else {
        render_body(o, scan, args, p, paths);
    }
    footer(o, scan, p, &roots);
    out
}

/// Headline, table, stale records and suggested commands (no footer). Starts
/// with a blank line. Used as-is for the worktree section of `lysosome scan`.
pub fn render_body(
    o: &mut String,
    scan: &WorktreeScan,
    args: &View,
    p: &Painter,
    paths: &PathShortener,
) {
    let all = scan.worktrees.len() as u64;
    writeln!(o).ok();

    let dormant: Vec<&Worktree> = scan.dormant().collect();
    let removable: Vec<&Worktree> = scan.removable().collect();
    let n_dormant = dormant.len() as u64;

    // Headline
    if n_dormant == 0 {
        writeln!(
            o,
            "  {} {}",
            p.paint("✓", Style::new().green().bold()),
            p.bold(format!(
                "No dormant worktrees. All {} used in the last {} days.",
                plural(all, "worktree was", "worktrees were"),
                args.days
            ))
        )
        .ok();
    } else {
        let verb = if n_dormant == 1 { "is" } else { "are" };
        writeln!(
            o,
            "  {} {} holding {}  {}",
            p.bold(plural(n_dormant, "dormant worktree", "dormant worktrees")),
            p.bold(verb),
            p.paint(
                human_bytes(scan.dormant_bytes()),
                Style::new().bold().underline()
            ),
            p.dim(format!("of {all} found · idle {}+ days", args.days)),
        )
        .ok();
        writeln!(o).ok();
        let rem = human_bytes(scan.removable_bytes());
        let art = human_bytes(scan.dormant_artifact_bytes());
        let w = rem.len().max(art.len());
        writeln!(
            o,
            "    {}  {}  {}",
            p.paint(
                pad_left(&rem, w),
                crate::ui::tier_style(lysosome_core::model::Tier::Regenerable).bold(),
            ),
            pad_right("safe to remove now", 26),
            p.dim(format!(
                "{} clean, merged or pushed",
                thousands(removable.len() as u64)
            )),
        )
        .ok();
        writeln!(
            o,
            "    {}  {}  {}",
            p.paint(pad_left(&art, w), Style::new().cyan().bold()),
            pad_right("build & dependency output", 26),
            p.dim("strippable even from worktrees you keep"),
        )
        .ok();
    }

    // Table
    let mut rows: Vec<&Worktree> = if args.all {
        scan.worktrees.iter().collect()
    } else {
        dormant.clone()
    };
    let total_rows = rows.len();
    rows.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes).then(a.path.cmp(&b.path)));
    rows.truncate(args.top);

    if !rows.is_empty() {
        writeln!(o).ok();
        let cells: Vec<[String; 5]> = rows
            .iter()
            .map(|w| {
                [
                    human_bytes(w.size_bytes),
                    if w.artifact_bytes > 0 {
                        human_bytes(w.artifact_bytes)
                    } else {
                        "–".into()
                    },
                    w.idle_days.map_or("–".into(), |d| format!("{d}d")),
                    ui::truncate(w.branch.as_deref().unwrap_or("–"), BRANCH_MAX),
                    paths.short(&w.path),
                ]
            })
            .collect();
        let width = |i: usize, title: &str| {
            cells
                .iter()
                .map(|c| c[i].chars().count())
                .chain([title.len()])
                .max()
                .unwrap_or(0)
        };
        let (ws, wa, wi, wb) = (
            width(0, "SIZE"),
            width(1, "ARTIFACTS"),
            width(2, "IDLE"),
            width(3, "BRANCH"),
        );
        const WSTAT: usize = 9;
        writeln!(
            o,
            "  {}",
            p.dim(format!(
                "{}  {}  {}  {}  {}  PATH",
                pad_right("STATUS", WSTAT),
                pad_left("SIZE", ws),
                pad_left("ARTIFACTS", wa),
                pad_left("IDLE", wi),
                pad_right("BRANCH", wb),
            ))
        )
        .ok();
        for (w, c) in rows.iter().zip(&cells) {
            let active = w.status == Status::Active;
            let cell = |s: String, style: Style| {
                if active {
                    p.dim(s)
                } else {
                    p.paint(s, style)
                }
            };
            let size_style = if w.status == Status::Removable {
                crate::ui::tier_style(lysosome_core::model::Tier::Regenerable)
            } else {
                Style::new()
            };
            let path = path_cell(&c[4], p, active);
            writeln!(
                o,
                "  {}  {}  {}  {}  {}  {}",
                p.paint(pad_right(w.status.as_str(), WSTAT), status_style(w.status)),
                cell(pad_left(&c[0], ws), size_style),
                if w.artifact_bytes > 0 {
                    cell(pad_left(&c[1], wa), Style::new().cyan())
                } else {
                    p.dim(pad_left(&c[1], wa))
                },
                p.dim(pad_left(&c[2], wi)),
                cell(pad_right(&c[3], wb), Style::new()),
                path,
            )
            .ok();
            if matches!(w.status, Status::Review | Status::Orphan) && !w.reason.is_empty() {
                writeln!(
                    o,
                    "  {}  {}",
                    " ".repeat(WSTAT),
                    p.paint(format!("└ {}", w.reason), Style::new().dimmed().italic())
                )
                .ok();
            }
        }
        if total_rows > args.top {
            writeln!(
                o,
                "  {}",
                p.dim(format!(
                    "… {} more (use --top N)",
                    thousands((total_rows - args.top) as u64)
                ))
            )
            .ok();
        }
    }

    // Stale metadata
    if !scan.stale.is_empty() {
        writeln!(o).ok();
        writeln!(
            o,
            "  {}  {}",
            p.bold(if scan.stale.len() == 1 {
                "1 stale worktree record points at a deleted folder".to_string()
            } else {
                format!(
                    "{} stale worktree records point at deleted folders",
                    thousands(scan.stale.len() as u64)
                )
            }),
            p.dim("harmless; `git worktree prune` clears them"),
        )
        .ok();
        for s in scan.stale.iter().take(MAX_STALE_SHOWN) {
            writeln!(
                o,
                "    {}  {}  {}",
                paths.short(&s.repo),
                p.dim("→"),
                p.dim(paths.short(&s.missing_path))
            )
            .ok();
        }
        if scan.stale.len() > MAX_STALE_SHOWN {
            writeln!(
                o,
                "    {}",
                p.dim(format!("… {} more", scan.stale.len() - MAX_STALE_SHOWN))
            )
            .ok();
        }
    }

    // Suggested commands
    if !removable.is_empty() {
        writeln!(o).ok();
        writeln!(
            o,
            "  {}  {}",
            p.bold("Suggested cleanup for REMOVABLE worktrees"),
            p.dim("review, then run yourself"),
        )
        .ok();
        let mut sorted = removable.clone();
        sorted.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes).then(a.path.cmp(&b.path)));
        for w in sorted.iter().take(MAX_COMMANDS_SHOWN) {
            // Same argv the JSON contract carries; quote only the arguments.
            let Some(cmd) = w.cleanup_command() else {
                continue;
            };
            let line = cmd
                .iter()
                .map(|a| {
                    if a.starts_with('/') {
                        ui::shell_quote(a)
                    } else {
                        p.dim(a)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            writeln!(o, "    {line}").ok();
        }
        if sorted.len() > MAX_COMMANDS_SHOWN {
            writeln!(
                o,
                "    {}",
                p.dim(format!(
                    "… {} more (see --json for every command)",
                    sorted.len() - MAX_COMMANDS_SHOWN
                ))
            )
            .ok();
        }
        writeln!(
            o,
            "    {}",
            p.dim("Removing a worktree keeps its branch; delete merged branches with `git branch -d`.")
        )
        .ok();
    }
}

/// Parent dim, last component normal.
pub fn path_cell(short: &str, p: &Painter, active: bool) -> String {
    if active {
        return p.dim(short);
    }
    match short.rfind('/') {
        Some(i) if i + 1 < short.len() => {
            format!("{}{}", p.dim(&short[..=i]), &short[i + 1..])
        }
        _ => short.to_string(),
    }
}

fn footer(o: &mut String, scan: &WorktreeScan, p: &Painter, roots: &str) {
    writeln!(o).ok();
    writeln!(
        o,
        "  {}",
        p.dim(format!(
            "Scanned {} under {} in {:.1}s. Nothing was changed.",
            plural(scan.stats.dirs_visited, "folder", "folders"),
            roots,
            scan.stats.total.as_secs_f64()
        ))
    )
    .ok();
    let denied = scan.stats.permission_denied;
    let other = scan.stats.other_errors;
    if denied > 0 || other > 0 {
        let mut bits = Vec::new();
        if denied > 0 {
            bits.push(format!(
                "{} skipped: permission denied",
                plural(denied, "folder", "folders")
            ));
        }
        if other > 0 {
            bits.push(format!(
                "{} skipped: unreadable",
                plural(other, "folder", "folders")
            ));
        }
        writeln!(
            o,
            "  {}",
            p.paint(bits.join(" · "), Style::new().dimmed().italic())
        )
        .ok();
    }
}
