# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/). The `--json` output has its own
`schema_version`; only additive changes happen within one.

## [Unreleased]

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

[Unreleased]: https://github.com/lysosome-dev/cli/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/lysosome-dev/cli/releases/tag/v0.1.0
