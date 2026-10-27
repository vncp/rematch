# Pi extension API: what `adapters/pi` relies on

Written against **`@earendil-works/pi-coding-agent` 1.4.0** (npm; repo `earendil-works/pi`; the old name `@mariozechner/pi-coding-agent` stops at 0.73.1), the pinned devDependency of this package. Every claim was read in the installed package (`dist/core/extensions/types.d.ts`, `dist/core/agent-session.js`, `dist/core/skills.js`, `dist/core/resource-loader.js`, `dist/core/session-manager.js`, `pi-agent-core/dist/agent-loop.js`, `docs/*.md`) or observed by `pi-process.test.ts` running stock Pi 1.4.0 under Node 22.23.1 with a scripted local model (no cloud inference). Compared with omp 18.12.6 (`extension/index.ts`, DESIGN §0, §10).

## Loading

| Fact | Consequence |
| --- | --- |
| A package is a dir with `package.json#pi.extensions`; Pi loads `.ts` through jiti under Node or Bun (`docs/extensions.md`) | `index.ts` uses `node:` APIs only (no `Bun` global); spawning is `node:child_process` |
| Relative imports outside the package resolve (observed) | the pure helpers come from `../../extension/index.ts`, not copies; the package works from a checkout |
| `pi install <dir>` writes the dir to `<agent dir>/settings.json` `packages` as a path relative to that dir and loads it in place; local packages are never copied or `npm install`ed (`docs/packages.md`, observed) | install = `pi install <rematch>/adapters/pi`; one run = `pi -e <rematch>/adapters/pi` |
| Host packages are provided to extensions (`@earendil-works/pi-coding-agent`, `pi-ai`, `pi-agent-core`, `pi-tui`, `typebox`); they go in `peerDependencies`, never `dependencies` | `getAgentDir` and `VERSION` are imported at runtime from the host |
| No `pi.logger`, `pi.zod`, `pi.pi.VERSION`, `ctx.agent`, tool `approval`/`loadMode`, `ctx.setTimeout` | debug log to stderr only under `REMATCH_DEBUG`; the tool schema is plain JSON Schema (accepted: `registerTool` only rejects a non-object, observed working) |
| The factory runs again on every runtime replacement (`/reload`, `/new`, `/resume`, `/fork`; `resource-loader.js` `clearExtensionCache` re-evaluates the module) | no state survives in module scope; the trace state is restored from the trace file; once-per-process work (profile sync) is keyed on a `globalThis` symbol |

## Session lifecycle

| Fact | Consequence |
| --- | --- |
| `session_start {reason: startup\|reload\|new\|resume\|fork}` is awaited, then `resources_discover {reason: startup\|reload}` runs (`agent-session.js` `bindExtensions`, `reload`) | `session_start` matches and syncs the cwd (≤ 5 s each), then `resources_discover` returns `skillPaths: [REMATCH_ROOT/skills]` — no settings-registry override as in omp |
| Discovered skill paths merge into the loader until its next reload (`resource-loader.js` `extendResources`); skills dedupe by real path and keep the first of a name (`skills.js` `loadSkills`) | a `~/.agents/skills/rematch` link (the Codex route, which Pi also scans) and the extension's path load each skill once; after `/rematch off` a `/reload` drops the path |
| `session_shutdown {reason: quit\|reload\|new\|resume\|fork}`; the end of `-p` and an RPC stdin EOF emit it (observed: both traces end with `session_shutdown`) | every reason but `reload` finishes the trace (`session_shutdown`, pending file); `reload` leaves it open for the re-bound runtime |
| `--continue`/`--session`/`/resume` reopen a session file with its id; `/fork`/`/clone` write a new file with a new id whose copied entries keep their old timestamps and whose header names `parentSession` (`session-manager.js` `createBranchedSession`) | trace segments (README); the importer treats entries older than the header as the parent's prefix |
| `ctx.reload()` exists on command contexts only ("can deadlock" from lifecycle handlers) | `/rematch approve`, `disable`, `rollback`, `activate`, `on`, `off` reload after success; the reloaded runtime appends a `session_start` only when what loads changed, so a newly approved hook is credited (observed: counted with the hook loaded). Digest approvals (from `session_start`) re-match hooks in place and append the same record; the skill list follows at the next reload |

## Turns, inputs, tools

