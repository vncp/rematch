# OpenCode 1.19.4: verified facts the adapter relies on

Sources:

- `opencode-ai` and `@opencode-ai/plugin` 1.19.4 from npm (the `.d.ts` files).
- The `sst/opencode` tag `v1.19.4` (commit `148842e`): `packages/opencode/src/{plugin,session,skill,tool,config,cli}`, `packages/core/src/{session/sql.ts,database,global.ts}`.
- Probes of the pinned binary in an isolated `HOME`/`XDG_*` against a scripted local OpenAI-compatible server (no cloud inference).

## Plugin loading

- A config `plugin` entry that is a path (absolute, `./…`, or `file://`) is a *file* plugin. For a directory with `package.json`, OpenCode loads `exports["./server"]`, else `main`. File plugins skip the npm install and the `engines.opencode` gate. `<config dir>/{plugin,plugins}/*.{ts,js}` load too (`config/plugin.ts`).
- A v1 module default-exports `{ id, server }`, and a file plugin must have an `id` (`plugin/shared.ts readV1Plugin/resolvePluginId`). `server(input)` receives `{ client, project, directory, worktree, serverUrl, $ }`. Plugin state is per instance, meaning per directory.
- `config(cfg)` runs at bootstrap before every other service. Mutating `cfg` changes what later `config.get()` calls return; `project/bootstrap.ts` documents this: "Plugin can mutate config so it has to be initialized before anything else". The probe confirmed that `skills.paths` set this way is scanned.
- `event({event})` is called fire-and-forget (`void hook.event(...)`) for every bus event of the instance's directory, as `{id, type, properties}`.
- Trigger hooks (`chat.message`, `tool.execute.before/after`, `command.execute.before`, `experimental.*`) are awaited in order. A rejection in `tool.execute.before` fails the tool. The part becomes `state.status: "error"` with `error` set to the thrown message, and `tool.execute.after` is **not** called. Probed.
- `permission.ask` is declared in the plugin types but never triggered in 1.19.4. The adapter does not use it.

## Events relied on (probed sequences)

- `session.created {info}` (new sessions; `info.parentID` for task sessions) and `session.updated {info}`. A session resumed by another process emits no `session.created`; the plugin falls back to `client.session.get({path:{id}})`.
- `chat.message(input, {message, parts})` fires for every user message created, including a task-tool child's prompt (`input.agent` = the subagent) and command expansions. Injected text parts carry `synthetic: true`.
- `command.execute.before {command, sessionID, arguments}` fires *after* the template's `` !`cmd` `` lines ran and before `chat.message`.
- `message.part.updated {part}`: a tool part passes through `pending` → `running` → `completed`|`error`; text parts stream (deltas also arrive as `message.part.delta`).
- `message.updated {info}` repeats for each message. An assistant message counts as final once `time.completed` is set or `error` (e.g. `MessageAbortedError`) is present. Each model step is its own assistant message, with `tokens {input, output, reasoning, cache{read,write}, total}`, `cost`, `finish` (`tool-calls`, `stop`, …), `modelID` and `providerID`.
- `session.status {busy|idle|retry}`, then `session.idle {sessionID}` at the end of each run, after aborts too. There is no session-end event.
- `bash` with a non-zero exit is `status: "completed"` with `metadata.exit` = the code. The adapter traces it as an error.
- The plugin's `dispose` runs when `opencode run` exits. `opencode serve` has no signal handler: a killed server never disposes, and its segments are settled by `reconcile` at the next start.
- `opencode run "a b"` wraps any argument containing a space in literal quotes before sending it, so that is the text the trace records.

## Skills

- Discovery runs lazily at first use, after plugin init: `~/.claude/skills`, `~/.agents/skills`, the project `.claude`/`.agents` directories, config dirs `{skill,skills}/**/SKILL.md`, then `cfg.skills.paths` (`**/SKILL.md`, symlinks followed), then `skills.urls`. On duplicate names the later one wins. The native tool is `skill({name})`; its result metadata carries `{name, dir}`. Probed: a skill under the added `~/.rematch/skills` was loaded through it.

## Commands

- `cfg.command[name] = {template, description, agent?, model?, subtask?}`. `$ARGUMENTS` and `$1…` are substituted, then each `` !`cmd` `` is replaced with its shell output (`Process.text`, process cwd), and the result is sent as an ordinary user message. No tool lets the model run a command.

## Transform

- `experimental.chat.messages.transform({}, {messages})` runs before every model request on messages freshly loaded from the store, so replacing parts changes only the outgoing request. The adapter runs approved `truncate` rules there.
- `experimental.chat.system.transform({sessionID, model}, {system})` runs in `session/llm/request.ts prepare` before every LLM request (title/small-model requests included). It receives the system array that becomes that request's system prompt, and OpenCode joins any pushed strings into `system[1]`. That array is not persisted. `agent/agent.ts` also calls it without `sessionID`, and the adapter skips those calls. The adapter runs approved `retrieve` rules there: it pushes one system string with the units and records `retrieval` in the trace, so no synthetic user message is created. Probed: the scripted provider received the units in the system message of every request of the run.

## Custom tools

- Plugin `tool` entries whose args are not Zod schemas are given to the model as a JSON Schema object (`tool/registry.ts legacyJsonSchema`), with no host-side validation. The `rematch` tool validates its own input.

## Storage (history import)

- One SQLite store per machine: `$OPENCODE_DB` (absolute, or relative to the data dir), else `<data>/opencode.db` for the latest/npm channel (dev channels use `opencode-<channel>.db`). `<data>` = `$XDG_DATA_HOME/opencode`, default `~/.local/share/opencode`, on every platform (`xdg-basedir`).
- Tables read:
  - `session(id, parent_id, directory, version, title, time_created, time_updated)`
  - `message(id, session_id, time_created, data)` — `data` = v1 info without `id`/`sessionID`
  - `part(id, message_id, session_id, data)`
- The store is opened read-only with a 5 s busy timeout. A database without these columns is refused. Pre-SQLite JSON `storage/` trees are not read.

## Capability matrix

| Capability | Status |
|---|---|
| Plugin load from checkout path | verified in the real runtime |
| Shared skills via `skills.paths` + native `skill` tool | verified in the real runtime |
| Live trace: prompt, tool success/failure/thrown/rejected, turns with model+usage, run end | verified in the real runtime |
| Approved `block` (thrown reason becomes tool error) | verified in the real runtime |
| Approved `stuck` | binary e2e test (`hook-check`) |
| Model approval paths closed: the read-only `rematch` tool has no approve operation, and a pattern guard refuses direct `rematch library approve`/`on`/`off` bash calls. Not a sandbox: arbitrary shell (scripts, `eval`) can bypass the pattern | verified in the real runtime (direct command refused, tool op refused) |
| Segments: dispose, resume in another process → `:r2`, killed `serve` reconciled | verified in the real runtime |
| Idle → new prompt in one process = same segment | verified in the real runtime (`serve`) |
| `/rematch-status` (`!` lines), `/rematch-mark` | verified in the real runtime |
| Digest toast | binary e2e test of the toast text; the TUI display was not exercised (no TUI in automation) |
| `truncate` via messages transform | unit tests only |
| `retrieve` via system transform (attribution through `retrieval`) | verified in the real runtime + binary e2e test |
| Agent rendering, live subagent traces, approval dialog | not supported |
| History import + `reflect --history` | binary e2e test (fixture store); the real store from the smoke run imports |
