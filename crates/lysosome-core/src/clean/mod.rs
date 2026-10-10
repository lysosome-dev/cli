//! Milestone 4: safe cleanup. Plans, re-checks and performs removals. Never prints.
//!
//! The flow is always: scan → [`plan`] → (dry run: stop) → [`Executor::execute`].
//!
//! * **Planning** ([`plan`]) is pure: it turns findings into [`PlanItem`]s.
//!   Only `REMOVABLE` findings that carry a cleanup are eligible, and never
//!   tier `review`. `REVIEW`, `ACTIVE` and `ORPHAN` findings are never planned
//!   (the one exception is [`Selection::strip_artifacts`], which plans the
//!   git-ignored artifact folders *inside* dormant worktrees, `REVIEW` ones
//!   included, and keeps the worktrees themselves).
//! * **Execution** ([`Executor::execute`]) re-checks every safety condition of
//!   an item immediately before acting on it, skips the item with a reason if
//!   anything changed, and records every outcome in the [`ActionLog`].
//!
//! How each action is carried out:
//! * worktrees: `git -C <main> worktree remove <path>` (never `--force`), then
//!   `git -C <main> worktree prune`. Never `rm`.
//! * command findings: the tool's own cleanup argv, no shell, with a timeout.
//! * everything else: moved to the Trash through a [`Trasher`]; with
//!   [`PlanOptions::permanent`], deleted without following symlinks.

mod execute;
mod log;
mod process;
mod recheck;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

pub use execute::{
    delete_permanently, DirTrash, Executor, ItemResult, Outcome, SystemTrash, Trasher,
    DEFAULT_COMMAND_TIMEOUT, INTERRUPTED,
};
pub use log::{default_log_path, ActionLog};
pub use process::kill_active_command;

use crate::artifacts;
use crate::model::{lossy_path, lossy_paths, Category, CleanupAction, Finding, Status, Tier};

/// Which group of findings a category belongs to (the `--worktrees`,
/// `--artifacts`, `--caches` flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Worktrees,
    Artifacts,
    Caches,
}

impl Scope {
    pub const ALL: [Scope; 3] = [Scope::Worktrees, Scope::Artifacts, Scope::Caches];

    pub fn of(c: Category) -> Scope {
        match c {
            Category::Worktrees => Scope::Worktrees,
            Category::Artifacts => Scope::Artifacts,
            _ => Scope::Caches,
        }
    }
}

/// What to clean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Selection {
    pub scopes: BTreeSet<Scope>,
    /// Tiers that may be cleaned. [`Tier::Review`] is ignored if present.
    pub tiers: BTreeSet<Tier>,
    /// Only items idle at least this many days (unknown age = excluded).
    pub older_than_days: Option<u64>,
    /// Only items whose id, kind, name or path matches one of these
    /// (exact text or a glob with `*` / `?`).
    pub only: Vec<String>,
    /// Instead of removing dormant worktrees, remove only the git-ignored
    /// artifact folders inside them (`REVIEW` worktrees included).
    pub strip_artifacts: bool,
}

impl Default for Selection {
    /// Everything, tier `regenerable` only.
    fn default() -> Self {
        Selection {
            scopes: Scope::ALL.into_iter().collect(),
            tiers: [Tier::Regenerable].into_iter().collect(),
            older_than_days: None,
            only: Vec::new(),
            strip_artifacts: false,
        }
    }
}

/// Planning options that change *how* items are removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanOptions {
    /// Delete instead of moving to the Trash (requires explicit consent).
    pub permanent: bool,
    /// "Now" for age filters.
    pub now: Option<SystemTime>,
    /// Keep items nested inside another item's target (normally dropped so
    /// each byte is acted on once). For pickers: the nesting is resolved by
    /// [`Plan::subset`] after the user chose, so unticking a big item never
    /// hides a smaller one inside it.
    pub keep_nested: bool,
}

/// What will be done to an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// `git worktree remove` + `git worktree prune`.
    WorktreeRemove,
    /// Run the tool's own cleanup command.
    Command,
    /// Move `paths` to the Trash.
    Trash,
    /// Delete `paths` permanently (`--permanent`).
    Delete,
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActionKind::WorktreeRemove => "worktree_remove",
            ActionKind::Command => "command",
            ActionKind::Trash => "trash",
            ActionKind::Delete => "delete",
        }
    }
}

