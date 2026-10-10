//! Per-worktree analysis: timestamps first, then git.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::classify::Facts;
use super::discover::{absolutize, Candidate};
use crate::git::Git;

const DAY: f64 = 86_400.0;

/// What we read from the gitdir *before* any git process runs (safety rule 2).
#[derive(Debug, Clone, Default)]
pub(crate) struct PreGit {
    pub gitdir_exists: bool,
    /// Max mtime of `index`, `logs/HEAD`, `HEAD` (seconds since epoch; 0 if none).
    pub stamp_secs: f64,
    pub branch: Option<String>,
    pub locked: bool,
    pub main_repo: Option<PathBuf>,
    /// Shared git dir (key for per-repository caching).
    pub common_dir: Option<PathBuf>,
}

pub(crate) fn read_pre_git(c: &Candidate) -> PreGit {
    let gd = &c.gitdir;
    if !gd.is_dir() {
        return PreGit::default();
    }
    let stamp_secs = git_stamp_secs(gd);
    let branch = Some(read_branch(&gd.join("HEAD")));
    let (main_repo, common_dir) = match fs::read_to_string(gd.join("commondir")) {
        Ok(cd) => {
            let common = absolutize(gd, Path::new(cd.trim()));
            let main = if common.file_name().is_some_and(|n| n == ".git") {
                common.parent().map(Path::to_path_buf)
            } else {
                Some(common.clone())
            };
            (main, Some(common))
        }
        Err(_) => (None, None),
    };
    PreGit {
        gitdir_exists: true,
        stamp_secs,
        branch,
        locked: gd.join("locked").exists(),
        main_repo,
        common_dir,
    }
}

/// `git symbolic-ref --short HEAD`, read straight from the HEAD file.
fn read_branch(head: &Path) -> String {
    let Ok(raw) = fs::read_to_string(head) else {
        return "(detached)".into();
    };
    match raw.trim().strip_prefix("ref:") {
        Some(r) => {
            let r = r.trim();
            r.strip_prefix("refs/heads/")
                .unwrap_or_else(|| r.strip_prefix("refs/").unwrap_or(r))
                .to_string()
        }
        None => "(detached)".into(),
    }
}

/// Max mtime (seconds since epoch, symlinks not followed) of a git dir's `index`, `logs/HEAD` and
/// `HEAD`; 0 if none exist. Read before running git (safety rule 2). Shared by
/// worktrees and main repos so both use the same staleness rule.
pub(crate) fn git_stamp_secs(git_dir: &Path) -> f64 {
    ["index", "logs/HEAD", "HEAD"]
        .iter()
        .map(|f| mtime_secs(&git_dir.join(f)))
        .fold(0.0, f64::max)
}

/// `max(stamp_secs, last commit time)` via `git log -1 --format=%ct` in `dir`.
pub(crate) fn activity_secs(git: &Git, dir: &Path, stamp_secs: f64) -> f64 {
    let log = git.run(dir, ["log", "-1", "--format=%ct"]);
    if log.success {
        if let Ok(ct) = log.text().parse::<u64>() {
            return stamp_secs.max(ct as f64);
        }
    }
    stamp_secs
}

/// Whole idle days and the activity time; `(None, None)` when `activity_secs` is 0.
pub(crate) fn idle(activity_secs: f64, now: SystemTime) -> (Option<i64>, Option<SystemTime>) {
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

fn mtime_secs(p: &Path) -> f64 {
    fs::symlink_metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Repository-level facts shared by all worktrees of one repo.
#[derive(Debug, Clone, Default)]
pub(crate) struct RepoInfo {
    pub default_branch: Option<String>,
    pub has_remote: bool,
}

/// Computes [`RepoInfo`] once per common git dir, even under concurrency.
#[derive(Default)]
pub(crate) struct RepoCache {
    map: Mutex<HashMap<PathBuf, Arc<OnceLock<RepoInfo>>>>,
}

impl RepoCache {
    fn get(&self, key: &Path, git: &Git, wt: &Path) -> RepoInfo {
        let cell = {
            let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
            map.entry(key.to_path_buf()).or_default().clone()
        };
        cell.get_or_init(|| RepoInfo {
            default_branch: default_branch(git, wt),
            has_remote: !git.run(wt, ["remote"]).text().is_empty(),
        })
        .clone()
    }
}

fn default_branch(git: &Git, wt: &Path) -> Option<String> {
    let out = git.run(wt, ["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"]);
    let r = out.text();
    if out.success && !r.is_empty() {
        return Some(r.replacen("refs/remotes/", "", 1));
    }
    ["main", "master", "origin/main", "origin/master"]
        .into_iter()
        .find(|cand| {
            git.run(wt, ["rev-parse", "--verify", "--quiet", cand])
                .success
        })
        .map(str::to_string)
}

/// Git-derived facts plus last activity.
#[derive(Debug, Clone)]
pub(crate) struct Analysis {
    pub facts: Facts,
    pub branch: Option<String>,
    pub last_activity: Option<SystemTime>,
}

pub(crate) fn analyze(
    git: &Git,
    cache: &RepoCache,
    c: &Candidate,
    pre: &PreGit,
    now: SystemTime,
) -> Analysis {
    let wt = &c.path;
    let activity = activity_secs(git, wt, pre.stamp_secs);
    let (idle_days, last_activity) = idle(activity, now);

    // Pin the options that decide "clean" so user config (e.g.
    // status.showUntrackedFiles=no) can't hide work.
    let status = git.run(
        wt,
        [
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ],
    );
    let text = String::from_utf8_lossy(&status.stdout);
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    let untracked = lines.iter().filter(|l| l.starts_with("??")).count() as u64;
    let changed = lines.len() as u64 - untracked;

    // With the reftable backend HEAD is a `refs/heads/.invalid` stub; ask git.
    let branch = match pre.branch.as_deref() {
        Some(".invalid") => {
            let out = git.run(wt, ["symbolic-ref", "--quiet", "--short", "HEAD"]);
            Some(if out.success {
                out.text()
            } else {
                "(detached)".to_string()
            })
        }
        other => other.map(str::to_string),
    };

    let key = pre.common_dir.clone().unwrap_or_else(|| c.gitdir.clone());
    let key = key.canonicalize().unwrap_or(key);
    let repo = cache.get(&key, git, wt);

    let merged_into = repo.default_branch.filter(|base| {
        git.run(wt, ["merge-base", "--is-ancestor", "HEAD", base])
            .success
    });

    let unpushed = if repo.has_remote {
        let out = git.run(wt, ["rev-list", "--count", "HEAD", "--not", "--remotes"]);
        if out.success {
            out.text().parse::<u64>().ok()
        } else {
            None
        }
    } else {
        None
    };

    Analysis {
        facts: Facts {
            idle_days,
            locked: pre.locked,
            status_ok: status.success,
            changed,
            untracked,
            merged_into,
            has_remote: repo.has_remote,
            unpushed,
        },
        branch,
        last_activity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_from_head_file() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("HEAD");
        fs::write(&h, "ref: refs/heads/feat/x\n").unwrap();
        assert_eq!(read_branch(&h), "feat/x");
        fs::write(&h, "0123456789abcdef0123456789abcdef01234567\n").unwrap();
        assert_eq!(read_branch(&h), "(detached)");
    }
}
