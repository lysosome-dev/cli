//! lysosome-core: finds the disk your dev tools left behind. Returns data; never prints.
//!
//! Module map:
//! * [`walk`] — parallel, symlink-safe directory discovery with a visitor.
//! * [`git`] — bounded, timeout-guarded, lock-free `git` subprocesses.
//! * [`size`] — allocated-size measurement with hard-link de-duplication.
//! * [`artifacts`] — artifact folder names and the `git check-ignore` gate.
//! * [`worktree`] — Milestone 1: worktree discovery, analysis, classification.
//! * [`detector`] — the [`detector::Detector`] trait future scanners implement.
//! * [`model`] — findings, tiers, statuses and the versioned JSON [`model::Report`].
//! * [`progress`] — progress hooks the CLI renders.
//! * [`error`] — the crate error type.

pub mod artifacts;
pub mod detector;
pub mod error;
pub mod git;
pub mod model;
pub mod progress;
pub mod size;
pub mod walk;
pub mod worktree;

pub use error::{Error, Result};

/// Product name. Change it here and everything else follows.
pub const TOOL_NAME: &str = "lysosome";

/// Version of the JSON output contract (see `schema/`).
pub const SCHEMA_VERSION: u32 = 1;

pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