/// The safety conditions an item must still meet right before acting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Guard {
    /// A linked worktree that must still be `REMOVABLE`, owned by `main_repo`.
    Worktree { main_repo: PathBuf, gitdir: PathBuf },
    /// An ignored artifact folder inside a dormant (REMOVABLE or REVIEW)
    /// worktree.
    WorktreeArtifact { worktree: PathBuf, gitdir: PathBuf },
    /// An ignored artifact folder of an idle main repository.
    RepoArtifact { project: PathBuf },
    /// Plain paths (system caches): each must still exist, not be a symlink,
    /// and sit in the same (canonical) parent folder as when planned.
    Paths { parents: Vec<Option<PathBuf>> },
    /// A tool command. `existing_path` is the finding's path when it was a
    /// real folder at planning time (it must still exist).
    Command { existing_path: Option<PathBuf> },
}

/// One thing `clean` will do.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanItem {
    /// The finding id (`<kind>:<path>`).
    pub id: String,
    pub category: Category,
    pub kind: String,
    /// Display name (`details.name`, else the folder name).
    pub name: String,
    #[serde(with = "lossy_path")]
    pub path: PathBuf,
    /// The project or worktree holding an artifact folder.
    #[serde(skip_serializing_if = "Option::is_none", with = "lossy_path_opt")]
    pub project: Option<PathBuf>,
    pub size_bytes: u64,
    /// The size is an upper bound (e.g. `pnpm store prune` frees only
    /// unreferenced packages): show it as "up to".
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub size_is_upper_bound: bool,
    pub tier: Tier,
    /// Status of the finding when planned (`REVIEW` only for artifacts
    /// stripped from a `REVIEW` worktree).
    pub status: Status,
    pub reason: String,
    pub action: ActionKind,
    /// argv for `command` and `worktree_remove` (the remove step).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Targets of `trash` / `delete`.
    #[serde(skip_serializing_if = "Vec::is_empty", with = "lossy_paths")]
    pub paths: Vec<PathBuf>,
    #[serde(skip)]
    pub(crate) guard: Guard,
}

/// Items and bytes left out of a plan for one reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Count {
    pub items: u64,
    pub bytes: u64,
}

impl Count {
    pub fn is_zero(&self) -> bool {
        self.items == 0
    }

    fn add(&mut self, bytes: u64) {
        self.items += 1;
        self.bytes += bytes;
    }
}

/// Dormant findings in the selected scopes that were left out, by reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Excluded {
    /// `REVIEW` findings (and tier `review`): never cleaned.
    pub review: Count,
    /// `ORPHAN` worktrees: never cleaned.
    pub orphan: Count,
    /// A tier that wasn't selected (e.g. `regenerable_expensive` by default).
    pub tier: Count,
    /// Younger than `--older-than`, or of unknown age.
    pub older_than: Count,
    /// Not matched by `--only`.
    pub only: Count,
    /// Dormant worktrees with no artifact folders to strip, or findings
    /// without any cleanup.
    pub nothing_to_do: Count,
    /// Planned, then left unticked in `clean --interactive`
    /// ([`Plan::subset`]). Omitted from JSON when zero.
    #[serde(skip_serializing_if = "Count::is_zero")]
    pub deselected: Count,
}

/// The output of [`plan`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plan {
    pub items: Vec<PlanItem>,
    pub total_bytes: u64,
    pub permanent: bool,
    pub excluded: Excluded,
}

/// Decide what to clean. Pure: reads only the findings.
pub fn plan(findings: &[Finding], sel: &Selection, opts: PlanOptions) -> Plan {
    let now = opts.now.unwrap_or_else(SystemTime::now);
    let mut items = Vec::new();
    let mut ex = Excluded::default();
    let trash_kind = if opts.permanent {
        ActionKind::Delete
    } else {
        ActionKind::Trash
    };

    for f in findings {
        let scope = Scope::of(f.category);
        if !sel.scopes.contains(&scope) || f.status == Status::Active {
            continue;
        }
        if f.category == Category::Worktrees && sel.strip_artifacts {
            plan_strip(f, sel, now, trash_kind, &mut items, &mut ex);
            continue;
        }
        match f.status {
            Status::Orphan => {
                ex.orphan.add(f.size_bytes);
                continue;
            }
            Status::Review => {
                ex.review.add(f.size_bytes);
                continue;
            }
            Status::Active => continue,
            Status::Removable => {}
        }
        if f.tier == Tier::Review {
            ex.review.add(f.size_bytes);
            continue;
        }
        let Some(cleanup) = &f.cleanup else {
            ex.nothing_to_do.add(f.size_bytes);
            continue;
        };
        if !sel.tiers.contains(&f.tier) || f.tier == Tier::Review {
            ex.tier.add(f.size_bytes);
            continue;
        }
        if !old_enough(f, sel.older_than_days, now) {
            ex.older_than.add(f.size_bytes);
            continue;
        }
        if !matches_only(&sel.only, &candidates_of(f)) {
            ex.only.add(f.size_bytes);
            continue;
        }
        match item_for(
            f,
            cleanup.action,
            &cleanup.command,
            &cleanup.paths,
            trash_kind,
        ) {
            Some(item) => items.push(item),
            None => ex.nothing_to_do.add(f.size_bytes),
        }
    }

    let mut items = if opts.keep_nested {
        items
    } else {
        drop_nested(items)
    };
    items.sort_by(|a, b| {
        cat_rank(a.category)
            .cmp(&cat_rank(b.category))
            .then(b.size_bytes.cmp(&a.size_bytes))
            .then(a.id.cmp(&b.id))
    });
    Plan {
        total_bytes: items.iter().map(|i| i.size_bytes).sum(),
        items,
        permanent: opts.permanent,
        excluded: ex,
    }
}

