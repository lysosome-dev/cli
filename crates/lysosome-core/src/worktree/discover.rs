//! Worktree discovery: walk the roots, then read main repos' worktree records.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use crate::model::{Cleanup, StaleMetadata};
use crate::progress::Progress;
use crate::walk::{self, Descend, EntryKind, WalkOptions, WalkStats};

/// A worktree folder and the gitdir it belongs to (which may not exist: orphan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub path: PathBuf,
    pub gitdir: PathBuf,
}

#[derive(Debug, Default)]
pub(crate) struct Discovery {
    pub candidates: Vec<Candidate>,
    pub stale: Vec<StaleMetadata>,
    pub walk: WalkStats,
}

#[derive(Default)]
struct Found {
    worktrees: BTreeMap<PathBuf, PathBuf>,
    mains: BTreeSet<PathBuf>,
}

/// Discover worktrees under `roots` (absolute, canonical directories).
pub(crate) fn discover(roots: &[PathBuf], max_depth: usize, progress: &dyn Progress) -> Discovery {
    let found = Mutex::new(Found::default());
    let opts = WalkOptions {
        max_depth,
        ..WalkOptions::default()
    };
    let stats = walk::walk(roots, &opts, progress, |v| {
        if let Some(e) = v.entry(".git") {
            let dotgit = v.path.join(".git");
            match dotgit_kind(e.kind) {
                DotGit::File => {
                    if let Some(gd) = read_gitdir_pointer(&dotgit) {
                        // Submodules (`.git/modules/x`) and `--separate-git-dir`
                        // repos also use .git files; keep only linked worktrees.
                        if is_linked_worktree_gitdir(&gd) {
                            lock(&found).worktrees.insert(v.path.to_path_buf(), gd);
                        }
                    }
                }
                DotGit::Dir => {
                    lock(&found).mains.insert(v.path.to_path_buf());
                }
                DotGit::Neither => {}
            }
        }
        Descend::Yes
    });

    let Found {
        mut worktrees,
        mains,
    } = found.into_inner().unwrap_or_else(|e| e.into_inner());

    // Main repos also know their worktrees, even ones outside the scan roots.
    let mut stale = Vec::new();
    for main in &mains {
        let meta_root = main.join(".git").join("worktrees");
        let Ok(rd) = fs::read_dir(&meta_root) else {
            continue;
        };
        let mut names: Vec<_> = rd.flatten().map(|e| e.file_name()).collect();
        names.sort();
        for name in names {
            let meta = meta_root.join(&name);
            let Ok(raw) = fs::read_to_string(meta.join("gitdir")) else {
                continue;
            };
            let dotgit = absolutize(&meta, Path::new(raw.trim()));
            let wt_path = dotgit.parent().map(Path::to_path_buf).unwrap_or_default();
            if wt_path.is_dir() {
                // Only trust the record if the folder's .git still points back
                // at it; otherwise `git -C` would analyze whatever encloses it.
                let points_back = read_gitdir_pointer(&wt_path.join(".git"))
                    .is_some_and(|gd| same_path(&gd, &meta));
                if points_back {
                    let key = wt_path.canonicalize().unwrap_or(wt_path);
                    worktrees.entry(key).or_insert(meta);
                }
            } else if meta.join("locked").exists() {
                // Locked records (e.g. worktrees on an unplugged drive) are
                // deliberately kept by `git worktree prune`; not stale.
            } else {
                stale.push(StaleMetadata {
                    repo: main.clone(),
                    name: name.to_string_lossy().into_owned(),
                    missing_path: wt_path,
                    cleanup: Cleanup {
                        command: vec![
                            "git".into(),
                            "-C".into(),
                            main.to_string_lossy().into_owned(),
                            "worktree".into(),
                            "prune".into(),
                        ],
                    },
                });
            }
        }
    }

    Discovery {
        candidates: worktrees
            .into_iter()
            .map(|(path, gitdir)| Candidate { path, gitdir })
            .collect(),
        stale,
        walk: stats,
    }
}

enum DotGit {
    File,
    Dir,
    Neither,
}

/// Classify a `.git` entry. Symlinks are never followed (safety rule 6), so a
/// symlinked `.git` is ignored.
fn dotgit_kind(kind: EntryKind) -> DotGit {
    match kind {
        EntryKind::File => DotGit::File,
        EntryKind::Dir => DotGit::Dir,
        EntryKind::Symlink | EntryKind::Other => DotGit::Neither,
    }
}

/// A linked worktree's gitdir is `<common>/worktrees/<name>`.
fn is_linked_worktree_gitdir(gd: &Path) -> bool {
    gd.parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "worktrees")
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
}

/// A worktree's `.git` is a file: `gitdir: /repo/.git/worktrees/<name>`.
pub(crate) fn read_gitdir_pointer(dotgit_file: &Path) -> Option<PathBuf> {
    let raw = fs::read(dotgit_file).ok()?;
    let text = String::from_utf8_lossy(&raw);
    let line = text.lines().next()?.trim();
    let p = line.strip_prefix("gitdir:")?.trim();
    Some(absolutize(dotgit_file.parent()?, Path::new(p)))
}

/// Join `p` onto `base` if relative, then normalize lexically (like `os.path.normpath`).
pub(crate) fn absolutize(base: &Path, p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    normalize(&joined)
}

/// Lexical normalization: drop `.`, resolve `..` against the preceding component.
pub(crate) fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_worktree_gitdirs_are_recognized_structurally() {
        assert!(is_linked_worktree_gitdir(Path::new("/r/.git/worktrees/x")));
        // main repo under a folder called "modules" still works
        assert!(is_linked_worktree_gitdir(Path::new(
            "/tf/modules/app/.git/worktrees/x"
        )));
        assert!(!is_linked_worktree_gitdir(Path::new("/r/.git/modules/sub")));
        assert!(!is_linked_worktree_gitdir(Path::new(
            "/worktrees/store/foo.git"
        )));
    }

    #[test]
    fn normalizes_like_normpath() {
        assert_eq!(
            normalize(Path::new("/r/.git/worktrees/x/../..")),
            PathBuf::from("/r/.git")
        );
        assert_eq!(
            absolutize(Path::new("/a/b"), Path::new("../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }

    #[test]
    fn reads_relative_and_absolute_pointers() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join(".git");
        fs::write(&f, "gitdir: ../repo/.git/worktrees/wt\n").unwrap();
        assert_eq!(
            read_gitdir_pointer(&f).unwrap(),
            normalize(&t.path().join("../repo/.git/worktrees/wt"))
        );
        fs::write(&f, "not a pointer").unwrap();
        assert!(read_gitdir_pointer(&f).is_none());
    }
}
