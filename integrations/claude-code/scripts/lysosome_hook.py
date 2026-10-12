#!/usr/bin/env python3
"""lysosome hooks for Claude Code: agents that clean up after themselves.

  session-end    (SessionEnd hook)   If the session ran inside a linked git
                 worktree, hand it to a detached worker and return at once
                 (SessionEnd hooks get ~1.5 s and must never slow an exit).
  worker         Asks `lysosome worktrees` (read-only) whether the worktree's
                 task looks finished (clean and merged), records it, and, only
                 if the user opted in with `auto_tidy`, runs `lysosome clean`.
  session-start  (SessionStart hook, startup only) Reports, once, what earlier
                 sessions left behind that is now safe to reclaim.
  report         Prints everything recorded (for /lysosome:status).

Safety: this script never deletes anything itself. Every cleanup is
`lysosome clean`, which re-checks the worktree right before acting and never
touches a dirty, locked, or unpushed one. With auto_tidy=off (the default)
nothing on disk changes. Any failure is silent: a hook must never break a
session.
"""

import json
import os
import shutil
import subprocess
import sys
import time

ACTOR = "claude-code"
# Show build output worth mentioning in REVIEW worktrees above this size.
NOTABLE_BYTES = 100 * 1000 * 1000
KEEP_DAYS = 30
# Reasons a session really ended (not /clear or /resume, which keep the
# session going in the same folder).
ENDED = {"prompt_input_exit", "logout", "other"}


def data_dir(explicit=None):
    d = explicit or os.environ.get("CLAUDE_PLUGIN_DATA")
    if not d:
        base = os.environ.get("XDG_STATE_HOME") or os.path.join(os.path.expanduser("~"), ".local", "state")
        d = os.path.join(base, "lysosome", "claude-code")
    os.makedirs(d, exist_ok=True)
    return d


def find_cli():
    explicit = os.environ.get("LYSOSOME_BIN")
    if explicit:
        return explicit if os.access(explicit, os.X_OK) else None
    found = shutil.which("lysosome")
    if found:
        return found
    home = os.path.expanduser("~")
    for c in ("/opt/homebrew/bin", "/usr/local/bin", home + "/.local/bin", home + "/.cargo/bin"):
        p = os.path.join(c, "lysosome")
        if os.access(p, os.X_OK):
            return p
    return None


