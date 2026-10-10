//! Embeds the git commit and target triple for `lysosome --version`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Cargo treats a missing rerun-if-changed path as always dirty, so only
/// watch files that exist.
fn watch(path: &str) {
    if Path::new(path).exists() {
        println!("cargo:rerun-if-changed={path}");
    }
}

/// `.cargo_vcs_info.json`, written by `cargo package`, for crates.io builds.
fn packaged_sha(manifest_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest_dir.join(".cargo_vcs_info.json")).ok()?;
    let rest = &text[text.find("\"sha1\"")? + 6..];
    let start = rest.find('"')? + 1;
    let sha = &rest[start..start + rest[start..].find('"')?];
    Some(sha.chars().take(9).collect())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LYSOSOME_GIT_SHA");
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());

    // Only trust git when this crate lives in the repo's own checkout, not
    // when an unpacked crates.io tarball happens to sit inside another repo.
    let in_checkout = git(&["rev-parse", "--show-toplevel"])
        .and_then(|top| std::fs::canonicalize(top).ok())
        .zip(std::fs::canonicalize(&manifest_dir).ok())
        .is_some_and(|(top, dir)| dir.starts_with(top))
        && !manifest_dir.join(".cargo_vcs_info.json").exists();

    if in_checkout {
        // Rebuild when HEAD moves (works in linked worktrees, where .git is a file).
        for p in ["HEAD", "packed-refs"] {
            if let Some(path) = git(&["rev-parse", "--git-path", p]) {
                watch(&path);
            }
        }
        if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
            if let Some(path) = git(&["rev-parse", "--git-path", &head_ref]) {
                watch(&path);
            }
        }
    }

    let sha = std::env::var("LYSOSOME_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            in_checkout
                .then(|| git(&["rev-parse", "--short=9", "HEAD"]))
                .flatten()
        })
        .or_else(|| packaged_sha(&manifest_dir))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=LYSOSOME_GIT_SHA={sha}");
    println!(
        "cargo:rustc-env=LYSOSOME_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
}
