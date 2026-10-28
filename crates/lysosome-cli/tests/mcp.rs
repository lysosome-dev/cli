//! End-to-end tests for `lysosome mcp`: JSON-RPC over the binary's
//! stdin/stdout, against a fixture with an isolated HOME and a fake Trash
//! (LYSOSOME_TRASH_DIR), so nothing touches the real home folder.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{json, Value};

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
            .arg(repo_root().join("tests/fixtures/make_projects.sh"))
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

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("lysosome").unwrap();
        cmd.env("HOME", self.dest.join("home"))
            .env("GIT_CONFIG_GLOBAL", self.dest.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env("LYSOSOME_TRASH_DIR", self.dest.join("fake-trash"))
            .env_remove("LYSOSOME_ACTOR")
            .env_remove("XDG_STATE_HOME")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .current_dir(&self.dest);
        cmd
    }

    /// Send `requests` (one per line), close stdin, and return the replies by id.
    fn session(&self, requests: &[Value]) -> BTreeMap<i64, Value> {
        let mut child = self
            .cmd()
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            for r in requests {
                writeln!(stdin, "{r}").unwrap();
            }
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut replies = BTreeMap::new();
        for line in String::from_utf8(out.stdout).unwrap().lines() {
            // Nothing but protocol on stdout.
            let v: Value = serde_json::from_str(line).expect("stdout line is JSON");
            assert_eq!(v["jsonrpc"], "2.0", "{line}");
            replies.insert(v["id"].as_i64().expect("reply has an id"), v);
        }
        replies
    }

    fn log_lines(&self) -> Vec<Value> {
        let home = self.dest.join("home");
        let log = if cfg!(target_os = "macos") {
            home.join("Library/Logs/lysosome/actions.log")
        } else {
            home.join(".local/state/lysosome/actions.log")
        };
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

fn init(id: i64, version: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "initialize", "params": {
        "protocolVersion": version, "capabilities": {},
        "clientInfo": { "name": "test", "version": "0" } } })
}

fn call(id: i64, name: &str, arguments: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": arguments } })
}

/// The tool result's text, parsed, and its isError flag.
fn result_doc(reply: &Value) -> (Value, bool) {
    let r = &reply["result"];
    let text = r["content"][0]["text"].as_str().expect("text content");
    (
        serde_json::from_str(text).expect("tool text is JSON"),
        r["isError"].as_bool().unwrap(),
    )
}

