## What and why

<!-- What does this change, and why? Link the issue: "Closes #123". -->

## How it was tested

<!-- Commands run, fixtures added, platforms tried (macOS / Linux). -->

## Checklist

- [ ] `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass
- [ ] Tests added or updated (fixtures in `tests/fixtures/` for new classification behavior)
- [ ] `CHANGELOG.md` updated under `[Unreleased]` (user-visible changes)
- [ ] `--json` changes are additive and reflected in `schema/` (or N/A)

### Safety rules ([docs/SPEC.md §2](https://github.com/lysosome-dev/cli/blob/main/docs/SPEC.md#2-non-negotiable-safety-rules))

- [ ] Scanning still modifies nothing; every git call uses `git --no-optional-locks`
- [ ] Index / `HEAD` / `logs/HEAD` timestamps are read before git runs
- [ ] No worktree that is dirty, has untracked files, is locked, or has unmerged-and-unpushed commits can become `REMOVABLE` or get a cleanup command
- [ ] Artifact folders only count when `git check-ignore` confirms them
- [ ] Deletion stays dry-run by default, uses the tool's own cleanup command, else the Trash, and re-checks before acting
- [ ] Symlinks are never followed
- [ ] Anything uncertain is `REVIEW`, never `REMOVABLE`
- [ ] Tests use temp dirs with an isolated `HOME` / `GIT_CONFIG_GLOBAL` and never touch the real home folder or Trash
- [ ] macOS-only code is behind `cfg(target_os = "macos")` and Linux still builds
