//! Re-checks run immediately before acting on an item. A scan may be minutes
//! (or a prompt) old; anything that changed since then means "skip".
//!
//! Every function returns `Err(reason)` when the item must be skipped.

use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use crate::artifacts;
use crate::git::Git;
use crate::model::Status;
use crate::projects;
use crate::size::Sizer;
use crate::worktree::analyze::{self, RepoCache};
use crate::worktree::classify::classify;
use crate::worktree::discover::{self, Candidate};

use super::process;

/// Inputs every re-check needs.
pub(crate) struct Ctx<'a> {
    /// `None` when git can't be run: every git-backed item is then skipped.
    pub git: Option<&'a Git>,
    pub home: &'a Path,
    pub dormant_after_days: u64,
    pub now: SystemTime,
    pub path_env: Option<&'a OsString>,
}

impl Ctx<'_> {
    fn git(&self) -> Result<&Git, String> {
        self.git
            .ok_or_else(|| "git could not be run, so it can't be re-checked".to_string())
    }
}

/// Refuse targets that are never acceptable, whatever a finding says: a
/// relative path, anything with `..`, the filesystem root or a shallow system
/// folder, the home folder or one of its ancestors.
pub(crate) fn protected(path: &Path, home: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return Err("not a plain absolute path".into());
    }
    let normal = path
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    if normal < 3 {
        return Err("refusing to touch a top-level folder".into());
    }
    let canon_home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    for h in [home, canon_home.as_path()] {
        if h.starts_with(path) {
            return Err("refusing to touch your home folder or a folder above it".into());
        }
    }
    Ok(())
}

/// `path` must exist and not be a symlink; if `dir`, it must be a folder.
/// Its parent must still resolve to `expected_parent` (no symlink swapped in
/// above it). `None` means "the parent must resolve to itself".
pub(crate) fn real_target(
    path: &Path,
    dir: bool,
    expected_parent: Option<&Path>,
) -> Result<(), String> {
    let m = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("no longer exists".into()),
        Err(e) => return Err(format!("cannot inspect it: {e}")),
    };
    if m.file_type().is_symlink() {
        return Err("is now a symlink".into());
    }
    if dir && !m.is_dir() {
        return Err("is no longer a folder".into());
    }
    let parent = path.parent().ok_or("has no parent folder")?;
    let canon = parent
        .canonicalize()
        .map_err(|e| format!("cannot resolve its folder: {e}"))?;
    let want = expected_parent.unwrap_or(parent);
    if canon != want {
        return Err(format!(
            "its folder now resolves to {} (a symlink changed)",
            canon.display()
        ));
    }
    Ok(())
}

/// Allocated size of a folder and whether a `.git` exists anywhere inside.
pub(crate) fn measure_dir(path: &Path) -> Option<(u64, bool)> {
    let sizer = Sizer::new(path).ok()?;
    Some(projects::measure(&sizer, path))
}

/// Fresh analysis of one worktree: `(status, reason, main_repo)`.
/// Fails if the folder is no longer the same linked worktree.
pub(crate) fn worktree(
    ctx: &Ctx<'_>,
    path: &Path,
    gitdir: &Path,
) -> Result<(Status, String, Option<PathBuf>), String> {
    real_target(path, true, None).map_err(|e| format!("worktree folder {e}"))?;
    let dotgit = path.join(".git");
    let is_file = fs::symlink_metadata(&dotgit).is_ok_and(|m| m.is_file());
    let pointer = is_file
        .then(|| discover::read_gitdir_pointer(&dotgit))
        .flatten()
        .ok_or("its .git file is gone or changed")?;
    if !discover::same_path(&pointer, gitdir) {
        return Err("it now points at a different git repository".into());
    }
    let cand = Candidate {
        path: path.to_path_buf(),
        gitdir: gitdir.to_path_buf(),
    };
    // Timestamps before any git process (safety rule 2).
    let pre = analyze::read_pre_git(&cand);
    if !pre.gitdir_exists {
        return Err("its git metadata is gone (orphan)".into());
    }
    let a = analyze::analyze(ctx.git()?, &RepoCache::default(), &cand, &pre, ctx.now);
    let (status, reason) = classify(&a.facts, ctx.dormant_after_days);
    Ok((status, reason, pre.main_repo))
}

/// Is `path` (inside `repo_dir`) still ignored by git?
pub(crate) fn still_ignored(ctx: &Ctx<'_>, repo_dir: &Path, path: &Path) -> Result<(), String> {
    let got = artifacts::ignored_subset(ctx.git()?, repo_dir, &[path.to_path_buf()]);
    if got.iter().any(|p| p == path) {
        Ok(())
    } else {
        Err("git no longer ignores it".into())
    }
}

/// Main-repo status with the scan's rules, right now.
pub(crate) fn repo_status(ctx: &Ctx<'_>, repo: &Path) -> Result<(Status, String), String> {
    Ok(projects::repo_status(
        ctx.git()?,
        repo,
        ctx.now,
        ctx.dormant_after_days,
    ))
}

/// Resolve a command's program on PATH.
pub(crate) fn program(ctx: &Ctx<'_>, argv0: &str) -> Result<PathBuf, String> {
    process::which(argv0, ctx.path_env).ok_or_else(|| format!("`{argv0}` is no longer on PATH"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_home_root_and_shallow_paths() {
        let home = Path::new("/Users/me");
        assert!(protected(Path::new("/"), home).is_err());
        assert!(protected(Path::new("/Users"), home).is_err());
        assert!(protected(Path::new("/Users/me"), home).is_err());
        assert!(protected(Path::new("/usr/local"), home).is_err());
        assert!(protected(Path::new("relative/a/b/c"), home).is_err());
        assert!(protected(Path::new("/Users/me/code/../.."), home).is_err());
        assert!(protected(Path::new("/Users/me/.npm"), home).is_ok());
        assert!(protected(Path::new("/Users/me/code/app/node_modules"), home).is_ok());
    }

    #[test]
    fn real_target_rejects_symlinks_and_missing() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let d = base.join("d");
        fs::create_dir(&d).unwrap();
        assert!(real_target(&d, true, None).is_ok());
        assert!(real_target(&d, true, Some(&base)).is_ok());
        assert!(real_target(&d, true, Some(Path::new("/elsewhere"))).is_err());
        let l = base.join("l");
        std::os::unix::fs::symlink(&d, &l).unwrap();
        assert_eq!(real_target(&l, true, None), Err("is now a symlink".into()));
        assert_eq!(
            real_target(&base.join("gone"), true, None),
            Err("no longer exists".into())
        );
        // A symlinked parent: the path's folder no longer resolves to itself.
        let via = base.join("via");
        std::os::unix::fs::symlink(&base, &via).unwrap();
        assert!(real_target(&via.join("d"), true, None).is_err());
    }
}
