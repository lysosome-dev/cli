//! The data model shared by every scanner and the versioned JSON contract
//! (`schema/lysosome.schema.json`, docs/SPEC.md §6).
//!
//! Every scanner — worktrees today, artifacts / caches / AI models later —
//! produces [`Finding`]s. A [`Report`] collects findings from any number of
//! scanners and derives the [`Summary`]. Changes to these types must be
//! *additive* within a schema major version.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::{SCHEMA_VERSION, TOOL_VERSION};

/// What kind of disk consumer a finding is. Serialized in `snake_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Worktrees,
    Artifacts,
    AiModels,
    Xcode,
    PackageCaches,
    Containers,
    Editors,
    Android,
}

impl Category {
    /// Every category, in display order. `summary.by_category` always lists all of them.
    pub const ALL: [Category; 8] = [
        Category::Worktrees,
        Category::Artifacts,
        Category::AiModels,
        Category::Xcode,
        Category::PackageCaches,
        Category::Containers,
        Category::Editors,
        Category::Android,
    ];
}

/// How safe an item is to delete (docs/SPEC.md §4, Milestone 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Cheap to rebuild (caches, DerivedData, a clean+merged worktree). One-click safe.
    Regenerable,
    /// Rebuildable but costly to get back (models, images, `node_modules`).
    RegenerableExpensive,
    /// We can't be sure it's rebuildable. A human must look.
    Review,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Regenerable, Tier::RegenerableExpensive, Tier::Review];
}

/// Classification status. Serialized in `SCREAMING_SNAKE_CASE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    /// Used recently; not a cleanup candidate.
    Active,
    /// Dormant and provably safe to remove.
    Removable,
    /// Dormant but holding something that might matter (dirty, unpushed, locked...).
    Review,
    /// The owning repository is gone; git can no longer manage it.
    Orphan,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Active => "ACTIVE",
            Status::Removable => "REMOVABLE",
            Status::Review => "REVIEW",
            Status::Orphan => "ORPHAN",
        }
    }

    /// Anything not `ACTIVE` is a cleanup candidate ("dormant").
    pub fn is_dormant(self) -> bool {
        self != Status::Active
    }
}

/// The command a user (or Milestone 4 `clean`) would run to remove a finding,
/// as an argv vector (no shell quoting needed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cleanup {
    pub command: Vec<String>,
}

/// One thing on disk that lysosome found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable identifier: `<kind>:<path>`.
    pub id: String,
    pub category: Category,
    /// Finer-grained type within the category (`"worktree"`, `"node_modules"`, `"ollama-model"`...).
    pub kind: String,
    #[serde(with = "lossy_path")]
    pub path: PathBuf,
    /// Allocated bytes on disk (like `du`), hard links counted once.
    pub size_bytes: u64,
    pub tier: Tier,
    pub status: Status,
    /// Human-readable explanation of the status. Empty for `ACTIVE`.
    pub reason: String,
    /// Last time this was used, if known.
    #[serde(with = "rfc3339_opt")]
    pub last_activity: Option<SystemTime>,
    /// Kind-specific details (free-form object; documented per kind in the schema).
    pub details: serde_json::Value,
    /// How to remove it. `null` whenever removal is not recommended.
    pub cleanup: Option<Cleanup>,
}

/// A `.git/worktrees/<name>` record whose worktree folder no longer exists.
/// Harmless; `git worktree prune` removes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleMetadata {
    /// The main repository that holds the record.
    #[serde(with = "lossy_path")]
    pub repo: PathBuf,
    /// Record name under `.git/worktrees/`.
    pub name: String,
    /// The worktree path the record points at (missing).
    #[serde(with = "lossy_path")]
    pub missing_path: PathBuf,
    pub cleanup: Cleanup,
}

/// Things the scan could not look at. Never fatal; always reported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    /// Folders skipped because reading them was not permitted.
    pub permission_denied: u64,
    /// Folders skipped for any other I/O error.
    pub other_errors: u64,
    /// Requested scan roots that don't exist or aren't folders.
    #[serde(with = "lossy_paths")]
    pub missing_roots: Vec<PathBuf>,
}

/// Aggregates over findings.
///
/// * `reclaimable_bytes` counts only `REMOVABLE` findings: space you can get back
///   right now with no risk.
/// * `by_tier` and `by_category` sum the sizes of all *dormant* findings (every
///   status except `ACTIVE`). Every tier and category key is always present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub reclaimable_bytes: u64,
    pub by_tier: BTreeMap<Tier, u64>,
    pub by_category: BTreeMap<Category, u64>,
}

