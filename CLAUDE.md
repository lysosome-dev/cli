# CLAUDE.md — lysosome CLI (open source, MIT)

Rust workspace for the `lysosome` CLI. Public repo: never reference private platform internals.

## Layout
- `crates/lysosome-core/` — library: discovery, analysis, sizing, classification. **No printing.**
- `crates/lysosome-cli/` — binary `lysosome`: arg parsing, table/JSON output, progress (stderr).
- `reference/wtscan.py` — Python prototype; behavioral source of truth for worktree classification. Do not ship.
- `tests/fixtures/` — scripts that build fake repos/caches for tests.
- `schema/` — JSON-schema for the versioned `--json` contract.
- `docs/SPEC.md` — product spec, milestones, safety rules.

## Commands
```bash
cargo build
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```
All three (fmt, clippy, test) must pass; CI runs them on macOS and Linux.

## Non-negotiable safety rules (docs/SPEC.md §2)
1. Scanning never modifies anything. Every git call uses `git --no-optional-locks`.
2. Read index/HEAD/logs/HEAD mtimes before running git.
3. Never remove/suggest removing a worktree that is dirty, has untracked files, is locked, or has commits neither merged nor pushed.
4. Only count artifact folders that `git check-ignore` confirms.
5. Deletion defaults to dry-run, uses each tool's own cleanup command, else macOS Trash.
6. Never follow symlinks.
7. When unsure: `REVIEW`, never `REMOVABLE`.

## Conventions
- Product name lives in `lysosome_core::TOOL_NAME`.
- Tests never touch the real home directory: temp dirs + isolated `HOME`/`GIT_CONFIG_GLOBAL`.
- Permission-denied folders are skipped and counted, never fatal.
- macOS-only code behind `cfg(target_os = "macos")`; keep Linux compiling.
- Branches: `feature/…`, `fix/…`, `chore/…`, `docs/…`. Never commit to main; squash-merge PRs.
