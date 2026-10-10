//! In-process, parallel disk-usage measurement.
//!
//! * Counts **allocated** bytes (`st_blocks * 512`, like `du`), not file length.
//! * Counts each hard-linked inode once per [`Sizer`] (keyed by `(dev, ino)`).
//! * Never follows symlinks and never crosses into another filesystem.
//! * Unreadable entries are skipped and counted in [`Sizer::errors`].
//!
//! On macOS each directory is listed with `getattrlistbulk(2)`, which returns
//! names, types, inode numbers and allocated sizes for a whole batch of entries
//! in one syscall instead of one `lstat` per file. Elsewhere it's
//! `readdir` + `lstat`.
//!
//! APFS clones share blocks but report full allocation each, so cloned files
//! are over-counted. That is acceptable for v1 (docs/SPEC.md §3).

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
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

/// One directory entry with what sizing needs.
#[derive(Debug, Clone)]
struct Child {
    name: OsString,
    is_dir: bool,
    /// Allocated bytes (for a directory: the directory itself, when the
    /// listing could tell; see `own`).
    bytes: u64,
    dev: u64,
    ino: u64,
    nlink: u64,
    /// For a directory: `(dev, bytes)` of the directory itself when the
    /// parent's listing already has them, so listing it needs no `fstat`.
    own: Option<(u64, u64)>,
    /// For a directory: its entry count, when known (0 = nothing to open).
    entries: Option<u32>,
    /// A directory another filesystem is mounted on.
    mount_point: bool,
}

/// A directory listing: the directory's own device and allocation, plus children.
#[derive(Debug, Default)]
struct Listing {
    dev: u64,
    self_bytes: u64,
    children: Vec<Child>,
    /// Entries that could not be read.
    errors: u64,
}

const SHARDS: usize = 64;

/// A subdirectory to measure: path, how to treat it, and its own
/// `(dev, bytes)` when the parent's listing had them.
type Subdir = (PathBuf, Split, Option<(u64, u64)>);

/// Measures allocated size. One `Sizer` = one hard-link de-duplication domain
/// and one filesystem (that of the root it was created for).
#[derive(Debug)]
pub struct Sizer {
    dev: u64,
    /// Sharded so parallel workers rarely contend (pnpm stores hard-link everything).
    seen: Vec<Mutex<HashSet<(u64, u64)>>>,
    /// [`Sizer::collecting`]: hard-linked inodes and their bytes, kept out of
    /// the totals so the caller can attribute them deterministically.
    links: Option<Vec<Mutex<Links>>>,
    errors: AtomicU64,
}

/// Hard-linked inodes (`nlink > 1`) found by a [`Sizer::collecting`] sizer:
/// `(dev, ino) -> allocated bytes`, each inode once.
pub(crate) type Links = HashMap<(u64, u64), u64>;

/// Attribute hard-linked inodes across independently measured trees, in the
/// given order: each inode's bytes go to the first tree that holds it. With
/// trees sorted by path this is deterministic, however the trees were
/// measured (e.g. in parallel). Returns each tree's linked bytes.
pub(crate) fn attribute_links<'a>(trees: impl IntoIterator<Item = &'a Links>) -> Vec<u64> {
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    trees
        .into_iter()
        .map(|links| {
            links
                .iter()
                .filter(|(k, _)| seen.insert(**k))
                .map(|(_, b)| *b)
                .sum()
        })
        .collect()
}

impl Sizer {
    /// Create a sizer bound to the filesystem that contains `root`.
    pub fn new(root: &Path) -> io::Result<Self> {
        let meta = std::fs::symlink_metadata(root)?;
        Ok(Sizer {
            dev: meta.dev(),
            seen: (0..SHARDS).map(|_| Mutex::new(HashSet::new())).collect(),
            links: None,
            errors: AtomicU64::new(0),
        })
    }

    /// Like [`Sizer::new`], but hard-linked files are left out of every
    /// total and collected instead (see [`Sizer::take_links`] and
    /// [`attribute_links`]), so several trees can be measured in parallel
    /// and still credit each shared inode to one of them deterministically.
    /// Totals only: `measure_split` parts exclude hard-linked bytes too and
    /// can't be attributed back per part.
    pub(crate) fn collecting(root: &Path) -> io::Result<Self> {
        let mut s = Sizer::new(root)?;
        s.seen = Vec::new(); // never consulted in collecting mode
        s.links = Some((0..SHARDS).map(|_| Mutex::new(Links::new())).collect());
        Ok(s)
    }

