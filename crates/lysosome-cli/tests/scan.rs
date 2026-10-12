//! End-to-end tests for `lysosome scan` against `tests/fixtures/make_projects.sh`
//! (and `make_worktrees.sh` for schema coverage). Every test uses its own temp
//! dir with an isolated HOME and GIT_CONFIG_GLOBAL, a cleared environment and
//! a PATH holding only `git`, so the system detectors that `scan` also runs
//! never see the machine's real Docker, Homebrew, Xcode or caches.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use sha2::{Digest, Sha256};

const MIB: u64 = 1024 * 1024;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Fixture {
    _tmp: tempfile::TempDir,
    dest: PathBuf,
    /// The only folder on PATH: a symlink to the real `git`, plus any fake tools.
    bin: PathBuf,
}

/// Absolute path of the real `git`.
fn real_git() -> PathBuf {
    let out = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("find git");
    let path = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
    assert!(
        out.status.success() && path.is_absolute(),
        "tests need `git` on PATH"
    );
    path
}

impl Fixture {
    fn new(script: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = Command::new("bash")
            .arg(repo_root().join("tests/fixtures").join(script))
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
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(real_git(), bin.join("git")).unwrap();
        Fixture {
            _tmp: tmp,
            dest,
            bin,
        }
    }

    fn projects() -> Self {
        Fixture::new("make_projects.sh")
    }

    fn code(&self) -> PathBuf {
        self.dest.join("code")
    }