impl Plan {
    /// The plan narrowed to the items at `picked` (indices into
    /// [`Plan::items`], e.g. what a user ticked in `clean --interactive`).
    /// Order, guards and `permanent` are kept and nothing is added, so a
    /// selection can only ever narrow a plan; out-of-range indices are
    /// ignored. Picked items nested inside another picked item are dropped
    /// (each byte is acted on once; see [`PlanOptions::keep_nested`]).
    ///
    /// Items left out (and not inside a picked item, which takes them along)
    /// are counted as [`Excluded::tier`] when their tier is not in
    /// `default_tiers` (what the user's `--tier` covers, so the "costly to
    /// get back" hint survives), else as [`Excluded::deselected`].
    /// Execution still re-checks every item.
    pub fn subset(&self, picked: &BTreeSet<usize>, default_tiers: &BTreeSet<Tier>) -> Plan {
        let mut excluded = self.excluded;
        let mut items = Vec::new();
        let picked_targets: Vec<&PathBuf> = self
            .items
            .iter()
            .enumerate()
            .filter(|(n, _)| picked.contains(n))
            .flat_map(|(_, i)| targets(i))
            .collect();
        for (n, i) in self.items.iter().enumerate() {
            if picked.contains(&n) {
                items.push(i.clone());
                continue;
            }
            let swallowed = targets(i)
                .iter()
                .any(|t| picked_targets.iter().any(|p| t != p && t.starts_with(p)));
            if swallowed {
                continue;
            }
            if default_tiers.contains(&i.tier) {
                excluded.deselected.add(i.size_bytes);
            } else {
                excluded.tier.add(i.size_bytes);
            }
        }
        let items = drop_nested(items);
        Plan {
            total_bytes: items.iter().map(|i| i.size_bytes).sum(),
            items,
            permanent: self.permanent,
            excluded,
        }
    }
}

/// Dormant findings in the selected scopes that are never cleaned because
/// they need a human: status `REVIEW`, or tier `review`, narrowed by
/// `--older-than` and `--only` like planned items. Sorted like a plan
/// (category, then size, largest first). `clean --interactive` lists them,
/// unselectable, with their reason. With [`Selection::strip_artifacts`],
/// worktrees are left out: their artifact folders are planned instead.
pub fn held_for_review<'a>(
    findings: &'a [Finding],
    sel: &Selection,
    now: SystemTime,
) -> Vec<&'a Finding> {
    let mut out: Vec<&Finding> = findings
        .iter()
        .filter(|f| sel.scopes.contains(&Scope::of(f.category)))
        .filter(|f| !(sel.strip_artifacts && f.category == Category::Worktrees))
        .filter(|f| {
            f.status == Status::Review || (f.status == Status::Removable && f.tier == Tier::Review)
        })
        .filter(|f| old_enough(f, sel.older_than_days, now))
        .filter(|f| matches_only(&sel.only, &candidates_of(f)))
        .collect();
    out.sort_by(|a, b| {
        cat_rank(a.category)
            .cmp(&cat_rank(b.category))
            .then(b.size_bytes.cmp(&a.size_bytes))
            .then(a.id.cmp(&b.id))
    });
    out
}

fn cat_rank(c: Category) -> usize {
    Category::ALL
        .iter()
        .position(|x| *x == c)
        .unwrap_or(usize::MAX)
}