    /// The hard-linked inodes a [`Sizer::collecting`] sizer has seen.
    pub(crate) fn take_links(&self) -> Links {
        let mut out = Links::new();
        for shard in self.links.iter().flatten() {
            out.extend(shard.lock().unwrap_or_else(|e| e.into_inner()).drain());
        }
        out
    }

    /// Entries that could not be read.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// `bytes`, or 0 if this hard-linked inode was already counted.
    fn account(&self, bytes: u64, dev: u64, ino: u64, nlink: u64) -> u64 {
        if nlink > 1 {
            let key = (dev, ino);
            if let Some(links) = &self.links {
                let shard = &links[(key.1 ^ key.0) as usize % SHARDS];
                shard
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, bytes);
                return 0;
            }
            let shard = &self.seen[(key.1 ^ key.0) as usize % SHARDS];
            if !shard.lock().unwrap_or_else(|e| e.into_inner()).insert(key) {
                return 0;
            }
        }
        bytes
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
        let meta = match std::fs::symlink_metadata(root) {
            Ok(m) => m,
            Err(_) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                return SplitSize::default();
            }
        };
        if !meta.is_dir() {
            return SplitSize {
                total: self.account(meta.blocks() * 512, meta.dev(), meta.ino(), meta.nlink()),
                parts: Vec::new(),
            };
        }
        let parts = Mutex::new(Vec::new());
        let total = self.dir(root, None, 0, true, decide, &parts);
        let mut parts = parts.into_inner().unwrap_or_else(|e| e.into_inner());
        parts.sort();
        SplitSize { total, parts }
    }

    fn dir<F>(
        &self,
        path: &Path,
        own: Option<(u64, u64)>,
        depth: usize,
        splitting: bool,
        decide: &F,
        parts: &Mutex<Vec<(PathBuf, u64)>>,
    ) -> u64
    where
        F: Fn(&Path, &OsStr, usize) -> Split + Sync,
    {
        let listing = match list_dir(path, own) {
            Ok(l) => l,
            Err(_) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                return 0;
            }
        };
        if listing.dev != self.dev {
            return 0; // another filesystem (mount point): not ours to count
        }
        if listing.errors > 0 {
            self.errors.fetch_add(listing.errors, Ordering::Relaxed);
        }
        let mut total = listing.self_bytes;
        let mut subdirs: Vec<Subdir> = Vec::new();
        for c in &listing.children {
            if c.is_dir {
                if c.mount_point {
                    continue; // another filesystem: not ours to count
                }
                let split = if splitting {
                    decide(path, &c.name, depth + 1)
                } else {
                    Split::InlineNoSplit
                };
                if c.entries == Some(0) && c.own.is_some() {
                    // Empty: its own allocation is all there is; no need to
                    // open it.
                    let bytes = c.own.map_or(0, |o| o.1);
                    total += bytes;
                    if split == Split::Separate {
                        parts
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((path.join(&c.name), bytes));
                    }
                    continue;
                }
                subdirs.push((path.join(&c.name), split, c.own));
            } else if c.dev == self.dev {
                total += self.account(c.bytes, c.dev, c.ino, c.nlink);
            }
            // (A file on another device means we are inside a filesystem
            // mounted where the parent's listing didn't say so: not ours.)
        }
        let recurse = |(p, split, own): &Subdir| match split {
            Split::Inline => self.dir(p, *own, depth + 1, true, decide, parts),
            Split::InlineNoSplit => self.dir(p, *own, depth + 1, false, decide, parts),
            Split::Separate => {
                let bytes = self.dir(p, *own, depth + 1, false, decide, parts);
                parts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((p.clone(), bytes));
                bytes
            }
        };
        total
            + if subdirs.len() > 1 {
                subdirs.par_iter().map(recurse).sum::<u64>()
            } else {
                subdirs.iter().map(recurse).sum::<u64>()
            }
    }
}

/// Allocated size of `path` with a fresh de-duplication domain.
pub fn allocated_size(path: &Path) -> u64 {
    Sizer::new(path).map(|s| s.size_of(path)).unwrap_or(0)
}

