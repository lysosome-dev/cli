# lysosome for Claude Code

This plugin helps agents clean up after themselves. It notices the git worktrees your Claude Code sessions leave behind, reports what is safe to reclaim, and, if you opt in, tidies a finished task's worktree when its session ends.

```text
/plugin install lysosome --marketplace lysosome-dev/cli
```

It needs the [lysosome CLI](https://github.com/lysosome-dev/cli#install) on your `PATH` and `python3`. See the [main README](../../README.md#claude-code-plugin-agents-that-clean-up-after-themselves) for what each piece does and the `auto_tidy` options.

| Piece | What it does |
|---|---|
| `hooks/hooks.json` | `SessionEnd` → `scripts/lysosome_hook.py session-end` (detaches, returns at once); `SessionStart` (startup) → `session-start` |
| `scripts/lysosome_hook.py` | Records worktrees with `lysosome worktrees --json --days 0` (read-only); runs `lysosome clean` only for opted-in, merged worktrees |
| `commands/status.md` | `/lysosome:status`: read-only overview |
| `commands/tidy.md` | `/lysosome:tidy`: plan, dry run, then clean after confirmation |

State lives in `${CLAUDE_PLUGIN_DATA}/worktrees.jsonl`. Set `LYSOSOME_HOOK_DEBUG=1` to see hook errors; normally they are silent so a session never breaks. Tests: `cargo test -p lysosome --test claude_code_plugin`.
