//! `lysosome mcp`: a Model Context Protocol server on stdin/stdout.
//!
//! Newline-delimited JSON-RPC 2.0. Every tool call is turned into the
//! arguments of the matching command and runs the same code as
//! `lysosome <command> --json`, so results follow the versioned JSON
//! contract in `schema/`. `clean` is a dry run unless the call confirms
//! exact item ids; `--permanent` and `--interactive` are not reachable.
//! Only protocol goes to stdout.

use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::process::ExitCode;

use clap::{Args, Parser};
use serde_json::{json, Map, Value};

use crate::{caches, clean, errors, scan, worktrees, Cli, Command};

/// Newest first; an unknown client version gets the newest.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "\
lysosome finds disk that dev tools and coding agents leave behind (dormant git \
worktrees, git-ignored build output of idle projects, package caches, AI models, \
Docker, Xcode) and labels every item by safety tier. scan, worktrees and caches \
are read-only. clean is a dry run unless you pass confirm: true together with the \
exact item ids (from a scan or a clean dry run) in only. Files go to the Trash, \
every item is re-checked right before acting, and REVIEW items are never cleaned. \
Pass roots to keep scans fast; without roots the whole home folder is walked.";

#[derive(Debug, Clone, Args)]
pub struct McpArgs {}

pub fn serve() -> ExitCode {
    // Cleanups through MCP are attributed in the action log unless the
    // client already says who it is. Set before any thread exists.
    if std::env::var_os("LYSOSOME_ACTOR").is_none_or(|a| a.is_empty()) {
        std::env::set_var("LYSOSOME_ACTOR", "mcp");
    }
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match stdin.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("lysosome mcp: could not read stdin: {e}");
                return ExitCode::FAILURE;
            }
        }
        // Invalid UTF-8 becomes a parse error reply, not a silent exit.
        let line = String::from_utf8_lossy(&buf);
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle_line(&line) {
            let mut text = reply.to_string();
            text.push('\n');
            if stdout
                .write_all(text.as_bytes())
                .and_then(|_| stdout.flush())
                .is_err()
            {
                break;
            }
        }
    }
    ExitCode::SUCCESS
}

/// One line in, at most one line out (notifications get no reply).
fn handle_line(line: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(line) {
        Ok(Value::Array(batch)) if batch.is_empty() => Some(error_reply(
            Value::Null,
            -32600,
            "invalid request: empty batch",
        )),
        Ok(Value::Array(batch)) => {
            let replies: Vec<Value> = batch.iter().filter_map(handle).collect();
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        Ok(msg) => handle(&msg),
        Err(e) => Some(error_reply(
            Value::Null,
            -32700,
            &format!("parse error: {e}"),
        )),
    }
}

fn handle(msg: &Value) -> Option<Value> {
    if !msg.is_object() {
        return Some(error_reply(
            Value::Null,
            -32600,
            "invalid request: not an object",
        ));
    }
    let id = msg.get("id").cloned();
    // JSON-RPC ids are strings or numbers; a null id can't be answered usefully.
    if id
        .as_ref()
        .is_some_and(|i| !(i.is_string() || i.is_number()))
    {
        return Some(error_reply(
            Value::Null,
            -32600,
            "invalid request: id must be a string or number",
        ));
    }
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        // A response (we never send requests, so ignore it), or junk with an id.
        if msg.get("result").is_some() || msg.get("error").is_some() {
            return None;
        }
        return id.map(|id| error_reply(id, -32600, "invalid request: no method"));
    };
    // Notifications (initialized, cancelled...) need no answer.
    let id = id?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call(&params),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => error_reply(id, code, &message),
    })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = PROTOCOL_VERSIONS
        .iter()
        .find(|v| Some(**v) == asked)
        .unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": lysosome_core::TOOL_NAME,
            "title": "lysosome",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

// ------------------------------------------------------------------ tools

fn roots_schema() -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" },
        "description": "Folders to scan (absolute paths or ~/...). Default: the whole home folder, which is slow."
    })
}