/// List `path`. `own`: the directory's `(dev, allocated bytes)` when the
/// parent's listing already had them (saves a `stat`).
#[cfg(target_os = "macos")]
fn list_dir(path: &Path, own: Option<(u64, u64)>) -> io::Result<Listing> {
    match bulk::list(path, own) {
        // Some filesystems (network, FAT, ...) don't implement getattrlistbulk.
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) => {
            portable_list(path, own)
        }
        other => other,
    }
}

#[cfg(not(target_os = "macos"))]
fn list_dir(path: &Path, own: Option<(u64, u64)>) -> io::Result<Listing> {
    portable_list(path, own)
}

/// `readdir` + `lstat`, for every Unix (and the macOS fallback).
fn portable_list(path: &Path, own: Option<(u64, u64)>) -> io::Result<Listing> {
    let (dev, self_bytes) = match own {
        Some(o) => o,
        None => {
            let meta = std::fs::symlink_metadata(path)?;
            (meta.dev(), meta.blocks() * 512)
        }
    };
    let mut listing = Listing {
        dev,
        self_bytes,
        ..Listing::default()
    };
    for entry in std::fs::read_dir(path)? {
        let Ok(entry) = entry else {
            listing.errors += 1;
            continue;
        };
        // DirEntry::metadata does not follow symlinks.
        let Ok(m) = entry.metadata() else {
            listing.errors += 1;
            continue;
        };
        let is_dir = m.is_dir();
        listing.children.push(Child {
            name: entry.file_name(),
            is_dir,
            bytes: m.blocks() * 512,
            dev: m.dev(),
            ino: m.ino(),
            // Directories are never de-duplicated (their link count means subdirs).
            nlink: if is_dir { 1 } else { m.nlink() },
            own: is_dir.then(|| (m.dev(), m.blocks() * 512)),
            entries: None,
            // Its own device differs from ours: a mount point.
            mount_point: is_dir && m.dev() != dev,
        });
    }
    Ok(listing)
}

#[cfg(target_os = "macos")]
mod bulk {
    //! `getattrlistbulk(2)` directory listing (see the man page's example for
    //! the packed buffer layout this parses).

    use super::{Child, Listing};
    use std::cell::RefCell;
    use std::ffi::{CString, OsString};
    use std::io;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    const ATTR_CMN_ERROR: u32 = 0x2000_0000;
    const ATTR_DIR_ENTRYCOUNT: u32 = 0x0000_0002;
    const ATTR_DIR_MOUNTSTATUS: u32 = 0x0000_0004;
    const ATTR_DIR_ALLOCSIZE: u32 = 0x0000_0008;
    const DIR_MNTSTATUS_MNTPOINT: u32 = 0x0000_0001;
    /// An autofs trigger: something may be mounted there on access.
    const DIR_MNTSTATUS_TRIGGER: u32 = 0x0000_0002;
    const VDIR: u32 = 2;
    const BUF_LEN: usize = 128 * 1024;

