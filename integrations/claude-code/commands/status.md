---
description: Show which git worktrees from your Claude Code sessions are holding disk, and what lysosome would reclaim (read-only)
allowed-tools: Bash(lysosome worktrees:*), Bash(git worktree list:*), Bash(git rev-parse:*), Bash(python3 *lysosome_hook.py report*)
---

Report what this repository's git worktrees (the ones agent sessions create) are holding. **Read-only: change nothing.**

1. Find the repository: `git rev-parse --path-format=absolute --git-common-dir` (its parent folder is the main checkout) and list worktrees with `git worktree list --porcelain`.
2. Run `lysosome worktrees <each worktree path> --json --depth 1 --progress off --all`. If `lysosome` isn't installed, say so and point to `brew install lysosome-dev/tap/lysosome` or https://lysosome.dev.
3. Also run `python3 "${CLAUDE_PLUGIN_ROOT}/scripts/lysosome_hook.py" report --data "${CLAUDE_PLUGIN_DATA}"` for what earlier sessions recorded when they ended (and anything the plugin already tidied).
4. Present a compact table: branch, status (`REMOVABLE`, `REVIEW`, `ORPHAN`, `ACTIVE`), size, build output size, idle days, and the reason. Total what is `REMOVABLE`. Mention that `REVIEW` items hold uncommitted, untracked or unpushed work and are never removed, and that their build output can still be freed. Suggest `/lysosome:tidy` if anything is reclaimable.

Mark the worktree this session is running in as "(this session)".
