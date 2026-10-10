#!/usr/bin/env python3
"""
wtscan — find dormant git worktrees and the disk they're holding.

READ-ONLY: this script never deletes, moves, or writes anything.
It only prints suggested commands for you to review and run yourself.

Usage:
  python3 wtscan.py                 # scan your home folder
  python3 wtscan.py ~/code ~/work   # scan specific folders (faster)
  python3 wtscan.py --days 14       # call anything idle 14+ days "dormant"
  python3 wtscan.py --all           # also list active worktrees
  python3 wtscan.py --json          # machine-readable output

Requires only python3 and git (both ship with Xcode Command Line Tools).
"""
import argparse
import json
import os
import subprocess
import sys
import time

# Folders never worth descending into when hunting for repos.
SKIP_DIRS = {
    ".git", "node_modules", ".venv", "venv", "target", ".next", ".nuxt",
    ".svelte-kit", ".turbo", ".gradle", "Pods", "DerivedData", "__pycache__",
    "Library", ".Trash", "Applications", "Movies", "Music", "Pictures",
    ".cache", ".npm", ".pnpm-store", ".yarn", ".bun", ".cargo", ".rustup",
    ".ollama", ".lmstudio", ".docker", ".orbstack", ".gem", ".m2",
    ".android", ".local", ".vscode", ".cursor", ".Spotlight-V100",
}

# Regenerable build/dependency folders (counted only if git ignores them).
ARTIFACT_DIRS = {
    "node_modules", ".venv", "venv", "target", ".next", ".nuxt",
    ".svelte-kit", ".turbo", ".parcel-cache", "dist", "build", ".gradle",
    "Pods", "DerivedData", ".build", ".dart_tool", ".expo", "coverage",
}

DAY = 86400


# ---------- helpers ----------

def git(path, *args, timeout=120):
    try:
        r = subprocess.run(
            ["git", "--no-optional-locks", "-C", path, *args],
            capture_output=True, text=True, timeout=timeout,
        )
        return r.returncode, r.stdout.strip()
    except (subprocess.TimeoutExpired, OSError):
        return 1, ""


def du_kb(paths):
    """Disk usage in KB for each path, using du (fast, counts real blocks)."""
    if not paths:
        return {}
    out = {}
    # chunk to stay well under argv limits
    for i in range(0, len(paths), 200):
        chunk = paths[i:i + 200]
        try:
            r = subprocess.run(["du", "-sk", *chunk], capture_output=True, text=True)
        except OSError:
            continue
        for line in r.stdout.splitlines():
            kb, _, p = line.partition("\t")
            if kb.isdigit():
                out[p] = int(kb)
    return out


def human(kb):
    b = kb * 1024
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if b < 1024 or unit == "TB":
            return f"{b:.0f}{unit}" if unit in ("B", "KB") else f"{b:.1f}{unit}"
        b /= 1024


def short(path):
    home = os.path.expanduser("~")
    return "~" + path[len(home):] if path.startswith(home) else path


def mtime(path):
    try:
        return os.stat(path).st_mtime
    except OSError:
        return 0


def read_gitdir_pointer(dotgit_file):
    """A worktree's .git is a file: 'gitdir: /repo/.git/worktrees/<name>'."""
    try:
        with open(dotgit_file, encoding="utf-8", errors="replace") as f:
            line = f.readline().strip()
    except OSError:
        return None
    if not line.startswith("gitdir:"):
        return None
    p = line[len("gitdir:"):].strip()
    if not os.path.isabs(p):
        p = os.path.normpath(os.path.join(os.path.dirname(dotgit_file), p))
    return p


# ---------- discovery ----------

