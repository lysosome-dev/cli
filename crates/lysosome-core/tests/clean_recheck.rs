//! `clean` re-checks every item right before acting: state that changed
//! between the plan and the action means "skip, with a reason". These tests
//! drive the library directly so they can change things in between.
//!
//! One `#[test]` runs the scenarios in sequence because it sets process-wide
//! environment (isolated HOME / GIT_CONFIG_GLOBAL for every git call).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use lysosome_core::clean::{self, DirTrash, Executor, Outcome, PlanOptions, Scope, Selection};
use lysosome_core::detector::ScanContext;
use lysosome_core::detectors;
use lysosome_core::model::Tier;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture(tmp: &Path, script: &str) -> PathBuf {
    let out = Command::new("bash")
        .arg(repo_root().join("tests/fixtures").join(script))
        .arg(tmp.join(script.trim_end_matches(".sh")))
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dest = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
    // Every git process from here on (scan, re-check, act) is isolated.
    std::env::set_var("HOME", dest.join("home"));
    std::env::set_var("GIT_CONFIG_GLOBAL", dest.join("gitconfig"));
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    std::env::remove_var("GIT_DIR");
    std::env::remove_var("GIT_WORK_TREE");
    dest
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn plan(roots: Vec<PathBuf>, home: &Path, sel: &Selection) -> clean::Plan {
    let ctx = ScanContext::new(roots, home.to_path_buf());
    let run = detectors::run(&detectors::project_detectors(), &ctx);
    clean::plan(&run.report.findings, sel, PlanOptions::default())
}

fn outcome_of<'a>(
    plan: &clean::Plan,
    results: &'a [clean::ItemResult],
    path: &Path,
) -> &'a clean::ItemResult {
    let i = plan
        .items
        .iter()
        .position(|i| i.path == path)
        .unwrap_or_else(|| panic!("{} not planned", path.display()));
    &results[i]
}

#[test]
fn state_changed_since_the_scan_is_skipped_with_a_reason() {
    let tmp = tempfile::tempdir().unwrap();
    worktrees_that_changed(tmp.path());
    artifacts_that_changed(tmp.path());
}

fn worktrees_that_changed(tmp: &Path) {
    let fx = fixture(tmp, "make_worktrees.sh");
    let home = fx.join("home");
    let sel = Selection {
        scopes: [Scope::Worktrees].into_iter().collect(),
        ..Selection::default()
    };
    let p = plan(vec![fx.join("code")], &home, &sel);
    let merged = fx.join("code/app-merged");
    let pushed = fx.join("code/app-pushed");
    let outside = fx.join("elsewhere/app-outside");
    assert_eq!(p.items.len(), 3, "{:#?}", p.items);

    // Between plan and act: a new untracked file, a new local commit, a lock.
    fs::write(merged.join("new-work.txt"), "important").unwrap();
    fs::write(pushed.join("pushed.txt"), "more work\n").unwrap();
    git(&pushed, &["commit", "-qam", "local only"]);
    git(
        &fx.join("code/app"),
        &["worktree", "lock", outside.to_str().unwrap()],
    );

    let trash = DirTrash::new(fx.join("fake-trash"));
    let ex = Executor::new(&trash, home.clone(), None);
    let results = ex.execute(&p, &mut |_, _, _| {}).unwrap();
    for (wt, want) in [
        (&merged, "untracked file"),
        (&pushed, "changed since scan"),
        (&outside, "locked worktree"),
    ] {
        let r = outcome_of(&p, &results, wt);
        assert_eq!(r.outcome, Outcome::Skipped, "{}: {r:?}", wt.display());
        assert!(r.message.starts_with("changed since scan"), "{r:?}");
        assert!(r.message.contains(want), "{}: {}", wt.display(), r.message);
        assert!(wt.is_dir(), "{} must still exist", wt.display());
    }
    assert_eq!(
        fs::read_to_string(merged.join("new-work.txt")).unwrap(),
        "important"
    );
    assert!(!fx.join("fake-trash").exists(), "nothing was trashed");
}

