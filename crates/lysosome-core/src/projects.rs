//! Milestone 2: projects and the git-ignored build/dependency folders they hold.
//!
//! [`scan`] does one walk of the roots and feeds it to two pipelines that run
//! concurrently:
//! * the Milestone 1 worktree pipeline ([`crate::worktree`]), and
//! * the main-repo artifact pipeline: attribute every artifact-named folder to
//!   its innermost repository, keep only folders `git check-ignore` confirms
//!   (safety rule 4), date the project, and measure the folders.
//!
//! # Staleness
//! A project's last activity is the newest of its `.git/index`, `.git/HEAD`,
//! `.git/logs/HEAD` mtimes (read before any git process runs, safety rule 2)
//! and its last commit (`git log -1 --format=%ct`). That is the spec's "last
//! commit date", made more conservative: checking out a branch or staging work
//! in an old repo also counts as activity. It is the same rule worktrees use.
//!
//! # No double counting
//! Every byte is reported by at most one dormant finding:
//! * Artifact folders are attributed to the *innermost* repository (main repo,
//!   linked worktree, submodule...). Folders whose innermost repository is a
//!   linked worktree are never re-reported from the walk: they come from the
//!   worktree pipeline instead.
//! * Artifacts inside a **dormant** worktree (including those of a repository
//!   nested in it) are only part of that worktree's finding (`size_bytes`,
//!   `details.artifacts`). Artifacts inside an
//!   **active** worktree are emitted as `ACTIVE` artifact findings, which never
//!   count towards summary totals.
//! * An artifact folder nested inside another ignored artifact folder is
//!   folded into the outer one, and an artifact folder that contains a
//!   repository the walk found is skipped.
//! * One hard-link de-duplication domain per filesystem spans all main-repo
//!   artifacts, so files hard-linked between projects count once in total.
//!   Projects are measured in path order, so the project credited with a
//!   shared inode is stable from run to run.
//!
//! Known limitation: the worktree pipeline measures each worktree in its own
//! de-duplication domain (Milestone 1 behavior), so a file hard-linked between
//! a dormant worktree and a main repo's artifact folder (e.g. via a pnpm store)
//! counts in both. Like APFS clones, hard links mean deleting one copy may free
//! less than its reported size.
//!
//! # Safety
//! * Artifact folders containing a `.git` folder anywhere inside (e.g. an
//!   editable `pip install -e git+...` checkout in `.venv/src`, which the
//!   walk never enters) are `REVIEW` with tier `review` and no cleanup.
//! * A dormant-looking main repo with uncommitted or untracked changes takes
//!   the newest mtime of those files as activity, so a project edited today is
//!   `ACTIVE` even if its last commit is old. If `git status` fails, `REVIEW`.
//! * Main repos are searched for artifacts [`ARTIFACT_SEARCH_DEPTH`] levels
//!   past the walk's `--depth` limit, so deep monorepo folders aren't missed.
//!
//! [`ARTIFACT_SEARCH_DEPTH`]: crate::artifacts::ARTIFACT_SEARCH_DEPTH

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use rayon::prelude::*;
use serde_json::json;

use crate::artifacts;
use crate::error::{Error, Result};
use crate::git::Git;
use crate::model::{Category, Cleanup, Finding, Skipped, Status, Tier};
use crate::progress::{NoProgress, Phase, Progress};
use crate::size::{Sizer, Split};
use crate::walk::{self, Descend, EntryKind, WalkOptions};
use crate::worktree::analyze::{activity_secs, git_stamp_secs, idle};
use crate::worktree::{self, discover, PhaseTiming, ScanOptions, WorktreeScan};

/// Reason for artifact folders that hold a git checkout.
pub const NESTED_GIT_REASON: &str = "contains a git checkout (.git inside); it may hold local work";

/// Is the project a main repository or a linked worktree?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    Repo,
    Worktree,
}

impl ProjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectKind::Repo => "repo",
            ProjectKind::Worktree => "worktree",
        }
    }
}

/// A git-ignored artifact folder inside a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectArtifact {
    pub path: PathBuf,
    /// Folder name: `node_modules`, `target`, `.venv`... (the finding's `kind`).
    pub kind: String,
    pub size_bytes: u64,
    pub tier: Tier,
    /// A `.git` folder exists somewhere inside: never offered for cleanup.
    pub contains_git: bool,
}