def discover(roots, max_depth, progress):
    worktrees = {}      # path -> gitdir (may not exist => orphan)
    mains = set()
    for root in roots:
        root = os.path.abspath(os.path.expanduser(root))
        if not os.path.isdir(root):
            print(f"  skipping {root} (not a folder)", file=sys.stderr)
            continue
        base = root.rstrip(os.sep).count(os.sep)
        n = 0
        for dirpath, dirnames, filenames in os.walk(root, onerror=lambda e: None):
            n += 1
            if progress and n % 5000 == 0:
                print(f"  ...{n:,} folders checked", file=sys.stderr)
            if ".git" in filenames:
                gd = read_gitdir_pointer(os.path.join(dirpath, ".git"))
                # submodules also use .git files; only keep real worktrees
                if gd and f"{os.sep}worktrees{os.sep}" in gd and f"{os.sep}modules{os.sep}" not in gd:
                    worktrees[dirpath] = gd
            elif ".git" in dirnames:
                mains.add(dirpath)
            depth = dirpath.count(os.sep) - base
            if depth >= max_depth:
                dirnames[:] = []
            else:
                dirnames[:] = [d for d in dirnames
                               if d not in SKIP_DIRS
                               and not os.path.islink(os.path.join(dirpath, d))]

    # Main repos also know their worktrees, even ones outside the scan roots.
    stale_meta = []
    for m in mains:
        wt_meta = os.path.join(m, ".git", "worktrees")
        if not os.path.isdir(wt_meta):
            continue
        for name in os.listdir(wt_meta):
            meta = os.path.join(wt_meta, name)
            try:
                with open(os.path.join(meta, "gitdir"), encoding="utf-8") as f:
                    wt_path = os.path.dirname(f.read().strip())
            except OSError:
                continue
            if os.path.isdir(wt_path):
                worktrees.setdefault(wt_path, meta)
            else:
                stale_meta.append({"repo": m, "name": name, "missing_path": wt_path})
    return worktrees, stale_meta


def find_artifacts(wt, max_depth=6):
    found = []
    base = wt.rstrip(os.sep).count(os.sep)
    for dirpath, dirnames, _ in os.walk(wt, onerror=lambda e: None):
        keep = []
        for d in dirnames:
            full = os.path.join(dirpath, d)
            if d == ".git" or os.path.islink(full):
                continue
            if d in ARTIFACT_DIRS:
                found.append(full)
            else:
                keep.append(d)
        dirnames[:] = keep if dirpath.count(os.sep) - base < max_depth else []
    if not found:
        return []
    # only count folders git actually ignores (so we never flag source as junk)
    rel = [os.path.relpath(p, wt) for p in found]
    try:
        r = subprocess.run(["git", "-C", wt, "check-ignore", "--stdin"],
                           input="\n".join(rel), capture_output=True, text=True)
        ignored = set(r.stdout.split("\n"))
    except OSError:
        return []
    return [os.path.join(wt, p) for p in rel if p in ignored]


# ---------- analysis ----------

def default_branch(wt):
    rc, ref = git(wt, "symbolic-ref", "--quiet", "refs/remotes/origin/HEAD")
    if rc == 0 and ref:
        return ref.replace("refs/remotes/", "")
    for cand in ("main", "master", "origin/main", "origin/master"):
        if git(wt, "rev-parse", "--verify", "--quiet", cand)[0] == 0:
            return cand
    return None


