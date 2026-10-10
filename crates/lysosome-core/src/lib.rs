//! lysosome-core: finds the disk your dev tools left behind. Returns data; never prints.
//!
//! Module map:
//! * [`walk`] — parallel, symlink-safe directory discovery with a visitor.
//! * [`git`] — bounded, timeout-guarded, lock-free `git` subprocesses.
//! * [`size`] — allocated-size measurement with hard-link de-duplication.
//! * [`system`] — Milestone 3: caches, AI models, Docker, Xcode, Android, editors.
//! * [`clean`] — Milestone 4: plan, re-check and perform safe cleanup.
//! * [`artifacts`] — artifact folder names and the `git check-ignore` gate.
//! * [`worktree`] — Milestone 1: worktree discovery, analysis, classification.
//! * [`projects`] — Milestone 2: project artifacts + worktrees from one shared walk.
//! * [`detector`] — the [`detector::Detector`] trait every scanner implements.
//! * [`detectors`] — the detector registry and the combined-report runner.
//! * [`model`] — findings, tiers, statuses and the versioned JSON [`model::Report`].
//! * [`progress`] — progress hooks the CLI renders.
//! * [`error`] — the crate error type.

pub mod artifacts;
pub mod clean;
pub mod detector;
pub mod detectors;
pub mod error;
pub mod git;
pub mod model;
pub mod progress;
pub mod projects;
pub mod size;
pub mod system;
pub mod walk;
pub mod worktree;

pub use error::{Error, Result};

/// Product name. Change it here and everything else follows.
pub const TOOL_NAME: &str = "lysosome";

/// Version of the JSON output contract (see `schema/`).
pub const SCHEMA_VERSION: u32 = 1;

pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
