# SPEC — lysosome: the dev disk reclaimer

> Handoff spec for Claude Code. Build milestone by milestone, in order. Each milestone has acceptance criteria; don't start the next until they pass.
> `reference/wtscan.py` is a working, tested Python prototype of the worktree scanner. Treat it as the behavioral source of truth for Milestone 1 classification rules.
> The product name lives in one constant (`lysosome_core::TOOL_NAME`).

## 1. What we're building

A fast, trustworthy CLI that shows developers on macOS where their disk went: dormant git worktrees, stale build artifacts, package-manager caches, local AI models, Docker and Xcode data. It labels every item by how safe it is to delete, and later (Milestone 4) cleans it up safely.

**Positioning:** "the disk cleaner for the agentic coding era." Agent tools (Claude Code, Cursor, parallel agents) create git worktrees constantly and nothing cleans them up. Each dormant worktree drags its own `node_modules` / `target` / `.venv`.

**Business model (context, not in scope to build):** the CLI is open source (MIT). A paid native Mac menu bar app comes later and consumes the CLI's `--json` output. That's why the JSON output is a stable, versioned contract (section 6).

**Platform:** macOS first (Apple Silicon and Intel). Keep Linux compiling and working where it costs nothing; don't build Windows support.

## 2. Non-negotiable safety rules

These apply to every milestone. Violating any of them is a bug that blocks release.

1. **Scanning never modifies anything.** No writes, no index refresh, no lock files. All git calls use `git --no-optional-locks`.
2. **Read timestamps before running git.** Git can touch the index; capture `index`, `HEAD` and `logs/HEAD` mtimes first.
3. **Never remove or suggest removing a worktree** that has uncommitted changes, untracked files, is locked, or has commits that are neither merged into the default branch nor pushed to a remote.
4. **Only count a folder as an artifact if git ignores it** (`git check-ignore`). Never call tracked source "junk".
5. **Deletion (Milestone 4+) goes to the macOS Trash by default**, uses each tool's own cleanup command where one exists (`git worktree remove`, `docker image prune`, `brew cleanup`, `xcrun simctl delete unavailable`, `go clean -modcache`), and defaults to dry-run.
6. **Never follow symlinks** during walks or size calculations.
7. When unsure, classify as `REVIEW`, never `REMOVABLE`.

## 3. Tech stack

- **Language:** Rust (stable, 2021 edition or newer).
- **Workspace layout:**
  ```
  crates/
    lysosome-core/   # library: discovery, analysis, sizing, classification. No printing.
    lysosome-cli/    # binary: arg parsing, table/JSON output, progress
  reference/
    wtscan.py       # prototype; do not ship, keep for behavior reference
  tests/fixtures/   # scripts that generate fake repos/caches for tests
  ```
