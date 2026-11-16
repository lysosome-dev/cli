# lysosome CLI

Open-source (MIT) Rust CLI that finds and safely reclaims developer disk: dormant git worktrees, build artifacts, caches, AI models, Docker, Xcode. For developers and AI-agent users on macOS and Linux. Site: https://lysosome.dev. Public repo: never reference private internals, secrets or infra here.

## Non-negotiable safety rules (docs/SPEC.md §2)
1. Scanning never modifies anything. Every git call uses `git --no-optional-locks`.
2. Read index/HEAD/logs/HEAD mtimes before running git.
3. Never remove/suggest removing a worktree that is dirty, has untracked files, is locked, or has commits neither merged nor pushed (a squash merge counts only when `gh` confirms a PR merged into the default branch containing HEAD; re-verified before removal).
4. Only count artifact folders that `git check-ignore` confirms.
5. Deletion defaults to dry-run, uses each tool's own cleanup command, else macOS Trash.
6. Never follow symlinks.
7. When unsure: `REVIEW`, never `REMOVABLE`.

## Commands
```bash
scripts/verify          # same checks as CI: fmt, clippy -D warnings, tests, doc tests, cargo-deny (if installed); --fast skips deny
cargo build
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```
No dev server, e2e or preview. CI runs the checks on macOS and Linux.

## Layout
- `crates/lysosome-core/` — library: discovery, analysis, sizing, classification. **No printing.**
- `crates/lysosome-cli/` — binary `lysosome`: arg parsing, table/JSON output, progress (stderr).
- `reference/wtscan.py` — Python prototype; behavioral source of truth for worktree classification. Do not ship.
- `tests/fixtures/` — scripts that build fake repos/caches for tests.
- `schema/` — JSON-schema for the versioned `--json` contract.
- `docs/SPEC.md` — product spec, milestones, safety rules.

## Deploy & release
Merging to main deploys nothing by itself. Releases (cargo-dist binaries, crates.io publish) are cut by maintainers via the release workflows; agents must not release or publish.

## Rules
- Product name lives in `lysosome_core::TOOL_NAME`.
- Tests never touch the real home directory: temp dirs + isolated `HOME`/`GIT_CONFIG_GLOBAL`.
- Permission-denied folders are skipped and counted, never fatal.
- macOS-only code behind `cfg(target_os = "macos")`; keep Linux compiling.
- Branches: `feature/…`, `fix/…`, `chore/…`, `docs/…`. Never commit to main; squash-merge PRs.

## Docs
- `docs/SPEC.md` — spec and safety rules
- `schema/` — `--json` contract
- `CONTRIBUTING.md`, `SECURITY.md`
