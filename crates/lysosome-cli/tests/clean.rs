//! End-to-end tests for `lysosome clean`. Every test builds its own fixture
//! in a temp dir and runs with an isolated HOME and GIT_CONFIG_GLOBAL; "Trash"
//! is redirected into the fixture with LYSOSOME_TRASH_DIR so nothing touches
//! the real home folder or the real Trash.

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
    dest: PathBuf,
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
        Fixture { _tmp: tmp, dest }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dest.join(rel)
    }

    fn s(&self, rel: &str) -> String {
        self.p(rel).to_string_lossy().into_owned()
    }

    fn trash(&self) -> PathBuf {
        self.dest.join("fake-trash")
    }

    fn log_path(&self) -> PathBuf {
        let home = self.dest.join("home");
        if cfg!(target_os = "macos") {
            home.join("Library/Logs/lysosome/actions.log")
        } else {
            home.join(".local/state/lysosome/actions.log")
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("lysosome").unwrap();
        cmd.env("HOME", self.dest.join("home"))
            .env("GIT_CONFIG_GLOBAL", self.dest.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env("LYSOSOME_TRASH_DIR", self.trash())
            .env_remove("XDG_STATE_HOME")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .current_dir(&self.dest);
        cmd
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.cmd().args(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> std::process::Output {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "lysosome {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn git(&self, dir: &str, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.p(dir))
            .args(args)
            .env("HOME", self.dest.join("home"))
            .env("GIT_CONFIG_GLOBAL", self.dest.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn log_lines(&self) -> Vec<Value> {
        fs::read_to_string(self.log_path())
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("log line is JSON"))
            .collect()
    }
}

/// path -> (mtime, sha256) for files, (mtime, "dir") for directories.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (SystemTime, String)> {
    let mut out = BTreeMap::new();
    if fs::symlink_metadata(root).is_err() {
        return out;
    }
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
                let hash = format!("{:x}", Sha256::digest(fs::read(&p).unwrap()));
                out.insert(p, (mtime, hash));
            } else {
                out.insert(p, (mtime, "other".into()));
            }
        }
    }
    out
}

fn diff(
    before: &BTreeMap<PathBuf, (SystemTime, String)>,
    after: &BTreeMap<PathBuf, (SystemTime, String)>,
) -> Vec<String> {
    before
        .iter()
        .filter(|(p, v)| after.get(*p) != Some(*v))
        .map(|(p, _)| p.display().to_string())
        .chain(
            after
                .keys()
                .filter(|p| !before.contains_key(*p))
                .map(|p| format!("{} (new)", p.display())),
        )
        .collect()
}

fn validate(doc: &Value) {
    let schema: Value = serde_json::from_str(
        &fs::read_to_string(repo_root().join("schema/clean.schema.json")).unwrap(),
    )
    .unwrap();
    let v = jsonschema::draft202012::new(&schema).expect("schema compiles");
    let errors: Vec<String> = v
        .iter_errors(doc)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "schema violations:\n{}",
        errors.join("\n")
    );
}

fn item_by_path<'a>(doc: &'a Value, path: &Path) -> Option<&'a Value> {
    doc["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["path"].as_str() == Some(path.to_str().unwrap()))
}