def analyze(wt, gitdir, days, now):
    rec = {"path": wt, "gitdir": gitdir}
    if not os.path.isdir(gitdir):
        rec.update(status="ORPHAN", reason="parent repo moved or deleted")
        return rec

    # last activity: read timestamps BEFORE running git (git can touch the index)
    activity = max(mtime(os.path.join(gitdir, "index")),
                   mtime(os.path.join(gitdir, "logs", "HEAD")),
                   mtime(os.path.join(gitdir, "HEAD")))
    rc, ct = git(wt, "log", "-1", "--format=%ct")
    if rc == 0 and ct.isdigit():
        activity = max(activity, int(ct))
    rec["idle_days"] = int((now - activity) // DAY) if activity else None

    rc, branch = git(wt, "symbolic-ref", "--quiet", "--short", "HEAD")
    rec["branch"] = branch if rc == 0 else "(detached)"

    rc, st = git(wt, "status", "--porcelain")
    lines = [l for l in st.splitlines() if l]
    rec["untracked"] = sum(1 for l in lines if l.startswith("??"))
    rec["changed"] = len(lines) - rec["untracked"]

    base = default_branch(wt)
    rec["merged_into"] = None
    if base and git(wt, "merge-base", "--is-ancestor", "HEAD", base)[0] == 0:
        rec["merged_into"] = base

    rc, remotes = git(wt, "remote")
    if remotes:
        rc, n = git(wt, "rev-list", "--count", "HEAD", "--not", "--remotes")
        rec["unpushed"] = int(n) if n.isdigit() else None
    else:
        rec["unpushed"] = None  # no remote at all

    rc, cd = 1, ""
    try:
        with open(os.path.join(gitdir, "commondir"), encoding="utf-8") as f:
            cd = f.read().strip()
        common = os.path.normpath(os.path.join(gitdir, cd))
        rec["main_repo"] = os.path.dirname(common) if os.path.basename(common) == ".git" else common
    except OSError:
        rec["main_repo"] = None
    rec["locked"] = os.path.exists(os.path.join(gitdir, "locked"))

    # classify
    dirty = rec["changed"] or rec["untracked"]
    if rec["merged_into"]:
        commits_safe = True
    elif rec["unpushed"] is None:
        commits_safe = False      # no remote and not merged: commits live only here/on the branch
    else:
        commits_safe = rec["unpushed"] == 0
    dormant = rec["idle_days"] is not None and rec["idle_days"] >= days

    if not dormant:
        rec["status"], rec["reason"] = "ACTIVE", ""
    elif rec["locked"]:
        rec["status"], rec["reason"] = "REVIEW", "locked worktree"
    elif dirty:
        bits = []
        if rec["changed"]:
            bits.append(f"{rec['changed']} uncommitted change(s)")
        if rec["untracked"]:
            bits.append(f"{rec['untracked']} untracked file(s)")
        rec["status"], rec["reason"] = "REVIEW", ", ".join(bits)
    elif not commits_safe:
        rec["status"] = "REVIEW"
        rec["reason"] = (f"{rec['unpushed']} unpushed commit(s), not merged"
                         if rec["unpushed"] else "no remote and not merged")
    else:
        rec["status"] = "REMOVABLE"
        rec["reason"] = f"clean, merged into {rec['merged_into']}" if rec["merged_into"] else "clean, fully pushed"
    return rec


# ---------- output ----------

def color(s, code, on):
    return f"\033[{code}m{s}\033[0m" if on else s


def main():
    ap = argparse.ArgumentParser(description="Read-only scan for dormant git worktrees.")
    ap.add_argument("roots", nargs="*", default=["~"], help="folders to scan (default: home)")
    ap.add_argument("--days", type=int, default=30, help="idle days before a worktree counts as dormant (default 30)")
    ap.add_argument("--depth", type=int, default=8, help="max folder depth to search (default 8)")
    ap.add_argument("--all", action="store_true", help="also list active worktrees")
    ap.add_argument("--top", type=int, default=30, help="rows to show (default 30)")
    ap.add_argument("--json", action="store_true", help="print JSON instead of a table")
    a = ap.parse_args()

    tty = sys.stdout.isatty() and not a.json
    now = time.time()
    t0 = time.time()
    if not a.json:
        print(f"Scanning {', '.join(a.roots)} for git worktrees (read-only)...", file=sys.stderr)

    worktrees, stale = discover(a.roots, a.depth, progress=not a.json)
    recs = [analyze(wt, gd, a.days, now) for wt, gd in sorted(worktrees.items())]

    # sizes
    sizes = du_kb([r["path"] for r in recs])
    for r in recs:
        r["size_kb"] = sizes.get(r["path"], 0)
        arts = find_artifacts(r["path"]) if r["status"] != "ACTIVE" or a.all else []
        asz = du_kb(arts)
        r["artifacts"] = [{"path": p, "size_kb": asz.get(p, 0)} for p in arts]
        r["artifact_kb"] = sum(asz.values())

    dormant = [r for r in recs if r["status"] != "ACTIVE"]
    removable = [r for r in recs if r["status"] == "REMOVABLE"]
    total_dormant = sum(r["size_kb"] for r in dormant)
    total_removable = sum(r["size_kb"] for r in removable)
    total_artifacts = sum(r["artifact_kb"] for r in dormant)

    if a.json:
        print(json.dumps({
            "scanned_roots": a.roots, "dormant_after_days": a.days,
            "summary": {
                "worktrees_found": len(recs), "dormant": len(dormant),
                "dormant_kb": total_dormant, "removable": len(removable),
                "removable_kb": total_removable, "dormant_artifact_kb": total_artifacts,
            },
            "worktrees": recs, "stale_metadata": stale,
        }, indent=2))
        return

    print()
    if not recs:
        print("No git worktrees found. Try pointing it at your code folder: python3 wtscan.py ~/code")
        return

    head = (f"{len(dormant)} dormant worktree(s) holding {human(total_dormant)}"
            f"  (of {len(recs)} found, idle {a.days}+ days)")
    print(color(head, "1", tty))
    print(f"  {color(human(total_removable), '32;1', tty)} safe to remove now "
          f"({len(removable)} clean & merged/pushed)")
    print(f"  {color(human(total_artifacts), '36;1', tty)} is just build/dependency artifacts "
          f"(strippable even from worktrees you keep)")
    print()

    rows = recs if a.all else dormant
    rows = sorted(rows, key=lambda r: r["size_kb"], reverse=True)[: a.top]
    tag = {"REMOVABLE": "32", "REVIEW": "33", "ORPHAN": "35", "ACTIVE": "90"}
    print(f"{'STATUS':<10} {'SIZE':>8} {'ARTIFACTS':>9} {'IDLE':>6}  {'BRANCH':<28} PATH")
    for r in rows:
        idle = f"{r['idle_days']}d" if r.get("idle_days") is not None else "-"
        br = (r.get("branch") or "-")[:28]
        st = f"{r['status']:<10}"
        print(f"{color(st, tag[r['status']], tty)} {human(r['size_kb']):>8} "
              f"{human(r['artifact_kb']):>9} {idle:>6}  {br:<28} {short(r['path'])}")
        if r["status"] in ("REVIEW", "ORPHAN") and r.get("reason"):
            print(color(f"{'':<10} └ {r['reason']}", "90", tty))
    if len(dormant if not a.all else recs) > a.top:
        print(color(f"... {len(dormant if not a.all else recs) - a.top} more (use --top N)", "90", tty))

    if stale:
        print()
        print(color(f"{len(stale)} stale worktree record(s) pointing at deleted folders "
                    f"(harmless, cleaned by `git worktree prune`):", "1", tty))
        for s in stale[:10]:
            print(f"  {short(s['repo'])}  →  {short(s['missing_path'])}")

    if removable:
        print()
        print(color("Suggested cleanup for REMOVABLE worktrees (review, then run yourself):", "1", tty))
        for r in sorted(removable, key=lambda r: r["size_kb"], reverse=True)[:15]:
            repo = r.get("main_repo") or r["path"]
            print(f'  git -C "{repo}" worktree remove "{r["path"]}"')
        print(color("  (removing a worktree keeps its branch; delete merged branches separately with git branch -d)", "90", tty))

    print()
    print(color(f"Scanned in {time.time() - t0:.1f}s. Nothing was changed.", "90", tty))


if __name__ == "__main__":
    main()