fn project_props() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("roots".into(), roots_schema());
    m.insert(
        "days".into(),
        json!({ "type": "integer", "minimum": 0, "description": "Idle days before a project or worktree counts as dormant (default 30)." }),
    );
    m.insert(
        "depth".into(),
        json!({ "type": "integer", "minimum": 0, "description": "Maximum folder depth to search for repositories." }),
    );
    m.insert(
        "no_gh".into(),
        json!({ "type": "boolean", "description": "Don't ask GitHub (via gh) whether REVIEW worktrees were squash-merged." }),
    );
    m.insert(
        "offline".into(),
        json!({ "type": "boolean", "description": "Never use the network." }),
    );
    m
}

fn object(props: Map<String, Value>) -> Value {
    json!({ "type": "object", "properties": props, "additionalProperties": false })
}

fn read_only(title: &str) -> Value {
    json!({ "title": title, "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false })
}

fn tools() -> Value {
    let mut scan = project_props();
    scan.insert(
        "all".into(),
        json!({ "type": "boolean", "description": "Also list ACTIVE items." }),
    );
    let wt = scan.clone();
    scan.insert(
        "no_system".into(),
        json!({ "type": "boolean", "description": "Skip machine-wide caches, AI models, Docker, Xcode, Android and editor data." }),
    );

    let mut caches = Map::new();
    caches.insert(
        "categories".into(),
        json!({
            "type": "array",
            "items": { "enum": ["ai_models", "containers", "xcode", "package_caches", "android", "editors"] },
            "description": "Only these categories. Default: all."
        }),
    );

    let mut clean = project_props();
    for (k, d) in [
        ("worktrees", "Clean dormant git worktrees."),
        (
            "artifacts",
            "Clean git-ignored build/dependency folders of idle projects.",
        ),
        (
            "caches",
            "Clean package caches, AI models, Docker, Xcode, Android and editor data.",
        ),
        (
            "strip_artifacts",
            "Keep dormant worktrees; remove only the git-ignored artifact folders inside them.",
        ),
    ] {
        clean.insert(
            k.into(),
            json!({ "type": "boolean", "description": format!("{d} With none of worktrees/artifacts/caches, all three.") }),
        );
    }
    clean.insert(
        "tier".into(),
        json!({ "enum": ["regenerable", "regenerable_expensive"], "description": "Highest tier to clean (default regenerable: cheap to rebuild). regenerable_expensive adds dependencies, AI models and images. review is never cleaned." }),
    );
    clean.insert(
        "older_than".into(),
        json!({ "type": "string", "description": "Only items idle at least this long: 90d, 12w, 6mo, 1y." }),
    );
    clean.insert(
        "only".into(),
        json!({ "type": "array", "items": { "type": "string" }, "description": "Item ids (<kind>:<path>) or, for a dry run, globs. Required with confirm, and then only exact ids." }),
    );
    clean.insert(
        "confirm".into(),
        json!({ "type": "boolean", "description": "Actually clean the items in only. Without it nothing is changed and the plan is returned. Ask the user first, and repeat the dry run's other arguments." }),
    );

    json!([
        {
            "name": "scan",
            "description": "Find reclaimable disk: idle project artifacts, dormant git worktrees, package caches, AI models, Docker and Xcode data. Read-only. Returns the versioned lysosome report (schema/lysosome.schema.json); findings carry id, size_bytes, tier and status.",
            "inputSchema": object(scan),
            "annotations": read_only("Scan for reclaimable disk"),
        },
        {
            "name": "worktrees",
            "description": "Find dormant git worktrees and the disk they hold, with REMOVABLE / REVIEW / ORPHAN / ACTIVE status. Read-only. Same report format as scan.",
            "inputSchema": object(wt),
            "annotations": read_only("Find dormant git worktrees"),
        },
        {
            "name": "caches",
            "description": "Find machine-wide package caches, AI models, Docker, Xcode, Android and editor data. Read-only. Same report format as scan.",
            "inputSchema": object(caches),
            "annotations": read_only("Find caches and models"),
        },
        {
            "name": "clean",
            "description": "Clean up safely. Without confirm it is a dry run that returns the plan (schema/clean.schema.json) and changes nothing. With confirm: true and exact item ids in only, it cleans just those items (pass the same roots, days, depth, tier and scope as the dry run, because the items are found again): each is re-checked first, files go to the Trash, dirty, unpushed or REVIEW items are never touched. Show the plan to the user and get their OK before confirming.",
            "inputSchema": object(clean),
            "annotations": {
                "title": "Clean up reclaimable disk",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": false
            },
        }
    ])
}

/// A tool result (`isError` for failures the model should see).
fn tool_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn tool_error(code: &'static str, message: String, hint: &str) -> Value {
    let err = errors::CliError {
        code,
        message,
        hint: Some(hint.to_string()),
    };
    tool_result(errors::json(&err), true)
}

fn call(params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "tools/call needs a name".to_string()))?;
    let empty = Map::new();
    let args = match params.get("arguments") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => return Err((-32602, "arguments must be an object".into())),
    };
    let argv = match name {
        "scan" | "worktrees" | "caches" | "clean" => match build_argv(name, args) {
            Ok(v) => v,
            Err(message) => {
                return Ok(tool_error(
                    "usage",
                    message,
                    "see the tool's inputSchema in tools/list",
                ))
            }
        },
        _ => return Err((-32602, format!("unknown tool: {name}"))),
    };
    Ok(run_argv(argv))
}

