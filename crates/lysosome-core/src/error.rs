//! Error type for lysosome-core.
//!
//! Most per-item failures (an unreadable folder, a git call that times out) are
//! *not* errors: they are skipped, counted and surfaced in scan statistics so a
//! single bad folder never aborts a scan. `Error` is reserved for conditions that
//! make the whole operation meaningless.

use std::path::PathBuf;
use std::sync::Arc;

/// Convenience alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The `git` binary could not be executed.
    #[error("could not run `git`: {0}")]
    GitUnavailable(#[source] std::io::Error),

    /// None of the requested scan roots exist or are folders.
    #[error("none of the scan roots are folders: {}", display_paths(.0))]
    NoValidRoots(Vec<PathBuf>),

    /// The home directory could not be determined (needed for the default root).
    #[error("could not determine your home directory")]
    NoHomeDir,

    /// A rayon thread pool could not be created.
    #[error("could not start worker threads: {0}")]
    ThreadPool(String),

    /// An error from a computation several detectors share (e.g. the project walk).
    #[error("{0}")]
    Shared(Arc<Error>),

    /// Generic I/O failure with the path that caused it.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    /// How to fix it, for people (the CLI prints it as `hint:`).
    pub fn hint(&self) -> &'static str {
        match self {
            Error::GitUnavailable(_) => {
                "install git (`xcode-select --install` on macOS) and make sure it is on PATH"
            }
            Error::NoValidRoots(_) => {
                "check the spelling, or pass a folder that exists, e.g. `lysosome scan ~/code`"
            }
            Error::NoHomeDir => "set HOME to your home folder and try again",
            Error::ThreadPool(_) => {
                "the system is out of threads or memory; close some programs and try again"
            }
            Error::Shared(inner) => inner.hint(),
            Error::Io { .. } => "check that the path exists and that you can read it",
        }
    }

    /// A stable machine-readable name (`--json` errors' `code`).
    pub fn code(&self) -> &'static str {
        match self {
            Error::GitUnavailable(_) => "git_unavailable",
            Error::NoValidRoots(_) => "no_valid_roots",
            Error::NoHomeDir => "no_home_dir",
            Error::ThreadPool(_) => "thread_pool",
            Error::Shared(inner) => inner.code(),
            Error::Io { .. } => "io",
        }
    }
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
