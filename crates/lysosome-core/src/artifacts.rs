//! Build/dependency artifact folders and the git-ignore gate.
//!
//! Safety rule 4: a folder only counts as an artifact if git ignores it.
//! [`ignored_subset`] asks `git check-ignore` and returns only confirmed paths,
//! so tracked source is never called junk. Shared by the worktree scanner and
//! (Milestone 2) the project artifact scanner.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::git::Git;
use crate::model::Tier;

/// Regenerable build/dependency folder names (identical to `ARTIFACT_DIRS` in
/// `reference/wtscan.py`).
pub const ARTIFACT_DIRS: &[&str] = &[
    "node_modules",
    ".venv",
    "venv",
    "target",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".parcel-cache",
    "dist",
    "build",
    ".gradle",
    "Pods",
    "DerivedData",
    ".build",
    ".dart_tool",
    ".expo",
    "coverage",
];

/// How deep below a worktree root to look for artifact folders.
/// Folders are listed up to this depth, so artifacts can sit one level deeper.
pub const ARTIFACT_SEARCH_DEPTH: usize = 6;

/// Installed third-party dependencies: rebuilding them means re-downloading
/// (network, registry availability, lockfile resolution), so they are
/// [`Tier::RegenerableExpensive`]. Everything else in [`ARTIFACT_DIRS`] is local
/// build output or a tool cache and is [`Tier::Regenerable`].
pub const DEPENDENCY_DIRS: &[&str] = &["node_modules", ".venv", "venv", "Pods"];

/// Safety tier of an artifact folder by name (see [`DEPENDENCY_DIRS`]).
pub fn tier_for(name: &OsStr) -> Tier {
    if DEPENDENCY_DIRS.iter().any(|d| OsStr::new(d) == name) {
        Tier::RegenerableExpensive
    } else {
        Tier::Regenerable
    }
}

/// Is `name` one of [`ARTIFACT_DIRS`]?
pub fn is_artifact_dir(name: &OsStr) -> bool {
    ARTIFACT_DIRS.iter().any(|a| OsStr::new(a) == name)
}

/// Return the members of `candidates` (absolute paths inside `repo_dir`) that
/// git ignores. Any git failure yields an empty list (the safe answer).
pub fn ignored_subset(git: &Git, repo_dir: &Path, candidates: &[PathBuf]) -> Vec<PathBuf> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let rel: Vec<&Path> = candidates
        .iter()
        .filter_map(|p| p.strip_prefix(repo_dir).ok())
        .collect();
    let mut input = Vec::new();
    for r in &rel {
        input.extend_from_slice(r.as_os_str().as_bytes());
        input.push(0);
    }
    // Exit status 1 means "nothing ignored"; 128 means error. Either way we
    // trust only what was printed.
    let out = git.run_with_input(repo_dir, ["check-ignore", "-z", "--stdin"], Some(&input));
    let ignored: std::collections::HashSet<&[u8]> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .collect();
    rel.iter()
        .filter(|r| ignored.contains(r.as_os_str().as_bytes()))
        .map(|r| repo_dir.join(r))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_dirs_are_expensive_and_build_output_is_cheap() {
        assert_eq!(
            tier_for(OsStr::new("node_modules")),
            Tier::RegenerableExpensive
        );
        assert_eq!(tier_for(OsStr::new(".venv")), Tier::RegenerableExpensive);
        assert_eq!(tier_for(OsStr::new("Pods")), Tier::RegenerableExpensive);
        assert_eq!(tier_for(OsStr::new("target")), Tier::Regenerable);
        assert_eq!(tier_for(OsStr::new("dist")), Tier::Regenerable);
        for d in DEPENDENCY_DIRS {
            assert!(is_artifact_dir(OsStr::new(d)));
        }
    }
}