/// Parse like the command line, then run the command's own code.
fn run_argv(argv: Vec<OsString>) -> Value {
    let cli = match Cli::try_parse_from(argv) {
        Ok(c) => c,
        Err(e) => {
            let text = e.render().to_string();
            let first = text.lines().next().unwrap_or("invalid arguments");
            return tool_error(
                "usage",
                first.trim_start_matches("error: ").to_string(),
                "see the tool's inputSchema in tools/list",
            );
        }
    };
    let ran = catch_unwind(AssertUnwindSafe(|| match &cli.command {
        Some(Command::Scan(a)) => scan::run(a, &cli.global).map(|t| (t, false)),
        Some(Command::Worktrees(a)) => worktrees::run(a, &cli.global).map(|t| (t, false)),
        Some(Command::Caches(a)) => caches::run(a, &cli.global).map(|t| (t, false)),
        Some(Command::Clean(a)) => clean::run(a, &cli.global).map(|o| (o.text, o.failed)),
        _ => unreachable!("build_argv only builds the four tools"),
    }));
    match ran {
        Ok(Ok((text, failed))) => tool_result(text, failed),
        Ok(Err(e)) => tool_result(errors::json(&errors::describe(&e)), true),
        Err(_) => tool_error(
            "internal",
            "lysosome hit an internal error running this tool".into(),
            "run the same command in a terminal with --json to see it, and please report it",
        ),
    }
}

fn build_argv(tool: &str, args: &Map<String, Value>) -> Result<Vec<OsString>, String> {
    // The advertised inputSchema is the allow-list.
    let tools = tools();
    let props = tools
        .as_array()
        .and_then(|ts| ts.iter().find(|t| t["name"] == tool))
        .and_then(|t| t["inputSchema"]["properties"].as_object())
        .ok_or(format!("unknown tool `{tool}`"))?;
    if let Some(k) = args.keys().find(|k| !props.contains_key(k.as_str())) {
        return Err(format!("{tool} has no argument `{k}`"));
    }
    let mut v: Vec<String> = vec![
        lysosome_core::TOOL_NAME.into(),
        "--progress".into(),
        "off".into(),
        "--no-color".into(),
        tool.into(),
        "--json".into(),
    ];
    for key in ["days", "depth"] {
        if let Some(x) = args.get(key) {
            let n = x
                .as_u64()
                .ok_or(format!("`{key}` must be a non-negative integer"))?;
            v.push(format!("--{key}={n}"));
        }
    }
    for key in [
        "no_gh",
        "offline",
        "all",
        "no_system",
        "worktrees",
        "artifacts",
        "caches",
        "strip_artifacts",
    ] {
        if flag(args, key)? {
            v.push(format!("--{}", key.replace('_', "-")));
        }
    }
    if let Some(t) = args.get("tier") {
        let t = t.as_str().ok_or("`tier` must be a string")?;
        v.push(format!("--tier={t}"));
    }
    if let Some(a) = args.get("older_than") {
        let a = a.as_str().ok_or("`older_than` must be a string")?;
        v.push(format!("--older-than={a}"));
    }
    for c in strings(args, "categories")? {
        v.push(format!("--category={c}"));
    }
    let only = strings(args, "only")?;
    if flag(args, "confirm")? {
        if only.is_empty() {
            return Err("confirm needs the exact ids of the items to clean in `only`; run clean without confirm first to get the plan".into());
        }
        if let Some(bad) = only.iter().find(|o| !is_exact_id(o)) {
            return Err(format!(
                "`{bad}` is not an item id; with confirm, `only` takes exact ids like \"node_modules:/path/to/project/node_modules\", no globs"
            ));
        }
        // --only also matches kinds, names and globs; with --only-exact the
        // plan keeps just the items whose id is listed.
        v.push("--yes".into());
        v.push("--only-exact".into());
    }
    for o in &only {
        v.push(format!("--only={o}"));
    }
    let mut v: Vec<OsString> = v.into_iter().map(OsString::from).collect();
    let roots = strings(args, "roots")?;
    if !roots.is_empty() {
        // Clients pass `~/code`; no shell expands it here.
        v.push("--".into());
        v.extend(
            roots
                .iter()
                .map(|r| crate::expand_tilde(Path::new(r)).into_os_string()),
        );
    }
    Ok(v)
}

