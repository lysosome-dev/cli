//! `lysosome scan`: run every detector, then render one sectioned report or
//! the JSON contract.
//!
//! Sections are generic: any category renders from its findings (display name,
//! status, tier badge, size, idle days from `last_activity`), grouped under
//! `details.project` when findings carry one. Only the worktree section has a
//! dedicated renderer (shared with `lysosome worktrees`). New detectors need no
//! changes here.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use lysosome_core::detector::ScanContext;
use lysosome_core::detectors::{self, ScanRun};
use lysosome_core::model::{Category, Finding, Report, Status, Tier};
use lysosome_core::progress::{NoProgress, Phase};
use owo_colors::Style;

use crate::progress::Reporter;
use crate::ui::{
    self, human_bytes, pad_left, pad_right, plural, thousands, tier_style, Painter, PathShortener,
};
use crate::worktrees::{self, View};
use crate::{GlobalArgs, ScanArgs, ScanCmdArgs};

pub fn run(cmd: &ScanCmdArgs, global: &GlobalArgs) -> Result<String> {
    let args = &cmd.common;
    let home = dirs::home_dir();
    let shortener = PathShortener::new(home.clone());
    let roots = worktrees::resolve_roots(&args.roots, home.as_deref())?;
    let warn = Painter::new(ui::color_enabled(global.no_color, ui::stderr_is_tty()));

    let spinner = Reporter::new(
        global.progress,
        roots
            .iter()
            .map(|r| shortener.short(r))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let mut ctx = ScanContext::new(roots, home.clone().unwrap_or_default());
    ctx.dormant_after_days = args.days;
    ctx.max_depth = args.depth;
    ctx.github = args.net.github();
    if let Some(s) = &spinner {
        ctx.progress = s;
    } else {
        ctx.progress = &NoProgress;
    }
    // System detectors read fixed locations under the home folder; without a
    // home they would look relative to the current directory.
    let system = !cmd.no_system && home.is_some();
    if !cmd.no_system && home.is_none() {
        Reporter::warn(
            spinner.as_ref(),
            &warn,
            "home folder unknown; skipping caches, AI models, Docker and Xcode",
        );
    }
    let detectors = if system {
        detectors::all_detectors()
    } else {
        detectors::project_detectors()
    };
    let mut run = detectors::run(&detectors, &ctx);
    if let Some(s) = &spinner {
        s.clear();
    }

    if run.all_failed() || run.project_detectors_failed() {
        let err = run.runs.iter_mut().find_map(|r| r.error.take());
        return Err(err.map(anyhow::Error::from).unwrap_or_else(|| {
            crate::errors::fail(
                "all_detectors_failed",
                "every detector failed",
                "check that the folders exist and that `git --version` works; `lysosome caches` checks the machine-wide part on its own",
            )
        }));
    }
    for r in &run.runs {
        if let Some(e) = &r.error {
            Reporter::warn(
                spinner.as_ref(),
                &warn,
                &format!("{} scan failed: {e:#}", r.id),
            );
        }
    }
    for m in &run.report.skipped.missing_roots {
        Reporter::warn(
            spinner.as_ref(),
            &warn,
            &format!("skipping {} (not a folder)", m.display()),
        );
    }
    if global.profile {
        eprint!("{}", render_profile(&run, &warn));
    }

    if args.json {
        let mut s = serde_json::to_string_pretty(&run.report)?;
        s.push('\n');
        return Ok(s);
    }
    let p = Painter::new(ui::color_enabled(global.no_color, ui::stdout_is_tty()));
    Ok(render(&run, args, &p, &shortener))
}

fn render_profile(run: &ScanRun, p: &Painter) -> String {
    let mut out = String::new();
    let secs = |d: std::time::Duration| p.bold(format!("{:.2}s", d.as_secs_f64()));
    let mut line = Vec::new();
    if let Some(proj) = &run.projects {
        for t in &proj.stats.timings {
            // git ∥ size (worktrees) ∥ artifacts (main repos) overlap in time.
            let concurrent = matches!(t.phase, Phase::Size | Phase::Artifacts);
            let mark = if concurrent { "∥ " } else { "" };
            line.push(format!("{mark}{} {}", t.phase, secs(t.elapsed)));
        }
    }
    writeln!(
        out,
        "{} {} {} total {}",
        p.paint("profile", Style::new().magenta().bold()),
        line.join(&p.dim(" · ")),
        p.dim("·"),
        secs(run.total),
    )
    .ok();
    let dets = run
        .runs
        .iter()
        .map(|r| format!("{} {:.2}s", r.id, r.elapsed.as_secs_f64()))
        .collect::<Vec<_>>()
        .join(" · ");
    writeln!(
        out,
        "{}",
        p.dim(format!("        detectors (concurrent): {dets}"))
    )
    .ok();
    if let Some(proj) = &run.projects {
        let s = &proj.stats;
        writeln!(
            out,
            "{}",
            p.dim(format!(
                "        {} · {} · {} · {} artifact candidates · {} git calls on {} threads · {} gh calls · {} size read errors",
                plural(proj.worktrees.stats.dirs_visited, "folder", "folders"),
                plural(s.repos, "repo", "repos"),
                plural(proj.worktrees.worktrees.len() as u64, "worktree", "worktrees"),
                thousands(s.candidates),
                thousands(s.git_calls),
                lysosome_core::git::default_concurrency(),
                thousands(proj.worktrees.stats.gh_calls),
                thousands(s.size_errors + proj.worktrees.stats.size_errors),
            ))
        )
        .ok();
    }
    out
}

/// Short safety badge for a tier.
fn tier_badge(t: Tier) -> &'static str {
    match t {
        Tier::Regenerable => "cheap",
        Tier::RegenerableExpensive => "costly",
        Tier::Review => "review",
    }
}