    thread_local! {
        static BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; BUF_LEN]);
    }

    struct Fd(libc::c_int);
    impl Drop for Fd {
        fn drop(&mut self) {
            // SAFETY: we own this descriptor and close it exactly once.
            unsafe { libc::close(self.0) };
        }
    }

    fn rd_u32(b: &[u8], at: usize) -> Option<u32> {
        b.get(at..at + 4)
            .map(|s| u32::from_ne_bytes(s.try_into().unwrap_or_default()))
    }
    fn rd_i32(b: &[u8], at: usize) -> Option<i32> {
        rd_u32(b, at).map(|v| v as i32)
    }
    fn rd_u64(b: &[u8], at: usize) -> Option<u64> {
        b.get(at..at + 8)
            .map(|s| u64::from_ne_bytes(s.try_into().unwrap_or_default()))
    }

    pub(super) fn list(path: &Path, own: Option<(u64, u64)>) -> io::Result<Listing> {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: valid NUL-terminated path; flags never follow a symlink.
        let raw = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = Fd(raw);
        let (dev, self_bytes) = match own {
            Some(o) => o,
            None => {
                // SAFETY: zeroed stat is a valid out-parameter for fstat.
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                // SAFETY: fd is open; st is a valid pointer.
                if unsafe { libc::fstat(fd.0, &mut st) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                (st.st_dev as u64, st.st_blocks as u64 * 512)
            }
        };
        let mut listing = Listing {
            dev,
            self_bytes,
            ..Listing::default()
        };

        let mut al = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: libc::ATTR_CMN_RETURNED_ATTRS
                | libc::ATTR_CMN_NAME
                | ATTR_CMN_ERROR
                | libc::ATTR_CMN_DEVID
                | libc::ATTR_CMN_OBJTYPE
                | libc::ATTR_CMN_FILEID,
            volattr: 0,
            dirattr: ATTR_DIR_ENTRYCOUNT | ATTR_DIR_MOUNTSTATUS | ATTR_DIR_ALLOCSIZE,
            fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
            forkattr: 0,
        };

        BUF.with(|cell| -> io::Result<()> {
            let mut buf = cell.borrow_mut();
            loop {
                // SAFETY: fd is an open directory; `al` and `buf` are valid for
                // the sizes passed.
                let n = unsafe {
                    libc::getattrlistbulk(
                        fd.0,
                        (&mut al as *mut libc::attrlist).cast(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        0,
                    )
                };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(e);
                }
                if n == 0 {
                    return Ok(());
                }
                parse(&buf, n as usize, &mut listing);
            }
        })?;
        Ok(listing)
    }

    fn parse(buf: &[u8], count: usize, listing: &mut Listing) {
        let mut entry = 0usize;
        for _ in 0..count {
            let Some(len) = rd_u32(buf, entry) else {
                return;
            };
            let len = len as usize;
            if len == 0 || entry + len > buf.len() {
                return;
            }
            let rec = &buf[entry..entry + len];
            entry += len;
            if parse_one(rec, listing).is_none() {
                listing.errors += 1;
            }
        }
    }

    /// Parse one packed record. `None` = error entry or malformed record.
    fn parse_one(rec: &[u8], listing: &mut Listing) -> Option<()> {
        let mut f = 4;
        let common = rd_u32(rec, f)?;
        let dirattr = rd_u32(rec, f + 8)?;
        let fileattr = rd_u32(rec, f + 12)?;
        f += 20; // attribute_set_t

        if common & ATTR_CMN_ERROR != 0 {
            if rd_u32(rec, f)? != 0 {
                return None;
            }
            f += 4;
        }
        let mut name = OsString::new();
        if common & libc::ATTR_CMN_NAME != 0 {
            let off = rd_i32(rec, f)?;
            let nlen = rd_u32(rec, f + 4)? as usize;
            let start = (f as isize + off as isize) as usize;
            let bytes = rec.get(start..start + nlen)?;
            let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
            name = OsString::from_vec(bytes.to_vec());
            f += 8;
        }
        let mut dev = 0u64;
        if common & libc::ATTR_CMN_DEVID != 0 {
            dev = rd_i32(rec, f)? as u32 as u64;
            f += 4;
        }
        let mut objtype = 0u32;
        if common & libc::ATTR_CMN_OBJTYPE != 0 {
            objtype = rd_u32(rec, f)?;
            f += 4;
        }
        let mut ino = 0u64;
        if common & libc::ATTR_CMN_FILEID != 0 {
            ino = rd_u64(rec, f)?;
            f += 8;
        }
        // Directory attributes come before file attributes.
        let mut entries = None;
        if dirattr & ATTR_DIR_ENTRYCOUNT != 0 {
            entries = Some(rd_u32(rec, f)?);
            f += 4;
        }
        let mut mount_point = false;
        if dirattr & ATTR_DIR_MOUNTSTATUS != 0 {
            mount_point = rd_u32(rec, f)? & (DIR_MNTSTATUS_MNTPOINT | DIR_MNTSTATUS_TRIGGER) != 0;
            f += 4;
        }
        let mut dir_bytes = None;
        if dirattr & ATTR_DIR_ALLOCSIZE != 0 {
            dir_bytes = Some(rd_u64(rec, f)?);
            f += 8;
        }
        let mut nlink = 1u64;
        if fileattr & libc::ATTR_FILE_LINKCOUNT != 0 {
            nlink = rd_u32(rec, f)? as u64;
            f += 4;
        }
        let mut bytes = 0u64;
        if fileattr & libc::ATTR_FILE_ALLOCSIZE != 0 {
            bytes = rd_u64(rec, f)?;
        }
        if name.is_empty() || name == "." || name == ".." {
            return Some(());
        }
        let is_dir = objtype == VDIR;
        listing.children.push(Child {
            name,
            is_dir,
            bytes: if is_dir {
                dir_bytes.unwrap_or(0)
            } else {
                bytes
            },
            dev,
            ino,
            nlink: if is_dir { 1 } else { nlink },
            // Without its allocation, listing it will fstat as before.
            own: if is_dir {
                dir_bytes.map(|b| (dev, b))
            } else {
                None
            },
            entries: if is_dir { entries } else { None },
            mount_point: is_dir && mount_point,
        });
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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
        assert!(total >= one, "lost bytes: {total} < {one}");
        assert!(
            total < 2 * one,
            "hard link double-counted: {total} vs {one}"
        );
    }

    #[test]
    fn collected_links_are_attributed_to_the_first_tree() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        write_file(&a.join("own"), 10_000);
        write_file(&a.join("shared"), 50_000);
        std::fs::hard_link(a.join("shared"), b.join("shared")).unwrap();
        std::fs::hard_link(a.join("shared"), a.join("shared2")).unwrap();
        let measure = |p: &Path| {
            let s = Sizer::collecting(p).unwrap();
            let base = s.size_of(p);
            (base, s.take_links())
        };
        let (ba, la) = measure(&a);
        let (bb, lb) = measure(&b);
        assert_eq!(la.len(), 1, "one inode, counted once within a tree");
        let linked = attribute_links([&la, &lb]);
        assert!(linked[0] >= 50_000 && linked[1] == 0, "{linked:?}");
        // Same totals as one sequential de-duplicating sizer.
        let seq = Sizer::new(t.path()).unwrap();
        assert_eq!(ba + linked[0], seq.size_of(&a));
        assert_eq!(bb + linked[1], seq.size_of(&b));
        // The other order credits the other tree.
        let linked = attribute_links([&lb, &la]);
        assert!(linked[0] >= 50_000 && linked[1] == 0);
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

    /// The platform listing (getattrlistbulk on macOS) must agree with the
    /// portable readdir+lstat listing entry for entry.
    #[test]
    fn platform_listing_matches_portable() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        fs::create_dir_all(root.join("sub dir/ünïcode")).unwrap();
        for i in 0..300 {
            write_file(
                &root.join(format!("file-{i:03}-{}", "x".repeat(i % 50))),
                i * 97,
            );
        }
        fs::hard_link(root.join("file-010-xxxxxxxxxx"), root.join("hl")).unwrap();
        std::os::unix::fs::symlink("file-001-x", root.join("link")).unwrap();

        let key = |l: Listing| {
            let mut v: Vec<_> = l
                .children
                .into_iter()
                .map(|c| (c.name, c.is_dir, c.bytes, c.ino, c.nlink))
                .collect();
            v.sort();
            (l.dev, v)
        };
        fs::create_dir_all(root.join("empty")).unwrap();
        let a_raw = list_dir(root, None).unwrap();
        if cfg!(target_os = "macos") {
            let entries = |n: &str| {
                a_raw
                    .children
                    .iter()
                    .find(|c| c.name == n)
                    .and_then(|c| c.entries)
            };
            assert_eq!(entries("empty"), Some(0));
            assert_eq!(entries("sub dir"), Some(1));
        }
        let a = key(a_raw);
        let b = key(portable_list(root, None).unwrap());
        assert_eq!(a.0, b.0, "device");
        assert_eq!(a.1.len(), b.1.len());
        for (x, y) in a.1.iter().zip(&b.1) {
            assert_eq!(x.0, y.0);
            assert_eq!(x.1, y.1, "is_dir for {:?}", x.0);
            assert_eq!(x.3, y.3, "ino for {:?}", x.0);
            assert_eq!(x.4, y.4, "nlink for {:?}", x.0);
            // Directories too: their allocation comes from the parent's
            // listing instead of an fstat.
            assert_eq!(x.2, y.2, "bytes for {:?}", x.0);
        }
        // Whole-tree totals agree with a plain lstat walk.
        let walk = |p: &Path| -> u64 {
            fn go(p: &Path) -> u64 {
                let m = fs::symlink_metadata(p).unwrap();
                let mut n = m.blocks() * 512;
                if m.is_dir() {
                    for e in fs::read_dir(p).unwrap() {
                        n += go(&e.unwrap().path());
                    }
                }
                n
            }
            go(p)
        };
        fs::remove_file(root.join("hl")).unwrap();
        assert_eq!(Sizer::new(root).unwrap().size_of(root), walk(root));
    }
}