fn flag(args: &Map<String, Value>, key: &str) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("`{key}` must be true or false")),
    }
}

fn strings(args: &Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .ok_or(format!("`{key}` must be a list of non-empty strings"))
            })
            .collect(),
        Some(_) => Err(format!("`{key}` must be a list of strings")),
    }
}

/// Shaped like an item id, `<kind>:<path>`, to catch a bare kind or name
/// passed by mistake. Matching itself is exact (`--only-exact`), so `*` and
/// `?` in a real path are fine.
fn is_exact_id(s: &str) -> bool {
    s.split_once(':')
        .is_some_and(|(kind, path)| !kind.is_empty() && !path.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(tool: &str, args: Value) -> Result<Vec<String>, String> {
        build_argv(tool, args.as_object().unwrap())
            .map(|v| v.into_iter().map(|s| s.into_string().unwrap()).collect())
    }

    #[test]
    fn invalid_messages_get_an_error_and_responses_none() {
        let code = |line: &str| handle_line(line).map(|r| r["error"]["code"].clone());
        assert_eq!(code("[]"), Some(json!(-32600)));
        assert_eq!(code("{not json"), Some(json!(-32700)));
        assert_eq!(
            code(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#),
            Some(json!(-32600))
        );
        let batch = handle_line("[1]").unwrap();
        assert_eq!(batch[0]["error"]["code"], -32600);
        assert_eq!(handle_line(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#), None);
        assert_eq!(
            handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            None
        );
    }

    #[test]
    fn exact_ids_only() {
        assert!(is_exact_id("node_modules:/a/b/node_modules"));
        assert!(!is_exact_id("node_modules"));
        assert!(!is_exact_id("npm*"));
        assert!(is_exact_id("node_modules:/code/what?/node_modules"));
        assert!(is_exact_id("docker-image:docker://image/abc"));
        assert!(is_exact_id("node_modules:/code/foo[1]/node_modules"));
        assert!(!is_exact_id(":/a"));
    }

    #[test]
    fn clean_is_a_dry_run_unless_confirmed_with_ids() {
        let a = argv("clean", json!({ "only": ["npm*"] })).unwrap();
        assert!(!a.contains(&"--yes".to_string()));
        assert!(argv("clean", json!({ "confirm": true })).is_err());
        assert!(argv("clean", json!({ "confirm": true, "only": ["npm*"] })).is_err());
        let a = argv(
            "clean",
            json!({ "confirm": true, "only": ["target:/x/target"] }),
        )
        .unwrap();
        assert!(a.contains(&"--yes".to_string()));
        assert!(a.contains(&"--only=target:/x/target".to_string()));
        assert!(a.contains(&"--only-exact".to_string()));
    }

    #[test]
    fn unknown_and_dangerous_arguments_are_refused() {
        assert!(argv("clean", json!({ "permanent": true })).is_err());
        assert!(argv("clean", json!({ "interactive": true })).is_err());
        assert!(argv("scan", json!({ "confirm": true })).is_err());
        assert!(argv("scan", json!({ "days": -1 })).is_err());
    }

    #[test]
    fn roots_follow_a_double_dash_and_expand_tilde() {
        let a = argv("scan", json!({ "roots": ["--yes", "~/code"] })).unwrap();
        let dd = a.iter().position(|s| s == "--").unwrap();
        assert_eq!(a[dd + 1], "--yes");
        assert!(!a[dd + 2].starts_with('~'));
    }
}