/// `details.idle_days` when the detector computed it (same clock as the
/// status), else derived from `last_activity`.
fn idle_days(f: &Finding, now: SystemTime) -> Option<u64> {
    if let Some(d) = f.details.get("idle_days").and_then(|v| v.as_i64()) {
        return Some(d.max(0) as u64);
    }
    let t = f.last_activity?;
    Some(
        now.duration_since(t)
            .map(|d| d.as_secs() / 86_400)
            .unwrap_or(0),
    )
}

fn project_of(f: &Finding) -> Option<&str> {
    f.details.get("project").and_then(|v| v.as_str())
}

fn render(run: &ScanRun, args: &ScanArgs, p: &Painter, paths: &PathShortener) -> String {
    let report = &run.report;
    let mut out = String::new();
    let o = &mut out;
    writeln!(o).ok();
    headline(o, report, args, p);
    tier_summary(o, report, p);

    let view = View::from(args);
    let scanned: Vec<Category> = run
        .runs
        .iter()
        .filter(|r| r.error.is_none())
        .map(|r| r.category)
        .collect();
    for cat in Category::ALL {
        if !scanned.contains(&cat) {
            continue;
        }
        let findings: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.category == cat)
            .collect();
        if cat == Category::Worktrees {
            if let Some(proj) = run
                .projects
                .as_ref()
                .filter(|p| !p.worktrees.worktrees.is_empty())
            {
                section_title(o, cat, &findings, p);
                worktrees::render_body(o, &proj.worktrees, &view, p, paths);
            }
            continue;
        }
        if findings.is_empty() {
            continue;
        }
        section_title(o, cat, &findings, p);
        generic_section(o, &findings, args, p, paths);
    }
    footer(o, run, p, paths);
    out
}

