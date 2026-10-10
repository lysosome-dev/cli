# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/). The `--json` output has its own
`schema_version`; only additive changes happen within one.

## [Unreleased]

### Added

- `lysosome clean -i` / `--interactive` (#16): pick what to clean in a
  terminal checklist grouped by category (size, tier badge, path). Items of
  the `--tier` tiers start ticked (cheap ones by default), costly ones are
  listed unticked, and `REVIEW` items are
  shown greyed out with their reason. `space` toggles, `a` ticks all/none,
  `enter` continues to the final plan and a `y/N` confirmation; execution
  uses the same per-item re-checks. Refuses to run without a terminal.
- Releases publish `lysosome-core` and `lysosome` to crates.io once the
  maintainers add a token, so `cargo install lysosome` will work.
- Issue and pull request templates, a code of conduct, Dependabot, and a
  `cargo-deny` CI check (licenses, RustSec advisories, duplicate crates).

## [0.2.1] - 2026-10-10

### Fixed

- Scanning could write to disk (#13): with corepack's `pnpm` shim on `PATH`,
  asking `pnpm store path` downloaded pnpm into `~/.cache/node/corepack`.
  `pnpm` is now asked only when it is a real pnpm install; behind corepack,
  Volta or a version-manager shim (asdf, mise...) the store is found from
  `store-dir` (`npm_config_store_dir`, `~/.npmrc`, pnpm's `rc`) or the default
  locations (`$PNPM_HOME/store`, `$XDG_DATA_HOME/pnpm/store`,
  `~/Library/pnpm/store`, `~/.local/share/pnpm/store`). Yarn is no longer
  asked at all, since `yarn cache dir` creates a missing cache folder: its
  cache comes from `$YARN_CACHE_FOLDER`, `cache-folder` in `~/.yarnrc`, or the
  defaults (`~/Library/Caches/Yarn`, `~/.cache/yarn`, Yarn 2+'s
  `$XDG_DATA_HOME/yarn/berry/cache` or `~/.yarn/berry/cache`).
- Go is no longer asked either: since Go 1.23 every go command updates
  telemetry counters on disk. `GOMODCACHE` / `GOCACHE` / `GOPATH` come from
  the environment, then Go's `go env -w` file, then the defaults.
- Every tool a scan still asks (`pnpm`, `pip`, `uv`, `brew`, `docker`,
  `xcrun`) now runs from `/` with only the scan's environment plus switches
  that turn off downloads, self-updates, auto-installs and bytecode writes
  (`COREPACK_ENABLE_NETWORK=0`, `GOTOOLCHAIN=local`,
  `HOMEBREW_NO_AUTO_UPDATE=1`, `PIP_DISABLE_PIP_VERSION_CHECK=1`,
  `PYTHONDONTWRITEBYTECODE=1`, `MISE_EXEC_AUTO_INSTALL=false`...), so a
  project's `packageManager` field or `go.mod` toolchain line can't make a
  tool fetch itself.
- `lysosome clean` runs cache cleanup commands with corepack's auto-pin and
  strict mode off, so it never edits the `package.json` of the folder you run
  it from.

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