def clean_env(**extra):
    env = {k: v for k, v in os.environ.items() if k not in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE")}
    env["NO_COLOR"] = "1"
    env.update(extra)
    return env


def linked_worktree(cwd):
    """The top folder of the linked worktree containing cwd, else None."""
    try:
        out = subprocess.run(
            ["git", "--no-optional-locks", "-C", cwd, "rev-parse", "--path-format=absolute",
             "--show-toplevel", "--git-dir", "--git-common-dir"],
            capture_output=True, text=True, timeout=1, env=clean_env(), stdin=subprocess.DEVNULL,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    lines = out.stdout.splitlines()
    if out.returncode != 0 or len(lines) != 3:
        return None
    top, git_dir, common = lines
    if os.path.realpath(git_dir) == os.path.realpath(common):
        return None  # the main checkout, not a linked worktree
    if os.sep + "modules" + os.sep in git_dir:
        return None  # a submodule
    return top


def session_pid():
    """The Claude Code process: the hook runs as claude -> sh -> python3."""
    try:
        out = subprocess.run(["ps", "-o", "ppid=", "-p", str(os.getppid())],
                             capture_output=True, text=True, timeout=1)
        pid = int(out.stdout.strip())
        return pid if pid > 1 else None
    except (OSError, subprocess.SubprocessError, ValueError):
        return None


def wait_exit(pid, limit=30.0):
    """Wait (up to `limit` seconds) for the session's process to exit."""
    if not pid:
        return
    end = time.time() + limit
    while time.time() < end:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        except PermissionError:
            pass
        time.sleep(0.25)


def in_use(top):
    """True if any process (a shell, another session, a dev server) still has
    its working directory inside `top`, or if that can't be determined."""
    real = os.path.realpath(top)
    inside = lambda p: p == real or p.startswith(real + os.sep)
    me = os.getpid()
    if os.path.isdir("/proc/self"):
        for pid in os.listdir("/proc"):
            if not pid.isdigit() or int(pid) == me:
                continue
            try:
                if inside(os.path.realpath(os.readlink("/proc/%s/cwd" % pid))):
                    return True
            except OSError:
                continue
        return False
    try:
        out = subprocess.run(["lsof", "-a", "-d", "cwd", "-Fpn"], capture_output=True,
                             text=True, timeout=10)
    except (OSError, subprocess.SubprocessError):
        return True
    pid = None
    for line in out.stdout.splitlines():
        if line.startswith("p"):
            pid = int(line[1:] or 0)
        elif line.startswith("n") and pid != me and inside(os.path.realpath(line[1:])):
            return True
    return False


GLOB = set("*?[")


def run_cli(args, timeout, actor=False):
    cli = find_cli()
    if not cli:
        return None
    env = clean_env(LYSOSOME_ACTOR=ACTOR) if actor else clean_env()
    try:
        out = subprocess.run([cli] + args, capture_output=True, text=True, timeout=timeout,
                             env=env, stdin=subprocess.DEVNULL, cwd="/")
        return json.loads(out.stdout)
    except (OSError, subprocess.SubprocessError, ValueError):
        return None


def read_input():
    try:
        return json.load(sys.stdin)
    except (ValueError, OSError):
        return {}


def entries(d):
    path = os.path.join(d, "worktrees.jsonl")
    out = []
    try:
        with open(path) as f:
            for line in f:
                try:
                    out.append(json.loads(line))
                except ValueError:
                    pass
    except OSError:
        pass
    return out


def save(d, items):
    path = os.path.join(d, "worktrees.jsonl")
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        for e in items:
            f.write(json.dumps(e) + "\n")
    os.replace(tmp, path)


def append(d, entry):
    with open(os.path.join(d, "worktrees.jsonl"), "a") as f:
        f.write(json.dumps(entry) + "\n")


def human(n):
    n = float(n or 0)
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1000 or unit == "TB":
            return ("%.0f %s" if unit == "B" or n >= 100 else "%.1f %s") % (n, unit)
        n /= 1000


# ------------------------------------------------------------------ hooks


def session_end():
    data = read_input()
    cwd = data.get("cwd") or os.getcwd()
    top = linked_worktree(cwd)
    if not top or not find_cli():
        return
    if os.environ.get("LYSOSOME_HOOK_SYNC") == "1":  # tests
        worker(top, data.get("session_id", ""), data.get("reason", "other"), None)
        return
    args = [sys.executable, os.path.abspath(__file__), "worker", top,
            data.get("session_id", ""), data.get("reason", "other"), str(session_pid() or 0)]
    # Detach: the session may exit before the CLI finishes, and that's fine.
    subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                     stderr=subprocess.DEVNULL, start_new_session=True, cwd="/", env=clean_env())


def worker(top, session_id, reason, pid):
    d = data_dir()
    # --days 0: is the task finished (clean, merged or pushed), not "is it old".
    rep = run_cli(["worktrees", top, "--json", "--days", "0", "--depth", "1", "--progress", "off"], 120)
    if not rep:
        return
    real = os.path.realpath(top)
    f = next((x for x in rep.get("findings", []) if os.path.realpath(x.get("path", "")) == real), None)
    if not f:
        return
    det = f.get("details") or {}
    entry = {
        "ts": int(time.time()),
        "session_id": session_id,
        "path": f["path"],
        "id": f["id"],
        "branch": det.get("branch"),
        "main_repo": det.get("main_repo"),
        "status": f["status"],
        "reason": f.get("reason", ""),
        "size_bytes": f.get("size_bytes", 0),
        "artifact_bytes": det.get("artifact_bytes", 0),
        "merged": bool(det.get("merged_into") or det.get("merged_pr")),
        "action": None,
        "reported": False,
    }
    mode = os.environ.get("CLAUDE_PLUGIN_OPTION_AUTO_TIDY", "off")
    # Only finished tasks (merged), only when the session really ended, and
    # never REVIEW: `lysosome clean` re-checks all of it before acting.
    act = mode in ("strip-merged", "remove-merged") and reason in ENDED and entry["merged"]
    if act:
        # Let the session finish exiting, then never pull a folder out from
        # under a shell, another session or a running dev server.
        wait_exit(pid)
        if in_use(top) or GLOB & set(f["path"]):
            act = False
            entry["skipped"] = "in use"
    if act:
        args = None
        if mode == "remove-merged" and f["status"] == "REMOVABLE":
            args = ["clean", top, "--worktrees", "--only", f["id"], "--days", "0", "--depth", "1"]
            kind = "removed"
        elif entry["artifact_bytes"] > 0:
            args = ["clean", top, "--strip-artifacts", "--tier", "regenerable_expensive",
                    "--only", f["path"], "--days", "0", "--depth", "1"]
            kind = "stripped"
        if args:
            res = run_cli(args + ["--yes", "--json", "--progress", "off"], 300, actor=True)
            if res:
                s = res.get("summary") or {}
                entry["action"] = {"kind": kind, "done": s.get("done", 0),
                                   "freed_bytes": s.get("freed_bytes", 0)}
    append(d, entry)


def summarize(items):
    """One line per worktree worth mentioning, newest first, deduped."""
    seen, lines = set(), []
    for e in sorted(items, key=lambda e: -e.get("ts", 0)):
        p = e.get("path")
        if not p or p in seen:
            continue
        seen.add(p)
        a = e.get("action")
        name = e.get("branch") or os.path.basename(p)
        if a and a.get("done"):
            what = "removed the worktree" if a["kind"] == "removed" else "freed its build output"
            lines.append((e, "%s: %s (%s)" % (name, what, human(a.get("freed_bytes")))))
        elif not os.path.exists(p):
            continue
        elif e.get("status") == "REMOVABLE":
            lines.append((e, "%s is %s: %s can go (%s)" % (name, e.get("reason") or "clean", human(e.get("size_bytes")), p)))
        elif e.get("artifact_bytes", 0) >= NOTABLE_BYTES:
            lines.append((e, "%s holds %s of build output; the worktree needs a look first (%s)"
                          % (name, human(e.get("artifact_bytes")), e.get("reason") or "")))
    return lines


def session_start():
    data = read_input()
    if data.get("source", "startup") != "startup":
        return
    d = data_dir()
    items = entries(d)
    cutoff = time.time() - KEEP_DAYS * 86400
    items = [e for e in items if e.get("ts", 0) >= cutoff]
    fresh = summarize([e for e in items if not e.get("reported")])
    for e in items:
        e["reported"] = True
    save(d, items)
    if not fresh:
        return
    tidied = [line for e, line in fresh if (e.get("action") or {}).get("done")]
    pending = [line for e, line in fresh if not (e.get("action") or {}).get("done")]
    parts = []
    if tidied:
        parts.append("tidied up after earlier sessions: " + "; ".join(tidied[:3]))
    if pending:
        more = " (+%d more)" % (len(pending) - 3) if len(pending) > 3 else ""
        parts.append("left behind by earlier sessions: " + "; ".join(pending[:3]) + more
                     + ". Run /lysosome:tidy to review and reclaim")
    msg = "lysosome " + ". ".join(parts) + "."
    print(json.dumps({
        "systemMessage": msg,
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": msg + " Nothing was removed without the user's opt-in; offer /lysosome:tidy if relevant, don't clean on your own.",
        },
    }))


def report(explicit_dir=None):
    d = data_dir(explicit_dir)
    items = entries(d)
    print(json.dumps({"data_dir": d, "recorded": items[-50:], "summary": [line for _, line in summarize(items)]}, indent=2))


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    try:
        if cmd == "session-end":
            session_end()
        elif cmd == "worker" and len(sys.argv) >= 5:
            pid = int(sys.argv[5]) if len(sys.argv) > 5 and sys.argv[5].isdigit() else None
            worker(sys.argv[2], sys.argv[3], sys.argv[4], pid or None)
        elif cmd == "session-start":
            session_start()
        elif cmd == "report":
            report(sys.argv[3] if len(sys.argv) > 3 and sys.argv[2] == "--data" else None)
    except Exception:  # never break a session
        if os.environ.get("LYSOSOME_HOOK_DEBUG") == "1":
            raise


if __name__ == "__main__":
    main()