impl Summary {
    pub fn from_findings(findings: &[Finding]) -> Self {
        let mut by_tier: BTreeMap<Tier, u64> = Tier::ALL.iter().map(|t| (*t, 0)).collect();
        let mut by_category: BTreeMap<Category, u64> =
            Category::ALL.iter().map(|c| (*c, 0)).collect();
        let mut reclaimable_bytes = 0u64;
        for f in findings {
            if f.status == Status::Removable {
                reclaimable_bytes += f.size_bytes;
            }
            if f.status.is_dormant() {
                *by_tier.entry(f.tier).or_default() += f.size_bytes;
                *by_category.entry(f.category).or_default() += f.size_bytes;
            }
        }
        Summary {
            reclaimable_bytes,
            by_tier,
            by_category,
        }
    }
}

/// The top-level `--json` document (schema version [`SCHEMA_VERSION`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub tool_version: String,
    #[serde(with = "lossy_paths")]
    pub scanned_roots: Vec<PathBuf>,
    pub dormant_after_days: u64,
    pub summary: Summary,
    pub findings: Vec<Finding>,
    pub stale_metadata: Vec<StaleMetadata>,
    pub skipped: Skipped,
}

impl Report {
    /// Build a report; the summary is derived from `findings`.
    pub fn new(
        scanned_roots: Vec<PathBuf>,
        dormant_after_days: u64,
        findings: Vec<Finding>,
        stale_metadata: Vec<StaleMetadata>,
        skipped: Skipped,
    ) -> Self {
        Report {
            schema_version: SCHEMA_VERSION,
            tool_version: TOOL_VERSION.to_string(),
            scanned_roots,
            dormant_after_days,
            summary: Summary::from_findings(&findings),
            findings,
            stale_metadata,
            skipped,
        }
    }
}

/// Paths serialize as strings; bytes that aren't valid UTF-8 become U+FFFD
/// instead of failing the whole report.
pub(crate) mod lossy_path {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::path::{Path, PathBuf};

    pub fn serialize<S: Serializer>(p: &Path, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&p.to_string_lossy())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        PathBuf::deserialize(d)
    }
}

mod lossy_paths {
    use serde::ser::SerializeSeq;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::path::PathBuf;

    pub fn serialize<S: Serializer>(v: &[PathBuf], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for p in v {
            seq.serialize_element(&p.to_string_lossy())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<PathBuf>, D::Error> {
        Vec::<PathBuf>::deserialize(d)
    }
}

/// `Option<SystemTime>` <-> RFC 3339 UTC string (`2026-07-01T10:00:00Z`) or `null`.
mod rfc3339_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::SystemTime;

    pub fn serialize<S: Serializer>(t: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.collect_str(&humantime::format_rfc3339_seconds(*t)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SystemTime>, D::Error> {
        let raw: Option<String> = Option::deserialize(d)?;
        raw.map(|r| humantime::parse_rfc3339_weak(&r).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn finding(status: Status, tier: Tier, size: u64) -> Finding {
        Finding {
            id: "worktree:/x".into(),
            category: Category::Worktrees,
            kind: "worktree".into(),
            path: "/x".into(),
            size_bytes: size,
            tier,
            status,
            reason: String::new(),
            last_activity: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_782_900_000)),
            details: serde_json::json!({}),
            cleanup: None,
        }
    }

    #[test]
    fn summary_counts_removable_as_reclaimable_and_ignores_active() {
        let s = Summary::from_findings(&[
            finding(Status::Removable, Tier::Regenerable, 100),
            finding(Status::Review, Tier::Review, 40),
            finding(Status::Active, Tier::Review, 1000),
        ]);
        assert_eq!(s.reclaimable_bytes, 100);
        assert_eq!(s.by_tier[&Tier::Regenerable], 100);
        assert_eq!(s.by_tier[&Tier::Review], 40);
        assert_eq!(s.by_tier[&Tier::RegenerableExpensive], 0);
        assert_eq!(s.by_category[&Category::Worktrees], 140);
        assert_eq!(s.by_category.len(), Category::ALL.len());
    }

    #[test]
    fn non_utf8_paths_serialize_lossily_instead_of_failing() {
        use std::os::unix::ffi::OsStrExt;
        let mut f = finding(Status::Review, Tier::Review, 1);
        f.path = PathBuf::from(std::ffi::OsStr::from_bytes(b"/home/u/\xff/wt"));
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["path"], "/home/u/\u{fffd}/wt");
    }

    #[test]
    fn serializes_enums_and_timestamps_per_contract() {
        let v = serde_json::to_value(finding(Status::Removable, Tier::RegenerableExpensive, 1))
            .unwrap();
        assert_eq!(v["status"], "REMOVABLE");
        assert_eq!(v["tier"], "regenerable_expensive");
        assert_eq!(v["category"], "worktrees");
        assert_eq!(v["last_activity"], "2026-07-01T10:00:00Z");
        assert!(v["cleanup"].is_null());
        let back: Finding = serde_json::from_value(v).unwrap();
        assert_eq!(back.status, Status::Removable);
    }
}
