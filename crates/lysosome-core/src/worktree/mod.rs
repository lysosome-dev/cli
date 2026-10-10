//! Milestone 1: find git worktrees, analyze them, classify them.
//!
//! Behavioral source of truth: `reference/wtscan.py`.
//!
//! ```no_run
//! use lysosome_core::{progress::NoProgress, worktree};
//! let opts = worktree::ScanOptions::new(vec!["/Users/me/code".into()]);
//! let scan = worktree::scan(&opts, &NoProgress).unwrap();
//! for wt in scan.dormant() {
//!     println!("{:?} {} {}", wt.status, wt.size_bytes, wt.path.display());
//! }
//! ```
//!
//! Pipeline (each phase timed in [`ScanStats::timings`]):
//! 1. **walk** — parallel discovery + `.git/worktrees/*` records.
//! 2. **git** — read gitdir timestamps for *all* worktrees, then run git
//!    analysis on a bounded pool and classify.
//! 3. **size** — concurrently with git: one parallel pass per worktree
//!    measuring total size and candidate artifact folders.
//! 4. **ignore-check** — keep only artifact folders git ignores.
//!
//! With [`ScanOptions::github`], phase 2 ends by asking GitHub (through
//! `gh`) about worktrees that are `REVIEW` only because of commit safety;
//! see [`gh`].

pub(crate) mod analyze;
pub mod classify;
pub(crate) mod discover;
pub mod gh;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use rayon::prelude::*;
use serde_json::json;

use crate::artifacts::{self, ARTIFACT_SEARCH_DEPTH};
use crate::error::{Error, Result};
use crate::git::{Git, DEFAULT_TIMEOUT};
use crate::model::{Category, Cleanup, Finding, Report, Skipped, StaleMetadata, Status, Tier};
use crate::progress::{Phase, Progress};
use crate::size::{Sizer, Split};
use crate::walk::WalkStats;

pub use classify::{classify, commit_safety_only, Facts, ORPHAN_REASON};
pub use gh::{Gh, MergedPr};

/// Default idle days before a worktree counts as dormant.
pub const DEFAULT_DORMANT_DAYS: u64 = 30;
/// Default walk depth.
pub const DEFAULT_MAX_DEPTH: usize = 8;

/// Inputs to [`scan`].
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Folders to scan. Must be non-empty; the CLI supplies `~` by default.
    pub roots: Vec<PathBuf>,
    pub max_depth: usize,
    pub dormant_after_days: u64,
    /// "Now" for idle-day computation (injectable for tests).
    pub now: SystemTime,
    pub git_timeout: Duration,
    /// Max concurrent git processes; `None` = `min(8, cores)`.
    pub git_concurrency: Option<usize>,
    /// Ask GitHub (via `gh`, when installed and signed in) whether
    /// commit-safety `REVIEW` worktrees were squash-merged. Off by default
    /// in the library (no network unless asked); the CLI turns it on unless
    /// `--no-gh` / `--offline`.
    pub github: bool,
}

impl ScanOptions {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        ScanOptions {
            roots,
            max_depth: DEFAULT_MAX_DEPTH,
            dormant_after_days: DEFAULT_DORMANT_DAYS,
            now: SystemTime::now(),
            git_timeout: DEFAULT_TIMEOUT,
            git_concurrency: None,
            github: false,
        }
    }
}

/// A git-ignored artifact folder inside a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub path: PathBuf,
    pub size_bytes: u64,
}

