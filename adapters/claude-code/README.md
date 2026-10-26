# rematch for Claude Code

A Claude Code plugin that gives Claude Code sessions the same rematch loop omp has: the same binary, ledger, lint, counters and entries. The plugin holds no schema and no prompt text: its hooks pipe each Claude Code hook payload, unchanged, to the `rematch` binary, which writes the trace, matches what loads, and evaluates approved hooks. Contract and limits: [`docs/DESIGN.md`](../../docs/DESIGN.md) §18.1.

## Install

Nothing is installed for you. Requirements: Claude Code 2.1.285 or later; the `rematch` binary, found as `$REMATCH_BIN`, else `target/release/rematch` / `target/debug/rematch` of this checkout (`cargo build --release` at the repo root), else `rematch` on `PATH`.

One session:

```sh
claude --plugin-dir /path/to/rematch/adapters/claude-code
```

Every session (Claude Code loads a plugin directory under `~/.claude/skills/<name>/` as `<name>@skills-dir`):

```sh
ln -s /path/to/rematch/adapters/claude-code ~/.claude/skills/rematch
```

`~/.claude/plugins/` is Claude Code's marketplace cache and settings only list `enabledPlugins` by `name@marketplace`; a marketplace install would copy the plugin and drop the `skills/` symlink, so use one of the two routes above. Check the load with `claude --plugin-dir /path/to/rematch/adapters/claude-code plugin details rematch`: 8 hooks, and under Skills the three commands plus every linked skill.

Optional: give `SessionEnd` more than its 1.5 s (plugin timeouts do not raise it): `CLAUDE_CODE_SESSIONEND_HOOKS_TIMEOUT_MS=5000`.

To remove: delete the `~/.claude/skills/rematch` link (or stop passing `--plugin-dir`). `rematch uninstall` removes `~/.rematch`.

## What it does

| Claude Code hook | rematch |
|---|---|
| `SessionStart` | `session_start` with the entries matched for the session's cwd; links the matching provisional and kept skills into `~/.rematch/claude-skills/` and, on the first start, `skills/` here → that directory (asks Claude Code to re-scan skills when a link changed) |
| `UserPromptSubmit` | `input` with the redirect pre-label (negation, revert, reprompt, interrupt, mark); with an approved `retrieve` hook, the matching units as added context |
| `PreToolUse` | approved `block` / `stuck` hooks: a refusal exits 2 with the hook's message, and is traced |
| `PostToolUse`, `PostToolUseFailure` | `tool_result` |
| `Stop`, `StopFailure` | the run's `turn_end`s (read from the transcript) and `agent_end` |
| `SessionEnd` | `session_shutdown`; a flagged session writes `~/.rematch/pending/cc:<id>.json` |

Every hook goes through `scripts/hook.sh`, which never fails a hook: any error exits 0, and only an intentional block exits 2.

Commands (each runs the binary before the prompt reaches the model and shows its output):

- `/rematch:rematch-status` — `rematch status` and what loads here.
- `/rematch:rematch-why [cc:<id>]` — `rematch library why`.
- `/rematch:rematch-mark correction|preference|scope` — `rematch mark` for this session.

Reflection never runs inside Claude Code. Run `rematch reflect --pending` in a terminal (or start omp, which spawns it); `rematch digest --text` lists what was staged and what waits for approval; `rematch library approve <id>` approves a hook. Past Claude Code sessions: `rematch import --history --harness claude-code`, then `rematch reflect --history`.

## Files

```
.claude-plugin/plugin.json   manifest
hooks/hooks.json             the eight hooks, exec form: sh scripts/hook.sh trace-event|hook-check
scripts/hook.sh              stdin → rematch <op> --harness claude-code --hook-output
scripts/rematch.sh           binary lookup ($REMATCH_BIN, target/{release,debug}, PATH)
commands/rematch-*.md        the three commands
skills                       (created at first SessionStart, git-ignored) → ~/.rematch/claude-skills
```
