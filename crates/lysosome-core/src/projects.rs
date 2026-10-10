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
//! * Artifacts inside a **dormant** worktree are only part of that worktree's
//!   finding (`size_bytes`, `details.artifacts`). Artifacts inside an
//!   **active** worktree are emitted as `ACTIVE` artifact findings, which never
//!   count towards summary totals.
//! * An artifact folder nested inside another ignored artifact folder is
//!   folded into the outer one, and an artifact folder that contains another
//!   repository or worktree is skipped (we can't vouch it's all regenerable).
//! * One hard-link de-duplication domain per filesystem spans all main-repo
//!   artifacts, so files hard-linked between projects count once in total.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use serde_json::json;

use crate::artifacts;
use crate::error::{Error, Result};
use crate::git::Git;
use crate::model::{Category, Cleanup, Finding, Skipped, Status, Tier};
use crate::progress::{Phase, Progress};
use crate::size::Sizer;
use crate::worktree::{self, discover, PhaseTiming, ScanOptions, WorktreeScan};

const DAY: f64 = 86_400.0;

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
    pub fn artifact_bytes(&self) -> u64 {
        self.artifacts.iter().map(|a| a.size_bytes).sum()
    }

    /// One [`Finding`] per artifact folder (category `artifacts`).
    ///
    /// `REMOVABLE` artifacts carry `cleanup: {"action": "trash", "paths": [<folder>]}`.
    pub fn to_findings(&self) -> Vec<Finding> {
        self.artifacts
            .iter()
            .map(|a| Finding {
                id: format!("artifact:{}", a.path.display()),
                category: Category::Artifacts,
                kind: a.kind.clone(),
                path: a.path.clone(),
                size_bytes: a.size_bytes,
                tier: a.tier,
                status: self.status,
                reason: self.reason.clone(),
                last_activity: self.last_activity,
                details: json!({
                    "project": self.path.to_string_lossy(),
                    "project_kind": self.kind.as_str(),
                    "idle_days": self.idle_days,
                    "relative_path": a.path.strip_prefix(&self.path)
                        .unwrap_or(&a.path)
                        .to_string_lossy(),
                }),
                cleanup: (self.status == Status::Removable)
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
    let found = worktree::timed(&mut timings, Phase::Walk, || {
        discover::discover(&walk_roots, opts.max_depth, progress)
    });
    progress.finish(Phase::Walk);

    let mut mains = found.mains;
    let mut repo_roots = found.repo_roots;
    add_enclosing_repos(&walk_roots, &mut mains, &mut repo_roots);
    let n_candidates = found.artifact_candidates.len() as u64;
    let by_repo = attribute(found.artifact_candidates, &mains, &repo_roots);

    // Safety rule 2: every main repo's timestamps before any git process runs.
    let stamps: BTreeMap<PathBuf, f64> = by_repo
        .par_iter()
        .map(|(repo, _)| (repo.clone(), git_stamp_secs(&repo.join(".git"))))
        .collect();

    let wt_found = worktree::Found {
        roots,
        missing_roots,
        candidates: found.candidates,
        stale: found.stale,
        walk: found.walk,
    };
    let (wt_scan, (mut projects, size_errors, art_elapsed)) = std::thread::scope(|s| {
        let art_job = s.spawn(|| {
            let t = Instant::now();
            let (projects, errors) = analyze_repos(&git, &by_repo, &stamps, &repo_roots, opts);
            (projects, errors, t.elapsed())
        });
        let wt_scan = worktree::analyze_found(&git, opts, wt_found, timings, started, progress);
        progress.start(Phase::Artifacts, None);
        let art = art_job
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e));
        progress.finish(Phase::Artifacts);
        (wt_scan, art)
    });

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
                .map(|a| artifact(a.path.clone(), a.size_bytes))
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

fn artifact(path: PathBuf, size_bytes: u64) -> ProjectArtifact {
    let name = path.file_name().unwrap_or_default();
    ProjectArtifact {
        kind: name.to_string_lossy().into_owned(),
        tier: artifacts::tier_for(name),
        path,
        size_bytes,
    }
}

/// A scan root inside a repository (`lysosome scan ~/code/app/packages`) still
/// belongs to that repository: look above each root for the nearest `.git`.
fn add_enclosing_repos(
    walk_roots: &[PathBuf],
    mains: &mut BTreeSet<PathBuf>,
    repo_roots: &mut BTreeSet<PathBuf>,
) {
    for root in walk_roots {
        for dir in root.ancestors().skip(1) {
            match fs::symlink_metadata(dir.join(".git")) {
                Ok(m) => {
                    if m.is_dir() {
                        mains.insert(dir.to_path_buf());
                    }
                    repo_roots.insert(dir.to_path_buf());
                    break;
                }
                Err(_) => continue,
            }
        }
    }
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

/// Max mtime (seconds) of a git dir's `index`, `HEAD` and `logs/HEAD`; 0 if none.
fn git_stamp_secs(git_dir: &Path) -> f64 {
    ["index", "HEAD", "logs/HEAD"]
        .iter()
        .filter_map(|f| {
            fs::metadata(git_dir.join(f))
                .and_then(|m| m.modified())
                .ok()
        })
        .filter_map(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .fold(0.0, f64::max)
}

/// Confirmed artifacts of one repo, before sizing.
struct RepoArtifacts {
    repo: PathBuf,
    paths: Vec<PathBuf>,
    activity_secs: f64,
}

/// Ignore-check, date and size the candidates of every main repo.
/// Returns projects with at least one artifact, and the size error count.
fn analyze_repos(
    git: &Git,
    by_repo: &BTreeMap<PathBuf, Vec<PathBuf>>,
    stamps: &BTreeMap<PathBuf, f64>,
    repo_roots: &BTreeSet<PathBuf>,
    opts: &ScanOptions,
) -> (Vec<Project>, u64) {
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
                let mut activity_secs = stamps.get(repo).copied().unwrap_or(0.0);
                let log = git.run(repo, ["log", "-1", "--format=%ct"]);
                if log.success {
                    if let Ok(ct) = log.text().parse::<u64>() {
                        activity_secs = activity_secs.max(ct as f64);
                    }
                }
                Some(RepoArtifacts {
                    repo: repo.clone(),
                    paths,
                    activity_secs,
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
    let projects: Vec<Project> = confirmed
        .par_iter()
        .map(|r| {
            let arts = r
                .paths
                .par_iter()
                .map(|p| {
                    let size = sizer_for(p).map_or(0, |s| s.size_of(p));
                    artifact(p.clone(), size)
                })
                .collect();
            let (idle_days, last_activity) = idle(r.activity_secs, opts.now);
            let (status, reason) = classify(idle_days, opts.dormant_after_days);
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

/// Drop paths nested inside another path of the set (sorted output).
fn outermost(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    let mut out: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for p in paths {
        if !out.iter().any(|o| p.starts_with(o)) {
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

fn idle(activity_secs: f64, now: SystemTime) -> (Option<i64>, Option<SystemTime>) {
    if activity_secs <= 0.0 {
        return (None, None);
    }
    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    (
        Some(((now_secs - activity_secs) / DAY).floor() as i64),
        Some(UNIX_EPOCH + Duration::from_secs_f64(activity_secs)),
    )
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