/// A repository (or active worktree) holding at least one artifact folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub path: PathBuf,
    pub kind: ProjectKind,
    /// Status shared by all of the project's artifacts.
    pub status: Status,
    pub reason: String,
    pub idle_days: Option<i64>,
    pub last_activity: Option<SystemTime>,
    /// Sorted by path.
    pub artifacts: Vec<ProjectArtifact>,
}

impl Project {
    /// One [`Finding`] per artifact folder (category `artifacts`).
    ///
    /// `REMOVABLE` artifacts carry `cleanup: {"action": "trash", "paths": [<folder>]}`.
    pub fn to_findings(&self) -> Vec<Finding> {
        self.artifacts
            .iter()
            .map(|a| {
                let review = a.contains_git && self.status.is_dormant();
                let (status, reason, tier) = if review {
                    (Status::Review, NESTED_GIT_REASON.to_string(), Tier::Review)
                } else {
                    (self.status, self.reason.clone(), a.tier)
                };
                (a, status, reason, tier)
            })
            .map(|(a, status, reason, tier)| Finding {
                id: format!("artifact:{}", a.path.display()),
                category: Category::Artifacts,
                kind: a.kind.clone(),
                path: a.path.clone(),
                size_bytes: a.size_bytes,
                tier,
                status,
                reason,
                last_activity: self.last_activity,
                details: json!({
                    "project": self.path.to_string_lossy(),
                    "project_kind": self.kind.as_str(),
                    "idle_days": self.idle_days,
                    "relative_path": a.path.strip_prefix(&self.path)
                        .unwrap_or(&a.path)
                        .to_string_lossy(),
                }),
                cleanup: (status == Status::Removable)
                    .then(|| Cleanup::trash(vec![a.path.clone()])),
            })
            .collect()
    }
}

/// Counters and timings from a project scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectStats {
    /// Every phase, in order: walk, git, size, ignore-check (worktrees), artifacts.
    pub timings: Vec<PhaseTiming>,
    /// Main repositories found.
    pub repos: u64,
    /// Main repositories with at least one candidate artifact folder.
    pub repos_with_candidates: u64,
    /// Artifact-named folders seen by the walk (before attribution/ignore-check).
    pub candidates: u64,
    /// Entries that couldn't be read while measuring main-repo artifacts.
    pub size_errors: u64,
    /// Git processes spawned by the whole scan.
    pub git_calls: u64,
    pub total: Duration,
}

/// Everything [`scan`] found.
#[derive(Debug, Clone)]
pub struct ProjectScan {
    /// The Milestone 1 worktree results (same as [`worktree::scan`]).
    pub worktrees: WorktreeScan,
    /// Projects holding artifacts: main repos plus *active* worktrees (dormant
    /// worktrees report their artifacts in their own finding). Sorted by path.
    pub projects: Vec<Project>,
    pub stats: ProjectStats,
}

impl ProjectScan {
    pub fn worktree_findings(&self) -> Vec<Finding> {
        self.worktrees
            .worktrees
            .iter()
            .map(worktree::Worktree::to_finding)
            .collect()
    }

    pub fn artifact_findings(&self) -> Vec<Finding> {
        self.projects
            .iter()
            .flat_map(Project::to_findings)
            .collect()
    }

    /// Folders the walk could not read, and missing roots.
    pub fn skipped(&self) -> Skipped {
        Skipped {
            permission_denied: self.worktrees.stats.permission_denied,
            other_errors: self.worktrees.stats.other_errors,
            missing_roots: self.worktrees.missing_roots.clone(),
        }
    }
}

