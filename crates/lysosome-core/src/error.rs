//! Error type for lysosome-core.
//!
//! Most per-item failures (an unreadable folder, a git call that times out) are
//! *not* errors: they are skipped, counted and surfaced in scan statistics so a
//! single bad folder never aborts a scan. `Error` is reserved for conditions that
//! make the whole operation meaningless.

use std::path::PathBuf;

/// Convenience alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The `git` binary could not be executed.
    #[error("could not run `git`: {0}. Install git (e.g. `xcode-select --install`) and make sure it is on PATH")]
    GitUnavailable(#[source] std::io::Error),

    /// None of the requested scan roots exist or are folders.
    #[error("none of the scan roots are folders: {}", display_paths(.0))]
    NoValidRoots(Vec<PathBuf>),

    /// The home directory could not be determined (needed for the default root).
    #[error("could not determine your home directory; pass the folders to scan explicitly")]
    NoHomeDir,

    /// A rayon thread pool could not be created.
    #[error("could not start worker threads: {0}")]
    ThreadPool(String),

    /// Generic I/O failure with the path that caused it.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
