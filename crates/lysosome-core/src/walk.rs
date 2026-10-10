//! Parallel, read-only directory discovery.
//!
//! [`walk`] visits every directory under a set of roots (up to a depth limit)
//! on the rayon thread pool and hands each one, with its full listing, to a
//! caller-supplied visitor. The visitor decides what to record and whether to
//! descend. Scanners (worktrees now, project artifacts in Milestone 2) build on
//! this instead of writing their own walks.
//!
//! Guarantees:
//! * Symlinks are never followed (entries are typed with `lstat` semantics).
//! * `.git` directories are never descended into.
//! * Directories named in the skip set are never descended into.
//! * Unreadable directories are skipped and counted, never fatal.
//!
//! Why not `ignore`/`jwalk`: the worktree scanner needs to see a directory's
//! complete listing (is `.git` a file or a folder?) before deciding to descend,
//! exactly like the prototype's `os.walk`. A ~100-line rayon walker gives that
//! with no extra dependencies and work-stealing parallelism.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::progress::Progress;

/// Folders never worth descending into when hunting for repositories
/// (identical to `SKIP_DIRS` in `reference/wtscan.py`).
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".venv",
    "venv",
    "target",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".gradle",
    "Pods",
    "DerivedData",
    "__pycache__",
    "Library",
    ".Trash",
    "Applications",
    "Movies",
    "Music",
    "Pictures",
    ".cache",
    ".npm",
    ".pnpm-store",
    ".yarn",
    ".bun",
    ".cargo",
    ".rustup",
    ".ollama",
    ".lmstudio",
    ".docker",
    ".orbstack",
    ".gem",
    ".m2",
    ".android",
    ".local",
    ".vscode",
    ".cursor",
    ".Spotlight-V100",
];

/// The type of a directory entry, determined without following symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
    Other,
}

impl From<fs::FileType> for EntryKind {
    fn from(ft: fs::FileType) -> Self {
        if ft.is_symlink() {
            EntryKind::Symlink
        } else if ft.is_dir() {
            EntryKind::Dir
        } else if ft.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        }
    }
}

/// One entry of a directory listing.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: OsString,
    pub kind: EntryKind,
}

/// A directory being visited, with its complete listing.
#[derive(Debug)]
pub struct DirVisit<'a> {
    /// The root this directory was reached from.
    pub root: &'a Path,
    pub path: &'a Path,
    /// 0 for the root itself.
    pub depth: usize,
    pub entries: &'a [Entry],
}

impl DirVisit<'_> {
    /// Look up an entry by name.
    pub fn entry(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }
}

/// The visitor's verdict for a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Descend {
    /// Visit this directory's eligible subdirectories.
    Yes,
    /// Don't look inside this directory any further.
    No,
}

/// Walk configuration.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Directories at this depth are listed but not descended into (root = 0).
    pub max_depth: usize,
    /// Directory names never descended into. `.git` is always skipped.
    pub skip_dirs: Vec<OsString>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        WalkOptions {
            max_depth: 8,
            skip_dirs: SKIP_DIRS.iter().map(OsString::from).collect(),
        }
    }
}

/// Counters from a walk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkStats {
    pub dirs_visited: u64,
    pub permission_denied: u64,
    pub other_errors: u64,
}

struct Ctx<'a, F> {
    opts: &'a WalkOptions,
    visit: &'a F,
    progress: &'a dyn Progress,
    dirs: AtomicU64,
    denied: AtomicU64,
    errors: AtomicU64,
}

