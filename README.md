# lysosome

[![CI](https://github.com/lysosome-dev/cli/actions/workflows/ci.yml/badge.svg)](https://github.com/lysosome-dev/cli/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/lysosome-dev/cli?sort=semver)](https://github.com/lysosome-dev/cli/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**The disk cleaner for the agentic coding era.**

Claude Code, Cursor and parallel agents create git worktrees constantly, and nothing cleans them up. Each dormant worktree drags its own `node_modules`, `target` or `.venv`. Add build artifacts, package caches, local AI models, Docker and Xcode data, and your SSD fills up fast.

`lysosome` shows you where the disk went, labels every item by how safe it is to delete, and (soon) cleans it up safely.

> 🚧 Under active development. Milestone 1 (worktree scanner) is here; artifacts, caches, AI models and safe cleanup are next. See [docs/SPEC.md](docs/SPEC.md).

<p align="center"><img src="docs/assets/worktrees.svg" alt="lysosome worktrees: dormant worktrees grouped by how safe they are to remove" width="860"></p>

## Install

macOS (Apple silicon and Intel) and Linux (x86_64 and arm64). `git` must be on your `PATH`.

**Homebrew** (installs the man pages and bash/zsh/fish completions too):

```bash
brew install lysosome-dev/tap/lysosome
```

**Install script** (puts `lysosome` in `$XDG_BIN_HOME` or `~/.local/bin` and adds it to your `PATH`; set `LYSOSOME_INSTALL_DIR` to choose another folder, `LYSOSOME_NO_MODIFY_PATH=1` to leave your shell config alone):

```bash
curl -fsSL https://lysosome.dev/install.sh | sh
```

**Prebuilt binaries**: download an archive for your platform from the [latest release](https://github.com/lysosome-dev/cli/releases/latest) and check it against its `.sha256` file. Each archive holds the binary, man pages (`man/`) and shell completions (`completions/`).

**From source** (needs a [Rust toolchain](https://rustup.rs), 1.85+):

```bash
cargo install --locked --git https://github.com/lysosome-dev/cli lysosome
```

Check the install with `lysosome --version`.

### Shell completions

Homebrew sets these up for you. Otherwise:

```bash
lysosome completions zsh > "${fpath[1]}/_lysosome"                              # zsh
lysosome completions bash > ~/.local/share/bash-completion/completions/lysosome # bash
lysosome completions fish > ~/.config/fish/completions/lysosome.fish             # fish
```

`powershell` and `elvish` work too.

## Quick start

```bash
lysosome worktrees                       # scan your home folder
lysosome worktrees ~/code ~/.worktrees   # scan specific folders (faster)
lysosome worktrees --days 14             # call anything idle 14+ days dormant
lysosome worktrees --all                 # also list active worktrees
lysosome worktrees --json | jq .summary  # machine-readable output
```

```text
  14 dormant worktrees are holding 18.6 GB  of 41 found · idle 30+ days

     6.1 GB  safe to remove now          9 clean, merged or pushed
    12.9 GB  build & dependency output   strippable even from worktrees you keep

  STATUS        SIZE  ARTIFACTS  IDLE  BRANCH                  PATH
  REVIEW      6.7 GB     4.0 GB   72d  feat/checkout-v2        ~/code/shop/.claude/worktrees/proud-thompson
             └ 1 uncommitted change
  REMOVABLE   3.1 GB     3.0 GB   89d  feat/cost-analytics     ~/code/app-worktrees/marinating-stream
  REVIEW      2.4 GB     2.3 GB   95d  feat/stream-json        ~/code/app-wt-pr2
             └ 8 unpushed commits, not merged
  REMOVABLE   954 MB     949 MB   84d  feat/feedback-loop      ~/code/app-worktrees/tickling-quasar
  ORPHAN      412 MB          –     –  –                       ~/code/old-api-wt
             └ parent repo moved or deleted
  REVIEW     65.5 MB          –  190d  feature/geo             ~/code/site/.claude/worktrees/warm-hare
             └ no remote and not merged

  2 stale worktree records point at deleted folders  harmless; `git worktree prune` clears them
    ~/code/app  →  ~/code/app-worktrees/old-experiment

  Suggested cleanup for REMOVABLE worktrees  review, then run yourself
    git -C "/Users/me/code/app" worktree remove "/Users/me/code/app-worktrees/marinating-stream"
    git -C "/Users/me/code/app" worktree remove "/Users/me/code/app-worktrees/tickling-quasar"
    Removing a worktree keeps its branch; delete merged branches with `git branch -d`.

  Scanned 22,871 folders under ~/code, ~/.worktrees in 6.3s. Nothing was changed.
```

### What the statuses mean

| Status | Meaning |
|---|---|
| `REMOVABLE` | Idle `--days`+ (default 30), clean, and its commits are safe: merged into the default branch or fully pushed. |
| `REVIEW` | Idle, but holding something that might matter: uncommitted changes, untracked files, a lock, or commits that exist nowhere else. The reason is shown underneath. |
| `ORPHAN` | Its parent repository was moved or deleted, so git can no longer manage it. |
| `ACTIVE` | Used recently. Hidden unless you pass `--all`. |

"Idle" is the newest of the worktree's git index / `HEAD` / reflog timestamps and its last commit. **ARTIFACTS** is the space in git-ignored build and dependency folders (`node_modules`, `target`, `.venv`, `dist`, ...) inside the worktree. You can strip those even from worktrees you keep.

Squash-merged branches show as `REVIEW` (git can't tell that a squash merge landed). That's the safe default.

### Flags

| Flag | Default | |
|---|---|---|
| `--days N` | `30` | Idle days before a worktree counts as dormant |
| `--depth N` | `8` | Maximum folder depth to search for repositories |
| `--all` | | Also list active worktrees |
| `--top N` | `30` | Rows to show in the table |
| `--json` | | Print the versioned JSON report instead of a table |
| `--profile` | | Print walk / git / size timings to stderr |
| `--no-color` | | Disable color (`NO_COLOR` is honored too; color is off when piped) |

## JSON output

`--json` prints a stable, versioned document (described by [`schema/lysosome.schema.json`](schema/lysosome.schema.json), JSON Schema 2020-12). Only additive changes happen within a `schema_version`.

```json
{
  "schema_version": 1,
  "tool_version": "0.1.0",
  "scanned_roots": ["/Users/me/code"],
  "dormant_after_days": 30,
  "summary": {
    "reclaimable_bytes": 6100000000,
    "by_tier": { "regenerable": 6100000000, "regenerable_expensive": 0, "review": 12500000000 },
    "by_category": { "worktrees": 18600000000, "artifacts": 0, "ai_models": 0, "xcode": 0, "...": 0 }
  },
  "findings": [
    {
      "id": "worktree:/Users/me/code/app-worktrees/marinating-stream",
      "category": "worktrees",
      "kind": "worktree",
      "path": "/Users/me/code/app-worktrees/marinating-stream",
      "size_bytes": 3100000000,
      "tier": "regenerable",
      "status": "REMOVABLE",
      "reason": "clean, merged into origin/main",
      "last_activity": "2026-07-12T18:04:11Z",
      "details": { "branch": "feat/cost-analytics", "unpushed": 0, "changed": 0, "untracked": 0, "artifact_bytes": 3000000000, "...": "..." },
      "cleanup": { "command": ["git", "-C", "/Users/me/code/app", "worktree", "remove", "/Users/me/code/app-worktrees/marinating-stream"] }
    }
  ],
  "stale_metadata": [],
  "skipped": { "permission_denied": 0, "other_errors": 0, "missing_roots": [] }
}
```

- Sizes are allocated bytes on disk (like `du`), with hard links counted once. APFS clones may be over-counted.
- `summary.reclaimable_bytes` counts `REMOVABLE` findings only: space you can get back now with no risk.
- `summary.by_tier` and `summary.by_category` sum every dormant (non-`ACTIVE`) finding.
- `cleanup` is `null` for anything that isn't `REMOVABLE`.

## Safety rules

These are non-negotiable. Breaking any of them is a release-blocking bug.

1. **Scanning never modifies anything.** No writes, no index refresh, no lock files. Every git call uses `git --no-optional-locks`. A test snapshots every file's mtime and hash before and after a scan.
2. **Timestamps are read before git runs**, so git can never skew "last used".
3. **Never remove or suggest removing a worktree** that has uncommitted changes or untracked files, is locked, or has commits that are neither merged into the default branch nor pushed.
4. **A folder only counts as an artifact if git ignores it** (`git check-ignore`). Tracked source is never called junk.
5. **Deletion (coming soon) goes to the macOS Trash by default**, uses each tool's own cleanup command (`git worktree remove`, `docker image prune`, `brew cleanup`, ...), and defaults to dry-run.
6. **Symlinks are never followed** during walks or size calculations.
7. **When unsure, `REVIEW`**, never `REMOVABLE`.

## Development

```bash
cargo build
cargo test --workspace        # needs git; the prototype parity test also uses python3
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

- `crates/lysosome-core`: discovery, analysis, sizing and classification. Returns data and never prints.
- `crates/lysosome-cli`: the `lysosome` binary (arguments, table/JSON output, progress).
- `tests/fixtures/make_worktrees.sh`: builds a throwaway tree of worktrees in every state (isolated `HOME` / `GIT_CONFIG_GLOBAL`).
- `reference/wtscan.py`: the Python prototype the Rust port must agree with.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full workflow and how releases are cut, [SECURITY.md](SECURITY.md) to report a vulnerability, and [CHANGELOG.md](CHANGELOG.md) for what changed.

## License

MIT
