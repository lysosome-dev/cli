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
/// folder, the home folder or one of its ancestors. Checked both as written
/// and as resolved (its parent folder's real location), so a symlinked parent
/// can't smuggle in `/` or the home folder.
pub(crate) fn protected(path: &Path, home: &Path) -> Result<(), String> {
    lexically_protected(path, home)?;
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(real_parent) = parent.canonicalize() {
            let resolved = real_parent.join(name);
            if resolved != path {
                lexically_protected(&resolved, home)?;
            }
        }
    }
    Ok(())
}

fn lexically_protected(path: &Path, home: &Path) -> Result<(), String> {
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

/// Walk a folder about to be removed, never following symlinks, and fail
/// closed: refuse if anything inside can't be vouched for. Returns its
/// allocated size (hard links counted once).
///
/// Refused: a `.git` entry of any kind when `refuse_git` (a nested checkout,
/// linked worktree or submodule may hold local work), a folder that can't be
/// read, and another filesystem mounted inside.
pub(crate) fn inspect_tree(dir: &Path, refuse_git: bool) -> Result<u64, String> {
    walk_checked(
        dir,
        if refuse_git {
            GitPolicy::Refuse
        } else {
            GitPolicy::Allow
        },
    )
}

/// Like [`inspect_tree`] for a whole linked worktree: its own top-level
/// `.git` file is expected; any other `.git` (a submodule, a clone inside an
/// ignored folder) is refused, since `git worktree remove` would delete it.
pub(crate) fn inspect_worktree(dir: &Path) -> Result<u64, String> {
    walk_checked(dir, GitPolicy::RefuseBelowRoot)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GitPolicy {
    Allow,
    Refuse,
    RefuseBelowRoot,
}

fn walk_checked(dir: &Path, git: GitPolicy) -> Result<u64, String> {
    use std::os::unix::fs::MetadataExt;
    let root = fs::symlink_metadata(dir).map_err(|e| format!("cannot inspect it: {e}"))?;
    let dev = root.dev();
    // The target itself must not be a mount point (its whole filesystem
    // would go).
    if let Some(parent) = dir.parent() {
        // The parent's real folder (it may be reached through a symlink).
        let pdev = fs::metadata(parent)
            .map_err(|e| format!("cannot inspect its folder: {e}"))?
            .dev();
        if pdev != dev {
            return Err("it is a separate mounted filesystem".into());
        }
    }
    mounts_inside(dir)?;
    let mut bytes = root.blocks() * 512;
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = fs::read_dir(&d)
            .map_err(|e| format!("part of it can't be read ({}: {e})", d.display()))?;
        for e in rd {
            let e = e.map_err(|e| format!("part of it can't be read ({}: {e})", d.display()))?;
            let p = e.path();
            let refuse = match git {
                GitPolicy::Allow => false,
                GitPolicy::Refuse => true,
                GitPolicy::RefuseBelowRoot => d != dir,
            };
            if refuse && e.file_name() == ".git" {
                return Err(format!(
                    "it contains a git checkout ({} inside)",
                    p.strip_prefix(dir).unwrap_or(&p).display()
                ));
            }
            let m = fs::symlink_metadata(&p)
                .map_err(|e| format!("part of it can't be read ({}: {e})", p.display()))?;
            if m.nlink() > 1 && !m.is_dir() && !seen.insert((m.dev(), m.ino())) {
                continue;
            }
            bytes += m.blocks() * 512;
            if m.is_dir() {
                if m.dev() != dev {
                    return Err(format!(
                        "another filesystem is mounted inside ({})",
                        p.display()
                    ));
                }
                // The worktree's own `.git` (a file) is never walked; a `.git`
                // folder at its root would be odd but is just git data.
                stack.push(p);
            }
        }
    }
    Ok(bytes)
}

/// Linux bind mounts can share the device number of the folder they sit in,
/// so also ask the kernel for every mount point.
#[cfg(target_os = "linux")]
fn mounts_inside(dir: &Path) -> Result<(), String> {
    let Ok(info) = fs::read_to_string("/proc/self/mountinfo") else {
        return Err("cannot read /proc/self/mountinfo to rule out mounts inside".into());
    };
    for line in info.lines() {
        let Some(mp) = line.split(' ').nth(4) else {
            continue;
        };
        let mp = unescape_mountinfo(mp);
        if Path::new(&mp).starts_with(dir) {
            return Err(format!("a filesystem is mounted inside ({mp})"));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unescape_mountinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// macOS has no bind mounts; every mount has its own device number, which
/// the walk already compares.
#[cfg(not(target_os = "linux"))]
fn mounts_inside(_dir: &Path) -> Result<(), String> {
    Ok(())
}

/// [`real_target`]'s reason when the path is simply gone (callers compare
/// against this constant, never a reworded literal).
pub(crate) const GONE: &str = "no longer exists";

/// No other repository may sit between `root` (a project or worktree) and
/// `path`: an artifact inside a nested repo belongs to that repo, whose
/// idleness and ignore rules were never checked.
pub(crate) fn no_repo_between(root: &Path, path: &Path) -> Result<(), String> {
    for a in path.ancestors().skip(1) {
        if a == root || !a.starts_with(root) {
            break;
        }
        if fs::symlink_metadata(a.join(".git")).is_ok() {
            return Err(format!(
                "it now belongs to a nested repository ({})",
                a.display()
            ));
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(GONE.into()),
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

/// Is `path` (inside `repo_dir`) still ignored by git, with nothing inside it
/// tracked? (`git check-ignore` calls a folder ignored even when files in it
/// were force-added, so `git ls-files` must come back empty too.)
pub(crate) fn still_ignored(ctx: &Ctx<'_>, repo_dir: &Path, path: &Path) -> Result<(), String> {
    let git = ctx.git()?;
    let got = artifacts::ignored_subset(git, repo_dir, &[path.to_path_buf()]);
    if !got.iter().any(|p| p == path) {
        return Err("git no longer ignores it".into());
    }
    let rel = path
        .strip_prefix(repo_dir)
        .map_err(|_| "not inside its repository".to_string())?;
    let mut spec = std::ffi::OsString::from(":(literal)");
    spec.push(rel.as_os_str());
    let out = git.run(
        repo_dir,
        [
            std::ffi::OsStr::new("ls-files"),
            std::ffi::OsStr::new("-z"),
            std::ffi::OsStr::new("--"),
            spec.as_os_str(),
        ],
    );
    if !out.success {
        return Err("could not list the files git tracks in it".into());
    }
    if !out.stdout.is_empty() {
        return Err("it contains files tracked by git".into());
    }
    Ok(())
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
    fn protection_also_applies_to_the_resolved_path() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        // ~/.cache is a symlink to / : ~/.cache/usr really is /usr.
        std::os::unix::fs::symlink("/", home.join(".cache")).unwrap();
        assert!(protected(&home.join(".cache/usr"), &home).is_err());
        // ~/link-home -> ~ : ~/link-home/x is fine, but a link to ~'s parent isn't.
        std::os::unix::fs::symlink(&base, home.join("up")).unwrap();
        assert!(protected(&home.join("up/home"), &home).is_err());
        assert!(protected(&home.join("real/cache"), &home).is_ok());
    }

    #[test]
    fn nested_repos_between_project_and_target_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("mono");
        let nm = root.join("tools/cli/node_modules");
        fs::create_dir_all(&nm).unwrap();
        assert!(no_repo_between(&root, &nm).is_ok());
        fs::write(root.join(".git"), "gitdir: x\n").unwrap(); // the root's own is fine
        assert!(no_repo_between(&root, &nm).is_ok());
        fs::create_dir_all(root.join("tools/cli/.git")).unwrap();
        let e = no_repo_between(&root, &nm).unwrap_err();
        assert!(e.contains("nested repository"), "{e}");
    }

    #[test]
    fn worktree_inspection_allows_only_its_own_git_file() {
        let t = tempfile::tempdir().unwrap();
        let wt = t.path().join("wt");
        fs::create_dir_all(wt.join("node_modules/x")).unwrap();
        fs::write(wt.join(".git"), "gitdir: /r/.git/worktrees/wt\n").unwrap();
        assert!(inspect_worktree(&wt).is_ok());
        fs::write(wt.join("node_modules/x/.git"), "gitdir: y\n").unwrap();
        assert!(inspect_worktree(&wt).unwrap_err().contains("git checkout"));
    }

    #[test]
    fn inspection_fails_closed() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("node_modules");
        fs::create_dir_all(d.join("a/b")).unwrap();
        fs::write(d.join("a/b/f"), vec![0u8; 5000]).unwrap();
        assert!(inspect_tree(&d, true).unwrap() >= 5000);
        // A linked worktree / submodule: `.git` is a file.
        fs::write(d.join("a/.git"), "gitdir: /elsewhere\n").unwrap();
        let e = inspect_tree(&d, true).unwrap_err();
        assert!(e.contains("git checkout"), "{e}");
        assert!(
            inspect_tree(&d, false).is_ok(),
            "caches may hold .git entries"
        );
        fs::remove_file(d.join("a/.git")).unwrap();
        // An unreadable folder can't be vouched for.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(d.join("a/b"), fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fs::read_dir(d.join("a/b")).is_ok(); // root can read anyway
        let r = inspect_tree(&d, true);
        fs::set_permissions(d.join("a/b"), fs::Permissions::from_mode(0o755)).unwrap();
        if !readable {
            assert!(r.unwrap_err().contains("can't be read"));
        }
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
