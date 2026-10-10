//! The extension point every scanner implements. Each detector is a small
//! module implementing [`Detector`]; [`crate::detectors::run`] runs a set of
//! them concurrently and merges their output into one
//! [`Report`](crate::model::Report).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::model::{Category, Finding, Skipped, StaleMetadata};
use crate::progress::{NoProgress, Progress};
use crate::projects::{self, ProjectScan};
use crate::worktree;

/// Shared inputs for every detector. Build with [`ScanContext::new`].
pub struct ScanContext<'a> {
    /// Folders the user asked to scan (detectors with fixed locations may ignore them).
    pub roots: Vec<PathBuf>,
    /// The user's home directory (tests point this at a temp dir).
    pub home: PathBuf,
    pub dormant_after_days: u64,
    pub max_depth: usize,
    pub now: SystemTime,
    pub progress: &'a dyn Progress,
    /// The project walk, computed once and shared by the worktree and artifact detectors.
    projects: OnceLock<std::result::Result<Arc<ProjectScan>, Arc<Error>>>,
}

impl<'a> ScanContext<'a> {
    pub fn new(roots: Vec<PathBuf>, home: PathBuf) -> Self {
        ScanContext {
            roots,
            home,
            dormant_after_days: worktree::DEFAULT_DORMANT_DAYS,
            max_depth: worktree::DEFAULT_MAX_DEPTH,
            now: SystemTime::now(),
            progress: &NoProgress,
            projects: OnceLock::new(),
        }
    }

    /// The shared project scan of [`ScanContext::roots`] (worktrees + artifacts,
    /// one walk). The first caller runs it; concurrent callers wait for it.
    pub fn projects(&self) -> Result<Arc<ProjectScan>> {
        self.projects
            .get_or_init(|| {
                let mut opts = worktree::ScanOptions::new(self.roots.clone());
                opts.dormant_after_days = self.dormant_after_days;
                opts.max_depth = self.max_depth;
                opts.now = self.now;
                projects::scan(&opts, self.progress)
                    .map(Arc::new)
                    .map_err(Arc::new)
            })
            .clone()
            .map_err(Error::Shared)
    }

    /// The project scan, if some detector already ran it successfully.
    pub fn projects_if_scanned(&self) -> Option<Arc<ProjectScan>> {
        self.projects.get().and_then(|r| r.as_ref().ok().cloned())
    }
}

/// Everything a detector can contribute to a report.
#[derive(Debug, Clone, Default)]
pub struct DetectorOutput {
    pub findings: Vec<Finding>,
    pub stale_metadata: Vec<StaleMetadata>,
    /// What *this detector* could not read. The shared project walk's counts
    /// are added once by the runner; don't repeat them here.
    pub skipped: Skipped,
}

/// A source of findings.
pub trait Detector: Send + Sync {
    /// Stable identifier, e.g. `"worktrees"`, `"ollama"`, `"xcode-deriveddata"`.
    fn id(&self) -> &'static str;
    /// The category every finding from this detector belongs to.
    fn category(&self) -> Category;
    /// Find things. Must be read-only. Unreadable locations are skipped, not errors.
    fn scan(&self, ctx: &ScanContext<'_>) -> Result<Vec<Finding>>;
    /// Like [`Detector::scan`], plus stale metadata and skip counts. The default
    /// wraps `scan`; override only when there's more to say.
    fn run(&self, ctx: &ScanContext<'_>) -> Result<DetectorOutput> {
        Ok(DetectorOutput {
            findings: self.scan(ctx)?,
            ..DetectorOutput::default()
        })
    }
}

/// Milestone 1 worktrees (category `worktrees`), from the shared project scan.
/// Use [`worktree::scan`] directly when you only need worktrees.
#[derive(Debug, Default, Clone, Copy)]
pub struct WorktreeDetector;

impl Detector for WorktreeDetector {
    fn id(&self) -> &'static str {
        "worktrees"
    }

    fn category(&self) -> Category {
        Category::Worktrees
    }

    fn scan(&self, ctx: &ScanContext<'_>) -> Result<Vec<Finding>> {
        Ok(ctx.projects()?.worktree_findings())
    }

    fn run(&self, ctx: &ScanContext<'_>) -> Result<DetectorOutput> {
        let p = ctx.projects()?;
        Ok(DetectorOutput {
            findings: p.worktree_findings(),
            stale_metadata: p.worktrees.stale.clone(),
            skipped: Skipped::default(),
        })
    }
}

/// Milestone 2: git-ignored build/dependency folders in every repository under
/// the roots (category `artifacts`, one finding per folder). See
/// [`crate::projects`] for the staleness and double-counting rules.
#[derive(Debug, Default, Clone, Copy)]
pub struct ArtifactDetector;

impl Detector for ArtifactDetector {
    fn id(&self) -> &'static str {
        "artifacts"
    }

    fn category(&self) -> Category {
        Category::Artifacts
    }

    fn scan(&self, ctx: &ScanContext<'_>) -> Result<Vec<Finding>> {
        Ok(ctx.projects()?.artifact_findings())
    }
}