/// Build the plan item for a REMOVABLE finding.
fn item_for(
    f: &Finding,
    action: CleanupAction,
    argv: &[String],
    paths: &[PathBuf],
    trash_kind: ActionKind,
) -> Option<PlanItem> {
    let base = |action: ActionKind, guard: Guard| PlanItem {
        id: f.id.clone(),
        category: f.category,
        kind: f.kind.clone(),
        name: display_name(f),
        path: f.path.clone(),
        project: None,
        size_bytes: f.size_bytes,
        size_is_upper_bound: f
            .details
            .get("size_is_upper_bound")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        tier: f.tier,
        status: f.status,
        reason: f.reason.clone(),
        action,
        command: Vec::new(),
        paths: Vec::new(),
        guard,
    };
    match f.category {
        Category::Worktrees => {
            let main_repo = detail_path(f, "main_repo")?;
            let gitdir = detail_path(f, "gitdir")?;
            let mut item = base(
                ActionKind::WorktreeRemove,
                Guard::Worktree {
                    main_repo: main_repo.clone(),
                    gitdir,
                },
            );
            item.command = worktree_remove_argv(&main_repo, &f.path);
            Some(item)
        }
        Category::Artifacts => {
            // Only folders of an idle *main repository* are ever REMOVABLE.
            if f.details.get("project_kind").and_then(|v| v.as_str()) != Some("repo") {
                return None;
            }
            let project = detail_path(f, "project")?;
            if action != CleanupAction::Trash || paths != [f.path.clone()] {
                return None;
            }
            let mut item = base(
                trash_kind,
                Guard::RepoArtifact {
                    project: project.clone(),
                },
            );
            item.project = Some(project);
            item.paths = vec![f.path.clone()];
            Some(item)
        }
        _ => match action {
            CleanupAction::Command => {
                if argv.is_empty() || argv[0].is_empty() {
                    return None;
                }
                // Pseudo-paths (`docker://image/...`) are not filesystem
                // paths: never look them up on disk.
                let existing = (f.path.is_absolute()
                    && std::fs::symlink_metadata(&f.path).is_ok_and(|m| m.is_dir()))
                .then(|| f.path.clone());
                let mut item = base(
                    ActionKind::Command,
                    Guard::Command {
                        existing_path: existing,
                    },
                );
                item.command = argv.to_vec();
                Some(item)
            }
            CleanupAction::Trash => {
                // Only real absolute filesystem paths (never `scheme://` pseudo-paths).
                let bad = |p: &PathBuf| !p.is_absolute() || p.to_string_lossy().contains("://");
                if paths.is_empty() || paths.iter().any(bad) {
                    return None;
                }
                let parents = paths
                    .iter()
                    .map(|p| p.parent().and_then(|d| d.canonicalize().ok()))
                    .collect();
                let mut item = base(trash_kind, Guard::Paths { parents });
                item.paths = paths.to_vec();
                Some(item)
            }
        },
    }
}

