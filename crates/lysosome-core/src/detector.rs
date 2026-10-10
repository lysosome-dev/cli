//! The extension point for Milestone 3+ scanners (caches, AI models, Docker,
//! Xcode...). Each detector is a small module implementing [`Detector`]; the
//! CLI runs a set of them and merges their findings into one
//! [`Report`](crate::model::Report).

use std::path::PathBuf;
use std::time::SystemTime;

use crate::error::Result;
use crate::model::{Category, Finding};
use crate::progress::{NoProgress, Progress};
use crate::worktree;

/// Shared inputs for every detector.
pub struct ScanContext<'a> {
    /// Folders the user asked to scan (detectors with fixed locations may ignore them).
    pub roots: Vec<PathBuf>,
    /// The user's home directory (tests point this at a temp dir).
    pub home: PathBuf,
    pub dormant_after_days: u64,
    pub max_depth: usize,
    pub now: SystemTime,
    pub progress: &'a dyn Progress,
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
        }
    }
}

/// A source of findings.
pub trait Detector: Send + Sync {
    /// Stable identifier, e.g. `"worktrees"`, `"ollama"`, `"xcode-deriveddata"`.
    fn id(&self) -> &'static str;
    /// The category every finding from this detector belongs to.
    fn category(&self) -> Category;
    /// Find things. Must be read-only. Unreadable locations are skipped, not errors.
    fn scan(&self, ctx: &ScanContext<'_>) -> Result<Vec<Finding>>;
}

/// The Milestone 1 worktree scanner as a [`Detector`]. Use
/// [`worktree::scan`] directly when you also need stale metadata and stats.
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
        let mut opts = worktree::ScanOptions::new(ctx.roots.clone());
        opts.dormant_after_days = ctx.dormant_after_days;
        opts.max_depth = ctx.max_depth;
        opts.now = ctx.now;
        let scan = worktree::scan(&opts, ctx.progress)?;
        Ok(scan.worktrees.iter().map(|w| w.to_finding()).collect())
    }
}