/// Scan roots for worktrees and project artifacts in one walk. Read-only.
pub fn scan(opts: &ScanOptions, progress: &dyn Progress) -> Result<ProjectScan> {
    let started = Instant::now();
    let (roots, walk_roots, missing_roots) = worktree::resolve_roots(&opts.roots);
    if roots.is_empty() {
        return Err(Error::NoValidRoots(missing_roots));
    }
    let git = Git::new(opts.git_timeout, opts.git_concurrency)?;
    let mut timings = Vec::new();

    progress.start(Phase::Walk, None);
    let (found, mains, repo_roots, wt_candidates, candidates) =
        worktree::timed(&mut timings, Phase::Walk, || {
            let found = discover::discover(&walk_roots, opts.max_depth, progress);
            let mut mains = found.mains;
            let mut repo_roots = found.repo_roots;
            let mut wt_candidates = found.candidates;
            add_enclosing_repos(&walk_roots, &mut mains, &mut repo_roots, &mut wt_candidates);
            let mut candidates = found.artifact_candidates;
            deep_search(
                &found.frontier,
                &mut mains,
                &mut repo_roots,
                &mut candidates,
            );
            let rest = discover::Discovery {
                stale: found.stale,
                walk: found.walk,
                ..Default::default()
            };
            (rest, mains, repo_roots, wt_candidates, candidates)
        });
    progress.finish(Phase::Walk);

    let n_candidates = candidates.len() as u64;
    let by_repo = attribute(candidates, &mains, &repo_roots);

    // Safety rule 2: every main repo's timestamps before any git process runs.
    let stamps: BTreeMap<PathBuf, f64> = by_repo
        .par_iter()
        .map(|(repo, _)| (repo.clone(), git_stamp_secs(&repo.join(".git"))))
        .collect();

    // ...and every worktree's, so no git process (from either pipeline) runs first.
    let wt_pre: Vec<_> = wt_candidates
        .par_iter()
        .map(worktree::analyze::read_pre_git)
        .collect();
    let wt_found = worktree::Found {
        roots,
        missing_roots,
        candidates: wt_candidates,
        stale: found.stale,
        walk: found.walk,
        pre: Some(wt_pre),
    };
    let reporter = Reporter::default();
    let (wt_scan, (mut projects, size_errors, art_elapsed)) = std::thread::scope(|s| {
        let art_job = s.spawn(|| {
            let t = Instant::now();
            let (projects, errors) =
                analyze_repos(&git, &by_repo, &stamps, &repo_roots, opts, &reporter);
            (projects, errors, t.elapsed())
        });
        let wt_scan = worktree::analyze_found(&git, opts, wt_found, timings, started, progress);
        // The worktree pipeline owned the progress display until now; hand it
        // to the artifact pipeline (which may still be sizing).
        progress.start(Phase::Artifacts, None);
        reporter.attach(progress);
        let art = art_job
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e));
        progress.finish(Phase::Artifacts);
        (wt_scan, art)
    });

    // A repo nested inside a worktree (e.g. a vendored clone) is covered by the
    // worktree: a dormant worktree's size includes it, and an active
    // worktree's artifact list already has its ignored folders (as ACTIVE).
    // Don't report it a second time.
    let wt_paths: Vec<&Path> = wt_scan.worktrees.iter().map(|w| w.path.as_path()).collect();
    projects.retain(|p| !wt_paths.iter().any(|w| p.path.starts_with(w)));

    // Active worktrees: their (already ignore-checked) artifacts, always ACTIVE.
    for w in wt_scan
        .worktrees
        .iter()
        .filter(|w| w.status == Status::Active && !w.artifacts.is_empty())
    {
        projects.push(Project {
            path: w.path.clone(),
            kind: ProjectKind::Worktree,
            status: Status::Active,
            reason: String::new(),
            idle_days: w.idle_days,
            last_activity: w.last_activity,
            artifacts: w
                .artifacts
                .iter()
                .map(|a| artifact(a.path.clone(), a.size_bytes, false))
                .collect(),
        });
    }
    projects.sort_by(|a, b| a.path.cmp(&b.path));

    let mut all_timings = wt_scan.stats.timings.clone();
    all_timings.push(PhaseTiming {
        phase: Phase::Artifacts,
        elapsed: art_elapsed,
    });
    let stats = ProjectStats {
        timings: all_timings,
        repos: mains.len() as u64,
        repos_with_candidates: by_repo.len() as u64,
        candidates: n_candidates,
        size_errors,
        git_calls: git.calls(),
        total: started.elapsed(),
    };
    Ok(ProjectScan {
        worktrees: wt_scan,
        projects,
        stats,
    })
}

fn artifact(path: PathBuf, size_bytes: u64, contains_git: bool) -> ProjectArtifact {
    let name = path.file_name().unwrap_or_default();
    ProjectArtifact {
        kind: name.to_string_lossy().into_owned(),
        tier: artifacts::tier_for(name),
        path,
        size_bytes,
        contains_git,
    }
}

/// Forwards artifact-sizing progress once the worktree pipeline is done with
/// the (single) progress display; counts silently before that.
#[derive(Default)]
struct Reporter<'a> {
    /// (items done before attaching, attached display)
    state: Mutex<(u64, Option<&'a dyn Progress>)>,
}

impl<'a> Reporter<'a> {
    fn attach(&self, p: &'a dyn Progress) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        p.advance(st.0);
        st.1 = Some(p);
    }

    fn tick(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match st.1 {
            Some(p) => p.advance(1),
            None => st.0 += 1,
        }
    }
}

