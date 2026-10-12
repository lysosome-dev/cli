---
description: Review this repo's dormant agent worktrees and reclaim what is safe, after you confirm
argument-hint: "[--older-than 7d]"
allowed-tools: Bash(lysosome worktrees:*), Bash(git worktree list:*), Bash(git rev-parse:*)
---

Help the user reclaim disk from this repository's git worktrees with the `lysosome` CLI. lysosome's safety rules decide what may go; never work around them, and never delete anything with `rm`, `git worktree remove --force` or `--permanent`.

1. Find the main checkout (`git rev-parse --path-format=absolute --git-common-dir`, parent folder) and the worktrees (`git worktree list --porcelain`). Note which one contains the current working directory: **never remove the worktree this session is running in** (freeing its build output is fine).
2. Read-only first: `lysosome worktrees <worktree paths...> --json --depth 1 --progress off --days 0` (`--days 0` asks "is the work finished", not "is it old"; if the user passed `$ARGUMENTS` such as `--older-than 7d`, use `--days 7` instead).
3. Build a plan and show it as a short table before doing anything:
   - **Remove** `REMOVABLE` worktrees (clean, and merged or fully pushed): `git worktree remove`, the branch is kept.
   - **Free build output** in `REVIEW` worktrees with large `artifact_bytes`: git-ignored `node_modules`, `target`, `.venv`... go to the Trash, the worktree and its uncommitted work stay.
   - **Leave** everything else, with lysosome's reason.
4. Show lysosome's own dry run for the plan, for example `lysosome clean <paths> --worktrees --only <worktree id> --days 0 --depth 1` and `lysosome clean <paths> --strip-artifacts --tier regenerable_expensive --only <worktree path> --days 0 --depth 1` (no `--yes`: nothing changes).
5. **Ask the user to confirm** (they may pick a subset). Only after an explicit yes, run the same commands with `--yes` and with `LYSOSOME_ACTOR=claude-code` in the environment, so the action log records who asked: `LYSOSOME_ACTOR=claude-code lysosome clean ... --yes`.
6. Report what was freed and where it went. Items moved to the Trash can be put back from the Trash (or with Undo in the lysosome desktop app); removed worktrees can be re-created from their branch with `git worktree add`. If lysosome skipped something because it changed since the scan, say why.
