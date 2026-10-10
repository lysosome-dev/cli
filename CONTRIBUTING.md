# Contributing to lysosome

Thanks for helping. lysosome is a Rust workspace; the product spec, milestones
and safety rules live in [docs/SPEC.md](docs/SPEC.md).

## Setup

You need a stable Rust toolchain (1.85+, pinned to `stable` by
`rust-toolchain.toml`), `git`, and `python3` for the prototype parity test.

```bash
cargo build
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

CI runs fmt, clippy and tests on macOS and Linux; all three must pass.

## Layout

- `crates/lysosome-core`: discovery, analysis, sizing, classification. Returns
  data and **never prints**.
- `crates/lysosome-cli`: the `lysosome` binary: arguments, table and JSON
  output, progress (stderr only), completions.
- `schema/lysosome.schema.json`: the versioned `--json` contract, and
  `schema/clean.schema.json` for `lysosome clean --json`. Changes within a
  `schema_version` must be additive.
- `reference/wtscan.py`: the Python prototype; the source of truth for
  worktree classification. Not shipped.
- `tests/fixtures/`: scripts that build fake repos and caches for tests.

## Safety rules

These are release blockers (full text in the [README](README.md#safety-rules)):

1. Scanning never modifies anything. Every git call uses `git --no-optional-locks`.
2. Read index / `HEAD` / `logs/HEAD` timestamps before running git.
3. Never remove or suggest removing a worktree that is dirty, has untracked
   files, is locked, or has commits that are neither merged nor pushed.
4. Only count an artifact folder when `git check-ignore` confirms it.
5. Deletion defaults to dry-run, uses each tool's own cleanup command, else
   the macOS Trash.
6. Never follow symlinks.
7. When unsure, `REVIEW`, never `REMOVABLE`.

## Tests and fixtures

- Tests never touch your real home folder. Build state in a temp dir and run
  with an isolated `HOME` and `GIT_CONFIG_GLOBAL` (see
  `tests/fixtures/make_worktrees.sh`, which creates a worktree in every state).
- New classification behavior needs a fixture case and, where it applies, a
  matching change in `reference/wtscan.py` so the parity test keeps agreeing.
- The read-only test snapshots every file's mtime and hash before and after a
  scan. Keep it passing.
- Permission-denied folders are skipped and counted, never fatal.
- `clean` tests set `LYSOSOME_TRASH_DIR` so nothing lands in your real
  Trash. The one test that uses the real system Trash only runs with
  `LYSOSOME_TEST_REAL_TRASH=1` (then remove the test folder from the Trash).
- Never run `lysosome clean --yes` against your real home folder while
  developing; use fixtures.
- macOS-only code goes behind `cfg(target_os = "macos")`; Linux must still build.

## Pull requests

- Branch from `main` as `feature/…`, `fix/…`, `chore/…` or `docs/…`.
- Keep PRs focused, add tests, and update `CHANGELOG.md` under `[Unreleased]`.
- PRs are squash-merged.

## Releases

Maintainers cut releases with [cargo-dist](https://axodotdev.github.io/cargo-dist/)
(config in `dist-workspace.toml`):

1. Bump `version` in the root `Cargo.toml` (and the `lysosome-core`
   dependency version in `crates/lysosome-cli/Cargo.toml`), move the
   `[Unreleased]` notes in `CHANGELOG.md` under the new version, merge.
2. Tag `main` and push: `git tag v0.2.0 && git push origin v0.2.0`.
3. `release.yml` builds the archives (with man pages and completions from
   `scripts/dist-extras.sh`), `lysosome-installer.sh`, `lysosome.rb` and
   checksums, and publishes the GitHub Release.
4. [lysosome-dev/homebrew-tap](https://github.com/lysosome-dev/homebrew-tap)
   checks for new releases every 30 minutes. To publish right away (and to
   be safe if GitHub ever paused the schedule):
   `gh workflow run -R lysosome-dev/homebrew-tap update-formula.yml`.

After editing `dist-workspace.toml`, run `dist generate` to refresh
`release.yml`, and `dist plan` to check it. For a local dry run:
`scripts/dist-extras.sh && dist build`.