/// A scan root inside a repository (`lysosome scan ~/code/app/packages`) still
/// belongs to that repository: look above each root for the nearest `.git`.
/// A main repo is added as such; a linked worktree becomes a worktree
/// candidate so the worktree pipeline analyzes it (and its artifacts).
fn add_enclosing_repos(
    walk_roots: &[PathBuf],
    mains: &mut BTreeSet<PathBuf>,
    repo_roots: &mut BTreeSet<PathBuf>,
    wt_candidates: &mut Vec<discover::Candidate>,
) {
    for root in walk_roots {
        for dir in root.ancestors().skip(1) {
            let dotgit = dir.join(".git");
            let Ok(m) = fs::symlink_metadata(&dotgit) else {
                continue;
            };
            if m.is_dir() {
                mains.insert(dir.to_path_buf());
            } else if m.is_file() {
                if let Some(gd) = discover::read_gitdir_pointer(&dotgit) {
                    let known = wt_candidates.iter().any(|c| c.path == dir);
                    if discover::is_linked_worktree_gitdir(&gd) && !known {
                        wt_candidates.push(discover::Candidate {
                            path: dir.to_path_buf(),
                            gitdir: gd,
                        });
                        wt_candidates.sort_by(|a, b| a.path.cmp(&b.path));
                    }
                }
            }
            repo_roots.insert(dir.to_path_buf());
            break;
        }
    }
}

/// Continue the artifact search below the walk's depth limit, but only inside
/// main repos: walk each frontier folder whose innermost repository is a main
/// repo for [`artifacts::ARTIFACT_SEARCH_DEPTH`] more levels, collecting
/// artifact-named folders and repository boundaries.
fn deep_search(
    frontier: &[PathBuf],
    mains: &mut BTreeSet<PathBuf>,
    repo_roots: &mut BTreeSet<PathBuf>,
    candidates: &mut Vec<PathBuf>,
) {
    let roots: Vec<PathBuf> = frontier
        .iter()
        .filter(|f| {
            f.ancestors()
                .skip(1)
                .find(|a| repo_roots.contains(*a))
                .is_some_and(|owner| mains.contains(owner))
        })
        .cloned()
        .collect();
    if roots.is_empty() {
        return;
    }
    // (artifact candidates, repo roots, main repos)
    let found: Mutex<(Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>)> = Mutex::default();
    let opts = WalkOptions {
        max_depth: artifacts::ARTIFACT_SEARCH_DEPTH,
        ..WalkOptions::default()
    };
    walk::walk(&roots, &opts, &NoProgress, |v| {
        let mut f = found.lock().unwrap_or_else(|e| e.into_inner());
        for e in v.entries {
            if e.kind == EntryKind::Dir && artifacts::is_artifact_dir(&e.name) {
                f.0.push(v.path.join(&e.name));
            }
        }
        if let Some(e) = v.entry(".git") {
            f.1.push(v.path.to_path_buf());
            if e.kind == EntryKind::Dir {
                f.2.push(v.path.to_path_buf());
            }
        }
        Descend::Yes
    });
    let (arts, repos, new_mains) = found.into_inner().unwrap_or_else(|e| e.into_inner());
    candidates.extend(arts);
    candidates.sort();
    candidates.dedup();
    repo_roots.extend(repos);
    mains.extend(new_mains);
}

/// Group candidate folders by their innermost repository, keeping only those
/// whose innermost repository is a main repo (worktrees are handled by the
/// worktree pipeline; submodules and other `.git`-file repos are skipped).
fn attribute(
    candidates: Vec<PathBuf>,
    mains: &BTreeSet<PathBuf>,
    repo_roots: &BTreeSet<PathBuf>,
) -> BTreeMap<PathBuf, Vec<PathBuf>> {
    let mut by_repo: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for c in candidates {
        let owner = c
            .ancestors()
            .skip(1)
            .find(|a| repo_roots.contains(*a))
            .map(Path::to_path_buf);
        if let Some(owner) = owner {
            if mains.contains(&owner) {
                by_repo.entry(owner).or_default().push(c);
            }
        }
    }
    by_repo
}

/// Confirmed artifacts of one repo, before sizing.
struct RepoArtifacts {
    repo: PathBuf,
    paths: Vec<PathBuf>,
    activity_secs: f64,
    /// `git status` failed for a dormant-looking repo: we can't vouch it's idle.
    status_failed: bool,
}

