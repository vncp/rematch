# rematch for Pi

A [Pi](https://github.com/earendil-works/pi) package (`@earendil-works/pi-coding-agent` 1.4.x) that gives Pi sessions the same rematch loop omp has: the same binary, ledger, lint, counters and shared skills. It runs in Pi's process like the omp extension, whose pure helpers it imports (`../../extension/index.ts`); every decision beyond the cheap pre-labels stays in the `rematch` binary. API facts it relies on: [`API-NOTES.md`](API-NOTES.md). Contract: [`docs/DESIGN.md`](../../docs/DESIGN.md) §3, §18.

## Install

Requirements: Pi 1.4 or later (Node ≥ 22.19 or Bun); the `rematch` binary, found as `$REMATCH_BIN`, else `target/release/rematch` / `target/debug/rematch` of this checkout (`cargo build --release` at the repo root), else `rematch` on `PATH`. Reflection makes its model calls through omp (`$OMP_BIN`, else `omp` on `PATH`); without omp, Pi sessions are still traced, flagged and loaded, and reflection waits for a `rematch reflect --pending` run where omp exists.

Every session (written to `~/.pi/agent/settings.json`; Pi loads a local package from the checkout, it does not copy it):

```sh
pi install /path/to/rematch/adapters/pi
```

One session: `pi -e /path/to/rematch/adapters/pi`. To remove: `pi remove /path/to/rematch/adapters/pi`. `rematch doctor` reports the package under `pi:` (a machine without Pi is "not set up", never a problem).

## What it does

| Pi event | rematch |
| --- | --- |
| `session_start` | matches the cwd (`rematch match`) and syncs the shared `~/.rematch/skills/` for it (`skills sync --harness pi`); opens the trace (below) with `session_start` (`mode: pi:<reason>`, `omp_version: pi <version>`, the versioned `loaded_entries`, `visible_entries`); then, except on `/reload`: starts `rematch reflect --pending` detached (only where omp exists; the binary's `pending/.lock` keeps one run), shows the digest (TUI/RPC), runs the profile sync once per process |
| `resources_discover` | returns the shared skills dir, so Pi lists its skills (`/skill:<name>` works too) |
| `input` | `input` with the redirect pre-label; a takeover (`interrupt`) is a prompt steered into the running loop (`input.streamingBehavior: "steer"`); a follow-up typed meanwhile (Pi delivers it after the run) and the prompt after an Esc abort are ordinary idle prompts (the abort shows as the run's `agent_end` with `yielded: false`); `pi.sendUserMessage` inputs (`source: extension`) are skipped; `/skill:` and `/rematch mark` count, other slash, `!`, `%` lines do not |
| `tool_call` | approved `block` / `stuck` hooks (fail closed), traced `tool_blocked`; a `bash` command line that plainly invokes `rematch library approve\|decline\|pin\|unpin\|disable\|rollback`, `rematch library export … --approve[-all]` or `rematch off\|on\|uninstall` is refused (a pattern check on `rematch` in command position, flags before or after `library` included — not a sandbox: a shell can reach the binary in ways no pattern sees) |
| `tool_execution_start` / `_end` | `tool_result` for every finished call, failed ones included (`skill_read` only for a successful read of a `SKILL.md`) |
| `context` | approved `truncate` and `retrieve` hooks (the first retrieve hook runs and is the one credited in the `retrieval` record); the retrieved units go as one hidden custom message (a user-role message to the provider); `/rematch` output is removed from every request |
| `turn_end`, `model_select` | `turn_end` (model, usage, stop reason, text), `model_change` |
| `agent_settled` | `agent_end` (Pi's `agent_end` is not final: retries and queued work continue the run) |
| `session_shutdown` | `session_shutdown` and, when flagged, `~/.rematch/pending/<trace id>.json` — except on `/reload`, which continues the trace |

**Trace identity and segments.** A Pi session `<id>` traces to `~/.rematch/traces/pi:<id>.jsonl`. Opening a session whose trace exists:

- the trace is unfinished and the runtime was reloaded (`/reload`, or an approval or `/rematch on|off` re-binding it): the trace continues, its state (seq, last input, deliverable, flag reasons, error window, stuck call history, changed files) read back from the file; a `session_start` is appended only when what loads changed (an approved hook, an activated or new-version entry), so the session is credited for what it ran from then on and is still finalized once;
- the trace is unfinished otherwise (a crash, a second process on the same session): one more `session_start` is appended, so `count` waits for this run and counts the trace once, whole;
- the trace is finished (`--continue`, `--session`, `/resume` of a closed session): a new segment `pi:<id>:r<n>` starts with its own `session_start` and the entries loaded now. A segment that `count` already folded is never written again, and what the resumed run does still counts.

`/fork` and `/clone` give a new session id, so a new trace. `/rematch mark` and pending files name the current segment.

**The `rematch` tool and `/rematch`.** The model gets the `rematch` tool (`doctor`, `status`, `library ls|why`, `create`, `improve`, `activate`, `run`, …); approving, declining, pinning, disabling, rolling back, approving exports (`library export --approve[-all]`), marking, `off`/`on` and `uninstall` are refused to it — they are the user's, through `/rematch <sub>` (`why|status|library|create|improve|run|activate|mark|off|on|disable|rollback|approve|decline|ship|transfer|digest|doctor|uninstall|builtins|anchor`). That is the tool and command policy; the model's shell runs with the user's permissions, so it is a boundary of the rematch interfaces, not isolation. The tool's abort signal kills the binary. After a successful `/rematch approve|disable|rollback|activate|on|off` the command reloads Pi's runtime, so hooks and the skill list change in this session. Output is shown in the transcript (stderr in `-p`) and never sent to the model.

**Digest.** At session start (TUI or RPC) the digest lines arrive as notifications and each approval as a confirm dialog with its reason and diff; nothing of it enters the model's context. An approval there applies its hook at once and appends a `session_start` recording it (an `activate` through the tool does the same); its skill links join the list at the next `/reload` or session.

**Off.** `/rematch off` closes the trace segment, empties the shared dir, and reloads: no handler traces or runs hooks and no skill path is discovered until `/rematch on`, which reloads into a new segment. `REMATCH=0` in the environment keeps the package inert except `/rematch`.

**Paths.** omp reads the same `PI_CODING_AGENT_DIR` Pi does. The binaries the package runs get Pi's dirs as `REMATCH_PI_AGENT_DIR` / `REMATCH_PI_SESSION_DIR` instead, so rematch's omp agent dir, hooks dir and the omp editor's roles never follow Pi's.

## History

`rematch import --history --harness pi` (and the default `--harness all`) reads Pi's session files: `$PI_CODING_AGENT_SESSION_DIR`, else `$PI_CODING_AGENT_DIR/sessions`, else `~/.pi/agent/sessions` (a value inside omp's `~/.omp` is omp's and is ignored; with `--harness all` a Pi dir that is an omp sessions dir is skipped). Each session's active branch becomes the trace the package would have written (`rematch_core::import_pi`): `pi:<id>`, the rematch skills its `<available_skills>` offered as loaded entries, `/skill:` expansions back to `/skill:<name> <args>`, a message right after a tool result inside a run (a steer) as `interrupt` — not after an aborted result, a compaction, a branch summary, an extension message or a `!` shell run, and a fork's copied prefix as context only. Sessions with a live trace are kept as traced; `reflect --history`, `improve` and `count` read Pi sessions by their `pi:` id.

## Not available in Pi

- **Subagents.** Pi has no subagent definitions: rematch `agent` entries do not load in Pi and are not counted for Pi sessions.
- **Batch mode** (`rematch run`) drives omp rollouts only.
- The import cannot tell an extension's `sendUserMessage` from a typed prompt (the session file records no source), sees blocked calls as failed results, and has no `loaded_entries` for hooks (it cannot know they ran).

## Tests

`bun install`, then `bun test` here: `index.test.ts` (pure helpers) and `pi-process.test.ts`, which runs stock Pi 1.4.0 under Node against the built binary (`cargo build -p rematch` first) and a scripted local model, in temporary HOME / agent / REMATCH_ROOT dirs.
