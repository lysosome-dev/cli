//! End-to-end tests for `lysosome caches` against the fake home and fake
//! tools built by `tests/fixtures/make_caches.sh`. The process environment is
//! cleared: only HOME (a temp dir) and PATH (fake tools) are set, so nothing
//! touches the real home folder or real tools (Docker, Xcode, Homebrew...).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

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
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = Command::new("bash")
            .arg(repo_root().join("tests/fixtures/make_caches.sh"))
            .arg(tmp.path().join("fx"))
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

    fn home(&self) -> PathBuf {
        self.dest.join("home")
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::cargo_bin("lysosome")
            .unwrap()
            .env_clear()
            .env("HOME", self.home())
            .env("PATH", self.dest.join("bin"))
            .env("NO_COLOR", "1")
            // Generous because macOS scans brand-new scripts on first exec,
            // which can take seconds when tests run in parallel.
            .env("LYSOSOME_TOOL_TIMEOUT_MS", "12000")
            .current_dir(&self.dest)
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> std::process::Output {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "lysosome {args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn json(&self, extra: &[&str]) -> Value {
        let mut args = vec!["caches", "--json"];
        args.extend_from_slice(extra);
        serde_json::from_slice(&self.ok(&args).stdout).expect("valid JSON")
    }
}

fn findings<'a>(r: &'a Value, kind: &str) -> Vec<&'a Value> {
    r["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["kind"] == kind)
        .collect()
}

fn one<'a>(r: &'a Value, kind: &str) -> &'a Value {
    let f = findings(r, kind);
    assert_eq!(f.len(), 1, "expected one {kind}, got {f:#?}");
    f[0]
}