impl<F> Ctx<'_, F> {
    fn record_error(&self, e: &io::Error) {
        if e.kind() == io::ErrorKind::PermissionDenied {
            self.denied.fetch_add(1, Ordering::Relaxed);
        } else if e.kind() != io::ErrorKind::NotFound {
            // NotFound = deleted while we walked; not worth reporting.
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn skip(&self, name: &OsStr) -> bool {
        name == ".git" || self.opts.skip_dirs.iter().any(|s| s == name)
    }
}

/// Walk `roots` in parallel, calling `visit` once per directory (roots included).
///
/// `visit` runs concurrently on worker threads; collect results through
/// a `Mutex` or channel. Each root should already be an absolute, canonical
/// directory path: descendants are reported as `root.join(...)`.
pub fn walk<F>(
    roots: &[PathBuf],
    opts: &WalkOptions,
    progress: &dyn Progress,
    visit: F,
) -> WalkStats
where
    F: Fn(&DirVisit<'_>) -> Descend + Sync,
{
    let ctx = Ctx {
        opts,
        visit: &visit,
        progress,
        dirs: AtomicU64::new(0),
        denied: AtomicU64::new(0),
        errors: AtomicU64::new(0),
    };
    rayon::scope(|s| {
        for root in roots {
            let ctx = &ctx;
            s.spawn(move |s| visit_dir(s, ctx, root, root.clone(), 0));
        }
    });
    WalkStats {
        dirs_visited: ctx.dirs.into_inner(),
        permission_denied: ctx.denied.into_inner(),
        other_errors: ctx.errors.into_inner(),
    }
}

fn visit_dir<'s, F>(
    scope: &rayon::Scope<'s>,
    ctx: &'s Ctx<'s, F>,
    root: &'s Path,
    path: PathBuf,
    depth: usize,
) where
    F: Fn(&DirVisit<'_>) -> Descend + Sync,
{
    let entries = match read_entries(&path) {
        Ok(entries) => entries,
        Err(e) => {
            ctx.record_error(&e);
            return;
        }
    };
    ctx.dirs.fetch_add(1, Ordering::Relaxed);
    ctx.progress.advance(1);

    let verdict = (ctx.visit)(&DirVisit {
        root,
        path: &path,
        depth,
        entries: &entries,
    });
    if verdict == Descend::No || depth >= ctx.opts.max_depth {
        return;
    }
    for e in entries {
        if e.kind == EntryKind::Dir && !ctx.skip(&e.name) {
            let child = path.join(&e.name);
            scope.spawn(move |s| visit_dir(s, ctx, root, child, depth + 1));
        }
    }
}

/// List a directory without following symlinks. Entries that vanish or can't
/// be typed are dropped.
pub fn read_entries(path: &Path) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(path)? {
        let Ok(entry) = entry else { continue };
        let Ok(ft) = entry.file_type() else { continue };
        out.push(Entry {
            name: entry.file_name(),
            kind: ft.into(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoProgress;
    use std::sync::Mutex;

    fn collect(root: &Path, opts: &WalkOptions) -> Vec<(PathBuf, usize)> {
        let seen = Mutex::new(Vec::new());
        walk(&[root.to_path_buf()], opts, &NoProgress, |v| {
            seen.lock().unwrap().push((v.path.to_path_buf(), v.depth));
            Descend::Yes
        });
        let mut v = seen.into_inner().unwrap();
        v.sort();
        v
    }

    #[test]
    fn respects_depth_skips_and_never_follows_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("a/b/c/d")).unwrap();
        fs::create_dir_all(root.join("node_modules/x")).unwrap();
        fs::create_dir_all(root.join("repo/.git/objects")).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("link")).unwrap();

        let opts = WalkOptions {
            max_depth: 2,
            ..WalkOptions::default()
        };
        let got = collect(&root, &opts);
        let rel: Vec<_> = got
            .iter()
            .map(|(p, d)| (p.strip_prefix(&root).unwrap().to_path_buf(), *d))
            .collect();
        assert!(rel.contains(&(PathBuf::from(""), 0)));
        assert!(rel.contains(&(PathBuf::from("a/b"), 2)));
        // depth limit: a/b is listed but not descended
        assert!(!rel.iter().any(|(p, _)| p == Path::new("a/b/c")));
        // skip set and .git
        assert!(!rel.iter().any(|(p, _)| p.starts_with("node_modules")));
        assert!(!rel.iter().any(|(p, _)| p.starts_with("repo/.git")));
        // symlinks are not followed
        assert!(!rel.iter().any(|(p, _)| p.starts_with("link")));
    }

    #[test]
    fn descend_no_stops_recursion() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("stop/inner")).unwrap();
        let seen = Mutex::new(Vec::new());
        walk(&[root.clone()], &WalkOptions::default(), &NoProgress, |v| {
            seen.lock().unwrap().push(v.path.to_path_buf());
            if v.path.ends_with("stop") {
                Descend::No
            } else {
                Descend::Yes
            }
        });
        let seen = seen.into_inner().unwrap();
        assert!(seen.contains(&root.join("stop")));
        assert!(!seen.contains(&root.join("stop/inner")));
    }
}
