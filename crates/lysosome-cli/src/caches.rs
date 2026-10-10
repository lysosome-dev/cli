//! `lysosome caches` (alias `system`): package caches, AI models, Docker,
//! Xcode, Android and editor data, grouped by category.

use std::fmt::Write as _;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use clap::Args;
use lysosome_core::detector::ScanContext;
use lysosome_core::model::{Category, CleanupAction, Finding, Report, Skipped, Status, Tier};
use lysosome_core::system::{self, RunOutput};
use owo_colors::Style;

use crate::progress::Spinner;
use crate::ui::{self, human_bytes, pad_left, pad_right, plural, Painter, PathShortener};
use crate::GlobalArgs;

const NAME_MAX: usize = 40;
const WHERE_MAX: usize = 64;

/// Categories the system detectors report on.
const SYSTEM_CATEGORIES: [Category; 6] = [
    Category::AiModels,
    Category::Containers,
    Category::Xcode,
    Category::PackageCaches,
    Category::Android,
    Category::Editors,
];

#[derive(Debug, Clone, Args)]
pub struct CachesArgs {
    /// Only these categories (repeatable): ai_models, containers, xcode,
    /// package_caches, android, editors
    #[arg(long = "category", value_name = "C", value_parser = parse_category)]
    pub categories: Vec<Category>,

    /// Rows to show per category
    #[arg(long, value_name = "N", default_value_t = 10)]
    pub top: usize,

    /// Print the versioned JSON report instead of a table
    #[arg(long)]
    pub json: bool,
}

fn parse_category(s: &str) -> Result<Category, String> {
    let norm = s.trim().to_ascii_lowercase().replace('-', "_");
    let c: Category = serde_json::from_value(serde_json::Value::String(norm))
        .map_err(|_| format!("unknown category `{s}`"))?;
    if SYSTEM_CATEGORIES.contains(&c) {
        Ok(c)
    } else {
        Err(format!(
            "`{s}` is not a cache category; use one of: ai_models, containers, xcode, package_caches, android, editors"
        ))
    }
}

pub fn run(args: &CachesArgs, global: &GlobalArgs) -> Result<String> {
    let home = dirs::home_dir().ok_or(lysosome_core::Error::NoHomeDir)?;
    let shortener = PathShortener::new(Some(home.clone()));

    let detectors: Vec<_> = system::detectors()
        .into_iter()
        .filter(|d| args.categories.is_empty() || args.categories.contains(&d.category()))
        .collect();
    let ctx = ScanContext::new(Vec::new(), home);

    let start = Instant::now();
    let spinner = ui::stderr_is_tty().then(|| {
        let s = Spinner::new(String::new());
        s.set_message("Checking caches, AI models, Docker, Xcode and editors");
        s
    });
    let out = system::run(&detectors, &ctx);
    if let Some(s) = spinner {
        s.clear();
    }
    let elapsed = start.elapsed();

    if global.profile {
        let warn = Painter::new(ui::color_enabled(global.no_color, ui::stderr_is_tty()));
        eprint!("{}", render_profile(&out, elapsed, &warn));
    }

    let RunOutput { findings, .. } = out;
    if args.json {
        let report = Report::new(
            Vec::new(),
            ctx.dormant_after_days,
            findings,
            Vec::new(),
            Skipped::default(),
        );
        let mut s = serde_json::to_string_pretty(&report)?;
        s.push('\n');
        return Ok(s);
    }
    let p = Painter::new(ui::color_enabled(global.no_color, ui::stdout_is_tty()));
    Ok(render_table(
        &findings,
        args,
        detectors.len(),
        elapsed,
        &p,
        &shortener,
    ))
}

fn render_profile(out: &RunOutput, total: Duration, p: &Painter) -> String {
    let mut s = String::new();
    let mut t = out.timings.clone();
    t.sort_by_key(|a| std::cmp::Reverse(a.1));
    writeln!(
        s,
        "  {}",
        p.dim(format!("profile: {:.2}s wall (detectors overlap; times include waiting for shared sizing threads)", total.as_secs_f64()))
    )
    .ok();
    for (id, d) in t {
        writeln!(s, "  {}", p.dim(format!("{:>8.3}s  {id}", d.as_secs_f64()))).ok();
    }
    s
}

fn tier_badge(t: Tier, p: &Painter) -> String {
    let label = match t {
        Tier::Regenerable => "cache",
        Tier::RegenerableExpensive => "costly",
        Tier::Review => "review",
    };
    p.paint(pad_right(label, 6), crate::ui::tier_style(t))
}

/// "today", "3d ago", "5mo ago", "2y ago", or "–".
pub fn ago(t: Option<SystemTime>, now: SystemTime) -> String {
    let Some(t) = t else {
        return "–".into();
    };
    let days = now
        .duration_since(t)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    match days {
        0 => "today".into(),
        1..=59 => format!("{days}d ago"),
        60..=729 => format!("{}mo ago", days / 30),
        _ => format!("{}y ago", days / 365),
    }
}

fn name_of(f: &Finding) -> String {
    f.details["name"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| f.kind.clone())
}