fn command(f: &Value) -> Vec<&str> {
    assert_eq!(f["cleanup"]["action"], "command", "{f:#?}");
    f["cleanup"]["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}

fn names(r: &Value, kind: &str) -> Vec<String> {
    let mut v: Vec<String> = findings(r, kind)
        .iter()
        .map(|f| f["details"]["name"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[test]
fn finds_every_fixture_with_names_tiers_and_cleanup() {
    let fx = Fixture::new();
    // Make the fake `uv` hang: the scan must kill it and carry on.
    fs::write(fx.dest.join("hang-uv"), "").unwrap();
    let start = Instant::now();
    let r = fx.json(&[]);
    assert!(
        start.elapsed() < Duration::from_secs(40),
        "a hanging tool must time out"
    );
    let home = fx.home();

    // AI models: named, never hashes; shared layers counted once.
    assert_eq!(
        names(&r, "ollama-model"),
        ["llama3.1:8b", "someone/qwen:latest"]
    );
    let llama = findings(&r, "ollama-model")
        .into_iter()
        .find(|f| f["details"]["name"] == "llama3.1:8b")
        .unwrap();
    assert_eq!(llama["tier"], "regenerable_expensive");
    assert_eq!(llama["status"], "REMOVABLE");
    assert_eq!(command(llama), ["ollama", "rm", "llama3.1:8b"]);
    let size = llama["size_bytes"].as_u64().unwrap();
    let redl = llama["details"]["redownload_bytes"].as_u64().unwrap();
    assert!(redl > size, "re-download includes shared layers");
    assert_eq!(one(&r, "ollama-shared-layers")["status"], "REVIEW");
    assert!(one(&r, "ollama-unreferenced")["cleanup"].is_null());
    assert_eq!(names(&r, "hf-model"), ["meta-llama/Llama-3-8B"]);
    assert_eq!(names(&r, "hf-dataset"), ["squad"]);
    let hf = one(&r, "hf-model");
    assert_eq!(hf["cleanup"]["action"], "trash");
    assert_eq!(
        hf["cleanup"]["paths"][0],
        home.join(".cache/huggingface/hub/models--meta-llama--Llama-3-8B")
            .to_str()
            .unwrap()
    );
    assert_eq!(
        names(&r, "lmstudio-model"),
        ["lmstudio-community/Qwen3-8B-GGUF"]
    );
    assert_eq!(names(&r, "whisper-model"), ["whisper large-v3"]);

    // Containers via the fake `docker`.
    let web = findings(&r, "docker-image")
        .into_iter()
        .find(|f| f["details"]["name"] == "web:1.0")
        .unwrap();
    assert_eq!(web["size_bytes"], 1_100_000_000u64);
    assert_eq!(command(web), ["docker", "image", "rm", "web:1.0"]);
    let pg = findings(&r, "docker-image")
        .into_iter()
        .find(|f| f["details"]["name"] == "postgres:16")
        .unwrap();
    assert_eq!(pg["status"], "REVIEW", "image used by a container");
    let bc = one(&r, "docker-build-cache");
    assert_eq!(bc["tier"], "regenerable");
    assert_eq!(bc["size_bytes"], 300_000_000u64);
    assert_eq!(command(bc), ["docker", "builder", "prune", "--force"]);
    let vol = one(&r, "docker-volume");
    assert_eq!(
        (vol["tier"].as_str(), vol["status"].as_str()),
        (Some("review"), Some("REVIEW"))
    );

    // Package caches: tool-reported paths (brew, go), fallbacks (npm, cargo),
    // and broken / bogus tools ignored (pip exits 1, pnpm prints a missing path, uv hangs).
    assert_eq!(
        command(one(&r, "npm-cache")),
        ["npm", "cache", "clean", "--force"]
    );
    let brew = one(&r, "homebrew-cache");
    assert_eq!(brew["path"], home.join("fake-brew-cache").to_str().unwrap());
    assert_eq!(command(brew), ["brew", "cleanup", "-s"]);
    assert_eq!(
        command(one(&r, "go-modcache")),
        ["go", "clean", "-modcache"]
    );
    assert_eq!(command(one(&r, "go-buildcache")), ["go", "clean", "-cache"]);
    assert_eq!(one(&r, "cargo-registry")["cleanup"]["action"], "trash");
    for absent in ["pip-cache", "pnpm-store", "uv-cache"] {
        assert!(findings(&r, absent).is_empty(), "{absent}");
    }

    // Android: AVD with its .ini trashed together.
    let avd = one(&r, "android-avd");
    assert_eq!(avd["details"]["name"], "Pixel_8");
    assert_eq!(avd["cleanup"]["paths"].as_array().unwrap().len(), 2);

    #[cfg(target_os = "macos")]
    {
        assert_eq!(one(&r, "xcode-deriveddata")["status"], "REMOVABLE");
        assert_eq!(names(&r, "xcode-devicesupport"), ["iOS 17.5 (21F79)"]);
        let ar = one(&r, "xcode-archives");
        assert_eq!(ar["status"], "REVIEW");
        assert!(ar["cleanup"].is_null());
        assert_eq!(
            command(one(&r, "xcode-simulator-unavailable")),
            ["xcrun", "simctl", "delete", "unavailable"]
        );
        let rt = one(&r, "xcode-simulator-runtime");
        assert_eq!(rt["details"]["name"], "iOS 26.0.1 (23A8464) runtime");
        assert_eq!(
            command(rt),
            ["xcrun", "simctl", "runtime", "delete", "RT-1"]
        );
    }
    #[cfg(not(target_os = "macos"))]
    assert!(r["findings"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f["category"] != "xcode"));

    // Every finding: names, tier/status/cleanup consistency, totals.
    let mut removable = 0u64;
    for f in r["findings"].as_array().unwrap() {
        assert!(f["details"]["name"].is_string(), "{f:#?}");
        match f["tier"].as_str().unwrap() {
            "review" => {
                assert_eq!(f["status"], "REVIEW");
                assert!(f["cleanup"].is_null());
            }
            tier => {
                assert_eq!(f["status"], "REMOVABLE", "{f:#?}");
                assert!(f["cleanup"].is_object());
                if tier == "regenerable_expensive" {
                    assert!(f["details"]["redownload_bytes"].as_u64().unwrap() > 0);
                }
                removable += f["size_bytes"].as_u64().unwrap();
            }
        }
    }
    assert_eq!(r["summary"]["reclaimable_bytes"], removable);
}

#[test]
fn json_matches_schema() {
    let fx = Fixture::new();
    let report = fx.json(&[]);
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

    // The schema rejects a review item that carries a cleanup.
    let mut broken = report.clone();
    let vol = broken["findings"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|f| f["kind"] == "docker-volume")
        .unwrap();
    vol["cleanup"] =
        serde_json::json!({"action": "command", "command": ["docker", "volume", "rm", "pgdata"]});
    assert!(!validator.is_valid(&broken));
}

#[test]
fn category_filter_and_table() {
    let fx = Fixture::new();
    let r = fx.json(&["--category", "ai_models"]);
    assert!(!r["findings"].as_array().unwrap().is_empty());
    assert!(r["findings"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f["category"] == "ai_models"));

    let out = fx.ok(&[
        "system",
        "--category",
        "containers",
        "--category",
        "package-caches",
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("CONTAINERS"), "{text}");
    assert!(text.contains("PACKAGE CACHES"), "{text}");
    assert!(!text.contains("AI MODELS"), "{text}");
    assert!(text.contains("web:1.0"), "{text}");
    assert!(text.contains("docker builder prune --force"), "{text}");
    assert!(
        text.contains("└ volume data"),
        "review reasons shown: {text}"
    );
    assert!(text.contains("Nothing was changed."), "{text}");

    let bad = fx.run(&["caches", "--category", "worktrees"]);
    assert!(!bad.status.success());
}

/// Lowercase hex of a digest (sha2 0.11 digests no longer implement `LowerHex`).
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

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
                let target = fs::read_link(&p).unwrap();
                out.insert(p, (mtime, format!("-> {}", target.display())));
            }
        }
    }
    out
}

#[test]
fn scanning_changes_nothing() {
    let fx = Fixture::new();
    let before = snapshot(&fx.dest);
    fx.ok(&["caches", "--json"]);
    fx.ok(&["caches"]);
    let after = snapshot(&fx.dest);
    assert_eq!(before.len(), after.len(), "files appeared or vanished");
    for (p, v) in &before {
        assert_eq!(Some(v), after.get(p), "{} changed", p.display());
    }
}