fn artifacts_that_changed(tmp: &Path) {
    let fx = fixture(tmp, "make_projects.sh");
    let home = fx.join("home");
    let sel = Selection {
        scopes: [Scope::Artifacts].into_iter().collect(),
        tiers: [Tier::Regenerable, Tier::RegenerableExpensive]
            .into_iter()
            .collect(),
        ..Selection::default()
    };
    let p = plan(vec![fx.join("code")], &home, &sel);
    let rust_target = fx.join("code/old-rust/target");
    let web_dist = fx.join("code/old-web/dist");
    let api_nm = fx.join("code/old-web/packages/api/node_modules");
    let web_nm = fx.join("code/old-web/node_modules");
    let inner_target = fx.join("code/old-web/packages/inner/target");
    for x in [&rust_target, &web_dist, &api_nm, &web_nm, &inner_target] {
        assert!(
            p.items.iter().any(|i| &i.path == x),
            "{} not planned",
            x.display()
        );
    }

    let subset = |paths: &[&PathBuf]| clean::Plan {
        items: p
            .items
            .iter()
            .filter(|i| paths.contains(&&i.path))
            .cloned()
            .collect(),
        ..p.clone()
    };
    let trash = DirTrash::new(fx.join("fake-trash"));
    let ex = Executor::new(&trash, home.clone(), None);

    // Between plan and act: old-rust gets a commit (in use again) and a git
    // checkout appears inside inner's target/. old-web is unchanged.
    fs::write(fx.join("code/old-rust/src.rs"), "fn main() {}\n").unwrap();
    git(&fx.join("code/old-rust"), &["add", "src.rs"]);
    git(&fx.join("code/old-rust"), &["commit", "-qm", "back at it"]);
    fs::create_dir_all(inner_target.join("dep/.git")).unwrap();
    let p1 = subset(&[&rust_target, &inner_target, &web_nm]);
    let results = ex.execute(&p1, &mut |_, _, _| {}).unwrap();

    let r = outcome_of(&p1, &results, &rust_target);
    assert_eq!(r.outcome, Outcome::Skipped);
    assert!(r.message.contains("used recently"), "{r:?}");
    assert!(rust_target.is_dir());

    let r = outcome_of(&p1, &results, &inner_target);
    assert_eq!(r.outcome, Outcome::Skipped);
    assert!(r.message.contains(".git inside"), "{r:?}");
    assert!(inner_target.join("dep/.git").is_dir());

    // An unchanged project's folder still goes, to the (fake) trash.
    let r = outcome_of(&p1, &results, &web_nm);
    assert_eq!(r.outcome, Outcome::Done, "{r:?}");
    assert!(!web_nm.exists());
    assert!(fx.join("fake-trash/node_modules").is_dir());

    // Then dist/ is swapped for a symlink to a folder outside the project.
    fs::remove_dir_all(&web_dist).unwrap();
    std::os::unix::fs::symlink(fx.join("big"), &web_dist).unwrap();
    let p2 = subset(&[&web_dist, &api_nm]);
    let results = ex.execute(&p2, &mut |_, _, _| {}).unwrap();
    let r = outcome_of(&p2, &results, &web_dist);
    assert_eq!(r.outcome, Outcome::Skipped);
    assert!(r.message.contains("symlink"), "{r:?}");
    assert!(
        fx.join("big").read_dir().unwrap().count() > 0,
        "symlink target untouched"
    );
    // `dist/` only ignores folders: the symlink is an untracked file, so
    // old-web counts as edited today and its other folders are kept too.
    let r = outcome_of(&p2, &results, &api_nm);
    assert_eq!(r.outcome, Outcome::Skipped, "{r:?}");
    assert!(r.message.contains("used recently"), "{r:?}");
    assert!(api_nm.is_dir());
}
