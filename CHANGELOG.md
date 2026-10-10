# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/). The `--json` output has its own
`schema_version`; only additive changes happen within one.

## [Unreleased]

## [0.2.1] - 2026-10-10

### Fixed

- Scanning could write to disk (#13): with corepack's `pnpm` shim on `PATH`,
  asking `pnpm store path` downloaded pnpm into `~/.cache/node/corepack`.
  Corepack shims (and Volta's, which can also fetch pinned versions) are now
  never run during a scan; the pnpm store and Yarn cache are found at their
  default locations instead (`$PNPM_HOME/store`, `$XDG_DATA_HOME/pnpm/store`,
  `~/Library/pnpm/store`; `$YARN_CACHE_FOLDER`,
  `~/Library/Caches/Yarn`, `~/.cache/yarn`, `~/.yarn/berry/cache`). Yarn is no
  longer asked at all, since `yarn cache dir` creates a missing cache folder.
- Every tool a scan asks (`pnpm`, `pip`, `uv`, `go`, `brew`, `docker`,
  `xcrun`) now runs from `/` with only the scan's environment plus switches
  that turn off downloads, self-updates, auto-installs and bytecode writes
  (`COREPACK_ENABLE_NETWORK=0`, `GOTOOLCHAIN=local`,
  `HOMEBREW_NO_AUTO_UPDATE=1`, `PIP_DISABLE_PIP_VERSION_CHECK=1`,
  `PYTHONDONTWRITEBYTECODE=1`, `MISE_EXEC_AUTO_INSTALL=false`...), so a
  project's `packageManager` field or `go.mod` toolchain line can't make a
  tool fetch itself.

## [0.2.0] - 2026-10-10

### Added

- `lysosome clean [ROOTS...]`: safe cleanup. Dry run by default; `--yes` (or
  `y` at the `Proceed? [y/N]` prompt on a terminal) acts. Only `REMOVABLE`
  items are cleaned, never `REVIEW`, `ACTIVE` or `ORPHAN`, and never tier
  `review`. Defaults to tier `regenerable`; `--tier regenerable_expensive`
  adds dependencies, models and images.
  - Worktrees: `git worktree remove` (never `--force`) then `git worktree
    prune`; branches are kept. `--strip-artifacts` keeps dormant worktrees
    (including `REVIEW` ones) and removes only their git-ignored artifact
    folders.
  - Caches: the tool's own cleanup command (no shell, with a timeout);
    failures are reported per item and the exit status is 1.
  - Everything else goes to the macOS Trash (freedesktop.org trash on
    Linux); `--permanent` deletes instead and requires `--yes`.
  - Every item is re-checked right before acting and skipped with a reason
    if anything changed since the scan.
  - Every outcome is appended to an action log
    (`~/Library/Logs/lysosome/actions.log` on macOS,
    `$XDG_STATE_HOME/lysosome/actions.log` on Linux).
  - `--worktrees`, `--artifacts`, `--caches`, `--older-than 90d`,
    `--only <id|glob>`, `--json` (`schema/clean.schema.json`).
- `lysosome scan`: git-ignored build and dependency folders (`node_modules`,
  `target`, `.venv`, `dist`, `.next`, `Pods`...) in every repository, dated by
  the project's last activity, with a headline of how much idle projects hold.
  Dependencies are `regenerable_expensive`, build output `regenerable`.
- `lysosome caches` (alias `system`): package caches (npm, pnpm, Yarn, Bun,
  pip, uv, conda, Cargo, Go, Gradle, Maven, Homebrew, CocoaPods), AI models
  by name (Ollama, Hugging Face, LM Studio, torch, whisper), Docker images,
  build cache and volumes, Xcode (DerivedData, device support, simulators,
  archives), Android emulators and system images, editor caches. Each finding
  names the tool's own cleanup command where one exists.
- `lysosome scan` includes the cache, model, Docker and Xcode findings
  (`--no-system` skips them).

## [0.1.0] - 2026-10-10

First release.

### Added

- `lysosome worktrees [ROOTS...]`: finds git worktrees under your home folder
  (or the given folders) and labels each one `REMOVABLE`, `REVIEW`, `ORPHAN` or
  `ACTIVE`, with its size, the space held by git-ignored build and dependency
  folders, idle days and branch.
- Suggested `git worktree remove` commands for `REMOVABLE` worktrees and a
  list of stale worktree records; nothing is ever changed by a scan.
- `--json`: versioned report (`schema_version: 1`) described by
  `schema/lysosome.schema.json`.
- `--days`, `--depth`, `--all`, `--top`, `--profile` and `--no-color` flags.
- `lysosome completions <bash|zsh|fish|powershell|elvish>`.
- `lysosome --version` shows the commit and target; `-V` prints just the version.
- Prebuilt binaries for macOS (arm64, x86_64) and Linux (x86_64, arm64; glibc
  and musl), a shell installer (`curl -fsSL https://lysosome.dev/install.sh | sh`)
  and a Homebrew tap (`brew install lysosome-dev/tap/lysosome`). Archives
  include man pages and shell completions.

[Unreleased]: https://github.com/lysosome-dev/cli/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/lysosome-dev/cli/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/lysosome-dev/cli/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/lysosome-dev/cli/releases/tag/v0.1.0
