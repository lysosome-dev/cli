//! Milestone 3: machine-wide disk consumers that live outside your projects —
//! package-manager caches, local AI models, Docker, Xcode, Android emulators
//! and editor caches (docs/SPEC.md §4).
//!
//! One file per detector family; [`detectors`] returns all of them. Every
//! detector:
//! * checks that its location exists before doing anything else;
//! * asks the owning tool where its data lives when it can (`brew --cache`,
//!   `go env GOMODCACHE`, `docker system df`...) through the timeout-guarded
//!   [`tools::Tools`] runner, falling back to well-known paths;
//! * never fails the scan because a tool is missing, slow or broken;
//! * is read-only: it lists, `lstat`s and sizes, and never writes.
//!
//! Finding shape (see `schema/lysosome.schema.json`): `details.name` is the
//! human name (a model name, an iOS version...), `regenerable` /
//! `regenerable_expensive` findings are `REMOVABLE` and carry either the tool's
//! own cleanup command or a trash path, `review` findings are `REVIEW` with no
//! cleanup. `regenerable_expensive` findings add `details.redownload_bytes`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rayon::prelude::*;
use serde_json::{Map, Value};

use crate::detector::{Detector, ScanContext};
use crate::error::Result;
use crate::model::{Category, Cleanup, Finding, Status, Tier};
use crate::size::Sizer;

mod ai;
mod android;
mod containers;
mod editors;
mod packages;
pub mod tools;
#[cfg(target_os = "macos")]
mod xcode;

pub use tools::Tools;

/// Environment the system detectors read: variables such as `HF_HOME` or
/// `ANDROID_HOME`, and the `PATH` used to find tools. Tests build an isolated
/// one instead of inheriting the process environment.
#[derive(Debug, Clone)]
pub struct SystemEnv {
    vars: HashMap<String, OsString>,
    /// Timeout for ordinary "where is your cache?" tool calls.
    pub tool_timeout: Duration,
    /// Timeout for quick liveness probes (is the Docker daemon up?).
    pub probe_timeout: Duration,
}

impl SystemEnv {
    /// Snapshot of the current process environment. `LYSOSOME_TOOL_TIMEOUT_MS`
    /// overrides both the tool and the probe timeouts.
    pub fn from_process() -> Self {
        let vars: HashMap<String, OsString> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v)))
            .collect();
        let overridden = vars
            .get("LYSOSOME_TOOL_TIMEOUT_MS")
            .and_then(|v| v.to_str()?.parse::<u64>().ok())
            .map(Duration::from_millis);
        SystemEnv {
            vars,
            tool_timeout: overridden.unwrap_or(tools::DEFAULT_TOOL_TIMEOUT),
            probe_timeout: overridden.unwrap_or(tools::DEFAULT_PROBE_TIMEOUT),
        }
    }

    /// Only the given variables (tests).
    pub fn isolated<K, V>(vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<OsString>,
    {
        SystemEnv {
            vars: vars
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            tool_timeout: tools::DEFAULT_TOOL_TIMEOUT,
            probe_timeout: tools::DEFAULT_PROBE_TIMEOUT,
        }
    }

    /// A non-empty variable.
    pub fn var(&self, key: &str) -> Option<&OsString> {
        self.vars.get(key).filter(|v| !v.is_empty())
    }

    /// A non-empty variable holding an absolute path.
    pub fn var_path(&self, key: &str) -> Option<PathBuf> {
        self.var(key).map(PathBuf::from).filter(|p| p.is_absolute())
    }

    /// Tool runner using this environment's `PATH` and the scan's home.
    pub fn tools(&self, home: &Path) -> Tools {
        Tools::new(
            self.var("PATH").cloned(),
            Some(home.to_path_buf()),
            self.tool_timeout,
        )
    }

    /// `$XDG_CACHE_HOME`, else `~/.cache`.
    pub fn xdg_cache(&self, home: &Path) -> PathBuf {
        self.var_path("XDG_CACHE_HOME")
            .unwrap_or_else(|| home.join(".cache"))
    }
}

type ScanFn = fn(&SystemEnv, &ScanContext<'_>) -> Vec<Finding>;

/// A system detector: an id, a category and a scan function.
struct SystemDetector {
    id: &'static str,
    category: Category,
    env: Arc<SystemEnv>,
    scan: ScanFn,
}

impl Detector for SystemDetector {
    fn id(&self) -> &'static str {
        self.id
    }

    fn category(&self) -> Category {
        self.category
    }

    fn scan(&self, ctx: &ScanContext<'_>) -> Result<Vec<Finding>> {
        Ok((self.scan)(&self.env, ctx))
    }
}

