# rematch for OpenCode

An OpenCode server plugin that gives OpenCode sessions the same rematch loop omp has: same binary, ledger, lint, counters and entries. The plugin forwards OpenCode's hook and event payloads to `rematch trace-event --harness opencode`. All mapping to the trace lives in Rust (`crates/rematch/src/opencode_event.rs`, `rematch_core::import_opencode`). Verified facts and limits: [`API-NOTES.md`](API-NOTES.md).

## Supported versions

OpenCode **1.19.4** (`opencode-ai` and `@opencode-ai/plugin` 1.19.4; the plugin's `engines.opencode` is `>=1.19.4 <2`). The plugin runs inside OpenCode's bundled Bun. Tested against the pinned binary in `devDependencies`.

## Install

Nothing is installed for you. Requirements:

- the `rematch` binary: `$REMATCH_BIN`, else `target/release/rematch` / `target/debug/rematch` of this checkout (`cargo build --release` at the repo root), else `rematch` on `PATH`;
- this checkout, because the plugin imports helpers from `../../extension/index.ts`. The adapter is not published to npm.

Declare the plugin by path in OpenCode's global config, `~/.config/opencode/opencode.json` (`$XDG_CONFIG_HOME/opencode/`), or in a project's `opencode.json`:

```json
{ "plugin": ["/path/to/rematch/adapters/opencode"] }
```

OpenCode resolves a directory with a `package.json` through `exports["./server"]` (`index.ts`). Because the path is a file plugin, OpenCode skips its npm install and compatibility gate. You can also symlink `index.ts` into `~/.config/opencode/plugin/rematch.ts`. `rematch doctor` reports the `opencode` block: which config file declares the plugin, and whether the path resolves to this adapter.

Reflection makes its model calls through omp (`$OMP_BIN`, else `omp` on `PATH`). Without omp, sessions are still traced, flagged and loaded, and reflection waits until `rematch reflect --pending` runs somewhere omp exists.

To remove the plugin, delete the `plugin` entry. `rematch uninstall` removes `~/.rematch`.

## What it does

| OpenCode | rematch |
|---|---|
| plugin init | `load`: syncs `~/.rematch/skills` for the directory and reads which approved hooks apply. `reconcile`: finalizes segments whose process died or that went quiet. Then starts `rematch reflect --pending` detached, when omp is found |
| `config` hook | adds `~/.rematch/skills` to `skills.paths` (in memory only; your config files are never written) and the `/rematch-*` commands, unless you already define one with the same name |
| `chat.message` | `input` with the redirect pre-label. Synthetic parts (file reads, agent mentions, compaction continuations) are not prompts, and neither are command expansions except `/rematch-mark` |
| `message.part.updated` (tool part `completed` / `error`) | `tool_result`. Failures, thrown tools, rejected calls and `bash` with a non-zero exit become `is_error`. A successful `skill` call becomes `skill_read` |
| `message.updated` (assistant, `time.completed` or error) | `turn_end`: model, tokens, cost, finish, text |
| `session.idle` | `agent_end` for the run |
| `tool.execute.before` | approved `block` / `stuck` hooks through `rematch hook-check`. An intentional block throws its message, and OpenCode records that message as the tool's error. A failed check never blocks |
| `experimental.chat.messages.transform` | approved `truncate` hooks, applied to the outgoing request only |
| `experimental.chat.system.transform` | approved `retrieve` hooks: units of the packages this session loaded that match the last prompt and the recent tool paths are appended to that request's system prompt. Nothing is stored in the session and no user input is recorded. A `retrieval` record credits the hook and the packages whenever the unit set changes. Main sessions only; the call has a 3 s bound, is cached until the next prompt or tool result, and fails open |
| `dispose` | finalizes the segments this process owned (`session_shutdown`, plus `pending/oc:<id>.json` when the session is flagged) |

**Sessions and segments.** OpenCode has no session-end event: `session.idle` ends a *run*, and you can continue a session at any time. A new prompt after idle, in the same process, is one more run in the same trace. A segment (`traces/oc:<id>.jsonl`) is finalized only in these cases:

- the last OpenCode process that wrote to it exits (`dispose`);
- a later start finds that every owning process is gone (crash or `kill`);
- it has been quiet for 6 hours with no run going.

If you continue a session after its segment was finalized, the continuation is written to `oc:<id>:r2`, `:r3`, and so on. A finished trace is never appended to, so `count` folds each segment exactly once and loses no resumed events. Subagent (task) sessions are not traced live; `block` rules still apply to their tool calls.

**Approvals are meant to be yours.** The adapter's approval paths are `/rematch-approve <id>` and `/rematch-decline <id>` (commands only you can run), or `rematch library approve|decline` in your own shell. The model's `rematch` tool is read-only and has no approve, decline or on/off operation. A check in `tool.execute.before` also refuses `bash` calls that run your controls directly: `rematch library approve|decline|pin|unpin|disable|rollback`, `library export … --approve`, `rematch on|off|uninstall`. It catches the binary by path or in quotes, in any `;`/`&&`/`|`/`$( )` segment, with `--text`/`--cwd` placed anywhere. That check is a guard against the direct command, not a sandbox: a model allowed to run arbitrary shell commands (a script file, `eval` of a built string, an alias, editing the ledger) can still get around it. Limit OpenCode's `bash` permission if that matters to you.

**Digest.** In the interactive TUI, your first prompt triggers `rematch digest`, and the result appears as a toast (approvals first, each with its commands). Headless commands (`run`, `serve`, …) skip it, so the digest is never consumed where nobody would see it. `/rematch-digest` shows the full digest at any time.

**Commands.** Each command runs the binary while the command expands (`!` lines), then asks the model to show the output:

- `/rematch-status` — status and the skills synced for this directory
- `/rematch-digest`
- `/rematch-why [oc:<id>]`
- `/rematch-library`
- `/rematch-approve <id>`, `/rematch-decline <id>`
- `/rematch-off`, `/rematch-on` — the kill switch
- `/rematch-mark correction|preference|scope` — records the mark on this session

OpenCode exposes no command tool to the model.

**History.** `rematch import --history --harness opencode [--sessions <opencode.db or data dir>]` reads OpenCode's SQLite store, read-only: `$OPENCODE_DB`, else `$XDG_DATA_HOME/opencode/opencode.db`. Then run `rematch reflect --history`. Subagent sessions are imported as `oc:<parent>:agent-<child>`; their prompt was written by the parent agent and is never counted as a correction.

## Not supported

- Subagent and agent-profile rendering. rematch agent entries are not delivered to OpenCode.
- A digest dialog or one-tap approval.
- Live tracing of subagent sessions.
- Batch mode (`run`), which drives omp only.

## Tests

```sh
npm install                      # pinned opencode-ai + @opencode-ai/plugin 1.19.4, local to this dir
bun test test/unit.test.ts       # truncate, approval guard, command templates
cargo build -p rematch && bun test/smoke.ts   # real OpenCode 1.19.4 + this plugin, isolated HOME/XDG, scripted local provider
cargo test -p rematch --test e2e_opencode     # envelopes, segments, hooks, import + reflect
```

## Files

```
package.json             name, exports["./server"] → index.ts, engines.opencode, pinned devDependencies
index.ts                 the plugin (transport only; parsing lives in Rust)
API-NOTES.md             verified OpenCode 1.19.4 facts the adapter relies on
test/unit.test.ts        pure helpers
test/smoke.ts            real-runtime acceptance (no cloud inference)
test/editor-block-hook.sh  scripted editor that stages the smoke test's block hook
```
