# rematch for Claude Code

A Claude Code plugin that gives Claude Code sessions the same rematch loop omp has: the same binary, ledger, lint, counters and entries. The plugin holds no schema and no prompt text: its hooks pipe each Claude Code hook payload, unchanged, to the `rematch` binary, which writes the trace, matches what loads, evaluates approved hooks and builds the session-start digest. Contract and limits: [`docs/DESIGN.md`](../../docs/DESIGN.md) §18.1.

## Install

Nothing is installed for you. Requirements: Claude Code 2.1.285 or later; the `rematch` binary, found as `$REMATCH_BIN`, else `target/release/rematch` / `target/debug/rematch` of this checkout (`cargo build --release` at the repo root), else `rematch` on `PATH`. Reflection makes its model calls through omp (`$OMP_BIN`, else `omp` on `PATH`, with the roles of the main [README](../../README.md#models)); without omp, sessions are still traced, flagged and loaded, and reflection waits for a `rematch reflect --pending` run where omp exists.

One session:

```sh
claude --plugin-dir /path/to/rematch/adapters/claude-code
```

Every session (Claude Code loads a plugin directory under `~/.claude/skills/<name>/` as `<name>@skills-dir`):

```sh
ln -s /path/to/rematch/adapters/claude-code ~/.claude/skills/rematch
```

`~/.claude/plugins/` is Claude Code's marketplace cache and settings only list `enabledPlugins` by `name@marketplace`; a marketplace install would copy the plugin and drop the `skills/` symlink, so use one of the two routes above. A `--plugin-dir` copy named `rematch` takes precedence over the `skills-dir` one for that session. Check the load with `claude --plugin-dir /path/to/rematch/adapters/claude-code plugin details rematch`: 8 hooks, under Skills the eight commands plus every linked skill, under Agents the rematch subagents.

Skills come from the shared directory `~/.rematch/skills/` that omp and Codex read too (one link per kept entry and per provisional entry whose predicate matched a directory that was synced; [Where skills load from](https://vncp.github.io/rematch/concepts/entries.html#where-skills-load-from)). The plugin's `skills/` is a symlink to it, made by the first `SessionStart`. Subagents are copied into the plugin's `agents/` (below).

Upgrading from a version that used `~/.rematch/claude-skills/`: the first `SessionStart` re-points `skills/` here to `~/.rematch/skills` and deletes `~/.rematch/claude-skills/` (only links lived there).

Optional: give `SessionEnd` more than its 1.5 s (plugin timeouts do not raise it): `CLAUDE_CODE_SESSIONEND_HOOKS_TIMEOUT_MS=5000`.

To remove: delete the `~/.claude/skills/rematch` link (or stop passing `--plugin-dir`). `rematch uninstall` removes `~/.rematch` and the agent copies in `agents/`.

## What it does

| Claude Code hook | rematch |
|---|---|
| `SessionStart` | `session_start` with the entries matched for the session's cwd; syncs the shared `~/.rematch/skills/` for that cwd (adds its matching provisional skills, keeps every other live link), links `skills/` here → that directory on the first start, and copies the subagent entries into `agents/` (asks Claude Code to re-scan skills when a link changed). Not on `compact`: the digest (below). Then starts `rematch reflect --pending`, detached |
| `UserPromptSubmit` | `input` with the redirect pre-label (negation, revert, reprompt, interrupt, mark); with an approved `retrieve` hook, the matching units as added context |
| `PreToolUse` | approved `block` / `stuck` hooks: a refusal exits 2 with the hook's message, and is traced |
| `PostToolUse`, `PostToolUseFailure` | `tool_result` |
| `Stop`, `StopFailure` | the run's `turn_end`s (read from the transcript) and `agent_end` |
| `SessionEnd` | `session_shutdown`; a flagged session writes `~/.rematch/pending/cc:<id>.json` |

Every hook goes through `scripts/hook.sh`, which never fails a hook: any error exits 0, and only an intentional block exits 2.

**Takeovers.** As in omp, an `interrupt` is a prompt that reaches Claude while its run is still going: a prompt typed while Claude works is delivered into the run, and its `UserPromptSubmit` comes before that run's `Stop`. After Esc no hook fires, but Claude Code writes `[Request interrupted by user]` to the transcript; the next prompt finds it and is an ordinary prompt, since the run had ended (its words still get the negation/revert/reprompt pre-labels).

**Reflection.** Each session start runs `rematch reflect --pending` in the background, as omp does: its own process group, no output, never waited for, one run at a time (the binary's `pending/.lock`). It runs only when omp is found; the model calls happen in that background process, never inside the Claude Code session.

**Digest.** At session start (not after compaction) the hook shows what was staged, kept or retired since the last digest (from any harness) and the approvals waiting, each with its commands:

```
rematch digest
- staged skill run-tests-before-commit in payments: Use when …
- 1 approval(s) pending
Waiting for your approval (Claude Code has no approval dialog; run one of these yourself, or `rematch library approve|decline <id>` in a shell):
- hook stuck-detector: /rematch:rematch-approve <id> · /rematch:rematch-decline <id>
```

The same text, with each approval's reason and diff, goes to Claude as context, so you can ask what a hook does; Claude is told that approving is yours. There is no one-tap approval: Claude Code hooks cannot open a dialog.

**Subagents.** Claude Code reads a plugin's agents only from real files, and rematch's provisional agents are symlinks (`~/.rematch/agents/<name>.md`). So `SessionStart` and `rematch skills sync --harness claude-code` copy every provisional and kept agent entry into `agents/<name>.md` here (git-ignored), in Claude Code's format: `name`, `description` and the body as written; omp tool names mapped (`read` → `Read`, …); an omp model role such as `@smol` dropped (the agent then runs on the session's model). `agents/.rematch` lists the copies: only those are ever rewritten or removed, and an agent file of yours with the same name is left alone. A retired agent's copy goes at the next session start; a new copy is counted from the session after the one that wrote it.

Commands (each runs the binary before the prompt reaches the model and shows its output; those marked * are yours only, the model cannot invoke them):

- `/rematch:rematch-status` — `rematch status`, and the skills and agents synced for this directory (`rematch skills sync --harness claude-code`).
- `/rematch:rematch-why [cc:<id>]` — `rematch library why`.
- `/rematch:rematch-library` — `rematch library ls`: every entry with its status and raw counters.
- `/rematch:rematch-mark correction|preference|scope` * — `rematch mark` for this session.
- `/rematch:rematch-approve <id>` * / `/rematch:rematch-decline <id>` * — `rematch library approve|decline` (ids from the digest).
- `/rematch:rematch-off` * / `/rematch:rematch-on` * — the kill switch (`on` also re-links the skills for this directory).

`create`, `improve`, `run`, `disable`, `rollback` and the rest are shell commands (`rematch --help`). Past Claude Code sessions: `rematch import --history --harness claude-code`, then `rematch reflect --history`.

**Known limits** (the reasons are in DESIGN §18.1 "What Claude Code cannot do"): no `truncate` hooks (no hook rewrites earlier tool results); `retrieve` once per prompt, not before every request, and its units stay in the conversation; no per-turn event (turns are read from the transcript at `Stop`); `stuck` sees completed calls only and not subagents; no approval dialog and no `rematch` tool for the model; agent copies refresh at session start only; reflection needs omp; batch mode (`run`) is omp-only.

## Files

```
.claude-plugin/plugin.json   manifest
hooks/hooks.json             the eight hooks, exec form: sh scripts/hook.sh session-start|trace-event|hook-check
scripts/hook.sh              stdin → rematch <op> --harness claude-code --hook-output; session-start also starts reflect --pending, detached
scripts/rematch.sh           binary lookup ($REMATCH_BIN, target/{release,debug}, PATH); exports CLAUDE_PLUGIN_ROOT
commands/rematch-*.md        the eight commands
skills                       (created at first SessionStart, git-ignored) → ~/.rematch/skills
agents/                      (created at SessionStart when an agent entry exists, git-ignored) copies + .rematch marker
```