fn headline(o: &mut String, report: &Report, args: &ScanArgs, p: &Painter) {
    let mut projects: BTreeMap<&str, u64> = BTreeMap::new();
    let mut any_artifacts = false;
    for f in report
        .findings
        .iter()
        .filter(|f| f.category == Category::Artifacts)
    {
        any_artifacts = true;
        if f.status == Status::Removable {
            *projects.entry(project_of(f).unwrap_or("")).or_default() += f.size_bytes;
        }
    }
    let bytes: u64 = projects.values().sum();
    let n = projects.len() as u64;
    if n > 0 {
        writeln!(
            o,
            "  {} untouched for {}+ days {} holding {} of dependencies and build output.",
            p.bold(plural(n, "project", "projects")),
            args.days,
            if n == 1 { "is" } else { "are" },
            p.paint(human_bytes(bytes), Style::new().bold().underline()),
        )
        .ok();
    } else if any_artifacts {
        writeln!(
            o,
            "  {} {}",
            p.paint("✓", Style::new().green().bold()),
            p.bold(format!(
                "No project idle for {}+ days is holding dependencies or build output.",
                args.days
            ))
        )
        .ok();
    } else {
        writeln!(
            o,
            "  {}",
            p.bold("No git-ignored dependencies or build output found in your projects.")
        )
        .ok();
    }
}

fn tier_summary(o: &mut String, report: &Report, p: &Painter) {
    let s = &report.summary;
    let rows: [(String, &str, String, Style); 4] = [
        (
            human_bytes(s.reclaimable_bytes),
            "reclaimable now",
            "REMOVABLE items in every section".into(),
            tier_style(Tier::Regenerable).bold(),
        ),
        (
            human_bytes(s.by_tier[&Tier::Regenerable]),
            "regenerable",
            "cheap to rebuild (build output, caches, merged worktrees)".into(),
            tier_style(Tier::Regenerable),
        ),
        (
            human_bytes(s.by_tier[&Tier::RegenerableExpensive]),
            "regenerable_expensive",
            "costly to get back (dependencies to re-download)".into(),
            tier_style(Tier::RegenerableExpensive),
        ),
        (
            human_bytes(s.by_tier[&Tier::Review]),
            "review",
            "a human should look first".into(),
            tier_style(Tier::Review),
        ),
    ];
    let w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    writeln!(o).ok();
    for (size, label, note, style) in rows {
        writeln!(
            o,
            "    {}  {}  {}",
            p.paint(pad_left(&size, w), style),
            pad_right(label, 22),
            p.dim(note)
        )
        .ok();
    }
    writeln!(
        o,
        "    {}",
        p.dim("Tier totals cover every idle item; each byte is counted once.")
    )
    .ok();
}

fn section_title(o: &mut String, cat: Category, findings: &[&Finding], p: &Painter) {
    let dormant: Vec<&&Finding> = findings.iter().filter(|f| f.status.is_dormant()).collect();
    let bytes: u64 = dormant.iter().map(|f| f.size_bytes).sum();
    writeln!(o).ok();
    writeln!(
        o,
        "  {} {}  {}",
        p.dim("──"),
        p.paint(cat.display_name(), Style::new().bold().underline()),
        p.dim(format!(
            "{} idle in {} of {}",
            human_bytes(bytes),
            plural(dormant.len() as u64, "item", "items"),
            thousands(findings.len() as u64)
        ))
    )
    .ok();
}

/// Rows shown per project before folding the rest into one line.
const MAX_ROWS_PER_GROUP: usize = 5;

/// A row in a generic section.
struct Row<'a> {
    f: &'a Finding,
    path: String,
}

/// A project's rows (or, with `proj: None`, the ungrouped rows).
struct Block<'a> {
    proj: Option<&'a str>,
    rows: Vec<Row<'a>>,
    /// Size of every finding in the block, shown or not.
    total: u64,
    hidden: usize,
    hidden_bytes: u64,
}