/// `--strip-artifacts`: one item per ignored artifact folder in a dormant worktree.
fn plan_strip(
    f: &Finding,
    sel: &Selection,
    now: SystemTime,
    trash_kind: ActionKind,
    items: &mut Vec<PlanItem>,
    ex: &mut Excluded,
) {
    let artifacts: Vec<(PathBuf, u64)> = f
        .details
        .get("artifacts")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some((
                        PathBuf::from(x.get("path")?.as_str()?),
                        x.get("size_bytes")?.as_u64()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let artifact_bytes: u64 = artifacts.iter().map(|a| a.1).sum();
    if f.status == Status::Orphan {
        ex.orphan.add(artifact_bytes);
        return;
    }
    if !matches!(f.status, Status::Removable | Status::Review) {
        return;
    }
    let (Some(gitdir), true) = (detail_path(f, "gitdir"), !artifacts.is_empty()) else {
        ex.nothing_to_do.add(artifact_bytes);
        return;
    };
    for (path, size) in artifacts {
        if !path.starts_with(&f.path) || path == f.path {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tier = artifacts::tier_for(std::ffi::OsStr::new(&name));
        if tier == Tier::Review {
            ex.review.add(size);
            continue;
        }
        if !sel.tiers.contains(&tier) {
            ex.tier.add(size);
            continue;
        }
        if !old_enough(f, sel.older_than_days, now) {
            ex.older_than.add(size);
            continue;
        }
        let id = format!("{name}:{}", path.display());
        let path_s = path.to_string_lossy().into_owned();
        let mut cands = candidates_of(f);
        cands.extend([id.clone(), name.clone(), path_s]);
        if !matches_only(&sel.only, &cands) {
            ex.only.add(size);
            continue;
        }
        items.push(PlanItem {
            id,
            category: Category::Worktrees,
            kind: name.clone(),
            name,
            path: path.clone(),
            project: Some(f.path.clone()),
            size_bytes: size,
            size_is_upper_bound: false,
            tier,
            status: f.status,
            reason: "git-ignored artifact folder; the worktree is kept".into(),
            action: trash_kind,
            command: Vec::new(),
            paths: vec![path],
            guard: Guard::WorktreeArtifact {
                worktree: f.path.clone(),
                gitdir: gitdir.clone(),
            },
        });
    }
}

/// `git -C <main> worktree remove <path>` (no `--force`, ever).
pub fn worktree_remove_argv(main_repo: &Path, worktree: &Path) -> Vec<String> {
    vec![
        "git".into(),
        "-C".into(),
        main_repo.to_string_lossy().into_owned(),
        "worktree".into(),
        "remove".into(),
        worktree.to_string_lossy().into_owned(),
    ]
}

/// Drop items whose targets live inside another item's target (each byte is
/// acted on once).
fn drop_nested(items: Vec<PlanItem>) -> Vec<PlanItem> {
    let roots: Vec<PathBuf> = items
        .iter()
        .flat_map(|i| targets(i).into_iter().cloned())
        .collect();
    items
        .into_iter()
        .filter(|i| {
            let mine = targets(i);
            !mine.iter().any(|t| {
                roots
                    .iter()
                    .any(|r| r != *t && t.starts_with(r) && !mine.contains(&r))
            })
        })
        .collect()
}

/// Paths an item acts on (none for tool commands).
pub fn targets(i: &PlanItem) -> Vec<&PathBuf> {
    match i.action {
        ActionKind::WorktreeRemove => vec![&i.path],
        ActionKind::Trash | ActionKind::Delete => i.paths.iter().collect(),
        ActionKind::Command => Vec::new(),
    }
}

fn detail_path(f: &Finding, key: &str) -> Option<PathBuf> {
    f.details
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn display_name(f: &Finding) -> String {
    f.details
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| f.path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| f.kind.clone())
}

/// Strings `--only` patterns are matched against.
fn candidates_of(f: &Finding) -> Vec<String> {
    let mut v = vec![
        f.id.clone(),
        f.kind.clone(),
        f.path.to_string_lossy().into_owned(),
    ];
    if let Some(n) = f.details.get("name").and_then(|v| v.as_str()) {
        v.push(n.to_string());
    }
    if let Some(n) = f.path.file_name() {
        v.push(n.to_string_lossy().into_owned());
    }
    v
}

fn matches_only(patterns: &[String], candidates: &[String]) -> bool {
    patterns.is_empty()
        || patterns
            .iter()
            .any(|p| candidates.iter().any(|c| glob_match(p, c)))
}

/// Idle days of a finding: `details.idle_days` when present, else from `last_activity`.
pub fn idle_days(f: &Finding, now: SystemTime) -> Option<u64> {
    if let Some(d) = f.details.get("idle_days").and_then(|v| v.as_i64()) {
        return Some(d.max(0) as u64);
    }
    let t = f.last_activity?;
    Some(now.duration_since(t).map_or(0, |d| d.as_secs() / 86_400))
}

fn old_enough(f: &Finding, older_than: Option<u64>, now: SystemTime) -> bool {
    match older_than {
        None => true,
        Some(days) => idle_days(f, now).is_some_and(|d| d >= days),
    }
}

/// Shell-style glob: `*` matches any run of characters (including `/`), `?`
/// one character. Text without wildcards must match exactly.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Parse `90d`, `12w`, `6mo`, `1y` or a bare number of days.
pub fn parse_age_days(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().ok()?;
    let mult = match unit.trim() {
        "" | "d" | "day" | "days" => 1,
        "w" | "week" | "weeks" => 7,
        "mo" | "month" | "months" => 30,
        "y" | "year" | "years" => 365,
        _ => return None,
    };
    n.checked_mul(mult)
}

mod lossy_path_opt {
    use serde::Serializer;
    use std::path::PathBuf;

    pub fn serialize<S: Serializer>(p: &Option<PathBuf>, s: S) -> Result<S::Ok, S::Error> {
        match p {
            Some(p) => s.serialize_str(&p.to_string_lossy()),
            None => s.serialize_none(),
        }
    }
}

#[cfg(test)]
mod tests;
