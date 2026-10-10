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

use std::collections::HashSet;
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
    bytes: u64,
    dev: u64,
    ino: u64,
    nlink: u64,
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

/// Measures allocated size. One `Sizer` = one hard-link de-duplication domain
/// and one filesystem (that of the root it was created for).
#[derive(Debug)]
pub struct Sizer {
    dev: u64,
    /// Sharded so parallel workers rarely contend (pnpm stores hard-link everything).
    seen: Vec<Mutex<HashSet<(u64, u64)>>>,
    errors: AtomicU64,
}

impl Sizer {
    /// Create a sizer bound to the filesystem that contains `root`.
    pub fn new(root: &Path) -> io::Result<Self> {
        let meta = std::fs::symlink_metadata(root)?;
        Ok(Sizer {
            dev: meta.dev(),
            seen: (0..SHARDS).map(|_| Mutex::new(HashSet::new())).collect(),
            errors: AtomicU64::new(0),
        })
    }

    /// Entries that could not be read.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// `bytes`, or 0 if this hard-linked inode was already counted.
    fn account(&self, bytes: u64, dev: u64, ino: u64, nlink: u64) -> u64 {
        if nlink > 1 {
            let key = (dev, ino);
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
        let total = self.dir(root, 0, true, decide, &parts);
        let mut parts = parts.into_inner().unwrap_or_else(|e| e.into_inner());
        parts.sort();
        SplitSize { total, parts }
    }

    fn dir<F>(
        &self,
        path: &Path,
        depth: usize,
        splitting: bool,
        decide: &F,
        parts: &Mutex<Vec<(PathBuf, u64)>>,
    ) -> u64
    where
        F: Fn(&Path, &OsStr, usize) -> Split + Sync,
    {
        let listing = match list_dir(path) {
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
        let mut subdirs: Vec<(PathBuf, Split)> = Vec::new();
        for c in &listing.children {
            if c.is_dir {
                let split = if splitting {
                    decide(path, &c.name, depth + 1)
                } else {
                    Split::InlineNoSplit
                };
                subdirs.push((path.join(&c.name), split));
            } else {
                total += self.account(c.bytes, c.dev, c.ino, c.nlink);
            }
        }
        let recurse = |(p, split): &(PathBuf, Split)| match split {
            Split::Inline => self.dir(p, depth + 1, true, decide, parts),
            Split::InlineNoSplit => self.dir(p, depth + 1, false, decide, parts),
            Split::Separate => {
                let bytes = self.dir(p, depth + 1, false, decide, parts);
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

#[cfg(target_os = "macos")]
fn list_dir(path: &Path) -> io::Result<Listing> {
    bulk::list(path)
}

#[cfg(not(target_os = "macos"))]
fn list_dir(path: &Path) -> io::Result<Listing> {
    portable_list(path)
}

/// `readdir` + `lstat`, for every Unix.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn portable_list(path: &Path) -> io::Result<Listing> {
    let meta = std::fs::symlink_metadata(path)?;
    let mut listing = Listing {
        dev: meta.dev(),
        self_bytes: meta.blocks() * 512,
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

    pub(super) fn list(path: &Path) -> io::Result<Listing> {
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
        // SAFETY: zeroed stat is a valid out-parameter for fstat.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fd is open; st is a valid pointer.
        if unsafe { libc::fstat(fd.0, &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut listing = Listing {
            dev: st.st_dev as u64,
            self_bytes: st.st_blocks as u64 * 512,
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
            dirattr: 0,
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
            bytes: if is_dir { 0 } else { bytes },
            dev,
            ino,
            nlink: if is_dir { 1 } else { nlink },
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
        let a = key(list_dir(root).unwrap());
        let b = key(portable_list(root).unwrap());
        assert_eq!(a.0, b.0, "device");
        assert_eq!(a.1.len(), b.1.len());
        for (x, y) in a.1.iter().zip(&b.1) {
            assert_eq!(x.0, y.0);
            assert_eq!(x.1, y.1, "is_dir for {:?}", x.0);
            assert_eq!(x.3, y.3, "ino for {:?}", x.0);
            assert_eq!(x.4, y.4, "nlink for {:?}", x.0);
            if !x.1 {
                assert_eq!(x.2, y.2, "bytes for {:?}", x.0);
            }
        }
    }
}
