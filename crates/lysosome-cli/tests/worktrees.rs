//! End-to-end tests for `lysosome worktrees` against the fixture tree built by
//! `tests/fixtures/make_worktrees.sh`. Every test uses its own temp dir with an
//! isolated HOME and GIT_CONFIG_GLOBAL; nothing touches the real home folder.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use sha2::{Digest, Sha256};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Fixture {
    _tmp: tempfile::TempDir,
    /// Canonical fixture directory.
    dest: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = repo_root().join("tests/fixtures/make_worktrees.sh");
        let out = Command::new("bash")
            .arg(&script)
            .arg(tmp.path().join("fx"))
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("run fixture script");
        assert!(
            out.status.success(),
            "fixture script failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let dest = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
        Fixture { _tmp: tmp, dest }
    }

    fn code(&self) -> PathBuf {
        self.dest.join("code")
    }

    /// Apply the isolated environment to a command.
    fn env<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        cmd.env("HOME", self.dest.join("home"))
            .env("GIT_CONFIG_GLOBAL", self.dest.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .current_dir(&self.dest)
    }

    fn lysosome(&self) -> Command {
        let mut cmd = Command::cargo_bin("lysosome").unwrap();
        self.env(&mut cmd);
        cmd
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        let out = self.lysosome().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "lysosome {args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn json(&self) -> Value {
        let code = self.code();
        let out = self.run(&["worktrees", code.to_str().unwrap(), "--json"]);
        serde_json::from_slice(&out.stdout).expect("valid JSON on stdout")
    }
}

fn by_name(report: &Value) -> BTreeMap<String, Value> {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            let name = Path::new(f["path"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (name, f.clone())
        })
        .collect()
}

#[test]
fn classifies_every_fixture_with_the_right_reason() {
    let fx = Fixture::new();
    let report = fx.json();
    let got: BTreeMap<String, (String, String)> = by_name(&report)
        .into_iter()
        .map(|(k, f)| {
            (
                k,
                (
                    f["status"].as_str().unwrap().to_string(),
                    f["reason"].as_str().unwrap().to_string(),
                ),
            )
        })
        .collect();
    let want: BTreeMap<String, (String, String)> = [
        ("app-merged", "REMOVABLE", "clean, merged into origin/main"),
        ("app-pushed", "REMOVABLE", "clean, fully pushed"),
        ("app-outside", "REMOVABLE", "clean, merged into origin/main"),
        (
            "app-dirty",
            "REVIEW",
            "1 uncommitted change, 1 untracked file",
        ),
        ("app-unpushed", "REVIEW", "1 unpushed commit, not merged"),
        ("app-locked", "REVIEW", "locked worktree"),
        ("solo-wt", "REVIEW", "no remote and not merged"),
        ("doomed-wt", "ORPHAN", "parent repo moved or deleted"),
        ("app-active", "ACTIVE", ""),
    ]
    .into_iter()
    .map(|(n, s, r)| (n.to_string(), (s.to_string(), r.to_string())))
    .collect();
    assert_eq!(got, want);

    let stale = report["stale_metadata"].as_array().unwrap();
    assert_eq!(stale.len(), 1, "{stale:?}");
    assert_eq!(stale[0]["name"], "app-gone");
    assert!(stale[0]["missing_path"]
        .as_str()
        .unwrap()
        .ends_with("/code/app-gone"));
    assert_eq!(stale[0]["cleanup"]["command"][3], "worktree");
    assert_eq!(stale[0]["cleanup"]["command"][4], "prune");
}