#[test]
fn handshake_lists_tools_and_negotiates_the_version() {
    let fx = Fixture::new();
    let r = fx.session(&[
        init(1, "2025-06-18"),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/list" }),
        init(5, "1999-01-01"),
        init(6, "2024-11-05"),
        call(7, "nope", json!({})),
    ]);
    assert_eq!(r.len(), 7, "the notification gets no reply: {r:#?}");
    assert_eq!(r[&1]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(r[&1]["result"]["serverInfo"]["name"], "lysosome");
    assert!(r[&1]["result"]["capabilities"]["tools"].is_object());
    assert_eq!(r[&2]["result"], json!({}));
    let tools = r[&3]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["scan", "worktrees", "caches", "clean"]);
    for t in tools {
        let destructive = t["name"] == "clean";
        assert_eq!(t["annotations"]["readOnlyHint"], !destructive, "{t}");
        assert_eq!(t["annotations"]["destructiveHint"], destructive, "{t}");
        assert_eq!(t["inputSchema"]["type"], "object");
    }
    assert_eq!(r[&4]["error"]["code"], -32601);
    assert_eq!(r[&5]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(r[&6]["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(r[&7]["error"]["code"], -32602);
}

#[test]
fn scan_returns_the_same_report_as_scan_json() {
    let fx = Fixture::new();
    let args = json!({ "roots": [fx.s("code")], "no_system": true, "offline": true });
    let r = fx.session(&[init(1, "2025-06-18"), call(2, "scan", args)]);
    let (doc, is_error) = result_doc(&r[&2]);
    assert!(!is_error, "{doc:#}");

    let out = fx
        .cmd()
        .args(["scan", &fx.s("code"), "--no-system", "--offline", "--json"])
        .output()
        .unwrap();
    let cli: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids = |d: &Value| -> Vec<String> {
        d["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(doc["schema_version"], cli["schema_version"]);
    assert!(!ids(&doc).is_empty());
    assert_eq!(ids(&doc), ids(&cli));
    assert_eq!(doc["summary"], cli["summary"]);
}

#[test]
fn clean_changes_nothing_without_confirm_and_exact_ids() {
    let fx = Fixture::new();
    let target = fx.p("code/old-rust/target");
    let code = fx.s("code");
    let r = fx.session(&[
        init(1, "2025-06-18"),
        call(
            2,
            "clean",
            json!({ "roots": [code], "artifacts": true, "offline": true }),
        ),
        call(
            3,
            "clean",
            json!({ "roots": [code], "artifacts": true, "confirm": true }),
        ),
        call(
            4,
            "clean",
            json!({ "roots": [code], "artifacts": true, "confirm": true, "only": ["target*"] }),
        ),
        call(
            5,
            "clean",
            json!({ "roots": [code], "artifacts": true, "confirm": true, "only": ["target"] }),
        ),
        call(6, "clean", json!({ "roots": [code], "permanent": true })),
        call(
            7,
            "clean",
            json!({ "roots": [code], "only": ["target"], "offline": true }),
        ),
        // Shaped like an id but a glob for --only: matched exactly, so nothing.
        call(
            8,
            "clean",
            json!({ "roots": [code], "artifacts": true, "offline": true,
                    "confirm": true, "only": ["target:*"] }),
        ),
    ]);
    let (plan, is_error) = result_doc(&r[&2]);
    assert!(!is_error, "{plan:#}");
    assert_eq!(plan["mode"], "dry_run");
    assert!(plan["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["path"].as_str() == target.to_str()));
    for id in [3, 4, 5, 6] {
        let (doc, is_error) = result_doc(&r[&id]);
        assert!(is_error, "call {id} must be refused: {doc:#}");
        assert_eq!(doc["error"]["code"], "usage", "{doc:#}");
    }
    // A glob is fine for a dry run.
    let (doc, is_error) = result_doc(&r[&7]);
    assert!(!is_error && doc["mode"] == "dry_run", "{doc:#}");
    let (doc, is_error) = result_doc(&r[&8]);
    assert!(!is_error, "{doc:#}");
    assert!(doc["items"].as_array().unwrap().is_empty(), "{doc:#}");
    assert!(target.exists(), "nothing was cleaned");
    assert!(!fx.p("fake-trash").exists() || fs::read_dir(fx.p("fake-trash")).unwrap().count() == 0);
}

#[test]
fn confirmed_clean_trashes_only_the_given_ids_and_logs_the_actor() {
    let fx = Fixture::new();
    let target = fx.p("code/old-rust/target");
    let dist = fx.p("code/old-web/dist");
    let code = fx.s("code");
    let plan = fx.session(&[
        init(1, "2025-06-18"),
        call(
            2,
            "clean",
            json!({ "roots": [code], "artifacts": true, "offline": true }),
        ),
    ]);
    let (plan, _) = result_doc(&plan[&2]);
    let id = plan["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["path"].as_str() == target.to_str())
        .map(|i| i["id"].as_str().unwrap().to_string())
        .expect("target planned");

    let r = fx.session(&[
        init(1, "2025-06-18"),
        call(
            2,
            "clean",
            json!({ "roots": [code], "artifacts": true, "offline": true,
                                 "confirm": true, "only": [id] }),
        ),
    ]);
    let (doc, is_error) = result_doc(&r[&2]);
    assert!(!is_error, "{doc:#}");
    assert_eq!(doc["mode"], "execute");
    assert_eq!(doc["items"].as_array().unwrap().len(), 1, "{doc:#}");
    assert!(!target.exists());
    assert!(dist.exists(), "only the confirmed id is cleaned");
    assert!(fs::read_dir(fx.p("fake-trash")).unwrap().count() >= 1);
    let log = fx.log_lines();
    assert!(!log.is_empty());
    assert!(log.iter().all(|l| l["actor"] == "mcp"), "{log:#?}");
}