#[test]
fn dry_run_changes_nothing_and_says_how_to_proceed() {
    let fx = Fixture::new("make_worktrees.sh");
    let before = snapshot(&fx.dest);
    // Project scopes only: cache detectors would ask the real tools on PATH
    // (see the caches dry-run test, which uses fake tools).
    let out = fx.ok(&[
        "clean",
        &fx.s("code"),
        &fx.s("elsewhere"),
        "--worktrees",
        "--artifacts",
    ]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Dry run."), "{text}");
    assert!(text.contains("remove worktree"), "{text}");
    assert!(text.contains("Would reclaim"), "{text}");
    assert!(
        text.trim_end().ends_with("Run again with --yes to do it."),
        "{text}"
    );
    assert!(
        !text.contains("app-dirty"),
        "REVIEW worktrees are never planned:\n{text}"
    );

    let out = fx.ok(&[
        "clean",
        &fx.s("code"),
        "--tier",
        "regenerable_expensive",
        "--strip-artifacts",
        "--json",
    ]);
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    validate(&doc);
    assert_eq!(doc["mode"], "dry_run");
    fx.ok(&[
        "clean",
        &fx.s("code"),
        "--artifacts",
        "--worktrees",
        "--json",
    ]);

    let after = snapshot(&fx.dest);
    assert!(!fx.log_path().exists(), "a dry run writes no log");
    let changed = diff(&before, &after);
    assert!(
        changed.is_empty(),
        "dry run modified:\n{}",
        changed.join("\n")
    );
}

#[test]
fn yes_removes_removable_worktrees_with_git_and_never_touches_review_or_orphans() {
    let fx = Fixture::new("make_worktrees.sh");
    let untouchable = [
        "code/app-dirty",
        "code/app-unpushed",
        "code/app-locked",
        "code/solo-wt",
        "code/doomed-wt",
        "code/app-active",
    ];
    let before: Vec<_> = untouchable.iter().map(|d| snapshot(&fx.p(d))).collect();

    let out = fx.ok(&[
        "clean",
        &fx.s("code"),
        "--worktrees",
        "--tier",
        "regenerable_expensive",
        "--yes",
        "--json",
    ]);
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    validate(&doc);
    assert_eq!(doc["mode"], "execute");
    assert_eq!(doc["summary"]["done"], 3, "{doc:#}");
    assert_eq!(doc["summary"]["failed"], 0);
    for i in doc["items"].as_array().unwrap() {
        assert_eq!(i["action"], "worktree_remove");
        assert_eq!(i["status"], "REMOVABLE");
        assert_eq!(i["result"]["outcome"], "done");
    }

    // Gone from disk and from `git worktree list`; branches kept; pruned.
    let list = fx.git("code/app", &["worktree", "list", "--porcelain"]);
    for name in ["app-merged", "app-pushed"] {
        assert!(!fx.p(&format!("code/{name}")).exists());
        assert!(!list.contains(&format!("/{name}\n")), "{list}");
    }
    assert!(!fx.p("elsewhere/app-outside").exists());
    for branch in ["merged", "pushed", "outside"] {
        assert!(
            !fx.git("code/app", &["branch", "--list", branch])
                .trim()
                .is_empty(),
            "branch {branch} must be kept"
        );
    }
    assert!(
        !fx.p("code/app/.git/worktrees/app-gone").exists(),
        "git worktree prune ran"
    );

    // REVIEW, ORPHAN and ACTIVE worktrees: byte-for-byte untouched.
    for (d, b) in untouchable.iter().zip(&before) {
        let changed = diff(b, &snapshot(&fx.p(d)));
        assert!(changed.is_empty(), "{d} modified:\n{}", changed.join("\n"));
    }
    assert!(list.contains("app-dirty") && list.contains("app-locked"));

    // Action log: one entry per item.
    let log = fx.log_lines();
    assert_eq!(log.len(), 3, "{log:#?}");
    for e in &log {
        assert_eq!(e["action"], "worktree_remove");
        assert_eq!(e["outcome"], "done");
        assert_eq!(e["destination"], "git worktree remove");
        assert!(e["ts"].as_str().unwrap().ends_with('Z'));
        assert!(e["target"].as_str().unwrap().contains("app-"));
    }

    // Running again finds nothing left to do.
    let out = fx.ok(&["clean", &fx.s("code"), "--worktrees", "--yes"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("Nothing to clean"));
}

#[test]
fn strip_artifacts_on_a_dirty_worktree_removes_only_ignored_folders() {
    let fx = Fixture::new("make_worktrees.sh");
    let dirty = fx.p("code/app-dirty");
    fs::create_dir_all(dirty.join("node_modules/pkg")).unwrap();
    fs::write(dirty.join("node_modules/pkg/index.js"), vec![b'x'; 50_000]).unwrap();
    let keep: Vec<PathBuf> = ["README.md", "notes.txt", ".git"]
        .iter()
        .map(|f| dirty.join(f))
        .collect();
    let keep_before: Vec<_> = keep.iter().map(|p| fs::read(p).ok()).collect();

    let out = fx.ok(&[
        "clean",
        &fx.s("code"),
        "--strip-artifacts",
        "--tier",
        "regenerable_expensive",
        "--yes",
        "--json",
    ]);
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    validate(&doc);
    let item = item_by_path(&doc, &dirty.join("node_modules")).expect("planned");
    assert_eq!(item["status"], "REVIEW");
    assert_eq!(item["action"], "trash");
    assert_eq!(item["result"]["outcome"], "done");
    assert!(doc["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["action"] == "trash"));

    // The worktree, its edits and its untracked file are still there.
    assert!(dirty.is_dir());
    assert!(!dirty.join("node_modules").exists());
    for (p, b) in keep.iter().zip(&keep_before) {
        assert_eq!(&fs::read(p).ok(), b, "{} changed", p.display());
    }
    let status = fx.git("code/app-dirty", &["status", "--porcelain"]);
    assert!(status.contains("README.md") && status.contains("notes.txt"));
    // Recoverable: the folder is in the (fake) trash with its contents.
    let trashed: Vec<PathBuf> = fs::read_dir(fx.trash())
        .unwrap()
        .map(|e| e.unwrap().path().join("pkg/index.js"))
        .filter(|p| p.exists())
        .collect();
    assert_eq!(trashed.len(), 1, "{trashed:?}");
    assert_eq!(fs::read(&trashed[0]).unwrap().len(), 50_000);
    // The merged worktree kept its folder too (only its node_modules went).
    assert!(fx.p("code/app-merged").is_dir());
    assert!(!fx.p("code/app-merged/node_modules").exists());
    assert!(
        fx.p("code/app-merged/dist/bundle.js").exists(),
        "tracked dist/ stays"
    );
}

#[test]
fn permanent_requires_yes() {
    let fx = Fixture::new("make_worktrees.sh");
    let before = snapshot(&fx.dest);
    let out = fx.run(&["clean", &fx.s("code"), "--permanent"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--permanent") && err.contains("--yes"),
        "{err}"
    );
    assert!(diff(&before, &snapshot(&fx.dest)).is_empty());
}

#[test]
fn artifacts_go_to_the_trash_and_permanent_deletes() {
    let fx = Fixture::new("make_projects.sh");
    let rust_target = fx.p("code/old-rust/target");
    let web_dist = fx.p("code/old-web/dist");
    let web_nm = fx.p("code/old-web/node_modules");
    let venv = fx.p("code/old-web/.venv");
    let active_venv = fx.p("code/fresh-py/.venv");
    let tracked_build = fx.p("code/old-web/build");
    let big = snapshot(&fx.p("big"));

    // Default tier: build output only, to the Trash.
    let out = fx.ok(&["clean", &fx.s("code"), "--artifacts", "--yes", "--json"]);
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    validate(&doc);
    assert_eq!(doc["summary"]["failed"], 0, "{doc:#}");
    assert!(!rust_target.exists() && !web_dist.exists());
    assert!(fx.trash().join("target").exists() || fx.trash().read_dir().unwrap().count() >= 2);
    assert!(web_nm.exists(), "regenerable_expensive needs --tier");
    assert!(venv.exists(), "REVIEW (.git inside) is never touched");
    assert!(active_venv.exists(), "ACTIVE is never touched");
    assert!(
        tracked_build.exists(),
        "tracked folders are never artifacts"
    );

    // --permanent --yes with the costlier tier: node_modules deleted for good,
    // without following the symlink inside it.
    let out = fx.ok(&[
        "clean",
        &fx.s("code"),
        "--artifacts",
        "--tier",
        "regenerable_expensive",
        "--permanent",
        "--yes",
        "--json",
    ]);
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    validate(&doc);
    let item = item_by_path(&doc, &web_nm).expect("node_modules planned");
    assert_eq!(item["action"], "delete");
    assert_eq!(item["result"]["destination"], "deleted permanently");
    assert!(!web_nm.exists());
    assert!(venv.exists() && active_venv.exists());
    assert!(
        diff(&big, &snapshot(&fx.p("big"))).is_empty(),
        "symlink target untouched"
    );
    let log = fx.log_lines();
    assert!(log
        .iter()
        .any(|e| e["action"] == "trash" && e["outcome"] == "done"));
    assert!(log
        .iter()
        .any(|e| e["action"] == "delete" && e["outcome"] == "done"));
}

/// Fake tools on PATH; HOME is the fake caches home; nothing else inherited.
fn caches_cmd(fx: &Fixture) -> Command {
    let mut cmd = Command::cargo_bin("lysosome").unwrap();
    cmd.env_clear()
        .env("HOME", fx.p("home"))
        .env("PATH", fx.p("bin"))
        .env("NO_COLOR", "1")
        .env("LYSOSOME_TOOL_TIMEOUT_MS", "12000")
        .env("LYSOSOME_CLEAN_TIMEOUT_SECS", "60")
        .env("LYSOSOME_TRASH_DIR", fx.trash())
        .current_dir(&fx.dest);
    cmd
}

#[test]
fn command_cleanups_run_the_tool_and_failures_are_reported_per_item() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new("make_caches.sh");
    let marker = fx.p("npm-ran");
    let npm = fx.p("bin/npm");
    fs::write(
        &npm,
        format!(
            "#!/bin/sh\necho \"$@\" > '{}'\n/bin/rm -rf '{}'\necho 'npm cache cleaned'\n",
            marker.display(),
            fx.p("home/.npm/_cacache").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&npm, fs::Permissions::from_mode(0o755)).unwrap();

    // Dry run first: commands are listed, nothing runs, nothing changes.
    let before = snapshot(&fx.dest);
    let out = caches_cmd(&fx)
        .args(["clean", "--caches"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("npm cache clean --force"), "{text}");
    let out = caches_cmd(&fx)
        .args([
            "clean",
            "--caches",
            "--tier",
            "regenerable_expensive",
            "--json",
        ])
        .output()
        .unwrap();
    validate(&serde_json::from_slice(&out.stdout).unwrap());
    let changed = diff(&before, &snapshot(&fx.dest));
    assert!(
        changed.is_empty(),
        "dry run modified:\n{}",
        changed.join("\n")
    );

    let out = caches_cmd(&fx)
        .args(["clean", "--caches", "--yes", "--json"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a failed item makes the exit status 1"
    );
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    validate(&doc);
    let items = doc["items"].as_array().unwrap();
    let by_kind = |k: &str| items.iter().find(|i| i["kind"] == k).cloned();

    let npm = by_kind("npm-cache").expect("npm planned");
    assert_eq!(npm["action"], "command");
    assert_eq!(npm["result"]["outcome"], "done", "{npm:#}");
    assert_eq!(npm["result"]["message"], "npm cache cleaned");
    assert_eq!(
        fs::read_to_string(&marker).unwrap().trim(),
        "cache clean --force"
    );

    // The fake brew only answers `--cache`: `brew cleanup -s` exits 1.
    let brew = by_kind("homebrew-cache")
        .or_else(|| by_kind("brew-cache"))
        .expect("brew planned");
    assert_eq!(brew["result"]["outcome"], "failed", "{brew:#}");
    assert!(brew["result"]["message"]
        .as_str()
        .unwrap()
        .starts_with("exit 1"));

    // Trash-type caches went to the (fake) trash, recoverably.
    let cargo = items
        .iter()
        .find(|i| {
            i["action"] == "trash" && i["path"].as_str().unwrap().ends_with(".cargo/registry")
        })
        .expect("cargo registry planned");
    assert_eq!(cargo["result"]["outcome"], "done");
    assert!(fx.trash().join("registry/cache/index/serde.crate").exists());

    // Costly and review items were never planned.
    assert!(items.iter().all(|i| i["tier"] == "regenerable"));
    assert!(!items
        .iter()
        .any(|i| i["kind"] == "docker-volume" || i["kind"] == "docker-image"));
    assert!(doc["summary"]["failed"].as_u64().unwrap() >= 1);
    assert!(doc["summary"]["done"].as_u64().unwrap() >= 2);

    let log = fx.log_lines_at(&fx.p("home"));
    assert_eq!(log.len(), items.len(), "every item is logged");
    assert!(log
        .iter()
        .any(|e| e["outcome"] == "failed" && e["command"][0] == "brew"));
}

impl Fixture {
    fn log_lines_at(&self, home: &Path) -> Vec<Value> {
        let p = if cfg!(target_os = "macos") {
            home.join("Library/Logs/lysosome/actions.log")
        } else {
            home.join(".local/state/lysosome/actions.log")
        };
        fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

#[test]
fn help_documents_the_safety_defaults() {
    let fx = Fixture::new("make_worktrees.sh");
    let out = fx.ok(&["clean", "--help"]);
    let h = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "--yes",
        "--permanent",
        "--strip-artifacts",
        "--older-than",
        "regenerable_expensive",
        "never REVIEW, ACTIVE or ORPHAN",
        "never --force",
        "actions.log",
    ] {
        assert!(h.contains(needle), "help is missing {needle}:\n{h}");
    }
}