/// Newest mtime of the files `git status` reports as changed or untracked
/// (0 if none). `None` if `git status` failed.
fn worktree_changes_secs(git: &Git, repo: &Path) -> Option<f64> {
    let out = git.run(
        repo,
        [
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ],
    );
    if !out.success {
        return None;
    }
    let mut newest = 0.0f64;
    let mut entries = out.stdout.split(|b| *b == 0).filter(|e| !e.is_empty());
    while let Some(e) = entries.next() {
        if e.len() < 4 {
            continue;
        }
        let (xy, path) = (&e[..2], &e[3..]);
        // Renames and copies carry the original path as the next entry.
        if xy.contains(&b'R') || xy.contains(&b'C') {
            entries.next();
        }
        // Untracked folders are reported once, as `dir/`: look inside them.
        let (path, is_dir) = match path.strip_suffix(b"/") {
            Some(p) => (p, true),
            None => (path, false),
        };
        let abs = repo.join(std::ffi::OsStr::from_bytes(path));
        newest = newest.max(if is_dir {
            newest_inside(&abs)
        } else {
            mtime_nofollow(&abs)
        });
    }
    Some(newest)
}

/// mtime without following symlinks; 0 if unreadable (e.g. a deleted file).
fn mtime_nofollow(p: &Path) -> f64 {
    fs::symlink_metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Entries inspected inside one untracked folder before giving up.
const UNTRACKED_SCAN_BUDGET: usize = 20_000;

/// Newest mtime of anything inside an untracked folder (editing a file
/// doesn't touch its folder's mtime). Symlinks aren't followed. If the folder
/// is too big to inspect, returns `f64::MAX` ("assume it's in use").
fn newest_inside(dir: &Path) -> f64 {
    let mut newest = mtime_nofollow(dir);
    let mut stack = vec![dir.to_path_buf()];
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > UNTRACKED_SCAN_BUDGET {
                return f64::MAX;
            }
            let p = e.path();
            newest = newest.max(mtime_nofollow(&p));
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(p);
            }
        }
    }
    newest
}

/// Ignore-check, date and size the candidates of every main repo.
/// Returns projects with at least one artifact, and the size error count.
fn analyze_repos(
    git: &Git,
    by_repo: &BTreeMap<PathBuf, Vec<PathBuf>>,
    stamps: &BTreeMap<PathBuf, f64>,
    repo_roots: &BTreeSet<PathBuf>,
    opts: &ScanOptions,
    reporter: &Reporter<'_>,
) -> (Vec<Project>, u64) {
    let threshold = i64::try_from(opts.dormant_after_days).unwrap_or(i64::MAX);
    let now_secs = opts
        .now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    let confirmed: Vec<RepoArtifacts> = git.pool().install(|| {
        by_repo
            .par_iter()
            .filter_map(|(repo, cands)| {
                let ignored = artifacts::ignored_subset(git, repo, cands);
                let paths = outermost(ignored)
                    .into_iter()
                    .filter(|p| !contains_repo(repo_roots, p))
                    .collect::<Vec<_>>();
                if paths.is_empty() {
                    return None;
                }
                let stamp = stamps.get(repo).copied().unwrap_or(0.0);
                let mut activity = activity_secs(git, repo, stamp);
                let mut status_failed = false;
                // Only a dormant-looking repo needs the (costlier) dirty check.
                if matches!(idle(activity, opts.now).0, Some(d) if d >= threshold) {
                    match worktree_changes_secs(git, repo) {
                        // Clamp: "too big to inspect" (f64::MAX) and future
                        // mtimes mean "in use now".
                        Some(changed) => activity = activity.max(changed.min(now_secs)),
                        None => status_failed = true,
                    }
                }
                Some(RepoArtifacts {
                    repo: repo.clone(),
                    paths,
                    activity_secs: activity,
                    status_failed,
                })
            })
            .collect()
    });

    // One de-duplication domain per filesystem across all projects.
    let sizers: Mutex<HashMap<u64, Arc<Sizer>>> = Mutex::new(HashMap::new());
    let sizer_for = |p: &Path| -> Option<Arc<Sizer>> {
        let dev = fs::symlink_metadata(p).ok()?.dev();
        let mut map = sizers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = map.get(&dev) {
            return Some(s.clone());
        }
        let s = Arc::new(Sizer::new(p).ok()?);
        map.insert(dev, s.clone());
        Some(s)
    };
    let mut lost = 0u64;
    // Sequential in path order (each folder is measured in parallel inside),
    // so the project credited with a shared hard link is deterministic.
    let projects: Vec<Project> = confirmed
        .iter()
        .map(|r| {
            let arts = r
                .paths
                .iter()
                .map(|p| {
                    let (size, has_git) = sizer_for(p).map_or((0, false), |s| measure(&s, p));
                    reporter.tick();
                    artifact(p.clone(), size, has_git)
                })
                .collect();
            let (idle_days, last_activity) = idle(r.activity_secs, opts.now);
            let (status, reason) = if r.status_failed {
                (Status::Review, "could not read git status".to_string())
            } else {
                classify(idle_days, opts.dormant_after_days)
            };
            Project {
                path: r.repo.clone(),
                kind: ProjectKind::Repo,
                status,
                reason,
                idle_days,
                last_activity,
                artifacts: arts,
            }
        })
        .collect();
    for s in sizers
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .values()
    {
        lost += s.errors();
    }
    (projects, lost)
}