| Fact | Consequence |
| --- | --- |
| `prompt()` dispatches registered extension commands before the `input` event; `input` fires before `/skill:` and template expansion (`agent-session.js` `prompt`) | `/rematch …` never becomes an `input` record (`/rematch mark` writes a `mark`); `/skill:name args` is traced as typed, `skills_invoked: [name]` |
| `input.source` is `interactive` (TUI, and `-p` — observed), `rpc`, or `extension` (`pi.sendUserMessage`) | `extension` inputs are not the human's and are skipped |
| `input.streamingBehavior` is `steer`/`followUp` exactly when Pi is streaming (`prompt`, `_queueUserInput`); an Esc abort settles the run (`agent_settled {aborted: true}`) and writes a `Command aborted` error result plus an empty `error` assistant message to the session file (observed) | `idle = streamingBehavior !== "steer"`: a steer is `interrupt` (observed); a follow-up waits for the run to end and is idle; the prompt after an abort is idle, pre-labelled by its own words only (observed `none`) |
| `agent_end {messages}` is not final (retries, compaction, queued work); `agent_settled {aborted}` is | `agent_end` records are written at `agent_settled` |
| `turn_end {message, toolResults}` fires after the turn's tool calls | one `turn_end` per assistant message, after its `tool_result`s, as the importer orders them |
| `tool_result` (the after-hook) runs only for calls that executed; unknown tools, invalid arguments and blocked calls become error results with `tool_execution_start {args}` / `tool_execution_end {result, isError}` but no `tool_result` event (`agent-loop.js` `prepareToolCall` "immediate") | every call is traced from `tool_execution_end` with the arguments of its `tool_execution_start`; calls this adapter blocked are `tool_blocked` only; nested calls (`parentToolCallId`, codemode) are not traced |
| `tool_call` returns `{block, reason}`; a throwing handler blocks; the model receives the reason as the error result (observed in the next request) | `block`/`stuck` fail closed, exactly as in omp |
| Tools: `execute(id, params, signal, onUpdate, ctx)`; a result with `isError: true` is an error result (observed) | `rematch` tool errors are visible to the model; `signal` cancels the binary (SIGTERM) |
| `ExecOptions` has no `env` (`core/exec.d.ts`) | the binary is spawned by the adapter so its environment can be set (below) |

## Context and messages

| Fact | Consequence |
| --- | --- |
| `context {messages}` carries the conversation without system messages; the result replaces them for one request | `truncate` rewrites old `toolResult`s; `retrieve` appends one message |
| A `role: "custom"` message is sent to the provider as a **user-role** message (`docs/message-types.md`; observed: the request's injected message has `role: "user"`) | the retrieval message is a hidden `CustomMessage` (`display: false`), Pi's form of omp's developer message; its text names "its SKILL.md" (Pi has no `skill://`) |
| `pi.sendMessage` appends a custom message entry that participates in context; with `triggerTurn: false` while streaming it waits for the end of the turn (`sendCustomMessage`) | `/rematch` output is shown that way and removed from every request by the `context` handler (observed absent from all requests) |
| `ctx.hasUI` is true in TUI and RPC; `ctx.ui.notify/confirm` become `extension_ui_request` records in RPC | digest lines are notifications, approvals confirm dialogs, never context; without UI `/rematch` prints to stderr |

## Environment

| Fact | Consequence |
| --- | --- |
| Pi's agent dir is `$PI_CODING_AGENT_DIR` else `~/.pi/agent`; sessions `$PI_CODING_AGENT_SESSION_DIR` else `<agent dir>/sessions/--<cwd>--/` (`config.js`, `main.js`) | omp reads the same `PI_CODING_AGENT_DIR` (rematch's `paths.rs`). The adapter removes both from the binary's environment and passes `REMATCH_PI_AGENT_DIR` / `REMATCH_PI_SESSION_DIR`, so rematch's omp dir, hooks dir and the omp editor's roles never follow Pi's dir (observed: `doctor` under Pi reports `~/.omp/agent`) |
| `pi -p` with a `--model` its `models.json` lacks exits 1 (observed) | tests always register the scripted provider |

## Not available in Pi

- Subagent definitions: Pi has none (no `agents/` discovery, no `ctx.agent`); rematch `agent` entries are not loaded and never counted for Pi sessions.
- Per-tool approval levels (omp's `approval: read|exec`): the `rematch` tool refuses user-only operations itself, and a `bash` command that plainly invokes them is refused by a pattern check. That is a policy on the rematch interfaces, not a sandbox: the model's shell has the user's permissions.
- A settings registry to add skill dirs at load time: `resources_discover` replaces omp's `skills.customDirectories` override.