fn generic_section(
    o: &mut String,
    findings: &[&Finding],
    args: &ScanArgs,
    p: &Painter,
    paths: &PathShortener,
) {
    let now = SystemTime::now();
    let shown: Vec<&Finding> = findings
        .iter()
        .copied()
        .filter(|f| args.all || f.status.is_dormant())
        .collect();
    writeln!(o).ok();
    if shown.is_empty() {
        writeln!(
            o,
            "  {} Nothing idle for {}+ days ({} in use; --all lists them).",
            p.paint("✓", Style::new().green().bold()),
            args.days,
            plural(findings.len() as u64, "item", "items")
        )
        .ok();
        return;
    }

    // Group by details.project (one group per finding otherwise).
    let mut groups: BTreeMap<Option<&str>, Vec<&Finding>> = BTreeMap::new();
    for f in &shown {
        groups.entry(project_of(f)).or_default().push(f);
    }
    let mut flat: Vec<&Finding> = groups.remove(&None).unwrap_or_default();
    flat.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes).then(a.path.cmp(&b.path)));
    let mut grouped: Vec<(&str, Vec<&Finding>)> = groups
        .into_iter()
        .map(|(k, v)| (k.unwrap_or(""), v))
        .collect();
    let gsize = |v: &[&Finding]| v.iter().map(|f| f.size_bytes).sum::<u64>();
    grouped.sort_by(|a, b| gsize(&b.1).cmp(&gsize(&a.1)).then(a.0.cmp(b.0)));

    // Rows (with relative paths inside groups) for column widths.
    let mut blocks: Vec<Block> = Vec::new();
    let total_units = grouped.len() + flat.len();
    for (proj, mut v) in grouped.into_iter() {
        v.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes).then(a.path.cmp(&b.path)));
        let total = gsize(&v);
        let rest = v.split_off(v.len().min(MAX_ROWS_PER_GROUP));
        let rows = v
            .into_iter()
            .map(|f| Row {
                path: f
                    .path
                    .strip_prefix(Path::new(proj))
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| paths.short(&f.path)),
                f,
            })
            .collect();
        blocks.push(Block {
            proj: Some(proj),
            rows,
            total,
            hidden: rest.len(),
            hidden_bytes: gsize(&rest),
        });
    }
    if !flat.is_empty() {
        blocks.push(Block {
            proj: None,
            total: gsize(&flat),
            rows: flat
                .into_iter()
                .map(|f| Row {
                    path: paths.short(&f.path),
                    f,
                })
                .collect(),
            hidden: 0,
            hidden_bytes: 0,
        });
    }
    // --top counts groups (projects) and ungrouped rows.
    let mut budget = args.top;
    let mut kept: Vec<Block> = Vec::new();
    for mut b in blocks {
        if budget == 0 {
            break;
        }
        if b.proj.is_none() {
            b.rows.truncate(budget);
            budget -= b.rows.len();
        } else {
            budget -= 1;
        }
        kept.push(b);
    }
    let shown_units: usize = kept
        .iter()
        .map(|b| if b.proj.is_some() { 1 } else { b.rows.len() })
        .sum();

    let all_rows = kept.iter().flat_map(|b| b.rows.iter());
    // System findings carry a human name (`llama3.1:8b`, `web:1.0`, `iOS 17.5`);
    // show it instead of the kind when the section has one.
    let named = kept
        .iter()
        .flat_map(|b| b.rows.iter())
        .any(|r| r.f.details["name"].is_string());
    let label_title = if named { "NAME" } else { "KIND" };
    let cells: Vec<[String; 4]> = all_rows
        .map(|r| {
            let label = match r.f.details["name"].as_str() {
                Some(name) => ui::truncate(name, 32),
                None => ui::truncate(&r.f.kind, 16),
            };
            [
                human_bytes(r.f.size_bytes),
                idle_days(r.f, now).map_or("–".into(), |d| format!("{d}d")),
                label,
                r.path.clone(),
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
    let (ws, wi, wk) = (width(0, "SIZE"), width(1, "IDLE"), width(2, label_title));
    const WSTAT: usize = 9;
    const WTIER: usize = 6;
    // Rows sit under project headers when there are any; indent them all alike.
    let indent = if kept.iter().any(|b| b.proj.is_some()) {
        "  "
    } else {
        ""
    };
    writeln!(
        o,
        "  {indent}{}",
        p.dim(format!(
            "{}  {}  {}  {}  {}  PATH",
            pad_right("STATUS", WSTAT),
            pad_right("TIER", WTIER),
            pad_left("SIZE", ws),
            pad_left("IDLE", wi),
            pad_right(label_title, wk),
        ))
    )
    .ok();
    let mut i = 0;
    for b in &kept {
        let rows = &b.rows;
        if let Some(proj) = b.proj {
            let first = rows.first().map(|r| r.f);
            let size = b.total;
            let idle = first
                .and_then(|f| idle_days(f, now))
                .map_or(String::new(), |d| format!(" · idle {d}d"));
            writeln!(
                o,
                "  {}  {}",
                p.bold(paths.short(Path::new(proj))),
                p.dim(format!("{}{idle}", human_bytes(size)))
            )
            .ok();
        }
        for r in rows {
            let c = &cells[i];
            i += 1;
            let active = r.f.status == Status::Active;
            let path = if active {
                p.dim(&c[3])
            } else {
                worktrees::path_cell(&c[3], p, false)
            };
            writeln!(
                o,
                "  {indent}{}  {}  {}  {}  {}  {}",
                p.paint(
                    pad_right(r.f.status.as_str(), WSTAT),
                    worktrees::status_style(r.f.status)
                ),
                p.paint(pad_right(tier_badge(r.f.tier), WTIER), tier_style(r.f.tier)),
                if active {
                    p.dim(pad_left(&c[0], ws))
                } else {
                    pad_left(&c[0], ws)
                },
                p.dim(pad_left(&c[1], wi)),
                pad_right(&c[2], wk),
                path,
            )
            .ok();
            if matches!(r.f.status, Status::Review | Status::Orphan) && !r.f.reason.is_empty() {
                writeln!(
                    o,
                    "  {indent}{}  {}",
                    " ".repeat(WSTAT),
                    p.paint(format!("└ {}", r.f.reason), Style::new().dimmed().italic())
                )
                .ok();
            }
        }
        if b.hidden > 0 {
            writeln!(
                o,
                "    {}",
                p.dim(format!(
                    "… {} more {} ({})",
                    thousands(b.hidden as u64),
                    if b.hidden == 1 { "folder" } else { "folders" },
                    human_bytes(b.hidden_bytes)
                ))
            )
            .ok();
        }
    }
    if total_units > shown_units {
        writeln!(
            o,
            "  {}",
            p.dim(format!(
                "… {} more (use --top N)",
                thousands((total_units - shown_units) as u64)
            ))
        )
        .ok();
    }
}

fn footer(o: &mut String, run: &ScanRun, p: &Painter, paths: &PathShortener) {
    let roots = run
        .report
        .scanned_roots
        .iter()
        .map(|r| paths.short(r))
        .collect::<Vec<_>>()
        .join(", ");
    let folders = run
        .projects
        .as_ref()
        .map_or(0, |p| p.worktrees.stats.dirs_visited);
    writeln!(o).ok();
    writeln!(
        o,
        "  {}",
        p.dim(format!(
            "Scanned {} under {} in {:.1}s. Nothing was changed.",
            plural(folders, "folder", "folders"),
            roots,
            run.total.as_secs_f64()
        ))
    )
    .ok();
    let sk = &run.report.skipped;
    let mut bits = Vec::new();
    if sk.permission_denied > 0 {
        bits.push(format!(
            "{} skipped: permission denied",
            plural(sk.permission_denied, "folder", "folders")
        ));
    }
    if sk.other_errors > 0 {
        bits.push(format!(
            "{} skipped: unreadable",
            plural(sk.other_errors, "folder", "folders")
        ));
    }
    if !bits.is_empty() {
        writeln!(
            o,
            "  {}",
            p.paint(bits.join(" · "), Style::new().dimmed().italic())
        )
        .ok();
    }
}