/// Allocated size of `dir`, and whether a `.git` folder exists anywhere inside.
fn measure(sizer: &Sizer, dir: &Path) -> (u64, bool) {
    let has_git = AtomicBool::new(false);
    let size = sizer
        .measure_split(dir, &|_, name, _| {
            if name == ".git" {
                has_git.store(true, Ordering::Relaxed);
                Split::InlineNoSplit
            } else {
                Split::Inline
            }
        })
        .total;
    (size, has_git.into_inner())
}

/// Drop paths nested inside another path of the set (sorted output). After
/// sorting, a path's descendants follow it directly, so checking the last kept
/// path is enough.
fn outermost(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    let mut out: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for p in paths {
        if !out.last().is_some_and(|o| p.starts_with(o)) {
            out.push(p);
        }
    }
    out
}

/// Does `dir` contain (or is it) a repository root?
fn contains_repo(repo_roots: &BTreeSet<PathBuf>, dir: &Path) -> bool {
    repo_roots
        .range(dir.to_path_buf()..)
        .next()
        .is_some_and(|r| r.starts_with(dir))
}

/// Ignored artifacts are regenerable, so the only question is whether the
/// project is in use. No timestamp at all means we can't tell: `REVIEW`.
pub fn classify(idle_days: Option<i64>, dormant_after_days: u64) -> (Status, String) {
    let threshold = i64::try_from(dormant_after_days).unwrap_or(i64::MAX);
    match idle_days {
        None => (
            Status::Review,
            "no git activity found for the project".into(),
        ),
        Some(d) if d >= threshold => (
            Status::Removable,
            format!(
                "git-ignored; project idle {d} {}",
                if d == 1 { "day" } else { "days" }
            ),
        ),
        Some(_) => (Status::Active, String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_idle_days() {
        assert_eq!(classify(Some(29), 30).0, Status::Active);
        assert_eq!(
            classify(Some(30), 30),
            (
                Status::Removable,
                "git-ignored; project idle 30 days".to_string()
            )
        );
        assert_eq!(classify(None, 30).0, Status::Review);
    }

    #[test]
    fn keeps_only_outermost_paths() {
        let got = outermost(vec![
            "/r/build/x/dist".into(),
            "/r/build".into(),
            "/r/build-tools/dist".into(),
            "/r/a/node_modules".into(),
        ]);
        assert_eq!(
            got,
            vec![
                PathBuf::from("/r/a/node_modules"),
                PathBuf::from("/r/build"),
                PathBuf::from("/r/build-tools/dist"),
            ]
        );
    }

    #[test]
    fn attributes_to_innermost_main_repo() {
        let mains: BTreeSet<PathBuf> = ["/c/app", "/c/app/vendor/lib"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let mut roots = mains.clone();
        roots.insert("/c/app/.claude/worktrees/wt".into()); // linked worktree
        roots.insert("/c/app/sub".into()); // submodule
        let got = attribute(
            vec![
                "/c/app/node_modules".into(),
                "/c/app/vendor/lib/target".into(),
                "/c/app/.claude/worktrees/wt/node_modules".into(),
                "/c/app/sub/dist".into(),
                "/c/loose/node_modules".into(),
            ],
            &mains,
            &roots,
        );
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[Path::new("/c/app")],
            vec![PathBuf::from("/c/app/node_modules")]
        );
        assert_eq!(
            got[Path::new("/c/app/vendor/lib")],
            vec![PathBuf::from("/c/app/vendor/lib/target")]
        );
        assert!(contains_repo(&roots, Path::new("/c/app/.claude")));
        assert!(!contains_repo(&roots, Path::new("/c/app/build")));
        assert!(!contains_repo(&roots, Path::new("/c/app/su")));
    }
}
