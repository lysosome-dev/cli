//! Repository discovery: one walk of the roots finds main repos, linked
//! worktrees and candidate artifact folders; then main repos' worktree records
//! are read. The worktree scanner and the project artifact scanner share it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use crate::artifacts;
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
    /// Linked worktrees (sorted by path).
    pub candidates: Vec<Candidate>,
    pub stale: Vec<StaleMetadata>,
    pub walk: WalkStats,
    /// Folders with a `.git` directory found by the walk.
    pub mains: BTreeSet<PathBuf>,
    /// Every folder with a `.git` entry of any type (main repos, worktrees,
    /// submodules, separate-git-dir repos): repository boundaries.
    pub repo_roots: BTreeSet<PathBuf>,
    /// Folders named like an artifact (`ARTIFACT_DIRS`) seen by the walk, not
    /// yet attributed to a repository or ignore-checked. Symlinks excluded.
    pub artifact_candidates: Vec<PathBuf>,
    /// Subfolders of folders at the depth limit: listed, never visited. The
    /// project scanner continues the artifact search below them inside repos.
    pub frontier: Vec<PathBuf>,
}

#[derive(Default)]
struct Found {
    worktrees: BTreeMap<PathBuf, PathBuf>,
    mains: BTreeSet<PathBuf>,
    repo_roots: BTreeSet<PathBuf>,
    artifacts: Vec<PathBuf>,
    frontier: Vec<PathBuf>,
}

/// Discover repositories under `roots` (absolute, canonical directories).
pub(crate) fn discover(roots: &[PathBuf], max_depth: usize, progress: &dyn Progress) -> Discovery {
    let opts = WalkOptions {
        max_depth,
        ..WalkOptions::default()
    };
    discover_with(roots, &opts, progress)
}

/// [`discover`] with explicit walk options. Nothing inside `opts.skip_paths`
/// is read, including worktrees reached through pointers from outside.
fn discover_with(roots: &[PathBuf], opts: &WalkOptions, progress: &dyn Progress) -> Discovery {
    let max_depth = opts.max_depth;
    let skip = &opts.skip_paths;
    let found = Mutex::new(Found::default());
    let stats = walk::walk(roots, opts, progress, |v| {
        let arts: Vec<PathBuf> = v
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::Dir && artifacts::is_artifact_dir(&e.name))
            .map(|e| v.path.join(&e.name))
            .collect();
        if !arts.is_empty() {
            lock(&found).artifacts.extend(arts);
        }
        if v.depth >= max_depth {
            let deeper: Vec<PathBuf> = v
                .entries
                .iter()
                .filter(|e| e.kind == EntryKind::Dir && !walk::is_skipped(&e.name))
                .map(|e| v.path.join(&e.name))
                .collect();
            if !deeper.is_empty() {
                lock(&found).frontier.extend(deeper);
            }
        }
        if let Some(e) = v.entry(".git") {
            lock(&found).repo_roots.insert(v.path.to_path_buf());
            let dotgit = v.path.join(".git");
            match dotgit_kind(e.kind) {
                DotGit::File => {
                    // A worktree whose repository is in a skipped folder is
                    // left out unread: its git dir lives there.
                    let gd = read_gitdir_pointer(&dotgit).filter(|gd| !walk::is_within(gd, skip));
                    if let Some(gd) = gd {
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
        repo_roots,
        mut artifacts,
        frontier,
    } = found.into_inner().unwrap_or_else(|e| e.into_inner());
    artifacts.sort();

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
            // A worktree placed in a skipped folder isn't looked at (or called stale).
            if walk::is_within(&wt_path, skip) {
                continue;
            }
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
                    cleanup: Cleanup::command(vec![
                        "git".into(),
                        "-C".into(),
                        main.to_string_lossy().into_owned(),
                        "worktree".into(),
                        "prune".into(),
                    ]),
                });
            }
        }
    }

    Discovery {
        candidates: worktrees
            .into_iter()
            .filter(|(path, gitdir)| !walk::is_within(path, skip) && !walk::is_within(gitdir, skip))
            .map(|(path, gitdir)| Candidate { path, gitdir })
            .collect(),
        stale,
        walk: stats,
        mains,
        repo_roots,
        artifact_candidates: artifacts,
        frontier,
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
pub(crate) fn is_linked_worktree_gitdir(gd: &Path) -> bool {
    gd.parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "worktrees")
}

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
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
    fn worktree_pointers_into_skipped_folders_are_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().canonicalize().unwrap();
        let docs = r.join("Documents");
        let mk = |p: PathBuf, body: String| {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        };
        // A main repo outside, with one worktree inside the skipped folder (w1)
        // and one outside (w3).
        fs::create_dir_all(r.join("code/main/.git/worktrees")).unwrap();
        for (name, wt) in [("w1", docs.join("w1")), ("w3", r.join("wts/w3"))] {
            let meta = r.join("code/main/.git/worktrees").join(name);
            mk(
                meta.join("gitdir"),
                format!("{}\n", wt.join(".git").display()),
            );
            mk(wt.join(".git"), format!("gitdir: {}\n", meta.display()));
        }
        // A worktree outside whose repository is inside the skipped folder.
        let inner = docs.join("repo/.git/worktrees/w2");
        mk(
            inner.join("gitdir"),
            format!("{}\n", r.join("wts/w2/.git").display()),
        );
        mk(
            r.join("wts/w2/.git"),
            format!("gitdir: {}\n", inner.display()),
        );

        let names = |skip: Vec<PathBuf>| {
            let opts = WalkOptions {
                skip_paths: skip,
                ..WalkOptions::default()
            };
            let d = discover_with(
                &[r.join("code"), r.join("wts")],
                &opts,
                &crate::progress::NoProgress,
            );
            assert!(d.stale.is_empty());
            let mut n: Vec<String> = d
                .candidates
                .iter()
                .map(|c| c.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            n.sort();
            n
        };
        assert_eq!(names(Vec::new()), ["w1", "w2", "w3"]);
        assert_eq!(names(vec![docs]), ["w3"]);
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