/// One analyzed worktree.
#[derive(Debug, Clone, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    /// The worktree's private git dir (`<main>/.git/worktrees/<name>`).
    pub gitdir: PathBuf,
    pub status: Status,
    pub reason: String,
    /// Whole days idle; `None` for orphans or when no timestamp was found.
    pub idle_days: Option<i64>,
    pub last_activity: Option<SystemTime>,
    /// Branch name, `"(detached)"`, or `None` for orphans.
    pub branch: Option<String>,
    pub changed: u64,
    pub untracked: u64,
    pub merged_into: Option<String>,
    pub unpushed: Option<u64>,
    pub main_repo: Option<PathBuf>,
    pub locked: bool,
    /// Allocated bytes of the whole worktree folder (artifacts included).
    pub size_bytes: u64,
    /// Sum of `artifacts[].size_bytes`.
    pub artifact_bytes: u64,
    pub artifacts: Vec<Artifact>,
    /// The merged PR whose head is this worktree's HEAD, when GitHub made it
    /// `REMOVABLE` (see [`gh`]).
    pub merged_pr: Option<MergedPr>,
    /// `REVIEW` only because of commit safety (a PR lookup candidate).
    pub(crate) squash_candidate: bool,
}

impl Worktree {
    /// Safety tier: a removable worktree is recreatable from its (safe) commits.
    pub fn tier(&self) -> Tier {
        match self.status {
            Status::Removable => Tier::Regenerable,
            _ => Tier::Review,
        }
    }

    /// `git -C <main> worktree remove <path>`, only for `REMOVABLE` worktrees.
    pub fn cleanup_command(&self) -> Option<Vec<String>> {
        if self.status != Status::Removable {
            return None;
        }
        let repo = self.main_repo.as_deref().unwrap_or(&self.path);
        Some(vec![
            "git".into(),
            "-C".into(),
            repo.to_string_lossy().into_owned(),
            "worktree".into(),
            "remove".into(),
            self.path.to_string_lossy().into_owned(),
        ])
    }

    /// Convert to the contract's [`Finding`].
    pub fn to_finding(&self) -> Finding {
        Finding {
            id: format!("worktree:{}", self.path.display()),
            category: Category::Worktrees,
            kind: "worktree".into(),
            path: self.path.clone(),
            size_bytes: self.size_bytes,
            tier: self.tier(),
            status: self.status,
            reason: self.reason.clone(),
            last_activity: self.last_activity,
            details: json!({
                "branch": self.branch,
                "idle_days": self.idle_days,
                "changed": self.changed,
                "untracked": self.untracked,
                "unpushed": self.unpushed,
                "merged_into": self.merged_into,
                "locked": self.locked,
                "main_repo": self.main_repo.as_deref().map(lossy),
                "gitdir": lossy(&self.gitdir),
                "artifact_bytes": self.artifact_bytes,
                "merged_pr": self.merged_pr,
                "artifacts": self.artifacts.iter().map(|a| json!({
                    "path": lossy(&a.path),
                    "size_bytes": a.size_bytes,
                })).collect::<Vec<_>>(),
            }),
            cleanup: self.cleanup_command().map(Cleanup::command),
        }
    }
}

/// Path as a (lossy) UTF-8 string for JSON; never panics on odd bytes.
fn lossy(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Time spent in one phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseTiming {
    pub phase: Phase,
    pub elapsed: Duration,
}

/// Counters and timings from a scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub timings: Vec<PhaseTiming>,
    pub dirs_visited: u64,
    pub permission_denied: u64,
    pub other_errors: u64,
    pub git_calls: u64,
    /// Entries that couldn't be read while measuring sizes.
    pub size_errors: u64,
    /// `gh` processes run (squash-merge lookups).
    pub gh_calls: u64,
    pub total: Duration,
}

/// Everything [`scan`] found.
#[derive(Debug, Clone)]
pub struct WorktreeScan {
    /// Canonical roots that were actually scanned.
    pub roots: Vec<PathBuf>,
    /// Requested roots that don't exist or aren't folders.
    pub missing_roots: Vec<PathBuf>,
    pub dormant_after_days: u64,
    /// Sorted by path.
    pub worktrees: Vec<Worktree>,
    pub stale: Vec<StaleMetadata>,
    pub stats: ScanStats,
}

impl WorktreeScan {
    /// Every worktree that isn't `ACTIVE` (includes `ORPHAN`).
    pub fn dormant(&self) -> impl Iterator<Item = &Worktree> {
        self.worktrees.iter().filter(|w| w.status.is_dormant())
    }

