# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/). The `--json` output has its own
`schema_version`; only additive changes happen within one.

## [Unreleased]

### Added

- `lysosome mcp`: a Model Context Protocol server on stdio, so Cursor, Codex,
  Claude Code and other agents can call `scan`, `worktrees`, `caches` and
  `clean` (#57). Results are the same versioned JSON as `--json`. `clean` is
  a dry run unless the call passes `confirm: true` with exact item ids;
  `--permanent` isn't reachable, and cleanups are logged with actor `mcp`.

## [0.4.1] - 2026-10-15

### Fixed

- Sizes on overlay filesystems (Docker containers, many CI runners) count
  file contents again (#36). Overlayfs can give files a different device
  number than their folder, and the scan skipped those files, so a 40 MB
  `node_modules` showed as 0 bytes. Folders mounted from another filesystem
  are still left out.

### Added

- After a human-readable `scan` that found something reclaimable, or a
  `clean --yes` that freed space, one dim line mentions the free desktop app
  beta (https://lysosome.dev). It is shown only when stdout is a terminal,
  never with `--json` or `--progress json`, and `LYSOSOME_NO_HINTS=1` hides
  it. The JSON output is unchanged.

### Changed

- README: install options and an example run up top, and a "Desktop app
  (free beta)" section in place of the old waitlist line. More precise
  crates.io keywords.

## [0.4.0] - 2026-10-12

### Fixed

- The pip cache is found from pip's settings (`PIP_CACHE_DIR`, `cache-dir`
  in pip's config files, else the platform default) instead of running
  `pip cache dir` (#46). Running pip started the user's Python, whose startup
  lists editable installs' source folders, often in ~/Documents, which on
  macOS triggered a privacy prompt for the app running the scan. Scans are
  faster too.

### Added

- `--skip <PATH>` (#44), a global, repeatable option: leave that exact
  folder out of every walk (`scan`, `worktrees`, `clean`). It is never listed
  or entered, so on macOS a protected folder (Desktop, Documents, Downloads)
  never triggers a privacy prompt. Give `clean` the same `--skip` as the scan
  that found its items. Worktrees reached by pointer are covered too (#48):
  one whose repository is in a skipped folder, or that lives in one, is left
  out unread.
- `--progress json` streams each detector's findings (#40): a `findings`
  event right after its `detector` event, with the finding objects exactly
  as in the final report. Across all events they are the report's findings,
  so GUIs can show results while the scan runs. Additive; stdout unchanged.
- Claude Code plugin (#41), installed with `/plugin install lysosome --marketplace lysosome-dev/cli`. When a session that ran in a linked git worktree ends, a background job checks it read-only with `lysosome worktrees --days 0` and records it. The next session reports what is safe to reclaim, once. It adds `/lysosome:status` and `/lysosome:tidy`; tidy dry-runs, then cleans only after you confirm. With the opt-in `auto_tidy` setting (`strip-merged` / `remove-merged`), a merged task's worktree is tidied when its session really ends; nothing happens on `/clear`. Every cleanup is `lysosome clean`, so REVIEW worktrees are never touched.
- `LYSOSOME_ACTOR` (#41): `clean` records who asked as `actor` in the action log.

- `--progress <auto|json|off>` (#38), a global flag. `json` writes
  newline-delimited progress events to stderr (stdout keeps the report) for
  GUIs and scripts: `phase_start` / throttled `advance` / `phase_finish`, a
  `detector` event as each detector finishes, and `plan` plus one `item` event
  per item during `clean --yes`. Described by `schema/progress.schema.json`.
  `off` silences the spinner on a terminal; `auto` is today's behavior.
- `clean --json` item results gain `trashed: [{original, location}]` (#38):
  where each entry went inside the Trash. On macOS this is the real path
  `NSFileManager` reports, including renamed duplicates such as
  `target 21-08-03-944`. On Linux it comes from the freedesktop trash. Moving
  `location` back to `original` undoes the move. The action log records the
  same pairs. This is an additive schema change.

## [0.3.0] - 2026-10-10

### Added

- `lysosome clean -i` / `--interactive` (#16): pick what to clean in a
  terminal checklist grouped by category (size, tier badge, path). Items of
  the `--tier` tiers start ticked (cheap ones by default), costly ones are
  listed unticked, and `REVIEW` items are shown greyed out with their reason.
  `space` toggles, `a` ticks all/none, `enter` continues to the final plan
  and a `y/N` confirmation; execution uses the same per-item re-checks.
  Refuses to run without a terminal.
- Squash-merge awareness (#18): when `gh` is installed and signed in, dormant
  worktrees that are `REVIEW` only because their commits look unpushed are
  looked up on GitHub. If a PR merged into the default branch (from the repo
  itself) has the worktree's HEAD as, or inside, its last commit, and the
  worktree is clean and unlocked, it becomes `REMOVABLE` ("clean,
  squash-merged in PR #123", JSON `details.merged_pr {number, url}`); a
  merged PR with another head keeps `REVIEW` with a clearer reason. `clean`
  re-verifies with `gh` before removing and skips the worktree if `gh` can't
  be asked. `gh` runs sandboxed (from `/`, never prompting, timeouts, at most
  4 at a time). New flags `--no-gh` and `--offline` on `scan`, `worktrees`
  and `clean`.
- First-run polish (#28): a bare `lysosome` starts with a three-line "Try:"
  quick start above the help; every error carries a `hint:` with a fix; with
  `--json`, errors are printed on stdout as `{"error": {"code", "message",
  "hint"}}` with a non-zero exit (`schema/error.schema.json`; exit 2 for
  command-line mistakes).
- Releases publish `lysosome-core` and `lysosome` to crates.io, so
  `cargo install --locked lysosome` works.
- Issue and pull request templates, a code of conduct, Dependabot, and a
  `cargo-deny` CI check (licenses, RustSec advisories, duplicate crates).

### Changed

- Faster artifact sizing (#26): main-repo artifact folders are measured in
  parallel (hard-linked files are still credited to one folder,
  deterministically, in path order) and start as soon as `git check-ignore`
  confirms them instead of after every repository's git checks. On macOS a
  directory's allocation and entry count come from its parent's
  `getattrlistbulk` batch (no `fstat` per directory, empty ones never opened).
- Tier colors match the brand (#24): regenerable is GFP mint, expensive is
  amber, review is DAPI blue-violet (exact on truecolor terminals, nearest
  xterm-256 color on 256-color ones, else green / yellow / bright blue), in
  `scan`, `caches`, `clean` and the `REMOVABLE` / `REVIEW` statuses. Light
  terminals (`COLORFGBG`, or `LYSOSOME_THEME=light`) get the brand's
  light-theme hues. `NO_COLOR` and piped output stay plain; `--json` is
  unchanged.

### Fixed

- `lysosome caches` counted a simulator disk image twice when two runtime
  entries shared it; it is now listed once, as a review item
  (`details.shared_with_runtime_ids`).

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

[Unreleased]: https://github.com/lysosome-dev/cli/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/lysosome-dev/cli/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/lysosome-dev/cli/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/lysosome-dev/cli/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/lysosome-dev/cli/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/lysosome-dev/cli/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/lysosome-dev/cli/releases/tag/v0.1.0