- **Suggested crates** (swap if there's a clearly better option, and note why):
  - `ignore` (`WalkBuilder::build_parallel`) or `jwalk` for parallel directory walking
  - `rayon` for parallel analysis of found items
  - `clap` (derive) for the CLI
  - `serde` / `serde_json` for JSON output
  - `anyhow` (CLI) and `thiserror` (core) for errors
  - `comfy-table` or hand-rolled aligned output, `owo-colors` (respect `NO_COLOR` and non-TTY)
  - `indicatif` for progress on stderr
  - `dirs` for home/cache paths
  - `trash` (Milestone 4) for move-to-Trash
- **Git:** shell out to the `git` binary for status, merge-base, rev-list, check-ignore. That gives correctness with the user's real config, hooks-free reads and every edge case git handles. Bound concurrency (e.g. a rayon pool of `min(8, cores)` for git subprocesses) so we don't fork-bomb on 300 worktrees. Do **not** use `gix`/`git2` for status in v1; revisit only if profiling shows git subprocesses dominate.
- **Disk size:** compute in-process from `metadata().blocks() * 512` (allocated size, like `du`), not `len()`. De-duplicate hard links by `(dev, ino)`. Don't cross filesystem boundaries. Note in output docs that APFS clones may be over-counted; that's acceptable for v1.

## 4. Milestones

### Milestone 1 — Worktree scanner (port `wtscan.py`)

Port the prototype to Rust with identical classification behavior, then make it fast.

**Discovery**
- Walk the given roots (default `~`) to depth 8, skipping the `SKIP_DIRS` set from the prototype and never descending into `.git`.
- A **worktree** is a directory whose `.git` is a *file* starting with `gitdir:` and pointing into `.../worktrees/...`. Skip submodules (`.../modules/...`).
- A **main repo** has a `.git` directory. Read `.git/worktrees/*/gitdir` to find worktrees outside the scan roots; entries pointing at missing folders are **stale metadata**.
- A worktree whose `gitdir` target doesn't exist is an **orphan** (parent repo moved or deleted).

**Per-worktree analysis**
- `idle_days`: max of the gitdir's `index` / `HEAD` / `logs/HEAD` mtimes and the last commit time (`git log -1 --format=%ct`).
- `branch` (or `(detached)`), counts of changed and untracked files (`git status --porcelain`).
- `merged_into`: default branch from `refs/remotes/origin/HEAD`, falling back to `main`, `master`, `origin/main`, `origin/master`; merged if `git merge-base --is-ancestor HEAD <base>`.
- `unpushed`: `git rev-list --count HEAD --not --remotes` if any remote exists, otherwise `null`.
- `main_repo` from `commondir`; `locked` if the gitdir contains `locked`.
- Size of the worktree and of its git-ignored artifact folders (`ARTIFACT_DIRS` from the prototype, searched to depth 6).

**Classification** (exactly as the prototype)
- `ACTIVE`: idle less than `--days` (default 30).
- `ORPHAN`: gitdir missing.
- `REVIEW`: dormant and (locked, or dirty, or commits not safe), with a human-readable reason.
- `REMOVABLE`: dormant, clean, and (merged into the default branch or fully pushed).
- No remote and not merged → `REVIEW` ("no remote and not merged").

**CLI**
```
lysosome worktrees [ROOTS...] [--days N] [--depth N] [--all] [--top N] [--json]
```
Output matches the prototype: headline (dormant count and size, removable size, artifact size), table sorted by size, reasons under REVIEW/ORPHAN rows, stale metadata section, suggested `git -C "<main>" worktree remove "<path>"` commands, and a final "Nothing was changed." line. Progress goes to stderr; stdout stays clean for `--json`.

**Acceptance criteria**
- Fixture script (`tests/fixtures/make_worktrees.sh`) builds: a merged+clean+old worktree with a 20MB ignored `node_modules`, a dirty old one, an old one with an unpushed commit, an active one, a deleted worktree folder (stale metadata), and an orphan whose parent repo was deleted. Port the setup used to validate the prototype: back-date commits with `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` and gitdir files with `touch -t`, and use an isolated `GIT_CONFIG_GLOBAL`.
- Integration tests assert each fixture's status and reason, and that `--json` totals match.
- A test snapshots every file's mtime and content hash in the fixture tree before and after a scan and asserts nothing changed.
- `lysosome worktrees` and `python3 reference/wtscan.py` agree on status for every fixture.

### Milestone 2 — Project artifacts and staleness

```
lysosome scan [ROOTS...] [--days N] [--json]
```
- Find git-ignored artifact folders (`node_modules`, `.venv`, `venv`, `target`, `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `dist`, `build`, `.gradle`, `Pods`, `.dart_tool`, `.build`, `coverage`, `.parcel-cache`, `.expo`) in all repos, not just worktrees.
- Staleness per project = last commit date of that repo.
- Headline example: "47 projects untouched for 180+ days are holding 62GB of dependencies and build output."
- `scan` includes the worktree results from Milestone 1 as one section.

### Milestone 3 — Global caches, AI models, Docker, Xcode

Each detector is a module implementing one trait, e.g.:

```rust
trait Detector {
    fn id(&self) -> &'static str;          // "ollama", "xcode-deriveddata", ...
    fn category(&self) -> Category;        // AiModels, Xcode, PackageCache, Containers, Editor, Android
    fn scan(&self, ctx: &ScanContext) -> Result<Vec<Finding>>;
}
```

Known locations (check each exists; resolve tool-reported paths at runtime where a tool can tell us, e.g. `brew --cache`, `go env GOMODCACHE`):

| Category | Items | Tier |
|---|---|---|
| Xcode | `~/Library/Developer/Xcode/DerivedData` | Low-cost regenerable |
| Xcode | `~/Library/Developer/Xcode/iOS DeviceSupport` (list per iOS version) | Low-cost regenerable |
| Xcode | Simulator devices/runtimes via `xcrun simctl list --json` | Regenerable, high cost |
| Xcode | `~/Library/Developer/Xcode/Archives` | Review (contains dSYMs) |
| AI models | `~/.ollama/models` (map blobs back to model names via manifests) | High cost |
| AI models | `~/.cache/huggingface/hub` (one finding per `models--org--name`) | High cost |
| AI models | `~/.lmstudio/models`, `~/.cache/torch`, `~/.cache/whisper` | High cost |
| Containers | `docker system df` (images, build cache, volumes) and OrbStack | Build cache low cost; images high cost; volumes Review |
| Package caches | npm `~/.npm/_cacache`, pnpm store, Yarn, Bun `~/.bun/install/cache`, pip, uv, conda `pkgs`, Cargo `~/.cargo/registry`, Go mod/build cache, Gradle `~/.gradle/caches`, Maven `~/.m2/repository`, Homebrew, CocoaPods | Low-cost regenerable |
| Android | `~/.android/avd`, SDK `system-images` | High cost |
| Editors | VS Code, Cursor, JetBrains caches | Low-cost regenerable |

**Safety tiers** (shown on every finding):
1. `regenerable` — caches, DerivedData. One-click safe.
2. `regenerable_expensive` — models, images, `node_modules`. Show the re-download cost.
3. `review` — archives, volumes, anything we can't be sure is rebuildable.

AI model findings should show the model name, size and last-used time, not blob hashes. This is a differentiator.

### Milestone 4 — Safe cleanup

```
lysosome clean [--worktrees] [--artifacts] [--caches] [--tier regenerable] [--older-than 90d] [--yes] [--permanent]
```
- **Dry run by default.** It prints exactly what would happen and the total. `--yes` executes.
- Worktrees: `git worktree remove <path>` then `git worktree prune`, never `rm`. Offer "strip artifacts only" (delete ignored artifact folders, keep the worktree), which is safe even for dirty worktrees.
- Everything else: the tool's own cleanup command where one exists; otherwise move to Trash via the `trash` crate. `--permanent` is opt-in and requires `--yes`.
- Re-check every safety condition immediately before acting (state may have changed since the scan).
- Write an action log to `~/.local/state/lysosome/actions.log` (or the macOS equivalent) with what was removed and where it went.
- **Acceptance:** fixture tests prove REVIEW items are never touched even with `--yes`, dry-run changes nothing, and trashed items are recoverable.

### Milestone 5 — Distribution

- `cargo-dist` (or equivalent) to build a universal macOS binary plus Linux x86_64/aarch64, with GitHub Releases.
- Homebrew tap (`brew install lysosome-dev/tap/lysosome`).
- README with a shareable screenshot of the headline output, install instructions and the safety rules from section 2.
- Code signing and notarization are deferred until the Mac app.

## 5. Performance targets

- Home-directory scan on a typical dev Mac (~1–3M files) completes in under ~30s cold, and much faster warm.
- `lysosome worktrees ~/code` with 100+ worktrees completes in a few seconds.
- Parallel walking and size computation; bounded git subprocess concurrency.
- Progress indicator on stderr when stderr is a TTY.
- Add a `--profile` flag or tracing spans so slow phases (walk vs. size vs. git) are visible.

## 6. JSON output contract

Stable and versioned; the Mac app will depend on it. Additive changes only within a major version.

```json
{
  "schema_version": 1,
  "tool_version": "0.1.0",
  "scanned_roots": ["/Users/me/code"],
  "dormant_after_days": 30,
  "summary": {
    "reclaimable_bytes": 0,
    "by_tier": { "regenerable": 0, "regenerable_expensive": 0, "review": 0 },
    "by_category": { "worktrees": 0, "artifacts": 0, "ai_models": 0, "xcode": 0 }
  },
  "findings": [
    {
      "id": "worktree:/Users/me/code/app-feat-x",
      "category": "worktrees",
      "kind": "worktree",
      "path": "/Users/me/code/app-feat-x",
      "size_bytes": 0,
      "tier": "regenerable",
      "status": "REMOVABLE",
      "reason": "clean, merged into origin/main",
      "last_activity": "2026-07-01T10:00:00Z",
      "details": { "branch": "feat-x", "unpushed": 0, "changed": 0, "untracked": 0, "artifact_bytes": 0 },
      "cleanup": { "command": ["git", "-C", "/Users/me/code/app", "worktree", "remove", "/Users/me/code/app-feat-x"] }
    }
  ],
  "stale_metadata": []
}
```
Use bytes in JSON (humanize only in the table output). Add a JSON-schema file under `schema/` and a test that validates output against it.

## 7. Engineering conventions

- `lysosome-core` has no `println!`; it returns data. The CLI handles all presentation.
- Every detector has a fixture-based test. No test touches the real home directory; use temp dirs and an isolated `HOME`/`GIT_CONFIG_GLOBAL`.
- `cargo fmt`, `cargo clippy -- -D warnings` and `cargo test` must pass before each milestone is done.
- Prefer clear errors over panics: a permission-denied folder is skipped and counted ("12 folders skipped: permission denied"), never fatal.
- macOS-specific code behind `cfg(target_os = "macos")`.

## 8. Out of scope (for now)

- The Mac app, background monitoring, scheduling, alerts (paid product, later).
- Windows support.
- Team/fleet features.
- Squash-merge detection via the GitHub API (possible later with `gh`; squash-merged branches currently show as REVIEW, which is the safe default).
