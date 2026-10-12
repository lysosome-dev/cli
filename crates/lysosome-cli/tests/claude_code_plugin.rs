//! The Claude Code plugin (integrations/claude-code): its manifests, and its
//! hooks driving the real `lysosome` binary against fixture worktrees. HOME,
//! git config, the Trash and the plugin's data folder are all isolated; the
//! worker runs inline (LYSOSOME_HOOK_SYNC=1). Skipped without python3.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn python() -> Option<&'static str> {
    Command::new("python3")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| "python3")
}

struct Fx {
    _tmp: tempfile::TempDir,
    dest: PathBuf,
    data: PathBuf,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let out = Command::new("bash")
            .arg(repo_root().join("tests/fixtures/make_worktrees.sh"))
            .arg(tmp.path().join("fx"))
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
        let data = tmp.path().join("plugin-data");
        Fx {
            _tmp: tmp,
            dest,
            data,
        }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dest.join(rel)
    }

    /// Run a hook with `input` on stdin; returns stdout.
    fn hook(&self, event: &str, mode: &str, input: Value) -> String {
        let mut child = Command::new(python().unwrap())
            .arg(repo_root().join("integrations/claude-code/scripts/lysosome_hook.py"))
            .arg(event)
            .env("HOME", self.p("home"))
            .env("GIT_CONFIG_GLOBAL", self.p("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("LYSOSOME_BIN", assert_cmd::cargo::cargo_bin("lysosome"))
            .env("CLAUDE_PLUGIN_DATA", &self.data)
            .env("CLAUDE_PLUGIN_OPTION_AUTO_TIDY", mode)
            .env("LYSOSOME_HOOK_SYNC", "1")
            .env("LYSOSOME_HOOK_DEBUG", "1")
            .env("LYSOSOME_TRASH_DIR", self.p("trash"))
            .env("XDG_STATE_HOME", self.p("state"))
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "hook {event} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn end(&self, mode: &str, reason: &str, cwd: &Path) {
        self.hook(
            "session-end",
            mode,
            serde_json::json!({"session_id": "s1", "reason": reason, "cwd": cwd, "hook_event_name": "SessionEnd"}),
        );
    }

    fn start(&self) -> String {
        self.hook(
            "session-start",
            "off",
            serde_json::json!({"session_id": "s2", "source": "startup", "cwd": self.p("code/app")}),
        )
    }

    fn recorded(&self) -> Vec<Value> {
        fs::read_to_string(self.data.join("worktrees.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn actors(&self) -> Vec<String> {
        let log = if cfg!(target_os = "macos") {
            self.p("home/Library/Logs/lysosome/actions.log")
        } else {
            self.p("state/lysosome/actions.log")
        };
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| v["actor"].as_str().map(String::from))
            .collect()
    }
}

#[test]
fn manifests_are_valid_json_and_point_at_the_plugin() {
    let m: Value = serde_json::from_str(
        &fs::read_to_string(repo_root().join(".claude-plugin/marketplace.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(m["name"], "lysosome");
    let entry = &m["plugins"][0];
    let dir = repo_root().join(entry["source"].as_str().unwrap());
    let p: Value =
        serde_json::from_str(&fs::read_to_string(dir.join(".claude-plugin/plugin.json")).unwrap())
            .unwrap();
    assert_eq!(p["name"], entry["name"], "entry and manifest names match");
    assert_eq!(
        p["userConfig"]["auto_tidy"]["default"], "off",
        "safe by default"
    );
    let hooks: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("hooks/hooks.json")).unwrap()).unwrap();
    for ev in ["SessionEnd", "SessionStart"] {
        let cmd = hooks["hooks"][ev][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.ends_with("|| true"), "{ev} never fails a session");
    }
    for c in ["status", "tidy"] {
        assert!(dir.join(format!("commands/{c}.md")).is_file());
    }
}

#[test]
fn off_by_default_records_and_reports_once_without_touching_anything() {
    if python().is_none() {
        eprintln!("skipped: no python3");
        return;
    }
    let fx = Fx::new();
    // The main checkout is not a worktree: nothing recorded.
    fx.end("off", "other", &fx.p("code/app"));
    assert!(fx.recorded().is_empty());

    fx.end("off", "prompt_input_exit", &fx.p("code/app-merged"));
    let r = fx.recorded();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["status"], "REMOVABLE");
    assert_eq!(r[0]["merged"], true);
    assert!(r[0]["action"].is_null());
    assert!(
        fx.p("code/app-merged/node_modules").is_dir(),
        "off changes nothing"
    );

    let out: Value = serde_json::from_str(fx.start().trim()).unwrap();
    let msg = out["systemMessage"].as_str().unwrap();
    assert!(
        msg.contains("/lysosome:tidy") && msg.contains("can go"),
        "{msg}"
    );
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(fx.start().trim().is_empty(), "reported once");
}

#[test]
fn strip_merged_frees_build_output_and_keeps_the_worktree() {
    if python().is_none() {
        return;
    }
    let fx = Fx::new();
    fx.end("strip-merged", "other", &fx.p("code/app-merged"));
    assert!(!fx.p("code/app-merged/node_modules").exists());
    assert!(fx.p("code/app-merged/README.md").exists(), "worktree kept");
    assert!(
        fx.p("code/app-merged/dist").exists(),
        "tracked dist/ is never an artifact"
    );
    assert!(fx.p("trash/node_modules").exists(), "moved to the Trash");
    assert_eq!(fx.recorded()[0]["action"]["kind"], "stripped");
    assert!(fx.actors().iter().all(|a| a == "claude-code") && !fx.actors().is_empty());
    let msg = fx.start();
    assert!(msg.contains("tidied up after earlier sessions"), "{msg}");
}

#[test]
fn remove_merged_removes_only_finished_worktrees_and_only_when_the_session_ended() {
    if python().is_none() {
        return;
    }
    let fx = Fx::new();
    // /clear keeps the session going in the same folder: never act.
    fx.end("remove-merged", "clear", &fx.p("code/app-merged"));
    assert!(fx.p("code/app-merged").is_dir());
    // Dirty (REVIEW) and pushed-but-unmerged worktrees stay.
    fx.end("remove-merged", "other", &fx.p("code/app-dirty"));
    fx.end("remove-merged", "other", &fx.p("code/app-pushed"));
    assert!(fx.p("code/app-dirty").is_dir());
    assert!(fx.p("code/app-pushed").is_dir());
    // A finished one goes, with git; its branch is kept.
    fx.end("remove-merged", "other", &fx.p("code/app-merged"));
    assert!(!fx.p("code/app-merged").exists());
    let r = fx.recorded();
    assert_eq!(r.last().unwrap()["action"]["kind"], "removed");
    assert!(r.iter().take(3).all(|e| e["action"].is_null()));
}

#[test]
fn missing_cli_or_bad_input_is_silent() {
    if python().is_none() {
        return;
    }
    let script = repo_root().join("integrations/claude-code/scripts/lysosome_hook.py");
    for (event, input) in [("session-end", "not json"), ("session-start", "{}")] {
        let tmp = tempfile::tempdir().unwrap();
        let mut child = Command::new("python3")
            .arg(&script)
            .arg(event)
            .env("LYSOSOME_BIN", "/nonexistent/lysosome")
            .env("CLAUDE_PLUGIN_DATA", tmp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
    }
}