    pub fn removable(&self) -> impl Iterator<Item = &Worktree> {
        self.worktrees
            .iter()
            .filter(|w| w.status == Status::Removable)
    }

    pub fn dormant_bytes(&self) -> u64 {
        self.dormant().map(|w| w.size_bytes).sum()
    }

    pub fn removable_bytes(&self) -> u64 {
        self.removable().map(|w| w.size_bytes).sum()
    }

    /// Artifact bytes inside dormant worktrees.
    pub fn dormant_artifact_bytes(&self) -> u64 {
        self.dormant().map(|w| w.artifact_bytes).sum()
    }

    /// Build the versioned JSON [`Report`].
    pub fn to_report(&self) -> Report {
        Report::new(
            self.roots.clone(),
            self.dormant_after_days,
            self.worktrees.iter().map(Worktree::to_finding).collect(),
            self.stale.clone(),
            Skipped {
                permission_denied: self.stats.permission_denied,
                other_errors: self.stats.other_errors,
                missing_roots: self.missing_roots.clone(),
            },
        )
    }
}

/// Canonicalize roots; split into usable folders and missing ones. Roots nested
/// inside another root are walked once (the outer walk covers them).
pub(crate) fn resolve_roots(roots: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let mut ok = Vec::new();
    let mut missing = Vec::new();
    for r in roots {
        match r.canonicalize() {
            Ok(c) if c.is_dir() => {
                if !ok.contains(&c) {
                    ok.push(c)
                }
            }
            _ => missing.push(r.clone()),
        }
    }
    let walk_roots = ok
        .iter()
        .filter(|r| !ok.iter().any(|o| o != *r && r.starts_with(o)))
        .cloned()
        .collect();
    (ok, walk_roots, missing)
}

pub(crate) fn timed<T>(timings: &mut Vec<PhaseTiming>, phase: Phase, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let out = f();
    timings.push(PhaseTiming {
        phase,
        elapsed: t.elapsed(),
    });
    out
}

/// Scan for worktrees. Strictly read-only.
pub fn scan(opts: &ScanOptions, progress: &dyn Progress) -> Result<WorktreeScan> {
    let started = Instant::now();
    let (roots, walk_roots, missing_roots) = resolve_roots(&opts.roots);
    if roots.is_empty() {
        return Err(Error::NoValidRoots(missing_roots));
    }
    let git = Git::new(opts.git_timeout, opts.git_concurrency)?;
    let mut timings = Vec::new();

    // 1. walk
    progress.start(Phase::Walk, None);
    let found = timed(&mut timings, Phase::Walk, || {
        discover::discover(&walk_roots, opts.max_depth, progress)
    });
    progress.finish(Phase::Walk);
    Ok(analyze_found(
        &git,
        opts,
        Found {
            roots,
            missing_roots,
            candidates: found.candidates,
            stale: found.stale,
            walk: found.walk,
            pre: None,
        },
        timings,
        started,
        progress,
    ))
}

/// Discovery output the analysis phases need.
pub(crate) struct Found {
    pub roots: Vec<PathBuf>,
    pub missing_roots: Vec<PathBuf>,
    pub candidates: Vec<discover::Candidate>,
    pub stale: Vec<StaleMetadata>,
    pub walk: WalkStats,
    /// Gitdir timestamps already read (one per candidate, same order), when
    /// the caller had to read them before starting other git work.
    pub pre: Option<Vec<analyze::PreGit>>,
}