#[test]
fn json_totals_artifacts_and_cleanup_are_consistent() {
    let fx = Fixture::new();
    let report = fx.json();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["dormant_after_days"], 30);
    assert_eq!(
        report["scanned_roots"][0].as_str().unwrap(),
        fx.code().to_str().unwrap()
    );

    let findings = report["findings"].as_array().unwrap();
    let sum = |pred: &dyn Fn(&Value) -> bool| -> u64 {
        findings
            .iter()
            .filter(|f| pred(f))
            .map(|f| f["size_bytes"].as_u64().unwrap())
            .sum()
    };
    let s = &report["summary"];
    assert_eq!(
        s["reclaimable_bytes"].as_u64().unwrap(),
        sum(&|f| f["status"] == "REMOVABLE")
    );
    assert_eq!(
        s["by_tier"]["regenerable"].as_u64().unwrap(),
        sum(&|f| f["status"] != "ACTIVE" && f["tier"] == "regenerable")
    );
    assert_eq!(
        s["by_tier"]["review"].as_u64().unwrap(),
        sum(&|f| f["status"] != "ACTIVE" && f["tier"] == "review")
    );
    assert_eq!(s["by_tier"]["regenerable_expensive"].as_u64().unwrap(), 0);
    assert_eq!(
        s["by_category"]["worktrees"].as_u64().unwrap(),
        sum(&|f| f["status"] != "ACTIVE")
    );

    let named = by_name(&report);
    // 20 MiB ignored node_modules counts; the tracked dist/ does not.
    let merged = &named["app-merged"];
    let art = merged["details"]["artifact_bytes"].as_u64().unwrap();
    assert!(art >= 20 * 1024 * 1024, "artifact_bytes {art}");
    assert!(merged["size_bytes"].as_u64().unwrap() >= art);
    let arts = merged["details"]["artifacts"].as_array().unwrap();
    assert_eq!(arts.len(), 1, "{arts:?}");
    assert!(arts[0]["path"].as_str().unwrap().ends_with("/node_modules"));

    // Only REMOVABLE findings get a cleanup command, and it's git's own.
    for (name, f) in &named {
        if f["status"] == "REMOVABLE" {
            let cmd: Vec<&str> = f["cleanup"]["command"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let main = fx.code().join("app");
            assert_eq!(
                cmd,
                [
                    "git",
                    "-C",
                    main.to_str().unwrap(),
                    "worktree",
                    "remove",
                    f["path"].as_str().unwrap()
                ],
                "{name}"
            );
            assert_eq!(f["tier"], "regenerable");
        } else {
            assert!(f["cleanup"].is_null(), "{name} must not suggest cleanup");
            assert_eq!(f["tier"], "review");
        }
        assert_eq!(f["category"], "worktrees");
        assert_eq!(f["kind"], "worktree");
        assert_eq!(f["id"], format!("worktree:{}", f["path"].as_str().unwrap()));
    }
    assert!(named["doomed-wt"]["last_activity"].is_null());
    assert!(named["app-merged"]["last_activity"]
        .as_str()
        .unwrap()
        .starts_with("2020-01-01T"));
}

#[test]
fn json_output_matches_the_schema() {
    let fx = Fixture::new();
    let report = fx.json();
    let schema: Value = serde_json::from_str(
        &fs::read_to_string(repo_root().join("schema/lysosome.schema.json")).unwrap(),
    )
    .unwrap();
    let validator = jsonschema::draft202012::new(&schema).expect("schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(&report)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "schema violations:\n{}",
        errors.join("\n")
    );

    // And the schema actually rejects a contract break.
    let mut broken = report.clone();
    broken["findings"][0]["status"] = Value::from("DELETE_ME");
    assert!(!validator.is_valid(&broken));
}

/// path -> (mtime, sha256) for files, (mtime, "dir") for directories.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (SystemTime, String)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let m = fs::symlink_metadata(&p).unwrap();
            let mtime = m.modified().unwrap();
            if m.is_dir() {
                out.insert(p.clone(), (mtime, "dir".into()));
                stack.push(p);
            } else if m.is_file() {
                let hash = format!("{:x}", Sha256::digest(fs::read(&p).unwrap()));
                out.insert(p, (mtime, hash));
            } else {
                out.insert(p, (mtime, "other".into()));
            }
        }
    }
    out
}

#[test]
fn scanning_changes_nothing_on_disk() {
    let fx = Fixture::new();
    let before = snapshot(&fx.dest);
    assert!(before.len() > 50, "fixture looks empty");
    let code = fx.code();
    let elsewhere = fx.dest.join("elsewhere");
    fx.run(&["worktrees", code.to_str().unwrap(), "--json"]);
    fx.run(&["worktrees", code.to_str().unwrap(), "--all"]);
    fx.run(&[
        "worktrees",
        fx.dest.to_str().unwrap(),
        elsewhere.to_str().unwrap(),
        "--days",
        "0",
        "--all",
        "--profile",
    ]);
    let after = snapshot(&fx.dest);
    let changed: Vec<_> = before
        .iter()
        .filter(|(p, v)| after.get(*p) != Some(*v))
        .map(|(p, _)| p.display().to_string())
        .chain(
            after
                .keys()
                .filter(|p| !before.contains_key(*p))
                .map(|p| format!("{} (new)", p.display())),
        )
        .collect();
    assert!(changed.is_empty(), "scan modified:\n{}", changed.join("\n"));
}

