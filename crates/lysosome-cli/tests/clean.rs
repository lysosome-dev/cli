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
            .map(|l| serde_json::from_str::<Value>(l).expect("log line is JSON"))
            // Every item gets an "attempt" line before acting; keep outcomes.
            .filter(|v| v["outcome"] != "attempt")
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
    // An ignored file that isn't build output: `git worktree remove` would
    // delete it for good, so clean moves it to the Trash first.
    fs::write(fx.p("code/app/.git/info/exclude"), "secrets.env\n").unwrap();
    fs::write(fx.p("code/app-merged/secrets.env"), "TOKEN=keep-me\n").unwrap();
    // A clone with local work inside an ignored folder: git would delete it
    // with the worktree, so that worktree must be kept.
    fs::create_dir_all(fx.p("code/app-pushed/node_modules/dep/.git")).unwrap();

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
    assert_eq!(doc["summary"]["done"], 2, "{doc:#}");
    assert_eq!(doc["summary"]["skipped"], 1, "{doc:#}");
    assert_eq!(doc["summary"]["failed"], 0);
    for i in doc["items"].as_array().unwrap() {
        assert_eq!(i["action"], "worktree_remove");
        assert_eq!(i["status"], "REMOVABLE");
    }
    let pushed = item_by_path(&doc, &fx.p("code/app-pushed")).unwrap();
    assert_eq!(pushed["result"]["outcome"], "skipped");
    assert!(
        pushed["result"]["message"]
            .as_str()
            .unwrap()
            .contains("git checkout"),
        "{pushed:#}"
    );
    assert!(fx.p("code/app-pushed/node_modules/dep/.git").is_dir());

    assert_eq!(
        fs::read_to_string(fx.trash().join("secrets.env")).unwrap(),
        "TOKEN=keep-me\n",
        "ignored non-artifact files are rescued to the Trash"
    );
    assert!(
        !fx.trash().join("node_modules").exists(),
        "regenerable artifact folders are left to git"
    );

    // Gone from disk and from `git worktree list`; branches kept; pruned.
    let list = fx.git("code/app", &["worktree", "list", "--porcelain"]);
    assert!(!fx.p("code/app-merged").exists());
    assert!(!list.contains("/app-merged\n"), "{list}");
    assert!(list.contains("/app-pushed\n"), "{list}");
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
    let raw = fs::read_to_string(fx.log_path()).unwrap();
    assert_eq!(raw.matches("\"outcome\":\"attempt\"").count(), 3, "{raw}");
    let log: Vec<Value> = fx
        .log_lines()
        .into_iter()
        .filter(|e| e["outcome"] == "done")
        .collect();
    assert_eq!(log.len(), 2, "{log:#?}");
    for e in &log {
        assert_eq!(e["action"], "worktree_remove");
        assert_eq!(e["destination"], "git worktree remove");
        assert!(e["ts"].as_str().unwrap().ends_with('Z'));
        assert!(e["target"].as_str().unwrap().contains("app-"));
    }

    // Running again: only the kept worktree is planned, and kept again.
    let out = fx.ok(&["clean", &fx.s("code"), "--worktrees", "--yes"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("1 skipped"), "{text}");
    assert!(fx.p("code/app-pushed").is_dir());
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

/// Write an executable script.
fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Regression test for #13: through corepack, `pnpm store path` downloads
/// pnpm into `~/.cache/node/corepack` on first use. A scan or dry run with
/// corepack's shims on PATH must not run them, and must still find the store
/// at its default location.
#[test]
fn corepack_shims_are_never_run_by_a_scan_or_dry_run() {
    let fx = Fixture::new("make_caches.sh");
    let home = fx.p("home");
    let marker = fx.p("corepack-ran");
    // What `corepack enable` leaves on PATH: symlinks into corepack's dist/.
    // Each "shim" simulates corepack's first-run download.
    let dist = fx.p("node/lib/node_modules/corepack/dist");
    for tool in ["pnpm", "yarn"] {
        script(
            &dist.join(format!("{tool}.js")),
            &format!(
                "#!/bin/sh\n/bin/mkdir -p \"$HOME/.cache/node/corepack/v1/{tool}/9.0.0\"\n/usr/bin/touch '{}'\necho \"$HOME\"\n",
                marker.display()
            ),
        );
        let link = fx.p("bin").join(tool);
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(
            format!("../node/lib/node_modules/corepack/dist/{tool}.js"),
            &link,
        )
        .unwrap();
    }
    let store = if cfg!(target_os = "macos") {
        home.join("Library/pnpm/store")
    } else {
        home.join(".local/share/pnpm/store")
    };
    fs::create_dir_all(store.join("v10/files/00")).unwrap();
    fs::write(store.join("v10/files/00/blob"), vec![b'x'; 64 * 1024]).unwrap();
    let yarn = if cfg!(target_os = "macos") {
        home.join("Library/Caches/Yarn")
    } else {
        home.join(".cache/yarn")
    };
    fs::create_dir_all(yarn.join("v6/npm-left-pad")).unwrap();
    fs::write(yarn.join("v6/npm-left-pad/index.js"), vec![b'x'; 64 * 1024]).unwrap();

    let before = snapshot(&fx.dest);
    let out = caches_cmd(&fx).args(["caches", "--json"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    let path_of = |kind: &str| {
        doc["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["kind"] == kind)
            .and_then(|f| f["path"].as_str())
            .map(PathBuf::from)
    };
    assert_eq!(
        path_of("pnpm-store"),
        Some(store.clone()),
        "default store found"
    );
    assert_eq!(
        path_of("yarn-cache"),
        Some(yarn.clone()),
        "default cache found"
    );
    for args in [
        &["clean", "--caches"][..],
        &[
            "clean",
            "--caches",
            "--tier",
            "regenerable_expensive",
            "--json",
        ][..],
    ] {
        let out = caches_cmd(&fx).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let text = String::from_utf8_lossy(
        &caches_cmd(&fx)
            .args(["clean", "--caches"])
            .output()
            .unwrap()
            .stdout,
    )
    .into_owned();
    assert!(text.contains("pnpm store prune"), "{text}");

    assert!(!marker.exists(), "a corepack shim was run during a scan");
    assert!(
        !home.join(".cache/node").exists(),
        "corepack downloaded during a scan"
    );
    let changed = diff(&before, &snapshot(&fx.dest));
    assert!(
        changed.is_empty(),
        "scan / dry run modified:\n{}",
        changed.join("\n")
    );
}

/// A real pnpm install (here npm-global style: bin/pnpm ->
/// lib/node_modules/pnpm/bin/pnpm.cjs) is still asked, but only with
/// downloads and self-management turned off and from `/`, never the user's
/// project folder.
#[test]
fn real_pnpm_is_asked_with_downloads_disabled() {
    let fx = Fixture::new("make_caches.sh");
    let marker = fx.p("unsafe-env");
    let custom = fx.p("home/custom-pnpm-store");
    fs::create_dir_all(custom.join("v10")).unwrap();
    fs::write(custom.join("v10/blob"), vec![b'x'; 64 * 1024]).unwrap();
    let real = fx.p("node/lib/node_modules/pnpm/bin/pnpm.cjs");
    script(
        &real,
        &format!(
            "#!/bin/sh\n\
             [ \"$COREPACK_ENABLE_NETWORK\" = 0 ] && [ \"$COREPACK_ENABLE_DOWNLOAD_PROMPT\" = 0 ] \\\n\
               && [ \"$npm_config_manage_package_manager_versions\" = false ] && [ \"$(pwd)\" = / ] \\\n\
               || /usr/bin/touch '{}'\n\
             echo '{}'\n",
            marker.display(),
            custom.display()
        ),
    );
    let _ = fs::remove_file(fx.p("bin/pnpm"));
    std::os::unix::fs::symlink(&real, fx.p("bin/pnpm")).unwrap();
    let before = snapshot(&fx.dest);
    let out = caches_cmd(&fx)
        .env("COREPACK_ENABLE_NETWORK", "1")
        .args(["caches", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    let pnpm = doc["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "pnpm-store")
        .expect("pnpm store found");
    assert_eq!(pnpm["path"], custom.to_str().unwrap());
    assert!(!marker.exists(), "pnpm ran with an unsafe environment");
    assert!(diff(&before, &snapshot(&fx.dest)).is_empty());
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
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["outcome"] != "attempt")
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

/// Drive `lysosome <args>` on a pseudo-terminal (Python's `pty`, already a
/// test dependency for the prototype parity test). `steps` are
/// `(wait_for_text, keys_to_send)`; returns everything the terminal showed.
/// `None` when python3 isn't available.
fn run_in_pty(fx: &Fixture, args: &[&str], steps: &[(&str, &str)]) -> Option<(i32, String)> {
    const DRIVER: &str = r#"
import fcntl, json, os, pty, select, struct, sys, termios, time
steps = json.loads(sys.argv[1]); argv = sys.argv[2:]
pid, fd = pty.fork()
if pid == 0:
    os.execv(argv[0], argv)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
buf = b""
def pump(deadline):
    global buf
    r, _, _ = select.select([fd], [], [], max(0, min(0.2, deadline - time.time())))
    if r:
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            return False
        if not chunk:
            return False
        buf += chunk
    return True
for want, keys in steps:
    deadline = time.time() + 120
    while want.encode() not in buf:
        if time.time() > deadline or not pump(deadline):
            sys.stdout.write(buf.decode("utf-8", "replace"))
            sys.stderr.write("timed out waiting for %r\n" % want)
            sys.exit(97)
    time.sleep(0.2)
    if keys:
        try:
            os.write(fd, keys.encode())
        except OSError:
            pass
deadline = time.time() + 300
while time.time() < deadline and pump(deadline):
    pass
_, status = os.waitpid(pid, 0)
sys.stdout.write(buf.decode("utf-8", "replace"))
sys.exit(os.waitstatus_to_exitcode(status))
"#;
    if Command::new("python3").arg("--version").output().is_err() {
        // Don't let a missing interpreter turn the only end-to-end test of
        // interactive deletion into a silent pass.
        assert!(
            std::env::var_os("LYSOSOME_SKIP_PTY_TESTS").is_some(),
            "python3 is needed for the pty tests (set LYSOSOME_SKIP_PTY_TESTS=1 to skip them)"
        );
        return None;
    }
    let bin = assert_cmd::cargo::cargo_bin("lysosome");
    let steps_json = serde_json::to_string(
        &steps
            .iter()
            .map(|(w, k)| vec![w.to_string(), k.to_string()])
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let base = fx.cmd();
    let mut cmd = Command::new("python3");
    cmd.arg("-c")
        .arg(DRIVER)
        .arg(steps_json)
        .arg(bin)
        .args(args);
    for (k, v) in base.get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd.env("TERM", "xterm-256color").current_dir(&fx.dest);
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_ne!(
        out.status.code(),
        Some(97),
        "pty driver timed out:\n{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some((out.status.code().unwrap_or(-1), text))
}

#[test]
fn interactive_clean_acts_only_on_picked_items_after_confirmation() {
    let fx = Fixture::new("make_worktrees.sh");
    let code = fx.s("code");
    let args = ["clean", code.as_str(), "--worktrees", "-i"];
    // Cancelling the picker changes nothing.
    let before = snapshot(&fx.dest);
    let Some((status, text)) = run_in_pty(&fx, &args, &[("Choose what to clean", "q")]) else {
        eprintln!("python3 not available; skipping the pty test");
        return;
    };
    assert_eq!(status, 0, "{text}");
    assert!(text.contains("Nothing was changed."), "{text}");
    // REVIEW worktrees are listed, greyed out, with their reason.
    assert!(text.contains("1 unpushed commit, not merged"), "{text}");
    assert!(text.contains("[-] review"), "{text}");
    assert!(diff(&before, &snapshot(&fx.dest)).is_empty());

    // Saying no at the final prompt changes nothing either.
    let (status, text) = run_in_pty(
        &fx,
        &args,
        &[("Choose what to clean", "\r"), ("Proceed? [y/N]", "n\r")],
    )
    .unwrap();
    assert_eq!(status, 0, "{text}");
    assert!(text.contains("Will reclaim"), "{text}");
    assert!(text.contains("Nothing was changed."), "{text}");
    assert!(diff(&before, &snapshot(&fx.dest)).is_empty());

    // Untick the first (largest) worktree, app-merged, and confirm.
    let (status, text) = run_in_pty(
        &fx,
        &args,
        &[("Choose what to clean", " \r"), ("Proceed? [y/N]", "y\r")],
    )
    .unwrap();
    assert_eq!(status, 0, "{text}");
    assert!(text.contains("Reclaimed"), "{text}");
    assert!(fx.p("code/app-merged").exists(), "unticked, kept:\n{text}");
    assert!(
        !fx.p("code/app-pushed").exists(),
        "ticked, removed:\n{text}"
    );
    for kept in [
        "code/app-dirty",
        "code/app-unpushed",
        "code/app-locked",
        "code/solo-wt",
    ] {
        assert!(fx.p(kept).exists(), "REVIEW {kept} must never be touched");
    }
    let removed: Vec<String> = fx
        .log_lines()
        .iter()
        .filter(|l| l["outcome"] == "done")
        .map(|l| l["target"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        removed.iter().any(|p| p.ends_with("app-pushed")),
        "{removed:?}"
    );
    assert!(
        !removed.iter().any(|p| p.ends_with("app-merged")),
        "{removed:?}"
    );
}

#[test]
fn interactive_needs_a_terminal_and_rejects_json() {
    let fx = Fixture::new("make_worktrees.sh");
    let out = fx.run(&["clean", &fx.s("code"), "-i"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--interactive needs a terminal"), "{err}");
    assert!(err.contains("without -i"), "suggests a fix: {err}");
    let out = fx.run(&["clean", &fx.s("code"), "-i", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    // With --json the clap error is a JSON document on stdout.
    let err = String::from_utf8_lossy(&out.stdout);
    assert!(
        err.contains("cannot be used with"),
        "a clap conflict: {err}"
    );
}

#[test]
fn interactive_with_nothing_to_pick_explains_what_is_kept() {
    let fx = Fixture::new("make_worktrees.sh");
    let code = fx.s("code");
    let Some((status, text)) = run_in_pty(
        &fx,
        &[
            "clean",
            code.as_str(),
            "--worktrees",
            "-i",
            "--only",
            "*app-dirty",
        ],
        &[("Kept for review", "")],
    ) else {
        return;
    };
    assert_eq!(status, 0, "{text}");
    assert!(text.contains("Nothing to clean"), "{text}");
    assert!(
        text.contains("1 uncommitted change, 1 untracked file"),
        "{text}"
    );
    assert!(!text.contains("Choose what to clean"), "{text}");
}

#[test]
fn squash_merged_worktrees_are_cleaned_after_asking_github_again() {
    let fx = Fixture::new("make_worktrees.sh");
    fx.git(
        "code/app",
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/acme/app.git",
        ],
    );
    let head = fx.git("code/app-unpushed", &["rev-parse", "HEAD"]);
    let head = head.trim();
    let bin = fx.p("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    let log = fx.p("gh.log");
    // Answers "merged at HEAD" to the first lookup only: the scan's. The
    // re-check right before removal asks again and then gets nothing.
    let once = fx.p("gh-answered");
    fs::write(
        bin.join("gh"),
        format!(
            "#!/bin/sh\necho \"$*\" >> '{log}'\ncase \"$1\" in\n  auth) exit 0;;\n  repo) echo '{{\"defaultBranchRef\":{{\"name\":\"main\"}},\"owner\":{{\"login\":\"acme\"}}}}';;\n  pr) if [ -e '{once}' ] && [ -n \"$GH_ONCE\" ]; then echo '[]'; else touch '{once}'; echo '[{{\"number\":42,\"headRefOid\":\"{head}\",\"mergedAt\":\"2026-01-01T00:00:00Z\",\"url\":\"https://github.com/acme/app/pull/42\",\"baseRefName\":\"main\",\"headRepositoryOwner\":{{\"login\":\"acme\"}}}}]'; fi;;\n  *) exit 1;;\nesac\n",
            log = log.display(),
            once = once.display(),
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(bin.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = std::ffi::OsString::from(&bin);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    let code = fx.s("code");
    let only = fx.s("code/app-unpushed");
    let run = |extra: &[&str], once: bool| {
        let mut cmd = fx.cmd();
        cmd.env("PATH", &path)
            .args([
                "clean",
                code.as_str(),
                "--worktrees",
                "--only",
                only.as_str(),
            ])
            .args(extra);
        if once {
            cmd.env("GH_ONCE", "1");
        }
        cmd.output().unwrap()
    };

    // --no-gh: it stays REVIEW, nothing is planned, gh never runs.
    let out = run(&["--no-gh", "--json"], false);
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    validate(&doc);
    assert_eq!(doc["items"].as_array().unwrap().len(), 0);
    assert!(!log.exists());

    // The PR "disappears" between scan and removal: skipped, kept.
    let out = run(&["--yes", "--json"], true);
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    validate(&doc);
    let item = &doc["items"][0];
    assert_eq!(item["reason"], "clean, squash-merged in PR #42");
    assert_eq!(item["result"]["outcome"], "skipped", "{item}");
    assert!(fx.p("code/app-unpushed").is_dir());

    // Confirmed again at removal time: removed with git, branch kept.
    let out = run(&["--yes", "--json"], false);
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["items"][0]["result"]["outcome"], "done", "{doc}");
    assert!(!fx.p("code/app-unpushed").exists());
    assert_eq!(fx.git("code/app", &["rev-parse", "unpushed"]).trim(), head);
}
