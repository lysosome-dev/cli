//! Progress reporting hooks.
//!
//! Core never prints. Long-running operations report through a [`Progress`]
//! implementation supplied by the caller (the CLI drives a spinner with it).
//! Every method has a no-op default so implementors only override what they show.

use std::fmt;

/// A named phase of a scan. Used for progress messages and `--profile` timings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// Walking the scan roots to discover repositories and worktrees.
    Walk,
    /// Running git to analyze each worktree.
    Git,
    /// Measuring on-disk sizes (and locating artifact folders).
    Size,
    /// Asking git which artifact folders are ignored.
    IgnoreCheck,
    /// Progress-only: [`Phase::Git`] and [`Phase::Size`] running concurrently.
    /// Each work item of either phase advances it.
    Analyze,
    /// Project artifacts in main repos: ignore-check, last activity, sizing.
    Artifacts,
    /// Asking GitHub (`gh`) about possibly squash-merged worktrees.
    Github,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Walk => "walk",
            Phase::Git => "git",
            Phase::Size => "size",
            Phase::IgnoreCheck => "ignore-check",
            Phase::Analyze => "analyze",
            Phase::Artifacts => "artifacts",
            Phase::Github => "gh",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Receives progress events. Must be `Sync`: events arrive from worker threads.
pub trait Progress: Sync {
    /// A phase started. `total` is the number of work items, when known.
    fn start(&self, _phase: Phase, _total: Option<u64>) {}
    /// `n` more work items finished in the current phase.
    fn advance(&self, _n: u64) {}
    /// The phase finished.
    fn finish(&self, _phase: Phase) {}
}

/// A [`Progress`] that ignores everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoProgress;

impl Progress for NoProgress {}
