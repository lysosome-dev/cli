//! In-process, parallel disk-usage measurement.
//!
//! * Counts **allocated** bytes (`st_blocks * 512`, like `du`), not file length.
//! * Counts each hard-linked inode once per [`Sizer`] (keyed by `(dev, ino)`).
//! * Never follows symlinks and never crosses into another filesystem.
//! * Unreadable entries are skipped and counted in [`Sizer::errors`].
//!
//! APFS clones share blocks but report full allocation each, so cloned files
//! are over-counted. That is acceptable for v1 (docs/SPEC.md §3).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use rayon::prelude::*;

/// How [`Sizer::measure_split`] should treat a subdirectory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    /// Count it as part of the parent and keep asking about its children.
    Inline,
    /// Count it as part of the parent; don't ask about anything beneath it.
    InlineNoSplit,
    /// Measure it as a separate part (still included in the total).
    Separate,
}

/// Result of [`Sizer::measure_split`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitSize {
    /// Allocated bytes of the whole tree (parts included).
    pub total: u64,
    /// Separately measured subdirectories and their sizes.
    pub parts: Vec<(PathBuf, u64)>,
}

/// Measures allocated size. One `Sizer` = one hard-link de-duplication domain
/// and one filesystem (that of the root it was created for).
#[derive(Debug)]
pub struct Sizer {
    dev: u64,
    seen: Mutex<HashSet<(u64, u64)>>,
    errors: AtomicU64,
}

impl Sizer {
    /// Create a sizer bound to the filesystem that contains `root`.
    pub fn new(root: &Path) -> std::io::Result<Self> {
        let meta = fs::symlink_metadata(root)?;
        Ok(Sizer {
            dev: meta.dev(),
            seen: Mutex::new(HashSet::new()),
            errors: AtomicU64::new(0),
        })
    }

    /// Entries that could not be read.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Allocated bytes for one inode, or 0 if this hard link was already counted.
    fn account(&self, meta: &Metadata) -> u64 {
        if meta.nlink() > 1 && !meta.is_dir() {
            let key = (meta.dev(), meta.ino());
            if !self
                .seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key)
            {
                return 0;
            }
        }
        meta.blocks() * 512
    }

    /// Allocated size of `path` (file, symlink or whole directory tree).
    pub fn size_of(&self, path: &Path) -> u64 {
        self.measure_split(path, &|_, _, _| Split::InlineNoSplit)
            .total
    }

    /// Measure the tree at `root`, letting `decide(parent, name, depth)` pick
    /// subdirectories to report separately. `depth` is 1 for children of `root`.
    pub fn measure_split<F>(&self, root: &Path, decide: &F) -> SplitSize
    where
        F: Fn(&Path, &OsStr, usize) -> Split + Sync,
    {
        let parts = Mutex::new(Vec::new());
        let meta = match fs::symlink_metadata(root) {
            Ok(m) => m,
            Err(_) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                return SplitSize::default();
            }
        };
        let total = if meta.is_dir() {
            self.dir(root, &meta, 0, true, decide, &parts)
        } else {
            self.account(&meta)
        };
        let mut parts = parts.into_inner().unwrap_or_else(|e| e.into_inner());
        parts.sort();
        SplitSize { total, parts }
    }

    fn dir<F>(
        &self,
        path: &Path,
        meta: &Metadata,
        depth: usize,
        splitting: bool,
        decide: &F,
        parts: &Mutex<Vec<(PathBuf, u64)>>,
    ) -> u64
    where
        F: Fn(&Path, &OsStr, usize) -> Split + Sync,
    {
        if meta.dev() != self.dev {
            return 0; // another filesystem (mount point): not ours to count
        }
        let mut total = self.account(meta);
        let rd = match fs::read_dir(path) {
            Ok(rd) => rd,
            Err(_) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                return total;
            }
        };
        let mut subdirs: Vec<(PathBuf, Metadata, Split)> = Vec::new();
        for entry in rd {
            let Ok(entry) = entry else {
                self.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            // DirEntry::metadata does not follow symlinks.
            let Ok(m) = entry.metadata() else {
                self.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if m.is_dir() {
                let split = if splitting {
                    decide(path, &entry.file_name(), depth + 1)
                } else {
                    Split::InlineNoSplit
                };
                subdirs.push((entry.path(), m, split));
            } else {
                total += self.account(&m);
            }
        }
        total
            + subdirs
                .par_iter()
                .map(|(p, m, split)| match split {
                    Split::Inline => self.dir(p, m, depth + 1, true, decide, parts),
                    Split::InlineNoSplit => self.dir(p, m, depth + 1, false, decide, parts),
                    Split::Separate => {
                        let bytes = self.dir(p, m, depth + 1, false, decide, parts);
                        parts
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((p.clone(), bytes));
                        bytes
                    }
                })
                .sum::<u64>()
    }
}

/// Allocated size of `path` with a fresh de-duplication domain.
pub fn allocated_size(path: &Path) -> u64 {
    Sizer::new(path).map(|s| s.size_of(path)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(p: &Path, bytes: usize) {
        let mut f = fs::File::create(p).unwrap();
        f.write_all(&vec![7u8; bytes]).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn counts_allocated_bytes_and_dedupes_hard_links() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        write_file(&root.join("a"), 1 << 20);
        let one = allocated_size(&root.join("a"));
        assert!(one >= 1 << 20, "allocated {one} < 1MiB");
        fs::hard_link(root.join("a"), root.join("b")).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("c")).unwrap();
        let total = allocated_size(root);
        // a and b share an inode; the symlink is tiny and not followed.
        assert!(
            total < 2 * one,
            "hard link double-counted: {total} vs {one}"
        );
    }

    #[test]
    fn split_reports_parts_inside_total() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        write_file(&root.join("src/main.rs"), 4096);
        write_file(&root.join("node_modules/pkg/big"), 256 * 1024);
        let s = Sizer::new(root).unwrap();
        let out = s.measure_split(root, &|_, name, _| {
            if name == "node_modules" {
                Split::Separate
            } else {
                Split::Inline
            }
        });
        assert_eq!(out.parts.len(), 1);
        assert_eq!(out.parts[0].0, root.join("node_modules"));
        assert!(out.parts[0].1 >= 256 * 1024);
        assert!(out.total > out.parts[0].1);
    }
}