/// Phases 2–4 (git ∥ size, then ignore-check) over already-discovered worktrees.
pub(crate) fn analyze_found(
    git: &Git,
    opts: &ScanOptions,
    found: Found,
    mut timings: Vec<PhaseTiming>,
    started: Instant,
    progress: &dyn Progress,
) -> WorktreeScan {
    let Found {
        roots,
        missing_roots,
        candidates,
        stale,
        walk,
        pre,
    } = found;

    // 2+3. git analysis and size measurement are independent, so they run
    // concurrently: git on its bounded pool, sizing on the global pool.
    // Timestamps for *every* worktree are read before any git process starts.
    let pre: Vec<_> =
        pre.unwrap_or_else(|| candidates.par_iter().map(analyze::read_pre_git).collect());
    progress.start(Phase::Analyze, Some(2 * candidates.len() as u64));
    let (git_out, size_out) = std::thread::scope(|s| {
        let git_job = s.spawn(|| {
            let t = Instant::now();
            let cache = analyze::RepoCache::default();
            let mut wts: Vec<Worktree> = git.pool().install(|| {
                candidates
                    .par_iter()
                    .zip(pre.par_iter())
                    .map(|(c, pre)| {
                        let wt = build_worktree(git, &cache, c, pre, opts);
                        progress.advance(1);
                        wt
                    })
                    .collect()
            });
            let git_elapsed = t.elapsed();
            let t = Instant::now();
            let gh_calls = if opts.github {
                apply_github(git, &mut wts, progress)
            } else {
                0
            };
            (wts, git_elapsed, (t.elapsed(), gh_calls))
        });
        let t = Instant::now();
        let measured: Vec<Measured> = candidates
            .par_iter()
            .map(|c| {
                let out = measure(&c.path);
                progress.advance(1);
                out
            })
            .collect();
        let size_elapsed = t.elapsed();
        let git_out = git_job
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e));
        (git_out, (measured, size_elapsed))
    });
    progress.finish(Phase::Analyze);
    let (mut worktrees, git_elapsed, (gh_elapsed, gh_calls)) = git_out;
    let (measured, size_elapsed) = size_out;
    timings.push(PhaseTiming {
        phase: Phase::Git,
        elapsed: git_elapsed,
    });
    if gh_calls > 0 {
        timings.push(PhaseTiming {
            phase: Phase::Github,
            elapsed: gh_elapsed,
        });
    }
    timings.push(PhaseTiming {
        phase: Phase::Size,
        elapsed: size_elapsed,
    });
    let mut size_errors = 0;
    let mut parts_per_wt = Vec::with_capacity(worktrees.len());
    for (w, m) in worktrees.iter_mut().zip(measured) {
        w.size_bytes = m.total;
        size_errors += m.errors;
        parts_per_wt.push(m.parts);
    }

    // 4. ignore-check
    let with_parts = parts_per_wt.iter().filter(|p| !p.is_empty()).count();
    progress.start(Phase::IgnoreCheck, Some(with_parts as u64));
    timed(&mut timings, Phase::IgnoreCheck, || {
        git.pool().install(|| {
            worktrees
                .par_iter_mut()
                .zip(parts_per_wt.par_iter())
                .filter(|(_, parts)| !parts.is_empty())
                .for_each(|(w, parts)| {
                    if w.status != Status::Orphan {
                        let paths: Vec<PathBuf> = parts.iter().map(|(p, _)| p.clone()).collect();
                        let ignored = artifacts::ignored_subset(git, &w.path, &paths);
                        w.artifacts = parts
                            .iter()
                            .filter(|(p, _)| ignored.contains(p))
                            .map(|(p, s)| Artifact {
                                path: p.clone(),
                                size_bytes: *s,
                            })
                            .collect();
                        w.artifact_bytes = w.artifacts.iter().map(|a| a.size_bytes).sum();
                    }
                    progress.advance(1);
                });
        })
    });
    progress.finish(Phase::IgnoreCheck);

    WorktreeScan {
        roots,
        missing_roots,
        dormant_after_days: opts.dormant_after_days,
        worktrees,
        stale,
        stats: ScanStats {
            timings,
            dirs_visited: walk.dirs_visited,
            permission_denied: walk.permission_denied,
            other_errors: walk.other_errors,
            git_calls: git.calls(),
            gh_calls,
            size_errors,
            total: started.elapsed(),
        },
    }
}