/// Every system detector, reading the process environment.
pub fn detectors() -> Vec<Box<dyn Detector>> {
    detectors_with(Arc::new(SystemEnv::from_process()))
}

/// Every system detector, reading `env` (tests pass an isolated one).
pub fn detectors_with(env: Arc<SystemEnv>) -> Vec<Box<dyn Detector>> {
    let mut table: Vec<(&'static str, Category, ScanFn)> = Vec::new();
    #[cfg(target_os = "macos")]
    table.extend(xcode::TABLE);
    table.extend(ai::TABLE);
    table.extend(containers::TABLE);
    table.extend(packages::TABLE);
    table.extend(android::TABLE);
    table.extend(editors::TABLE);
    table
        .into_iter()
        .map(|(id, category, scan)| {
            Box::new(SystemDetector {
                id,
                category,
                env: env.clone(),
                scan,
            }) as Box<dyn Detector>
        })
        .collect()
}

/// Result of [`run`].
#[derive(Debug, Default)]
pub struct RunOutput {
    /// All findings, largest first.
    pub findings: Vec<Finding>,
    /// Wall time of each detector, in detector order.
    pub timings: Vec<(&'static str, Duration)>,
}

/// Run `detectors` concurrently (one thread each: many block on tool
/// subprocesses; sizing inside them is parallel on rayon). A detector that
/// returns an error contributes nothing.
pub fn run(detectors: &[Box<dyn Detector>], ctx: &ScanContext<'_>) -> RunOutput {
    let results: Vec<(&'static str, Duration, Vec<Finding>)> = std::thread::scope(|s| {
        let handles: Vec<_> = detectors
            .iter()
            .map(|d| {
                s.spawn(move || {
                    let start = std::time::Instant::now();
                    let f = d.scan(ctx).unwrap_or_default();
                    (d.id(), start.elapsed(), f)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut out = RunOutput::default();
    for (id, t, f) in results {
        out.timings.push((id, t));
        out.findings.extend(f);
    }
    out.findings.sort_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

// ---------------------------------------------------------------------------
// Building findings

/// Everything needed to turn a location into a [`Finding`].
#[derive(Debug, Clone)]
pub(crate) struct Item {
    pub kind: &'static str,
    pub category: Category,
    pub path: PathBuf,
    /// Human name shown instead of the path (`llama3.1:8b`, `iOS 17.5`...).
    pub name: String,
    pub tier: Tier,
    /// The tool's own cleanup argv. `None` means "move `path` to the Trash".
    pub command: Option<Vec<String>>,
    /// Extra paths trashed together with `path` (e.g. an AVD's `.ini`).
    pub extra_trash: Vec<PathBuf>,
    pub reason: String,
    /// Known size; `None` means "measure `path`".
    pub size: Option<u64>,
    /// Known last use. `None` with an unknown size means "derive from
    /// timestamps under `path`"; `None` with a known size means unknown.
    pub last_activity: Option<SystemTime>,
    /// Kind-specific details merged into `details`.
    pub details: Map<String, Value>,
    /// Bytes of a re-download (defaults to the size).
    pub redownload_bytes: Option<u64>,
    /// Keep the finding even at 0 bytes (an Ollama tag whose layers are all
    /// shared frees nothing on its own but must still be listed by name).
    pub keep_empty: bool,
}

impl Item {
    pub fn new(
        kind: &'static str,
        category: Category,
        path: PathBuf,
        name: impl Into<String>,
        tier: Tier,
    ) -> Self {
        Item {
            kind,
            category,
            path,
            name: name.into(),
            tier,
            command: None,
            extra_trash: Vec::new(),
            reason: String::new(),
            size: None,
            last_activity: None,
            details: Map::new(),
            redownload_bytes: None,
            keep_empty: false,
        }
    }

    pub fn command<S: Into<String>>(mut self, argv: impl IntoIterator<Item = S>) -> Self {
        self.command = Some(argv.into_iter().map(Into::into).collect());
        self
    }

    pub fn reason(mut self, r: impl Into<String>) -> Self {
        self.reason = r.into();
        self
    }

    pub fn size(mut self, bytes: u64) -> Self {
        self.size = Some(bytes);
        self
    }

    pub fn last_activity(mut self, t: Option<SystemTime>) -> Self {
        self.last_activity = t;
        self
    }

    pub fn detail(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.details.insert(key.to_string(), value.into());
        self
    }
}

/// Measure (in parallel) and convert items. Items whose path doesn't exist or
/// that measure 0 bytes are dropped (unless `keep_empty`).
pub(crate) fn finish(items: Vec<Item>) -> Vec<Finding> {
    items
        .into_par_iter()
        .filter_map(|mut item| {
            if item.size.is_none() {
                // Timestamps first: sizing lists folders, which can bump atimes.
                if item.last_activity.is_none() {
                    item.last_activity = last_used(&item.path);
                }
                let (bytes, unreadable) = measure(&item.path)?;
                if unreadable > 0 {
                    item.details
                        .insert("unreadable_entries".into(), unreadable.into());
                }
                item.size = Some(bytes);
            }
            let size = item.size.unwrap_or(0);
            (size > 0 || item.keep_empty).then(|| to_finding(item, size))
        })
        .collect()
}

fn to_finding(item: Item, size: u64) -> Finding {
    let review = item.tier == Tier::Review;
    let mut details = Map::new();
    details.insert("name".into(), item.name.clone().into());
    if item.tier == Tier::RegenerableExpensive {
        details.insert(
            "redownload_bytes".into(),
            item.redownload_bytes.unwrap_or(size).into(),
        );
    }
    details.extend(item.details);
    let cleanup = if review {
        None
    } else {
        Some(match item.command {
            Some(argv) => Cleanup::command(argv),
            None => {
                let mut paths = vec![item.path.clone()];
                paths.extend(item.extra_trash);
                Cleanup::trash(paths)
            }
        })
    };
    Finding {
        id: format!("{}:{}", item.kind, item.path.to_string_lossy()),
        category: item.category,
        kind: item.kind.to_string(),
        path: item.path,
        size_bytes: size,
        tier: item.tier,
        status: if review {
            Status::Review
        } else {
            Status::Removable
        },
        reason: item.reason,
        last_activity: item.last_activity,
        details: Value::Object(details),
        cleanup,
    }
}

/// `(allocated bytes, unreadable entries)` of a path, or `None` if it doesn't exist.
pub(crate) fn measure(path: &Path) -> Option<(u64, u64)> {
    let sizer = Sizer::new(path).ok()?;
    let bytes = sizer.size_of(path);
    Some((bytes, sizer.errors()))
}

// ---------------------------------------------------------------------------
// Filesystem helpers (never follow symlinks)

/// `path` if it is a real directory (not a symlink to one).
pub(crate) fn real_dir(path: PathBuf) -> Option<PathBuf> {
    std::fs::symlink_metadata(&path)
        .is_ok_and(|m| m.is_dir())
        .then_some(path)
}

/// First candidate that is a real directory.
pub(crate) fn first_dir(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find_map(real_dir)
}

/// Real subdirectories of `dir`, sorted by name.
pub(crate) fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// Regular files directly inside `dir`, sorted by name.
pub(crate) fn files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// Newest of mtime (and, for files, atime) of one entry.
pub(crate) fn touched(path: &Path) -> Option<SystemTime> {
    let m = std::fs::symlink_metadata(path).ok()?;
    let mtime = m.modified().ok();
    let atime = if m.is_file() { m.accessed().ok() } else { None };
    mtime.into_iter().chain(atime).max()
}

/// Last use of a location: newest mtime of it and its direct children, and
/// atime of its direct child files. Directory atimes are ignored because
/// listing a folder (including our own scan) updates them.
pub(crate) fn last_used(path: &Path) -> Option<SystemTime> {
    let own = touched(path);
    let Ok(rd) = std::fs::read_dir(path) else {
        return own;
    };
    rd.flatten()
        .filter_map(|e| touched(&e.path()))
        .chain(own)
        .max()
}

pub(crate) fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Parse sizes as Docker prints them: `0B`, `428.5kB`, `1.262GB`, `2 MB`.
/// Decimal units, like Docker.
pub(crate) fn parse_human_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1e0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "pb" => 1e15,
        _ => return None,
    };
    Some((n * mult).round() as u64)
}

/// Parse timestamps like Docker prints them: `2026-04-28 21:21:29 -0400 EDT`
/// or `2026-04-04 00:19:24.806932346 +0000 UTC`. Also accepts RFC 3339.
pub(crate) fn parse_timestamp(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    if let Ok(t) = humantime::parse_rfc3339_weak(s) {
        return Some(t);
    }
    let mut parts = s.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let offset = parts.next().unwrap_or("+0000");
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, mo, da) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let time = time.split('.').next()?;
    let mut t = time.split(':').map(|x| x.parse::<i64>());
    let (h, mi, se) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    let (sign, off) = match offset.as_bytes().first()? {
        b'+' => (1, &offset[1..]),
        b'-' => (-1, &offset[1..]),
        _ => (1, "0000"),
    };
    let off = off.replace(':', "");
    let off_secs = if off.len() == 4 {
        sign * (off[..2].parse::<i64>().ok()? * 3600 + off[2..].parse::<i64>().ok()? * 60)
    } else {
        0
    };
    let days = days_from_civil(y, mo, da);
    let secs = days * 86_400 + h * 3600 + mi * 60 + se - off_secs;
    u64::try_from(secs)
        .ok()
        .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s))
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::io::Write;

    pub fn write(p: &Path, bytes: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(&vec![1u8; bytes]).unwrap();
    }

    pub fn ctx(home: &Path) -> ScanContext<'static> {
        ScanContext::new(Vec::new(), home.to_path_buf())
    }

    /// An environment with no PATH (no tools) and nothing else set.
    pub fn bare_env() -> SystemEnv {
        SystemEnv::isolated(Vec::<(String, String)>::new())
    }

    pub fn by_kind<'a>(f: &'a [Finding], kind: &str) -> Vec<&'a Finding> {
        f.iter().filter(|x| x.kind == kind).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_sizes() {
        assert_eq!(parse_human_bytes("0B"), Some(0));
        assert_eq!(parse_human_bytes("428.5kB"), Some(428_500));
        assert_eq!(parse_human_bytes("1.262GB"), Some(1_262_000_000));
        assert_eq!(parse_human_bytes("2 MB"), Some(2_000_000));
        assert_eq!(parse_human_bytes("nope"), None);
    }

    #[test]
    fn parses_docker_timestamps() {
        let t = parse_timestamp("2026-07-01 06:00:00 -0400 EDT").unwrap();
        assert_eq!(
            humantime::format_rfc3339_seconds(t).to_string(),
            "2026-07-01T10:00:00Z"
        );
        let t = parse_timestamp("2026-04-04 00:19:24.806932346 +0000 UTC").unwrap();
        assert_eq!(
            humantime::format_rfc3339_seconds(t).to_string(),
            "2026-04-04T00:19:24Z"
        );
        let t = parse_timestamp("2026-09-27T22:36:43Z").unwrap();
        assert_eq!(
            humantime::format_rfc3339_seconds(t).to_string(),
            "2026-09-27T22:36:43Z"
        );
        assert!(parse_timestamp("garbage").is_none());
    }

    #[test]
    fn review_items_never_carry_cleanup_and_trash_is_the_default() {
        let t = tempfile::tempdir().unwrap();
        testutil::write(&t.path().join("a/f"), 5000);
        testutil::write(&t.path().join("b/f"), 5000);
        let items = vec![
            Item::new(
                "x",
                Category::Editors,
                t.path().join("a"),
                "a",
                Tier::Regenerable,
            ),
            Item::new(
                "y",
                Category::Editors,
                t.path().join("b"),
                "b",
                Tier::Review,
            )
            .command(["rm", "-rf", "/"]),
            Item::new(
                "z",
                Category::Editors,
                t.path().join("missing"),
                "c",
                Tier::Regenerable,
            ),
        ];
        let f = finish(items);
        assert_eq!(f.len(), 2, "missing paths are dropped");
        let a = f.iter().find(|x| x.kind == "x").unwrap();
        assert_eq!(a.status, Status::Removable);
        let c = a.cleanup.as_ref().unwrap();
        assert_eq!(c.paths, vec![t.path().join("a")]);
        assert!(a.last_activity.is_some());
        let b = f.iter().find(|x| x.kind == "y").unwrap();
        assert_eq!(b.status, Status::Review);
        assert!(b.cleanup.is_none());
    }

    #[test]
    fn missing_tools_are_not_errors() {
        let t = tempfile::tempdir().unwrap();
        let env = testutil::bare_env();
        let tools = env.tools(t.path());
        assert!(tools.which("docker").is_none());
        assert!(tools.output("docker", ["version"]).is_none());
        // Every detector runs cleanly against an empty home with no tools.
        let dets = detectors_with(Arc::new(env));
        assert!(dets.len() >= 20);
        let ctx = testutil::ctx(t.path());
        assert!(run(&dets, &ctx).findings.is_empty());
    }
}