#[test]
fn agrees_with_the_python_prototype() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 not found; skipping prototype parity test");
        return;
    }
    let fx = Fixture::new();
    let ours: BTreeMap<String, String> = fx.json()["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap().to_string(),
                f["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();

    let mut py = Command::new("python3");
    fx.env(&mut py)
        .arg(repo_root().join("reference/wtscan.py"))
        .arg("--json")
        .arg(fx.code());
    let out = py.output().unwrap();
    assert!(
        out.status.success(),
        "wtscan.py failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let theirs: Value = serde_json::from_slice(&out.stdout).unwrap();
    let theirs: BTreeMap<String, String> = theirs["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            let p = PathBuf::from(w["path"].as_str().unwrap());
            let p = p.canonicalize().unwrap_or(p);
            (
                p.to_string_lossy().into_owned(),
                w["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(ours, theirs);
}

#[test]
fn table_output_is_complete_and_plain_when_piped() {
    let fx = Fixture::new();
    let code = fx.code();
    let out = fx.run(&["worktrees", code.to_str().unwrap()]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        !text.contains('\u{1b}'),
        "no ANSI codes when piped:\n{text}"
    );
    for needle in [
        "8 dormant worktrees are holding",
        "safe to remove now",
        "build & dependency output",
        "STATUS",
        "└ 1 uncommitted change, 1 untracked file",
        "└ 1 unpushed commit, not merged",
        "└ locked worktree",
        "└ no remote and not merged",
        "└ parent repo moved or deleted",
        "1 stale worktree record",
        "Suggested cleanup",
        "Nothing was changed.",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
    let main = code.join("app");
    let merged = code.join("app-merged");
    let cmd = format!(
        "git -C \"{}\" worktree remove \"{}\"",
        main.display(),
        merged.display()
    );
    assert!(text.contains(&cmd), "missing {cmd:?} in:\n{text}");
    // Active worktrees are hidden by default and listed with --all.
    assert!(!text.contains("app-active"));
    let all = fx.run(&["worktrees", code.to_str().unwrap(), "--all", "--top", "3"]);
    let all = String::from_utf8(all.stdout).unwrap();
    assert!(all.contains("… 6 more (use --top N)"), "{all}");
    // stderr stays clean of progress when not a TTY.
    let json = fx.run(&["worktrees", code.to_str().unwrap(), "--json"]);
    assert!(
        json.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
}

#[test]
fn days_window_and_profile_flag() {
    let fx = Fixture::new();
    let code = fx.code();
    let out = fx.run(&[
        "worktrees",
        code.to_str().unwrap(),
        "--days",
        "100000",
        "--json",
        "--profile",
    ]);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    for f in report["findings"].as_array().unwrap() {
        let want = if f["path"].as_str().unwrap().ends_with("doomed-wt") {
            "ORPHAN"
        } else {
            "ACTIVE"
        };
        assert_eq!(f["status"], want, "{}", f["path"]);
    }
    assert_eq!(report["summary"]["reclaimable_bytes"], 0);
    let err = String::from_utf8(out.stderr).unwrap();
    for phase in ["walk", "git", "size"] {
        assert!(err.contains(phase), "profile missing {phase}: {err}");
    }
}

#[test]
fn empty_and_missing_roots() {
    let fx = Fixture::new();
    let empty = fx.dest.join("home");
    let out = fx.run(&["worktrees", empty.to_str().unwrap()]);
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("No git worktrees found"));

    let missing = fx.dest.join("nope");
    let out = fx
        .lysosome()
        .args(["worktrees", missing.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8(out.stderr).unwrap().contains("nope"));

    // One good root and one missing: warn, still scan.
    let code = fx.code();
    let out = fx.run(&[
        "worktrees",
        code.to_str().unwrap(),
        missing.to_str().unwrap(),
        "--json",
    ]);
    assert!(String::from_utf8(out.stderr).unwrap().contains("skipping"));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        report["skipped"]["missing_roots"].as_array().unwrap().len(),
        1
    );
}

#[test]
fn help_is_helpful() {
    let out = Command::cargo_bin("lysosome").unwrap().output().unwrap();
    assert!(
        out.status.success(),
        "bare `lysosome` shows help and succeeds"
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("worktrees"));
    assert!(text.contains("Examples:"));

    let out = Command::cargo_bin("lysosome")
        .unwrap()
        .args(["worktrees", "--help"])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    for needle in [
        "--days",
        "--depth",
        "--all",
        "--top",
        "--json",
        "--no-color",
        "REMOVABLE",
    ] {
        assert!(text.contains(needle), "help missing {needle}");
    }
}