/// What the WHERE column shows: the cleanup command, else the path.
fn where_of(f: &Finding, paths: &PathShortener) -> String {
    match &f.cleanup {
        Some(c) if c.action == CleanupAction::Command => c
            .command
            .iter()
            .map(|a| {
                if a.contains(' ') || a.contains('"') {
                    ui::shell_quote(a)
                } else {
                    paths.short(std::path::Path::new(a))
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => paths.short(&f.path),
    }
}

fn render_table(
    findings: &[Finding],
    args: &CachesArgs,
    detectors: usize,
    elapsed: Duration,
    p: &Painter,
    paths: &PathShortener,
) -> String {
    let mut o = String::new();
    writeln!(o).ok();
    let now = SystemTime::now();
    let total: u64 = findings.iter().map(|f| f.size_bytes).sum();

    if findings.is_empty() {
        writeln!(
            o,
            "  {} {}",
            p.paint("✓", Style::new().green().bold()),
            p.bold("No caches, AI models or tool data found.")
        )
        .ok();
    } else {
        writeln!(
            o,
            "  {} {}  {}",
            p.bold("Caches, models and tool data are holding"),
            p.paint(human_bytes(total), Style::new().bold().underline()),
            p.dim(format!(
                "in {}",
                plural(findings.len() as u64, "item", "items")
            )),
        )
        .ok();
        writeln!(o).ok();
        let by_tier = |t: Tier| -> u64 {
            findings
                .iter()
                .filter(|f| f.tier == t)
                .map(|f| f.size_bytes)
                .sum()
        };
        let rows = [
            (
                by_tier(Tier::Regenerable),
                crate::ui::tier_style(Tier::Regenerable).bold(),
                "cheap to rebuild",
                "caches and build output, refilled on demand",
            ),
            (
                by_tier(Tier::RegenerableExpensive),
                crate::ui::tier_style(Tier::RegenerableExpensive).bold(),
                "costly to get back",
                "models, images, simulators: re-download to use again",
            ),
            (
                by_tier(Tier::Review),
                crate::ui::tier_style(Tier::Review).bold(),
                "needs review",
                "archives, volumes, shared data",
            ),
        ];
        let w = rows
            .iter()
            .map(|r| human_bytes(r.0).len())
            .max()
            .unwrap_or(0);
        for (bytes, style, label, note) in rows {
            writeln!(
                o,
                "    {}  {}  {}",
                p.paint(pad_left(&human_bytes(bytes), w), style),
                pad_right(label, 20),
                p.dim(note)
            )
            .ok();
        }
    }

    let mut cats: Vec<(Category, Vec<&Finding>)> = SYSTEM_CATEGORIES
        .iter()
        .map(|c| (*c, findings.iter().filter(|f| f.category == *c).collect()))
        .filter(|(_, v): &(Category, Vec<&Finding>)| !v.is_empty())
        .collect();
    cats.sort_by_key(|(_, v)| std::cmp::Reverse(v.iter().map(|f| f.size_bytes).sum::<u64>()));

    for (cat, mut rows) in cats {
        rows.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes).then(a.id.cmp(&b.id)));
        let cat_total: u64 = rows.iter().map(|f| f.size_bytes).sum();
        writeln!(o).ok();
        writeln!(
            o,
            "  {}  {}",
            p.bold(cat.display_name().to_uppercase()),
            p.dim(human_bytes(cat_total))
        )
        .ok();
        let shown = rows.len().min(args.top);
        let cells: Vec<[String; 4]> = rows[..shown]
            .iter()
            .map(|f| {
                [
                    human_bytes(f.size_bytes),
                    ago(f.last_activity, now),
                    ui::truncate(&name_of(f), NAME_MAX),
                    ui::truncate(&where_of(f, paths), WHERE_MAX),
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
        let (ws, wl, wn) = (width(0, "SIZE"), width(1, "LAST USED"), width(2, "NAME"));
        writeln!(
            o,
            "    {}",
            p.dim(format!(
                "{}  {}  {}  {}  CLEANUP / LOCATION",
                pad_right("TIER", 6),
                pad_left("SIZE", ws),
                pad_left("LAST USED", wl),
                pad_right("NAME", wn),
            ))
        )
        .ok();
        for (f, c) in rows[..shown].iter().zip(&cells) {
            writeln!(
                o,
                "    {}  {}  {}  {}  {}",
                tier_badge(f.tier, p),
                pad_left(&c[0], ws),
                pad_left(&c[1], wl),
                pad_right(&c[2], wn),
                p.dim(&c[3]),
            )
            .ok();
            if f.status == Status::Review && !f.reason.is_empty() {
                writeln!(o, "           {}", p.dim(format!("└ {}", f.reason))).ok();
            }
        }
        if rows.len() > shown {
            let rest: u64 = rows[shown..].iter().map(|f| f.size_bytes).sum();
            writeln!(
                o,
                "    {}",
                p.dim(format!(
                    "… and {} more ({}); use --top or --json",
                    rows.len() - shown,
                    human_bytes(rest)
                ))
            )
            .ok();
        }
    }

    writeln!(o).ok();
    writeln!(
        o,
        "  {}",
        p.dim(format!(
            "Ran {} in {:.1}s. Nothing was changed.",
            plural(detectors as u64, "detector", "detectors"),
            elapsed.as_secs_f64()
        ))
    )
    .ok();
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_categories() {
        assert_eq!(parse_category("ai_models"), Ok(Category::AiModels));
        assert_eq!(
            parse_category("package-caches"),
            Ok(Category::PackageCaches)
        );
        assert!(parse_category("worktrees").is_err());
        assert!(parse_category("nope").is_err());
    }

    #[test]
    fn formats_last_used() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 * 86_400);
        let d = |n: u64| Some(now - Duration::from_secs(n * 86_400));
        assert_eq!(ago(None, now), "–");
        assert_eq!(ago(d(0), now), "today");
        assert_eq!(ago(d(3), now), "3d ago");
        assert_eq!(ago(d(150), now), "5mo ago");
        assert_eq!(ago(d(800), now), "2y ago");
    }
}