    fn lysosome(&self) -> Command {
        let mut cmd = Command::cargo_bin("lysosome").unwrap();
        cmd.env_clear()
            .env("PATH", &self.bin)
            .env("HOME", self.dest.join("home"))
            .env("GIT_CONFIG_GLOBAL", self.dest.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .current_dir(&self.dest);
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

    fn json(&self, extra: &[&str]) -> Value {
        let code = self.code();
        let mut args = vec!["scan", code.to_str().unwrap(), "--json"];
        args.extend_from_slice(extra);
        serde_json::from_slice(&self.run(&args).stdout).expect("valid JSON on stdout")
    }

    /// Finding path relative to the fixture's `code/` folder.
    fn rel(&self, f: &Value) -> String {
        Path::new(f["path"].as_str().unwrap())
            .strip_prefix(self.code())
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
}

fn findings(report: &Value) -> &Vec<Value> {
    report["findings"].as_array().unwrap()
}

#[test]
fn artifact_statuses_tiers_and_exclusions() {
    let fx = Fixture::projects();
    let report = fx.json(&[]);
    let arts: BTreeMap<String, &Value> = findings(&report)
        .iter()
        .filter(|f| f["category"] == "artifacts")
        .map(|f| (fx.rel(f), f))
        .collect();
    let got: BTreeMap<&str, (&str, &str, &str, &str)> = arts
        .iter()
        .map(|(k, f)| {
            (
                k.as_str(),
                (
                    f["status"].as_str().unwrap(),
                    f["tier"].as_str().unwrap(),
                    f["kind"].as_str().unwrap(),
                    f["details"]["project_kind"].as_str().unwrap(),
                ),
            )
        })
        .collect();
    let want: BTreeMap<&str, (&str, &str, &str, &str)> = [
        (
            "old-web/node_modules",
            ("REMOVABLE", "regenerable_expensive", "node_modules", "repo"),
        ),
        ("old-web/dist", ("REMOVABLE", "regenerable", "dist", "repo")),
        (
            "old-web/packages/api/node_modules",
            ("REMOVABLE", "regenerable_expensive", "node_modules", "repo"),
        ),
        (
            "old-rust/target",
            ("REMOVABLE", "regenerable", "target", "repo"),
        ),
        // Nested independent repo owns its own artifacts.
        (
            "old-web/packages/inner/target",
            ("REMOVABLE", "regenerable", "target", "repo"),
        ),
        // A file edited today inside an old untracked folder: in use.
        (
            "old-scratch/node_modules",
            ("ACTIVE", "regenerable_expensive", "node_modules", "repo"),
        ),
        // A repo nested in an active worktree: reported once, via the worktree.
        (
            "mono-active-wt/vendor/lib2/dist",
            ("ACTIVE", "regenerable", "dist", "worktree"),
        ),
        // A tracked file deleted today: in use.
        (
            "old-deleted/node_modules",
            ("ACTIVE", "regenerable_expensive", "node_modules", "repo"),
        ),
        // (old-go/target is absent: it contains a linked worktree.)
        // An editable git checkout inside .venv: never offered for cleanup.
        ("old-web/.venv", ("REVIEW", "review", ".venv", "repo")),
        // Old commit, but a tracked file edited now: in use.
        (
            "old-edited/node_modules",
            ("ACTIVE", "regenerable_expensive", "node_modules", "repo"),
        ),
        (
            "fresh-py/.venv",
            ("ACTIVE", "regenerable_expensive", ".venv", "repo"),
        ),
        (
            "mono-active-wt/node_modules",
            (
                "ACTIVE",
                "regenerable_expensive",
                "node_modules",
                "worktree",
            ),
        ),
    ]
    .into_iter()
    .collect();
    // Not present: tracked build/, non-ignored coverage/, the `target` symlink,
    // nested node_modules/pkg/dist, loose/node_modules (no repo), and the
    // dormant worktree's node_modules (part of the worktree finding).
    assert_eq!(got, want);

    let nm = arts["old-web/node_modules"];
    let size = nm["size_bytes"].as_u64().unwrap();
    assert!(
        (5 * MIB..6 * MIB).contains(&size),
        "node_modules includes nested dist, never the 30 MiB symlink target: {size}"
    );
    assert_eq!(
        nm["id"],
        format!("node_modules:{}", nm["path"].as_str().unwrap())
    );
    assert_eq!(
        nm["details"]["project"].as_str().unwrap(),
        fx.code().join("old-web").to_str().unwrap()
    );
    assert_eq!(nm["details"]["relative_path"], "node_modules");
    assert!(nm["details"]["idle_days"].as_i64().unwrap() > 1000);
    assert!(nm["last_activity"]
        .as_str()
        .unwrap()
        .starts_with("2020-01-01T"));
    assert!(nm["reason"]
        .as_str()
        .unwrap()
        .starts_with("git-ignored; project idle"));

    for (name, f) in &arts {
        if f["status"] == "REMOVABLE" {
            assert_eq!(f["cleanup"]["action"], "trash", "{name}");
            assert_eq!(f["cleanup"]["paths"][0], f["path"], "{name}");
            assert!(f["cleanup"].get("command").is_none(), "{name}");
        } else {
            assert!(f["cleanup"].is_null(), "{name}");
        }
        if f["status"] == "ACTIVE" {
            assert_eq!(f["reason"], "", "{name}");
        }
    }
    assert_eq!(
        arts["old-web/.venv"]["reason"],
        "contains a git checkout (.git inside); it may hold local work"
    );
}

#[test]
fn deep_artifacts_and_roots_inside_worktrees() {
    let fx = Fixture::projects();
    // --depth limits the repository search, not the artifact search inside repos.
    let report = fx.json(&["--depth", "1"]);
    let arts: Vec<String> = findings(&report)
        .iter()
        .filter(|f| f["category"] == "artifacts")
        .map(|f| fx.rel(f))
        .collect();
    for want in [
        "old-web/packages/api/node_modules",
        // a repo found only below the depth limit still owns its artifacts
        "old-web/packages/inner/target",
    ] {
        assert!(arts.contains(&want.to_string()), "{want}: {arts:?}");
    }

    // A root inside a linked worktree still analyzes that worktree.
    let sub = fx.code().join("mono-old-wt/vendor");
    let out = fx.run(&["scan", sub.to_str().unwrap(), "--json"]);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let wts: Vec<String> = findings(&report)
        .iter()
        .filter(|f| f["kind"] == "worktree")
        .map(|f| fx.rel(f))
        .collect();
    assert_eq!(wts, ["mono-old-wt"]);
}

#[test]
fn totals_are_honest_and_nothing_is_double_counted() {
    let fx = Fixture::projects();
    let report = fx.json(&[]);
    let all = findings(&report);
    let sum = |pred: &dyn Fn(&Value) -> bool| -> u64 {
        all.iter()
            .filter(|f| pred(f))
            .map(|f| f["size_bytes"].as_u64().unwrap())
            .sum()
    };
    let s = &report["summary"];
    assert_eq!(
        s["reclaimable_bytes"].as_u64().unwrap(),
        sum(&|f| f["status"] == "REMOVABLE")
    );
    for tier in ["regenerable", "regenerable_expensive", "review"] {
        assert_eq!(
            s["by_tier"][tier].as_u64().unwrap(),
            sum(&|f| f["status"] != "ACTIVE" && f["tier"] == tier),
            "{tier}"
        );
    }
    for cat in ["worktrees", "artifacts"] {
        assert_eq!(
            s["by_category"][cat].as_u64().unwrap(),
            sum(&|f| f["status"] != "ACTIVE" && f["category"] == cat),
            "{cat}"
        );
    }
    // 5 + 1 + 1 + 1 + 3 MiB of idle project artifacts (+ the small REVIEW .venv).
    let arts = s["by_category"]["artifacts"].as_u64().unwrap();
    assert!((11 * MIB..12 * MIB).contains(&arts), "{arts}");

    // The worktree inside old-go's ignored target/ is a worktree finding, and
    // target/ itself is not an artifact finding.
    assert!(all
        .iter()
        .any(|f| f["kind"] == "worktree" && fx.rel(f) == "old-go/target/wt"));
    assert!(!all.iter().any(|f| fx.rel(f) == "old-go/target"));

    // Every finding is reported exactly once.
    let mut ids: Vec<&str> = all.iter().map(|f| f["id"].as_str().unwrap()).collect();
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n, "duplicate finding ids");

    // The dormant worktree's artifacts live only in the worktree finding.
    let old_wt = all
        .iter()
        .find(|f| f["kind"] == "worktree" && fx.rel(f) == "mono-old-wt")
        .unwrap();
    assert_eq!(old_wt["status"], "REMOVABLE");
    assert!(old_wt["details"]["artifact_bytes"].as_u64().unwrap() >= 4 * MIB);
    let dormant_wts: Vec<PathBuf> = all
        .iter()
        .filter(|f| f["category"] == "worktrees" && f["status"] != "ACTIVE")
        .map(|f| PathBuf::from(f["path"].as_str().unwrap()))
        .collect();
    for f in all.iter().filter(|f| f["category"] == "artifacts") {
        let p = Path::new(f["path"].as_str().unwrap());
        assert!(
            !dormant_wts.iter().any(|w| p.starts_with(w)),
            "{} is inside a dormant worktree",
            p.display()
        );
    }
    // No finding contains another dormant finding.
    let dormant: Vec<PathBuf> = all
        .iter()
        .filter(|f| f["status"] != "ACTIVE")
        .map(|f| PathBuf::from(f["path"].as_str().unwrap()))
        .collect();
    for a in &dormant {
        for b in &dormant {
            assert!(a == b || !b.starts_with(a), "{a:?} contains {b:?}");
        }
    }
}

fn validate(report: &Value) {
    let schema: Value = serde_json::from_str(
        &fs::read_to_string(repo_root().join("schema/lysosome.schema.json")).unwrap(),
    )
    .unwrap();
    let validator = jsonschema::draft202012::new(&schema).expect("schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(report)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "schema violations:\n{}",
        errors.join("\n")
    );
}

#[test]
fn scan_json_matches_the_schema() {
    let fx = Fixture::projects();
    let report = fx.json(&[]);
    validate(&report);

    // The schema rejects an artifact without its details or with a command cleanup.
    let schema: Value = serde_json::from_str(
        &fs::read_to_string(repo_root().join("schema/lysosome.schema.json")).unwrap(),
    )
    .unwrap();
    let validator = jsonschema::draft202012::new(&schema).unwrap();
    let i = findings(&report)
        .iter()
        .position(|f| f["category"] == "artifacts" && f["status"] == "REMOVABLE")
        .unwrap();
    let mut broken = report.clone();
    broken["findings"][i]["details"] = serde_json::json!({});
    assert!(!validator.is_valid(&broken));
    let mut broken = report.clone();
    broken["findings"][i]["cleanup"] = serde_json::json!({"action": "command", "command": ["rm"]});
    assert!(!validator.is_valid(&broken));
    let mut broken = report.clone();
    broken["findings"][i]["cleanup"] = serde_json::json!({"action": "trash"});
    assert!(!validator.is_valid(&broken));

    // Worktree-heavy fixture through `scan` validates too.
    let wt = Fixture::new("make_worktrees.sh");
    let code = wt.code();
    let out = wt.run(&["scan", code.to_str().unwrap(), "--json"]);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    validate(&report);
    assert_eq!(
        findings(&report)
            .iter()
            .filter(|f| f["category"] == "worktrees")
            .count(),
        9
    );
    assert_eq!(report["stale_metadata"].as_array().unwrap().len(), 1);
}

/// Lowercase hex of a digest (sha2 0.11 digests no longer implement `LowerHex`).
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// path -> (mtime, sha256) for files, (mtime, "dir"/"other") otherwise.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (SystemTime, String)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            let mtime = m.modified().unwrap();
            if m.is_dir() {
                out.insert(p.clone(), (mtime, "dir".into()));
                stack.push(p);
            } else if m.is_file() {
                let hash = hex(&Sha256::digest(fs::read(&p).unwrap()));
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
    let fx = Fixture::projects();
    let before = snapshot(&fx.dest);
    assert!(before.len() > 50, "fixture looks empty");
    let code = fx.code();
    fx.run(&["scan", code.to_str().unwrap(), "--json"]);
    fx.run(&["scan", code.to_str().unwrap(), "--all"]);
    fx.run(&[
        "scan",
        fx.dest.to_str().unwrap(),
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
fn table_output_is_sectioned_and_plain_when_piped() {
    let fx = Fixture::projects();
    let code = fx.code();
    let out = fx.run(&["scan", code.to_str().unwrap()]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        !text.contains('\u{1b}'),
        "no ANSI codes when piped:\n{text}"
    );
    for needle in [
        // Exact size depends on the filesystem's directory allocation.
        "3 projects untouched for 30+ days are holding 11.",
        " MB of dependencies and build output.",
        "reclaimable now",
        "regenerable_expensive",
        "── Git worktrees",
        "2 dormant worktrees are holding",
        "── Project build & dependency output",
        "old-web",
        "packages/api/node_modules",
        "REMOVABLE  costly",
        "REMOVABLE  cheap",
        "Nothing was changed.",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
    assert!(
        !text.contains("fresh-py"),
        "active hidden without --all:\n{text}"
    );
    let all = String::from_utf8(fx.run(&["scan", code.to_str().unwrap(), "--all"]).stdout).unwrap();
    assert!(all.contains("fresh-py") && all.contains("ACTIVE"), "{all}");
    let top = String::from_utf8(
        fx.run(&["scan", code.to_str().unwrap(), "--top", "1"])
            .stdout,
    )
    .unwrap();
    // 3 idle projects in the artifacts section, 1 shown.
    assert!(top.contains("… 2 more (use --top N)"), "{top}");

    let json = fx.run(&["scan", code.to_str().unwrap(), "--json"]);
    assert!(
        json.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
}

#[test]
fn days_window_and_profile() {
    let fx = Fixture::projects();
    let code = fx.code();
    let out = fx.run(&[
        "scan",
        code.to_str().unwrap(),
        "--days",
        "100000",
        "--json",
        "--profile",
    ]);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["dormant_after_days"], 100000);
    assert!(findings(&report).iter().all(|f| f["status"] == "ACTIVE"));
    assert_eq!(report["summary"]["reclaimable_bytes"], 0);
    let err = String::from_utf8(out.stderr).unwrap();
    for needle in ["walk", "git", "size", "artifacts", "detectors", "worktrees"] {
        assert!(err.contains(needle), "profile missing {needle}: {err}");
    }
    let text = String::from_utf8(
        fx.run(&["scan", code.to_str().unwrap(), "--days", "100000"])
            .stdout,
    )
    .unwrap();
    assert!(
        text.contains("No project idle for 100000+ days is holding dependencies or build output."),
        "{text}"
    );
}

#[test]
fn root_inside_a_repo_and_missing_roots() {
    let fx = Fixture::projects();
    // Scanning a subfolder still attributes artifacts to the enclosing repo.
    let sub = fx.code().join("old-web/packages");
    let out = fx.run(&["scan", sub.to_str().unwrap(), "--json"]);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let arts: Vec<&Value> = findings(&report)
        .iter()
        .filter(|f| {
            f["category"] == "artifacts"
                && f["details"]["project"]
                    .as_str()
                    .unwrap()
                    .ends_with("/old-web")
        })
        .collect();
    assert_eq!(arts.len(), 1, "{arts:?}");
    assert_eq!(
        arts[0]["details"]["relative_path"],
        "packages/api/node_modules"
    );
    assert_eq!(arts[0]["status"], "REMOVABLE");

    let missing = fx.dest.join("nope");
    let out = fx
        .lysosome()
        .args(["scan", missing.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8(out.stderr).unwrap().contains("nope"));

    let code = fx.code();
    let out = fx.run(&[
        "scan",
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
fn scan_help_is_helpful() {
    let out = Command::cargo_bin("lysosome")
        .unwrap()
        .args(["scan", "--help"])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    for needle in [
        "--days",
        "--depth",
        "--all",
        "--top",
        "--json",
        "check-ignore",
        "REMOVABLE",
    ] {
        assert!(text.contains(needle), "help missing {needle}");
    }
    let out = Command::cargo_bin("lysosome").unwrap().output().unwrap();
    assert!(String::from_utf8(out.stdout).unwrap().contains("scan"));
}

/// `scan` also runs the system detectors: with HOME and PATH pointed at the
/// caches fixture (fake docker, brew, go...), their categories show up next to
/// the project sections in both the JSON and the table.
#[test]
fn scan_includes_system_detectors() {
    let projects = Fixture::projects();
    let caches = Fixture::new("make_caches.sh");
    let fake_bin = caches.dest.join("bin");
    for tool in fs::read_dir(&fake_bin).unwrap() {
        let tool = tool.unwrap().path();
        std::os::unix::fs::symlink(&tool, projects.bin.join(tool.file_name().unwrap())).unwrap();
    }
    let code = projects.code();
    let scan = |extra: &[&str]| {
        let mut args = vec!["scan", code.to_str().unwrap()];
        args.extend_from_slice(extra);
        let out = projects
            .lysosome()
            .env("HOME", caches.dest.join("home"))
            .env("LYSOSOME_TOOL_TIMEOUT_MS", "12000")
            .args(&args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    };
    let report: Value = serde_json::from_slice(&scan(&["--json"])).unwrap();
    validate(&report);
    let has = |cat: &str| findings(&report).iter().any(|f| f["category"] == cat);
    for cat in [
        "artifacts",
        "ai_models",
        "containers",
        "package_caches",
        "android",
    ] {
        assert!(has(cat), "no {cat} findings in scan");
    }
    let llama = findings(&report)
        .iter()
        .find(|f| f["details"]["name"] == "llama3.1:8b")
        .expect("Ollama model by name");
    assert_eq!(llama["cleanup"]["command"][0], "ollama");
    assert!(
        report["summary"]["by_category"]["containers"]
            .as_u64()
            .unwrap()
            > 0
    );

    let only_projects: Value = serde_json::from_slice(&scan(&["--json", "--no-system"])).unwrap();
    assert!(findings(&only_projects)
        .iter()
        .all(|f| matches!(f["category"].as_str(), Some("artifacts" | "worktrees"))));

    let text = String::from_utf8(scan(&[])).unwrap();
    for title in ["AI models", "Containers", "Package caches"] {
        assert!(text.contains(title), "{title} section missing:\n{text}");
    }
    assert!(
        text.contains("llama3.1:8b"),
        "models shown by name:\n{text}"
    );
}

#[test]
fn skip_leaves_exact_folders_out() {
    let fx = Fixture::projects();
    let all = fx.json(&[]);
    let has = |r: &Value, prefix: &str| {
        findings(r)
            .iter()
            .filter(|f| f["category"] == "artifacts")
            .any(|f| fx.rel(f).starts_with(prefix))
    };
    assert!(has(&all, "old-rust/"), "fixture has old-rust artifacts");

    let skip = fx.code().join("old-rust");
    let report = fx.json(&["--skip", skip.to_str().unwrap()]);
    assert!(!has(&report, "old-rust/"), "skipped folder isn't scanned");
    assert!(has(&report, "old-web/"), "everything else still is");

    // The option is global: it also works before the subcommand.
    let code = fx.code();
    let out = fx.run(&[
        "--skip",
        skip.to_str().unwrap(),
        "scan",
        code.to_str().unwrap(),
        "--json",
    ]);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(!has(&report, "old-rust/"));
}