fn build_worktree(
    git: &Git,
    cache: &analyze::RepoCache,
    c: &discover::Candidate,
    pre: &analyze::PreGit,
    opts: &ScanOptions,
) -> Worktree {
    let mut wt = Worktree {
        path: c.path.clone(),
        gitdir: c.gitdir.clone(),
        status: Status::Orphan,
        reason: ORPHAN_REASON.into(),
        idle_days: None,
        last_activity: None,
        branch: None,
        changed: 0,
        untracked: 0,
        merged_into: None,
        unpushed: None,
        main_repo: None,
        locked: false,
        size_bytes: 0,
        artifact_bytes: 0,
        artifacts: Vec::new(),
        merged_pr: None,
        squash_candidate: false,
    };
    if !pre.gitdir_exists {
        return wt;
    }
    let a = analyze::analyze(git, cache, c, pre, opts.now);
    let (status, reason) = classify(&a.facts, opts.dormant_after_days);
    wt.squash_candidate = commit_safety_only(&a.facts, opts.dormant_after_days)
        && a.branch.as_deref().is_some_and(|b| !b.starts_with('('));
    wt.status = status;
    wt.reason = reason;
    wt.idle_days = a.facts.idle_days;
    wt.last_activity = a.last_activity;
    wt.branch = a.branch;
    wt.changed = a.facts.changed;
    wt.untracked = a.facts.untracked;
    wt.merged_into = a.facts.merged_into;
    wt.unpushed = a.facts.unpushed;
    wt.main_repo = pre.main_repo.clone();
    wt.locked = pre.locked;
    wt
}

/// Ask GitHub about every squash-merge candidate (when `gh` is installed and
/// signed in) and apply the verdicts. Returns the number of `gh` calls.
fn apply_github(git: &Git, wts: &mut [Worktree], progress: &dyn Progress) -> u64 {
    let idx: Vec<usize> = (0..wts.len())
        .filter(|i| wts[*i].squash_candidate)
        .collect();
    if idx.is_empty() {
        return 0;
    }
    let Some(gh) = Gh::from_process() else {
        return 0;
    };
    let jobs: Vec<(&Path, &str)> = idx
        .iter()
        .map(|i| {
            let w = &wts[*i];
            (w.path.as_path(), w.branch.as_deref().unwrap_or_default())
        })
        .collect();
    progress.start(Phase::Github, Some(jobs.len() as u64));
    let verdicts = gh.verdicts(git, &jobs, progress);
    progress.finish(Phase::Github);
    for (i, v) in idx.into_iter().zip(verdicts) {
        apply_verdict(&mut wts[i], v);
    }
    gh.calls()
}

/// Reclassify one squash-merge candidate from GitHub's answer.
pub(crate) fn apply_verdict(w: &mut Worktree, v: gh::Verdict) {
    if !w.squash_candidate || w.status != Status::Review {
        return;
    }
    let Some(reason) = v.reason() else {
        return;
    };
    if let gh::Verdict::SquashMerged(pr) = v {
        w.status = Status::Removable;
        w.merged_pr = Some(pr);
    }
    w.reason = reason;
}

/// Total size plus candidate artifact folders (not yet ignore-checked).
/// Candidates are folders named in `ARTIFACT_DIRS` whose parent is at most
/// `ARTIFACT_SEARCH_DEPTH` below the worktree, not inside `.git` or another
/// artifact folder — the same search as the prototype's `find_artifacts`.
fn measure(path: &Path) -> Measured {
    let Ok(sizer) = Sizer::new(path) else {
        return Measured {
            total: 0,
            parts: Vec::new(),
            errors: 1,
        };
    };
    let out = sizer.measure_split(path, &|_, name, depth| {
        if name == ".git" || depth > ARTIFACT_SEARCH_DEPTH + 1 {
            Split::InlineNoSplit
        } else if artifacts::is_artifact_dir(name) {
            Split::Separate
        } else if depth == ARTIFACT_SEARCH_DEPTH + 1 {
            Split::InlineNoSplit
        } else {
            Split::Inline
        }
    });
    Measured {
        total: out.total,
        parts: out.parts,
        errors: sizer.errors(),
    }
}

/// Output of [`measure`].
struct Measured {
    total: u64,
    /// Candidate artifact folders and their sizes.
    parts: Vec<(PathBuf, u64)>,
    errors: u64,
}
