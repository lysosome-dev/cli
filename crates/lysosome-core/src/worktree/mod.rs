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

mod analyze;
pub mod classify;
mod discover;

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

pub use classify::{classify, Facts, ORPHAN_REASON};

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
fn resolve_roots(roots: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
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

fn timed<T>(timings: &mut Vec<PhaseTiming>, phase: Phase, f: impl FnOnce() -> T) -> T {
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
    let candidates = found.candidates;

    // 2+3. git analysis and size measurement are independent, so they run
    // concurrently: git on its bounded pool, sizing on the global pool.
    // Timestamps for *every* worktree are read before any git process starts.
    let pre: Vec<_> = candidates.par_iter().map(analyze::read_pre_git).collect();
    progress.start(Phase::Analyze, Some(2 * candidates.len() as u64));
    let (git_out, size_out) = std::thread::scope(|s| {
        let git_job = s.spawn(|| {
            let t = Instant::now();
            let cache = analyze::RepoCache::default();
            let wts: Vec<Worktree> = git.pool().install(|| {
                candidates
                    .par_iter()
                    .zip(pre.par_iter())
                    .map(|(c, pre)| {
                        let wt = build_worktree(&git, &cache, c, pre, opts);
                        progress.advance(1);
                        wt
                    })
                    .collect()
            });
            (wts, t.elapsed())
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
    let (mut worktrees, git_elapsed) = git_out;
    let (measured, size_elapsed) = size_out;
    timings.push(PhaseTiming {
        phase: Phase::Git,
        elapsed: git_elapsed,
    });
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
                        let ignored = artifacts::ignored_subset(&git, &w.path, &paths);
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

    Ok(WorktreeScan {
        roots,
        missing_roots,
        dormant_after_days: opts.dormant_after_days,
        worktrees,
        stale: found.stale,
        stats: ScanStats {
            timings,
            dirs_visited: found.walk.dirs_visited,
            permission_denied: found.walk.permission_denied,
            other_errors: found.walk.other_errors,
            git_calls: git.calls(),
            size_errors,
            total: started.elapsed(),
        },
    })
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
    };
    if !pre.gitdir_exists {
        return wt;
    }
    let a = analyze::analyze(git, cache, c, pre, opts.now);
    let (status, reason) = classify(&a.facts, opts.dormant_after_days);
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
