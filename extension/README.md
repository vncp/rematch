# rematch extension

Plain TypeScript; omp loads `index.ts` with no build step (`package.json#omp.extensions`).
Install: `omp plugin link <repo>/extension` (see `scripts/install.sh`). Contract: `docs/DESIGN.md` §3 and §10.

What it does:

- **Factory**: resolves the binary (`$REMATCH_BIN` → `../target/release/rematch` → `../target/debug/rematch` → `PATH`; `rematch.exe` on Windows),
  runs `rematch match --cwd <cwd>` and `rematch skills sync --cwd <cwd> --harness omp` in parallel (5 s each), adds the
  sync's `dirs` (the shared `<root>/skills`, DESIGN §16 "Shared skills") to `skills.customDirectories` (runtime
  override, `[...configured, ...dirs]`), and imports the hook modules `match` returns. Any failure is logged through
  `pi.logger` and the session continues without that part.
- **Trace**: main sessions only (`ctx.agent.kind !== "sub"`) append to `<root>/traces/<session_id>.jsonl`
  (`session_start`, `input`, `tool_result`, `tool_blocked`, `retrieval`, `turn_end`, `model_change`, `agent_end`, `mark`,
  `session_shutdown`). A flagged session (prelabel ≠ none, ≥3 errors in 6 consecutive tool results, or a mark)
  gets `<root>/pending/<session_id>.json` at shutdown. `session_switch`/`session_branch` rotate the trace.
- **Session start**: spawns `rematch reflect --pending` detached, then runs `digest`: no toasts; the footer status
  (`◆ rematch ✓ cargo 0.4s · 6 on trial · ● 1 needs you alt+e`), a needs-you line above the editor while an action
  waits, and one `rematch.digest` card when something changed since the last digest (shown, stripped from every
  request by `context`). `alt+e` / `/rematch review` open the review panel (`ctx.ui.custom`, overlay): Needs you
  (`a` approve, `d` decline with the action's bound argv, `i` full diff) and Applied (`u` undo, `y` confirms);
  outside the TUI one dialog per action.
- **Look** (`ui.ts`, shared with Pi): every surface carries `◆ rematch` in the host theme's semantic colors, widths
  measured with `Bun.stringWidth`, no line wider than the terminal; names, never ids or hashes (expanded views and
  `inspect` excepted). Renderers: the check card, the digest card, `/rematch` output (grouped tables for
  library/status/digest, colored lines otherwise, ≤12 rows collapsed), and the `rematch` tool's call/result.
- **fast-check**: a successful edit of a main session awaits a debounced `rematch check` of every file edited since
  the last input, then sends ONE displayed message (`com.vncp.rematch.check`, `deliverAs: "aside"`): the loop injects
  it before the next request and the transcript renders it as the check card, so the agent gets each run's
  diagnostics once and no display copy exists. A later edit or input supersedes a running check.
- **Hooks** (modules are data: `{id, mode, tool, field, regex, message, pass_condition, params}`; DESIGN §10):
  `block` blocks a `tool_call` whose `input[field]` matches; `stuck` blocks a call identical to the `repeats`
  calls right before it (both fail closed); on `context`, `truncate` trims old tool-result text in the outgoing copy
  and `retrieve` appends one hidden `custom` message with matching package units from `rematch retrieve` (both fail open).
  Approving (`library approve`, the digest), `activate`, `disable` and `rollback` re-run `match` and `skills sync` and swap
  the live hook sets, as do a session switch/branch and a changed `ctx.cwd`, so an approved hook runs at once (skill links
  added by the re-sync reach omp's skill list at its next start). Tests: `bun test extension`.
- **Entry points**: tool `rematch {operation, arguments[]}` and `/rematch <sub> [args]`
  (`why|status|library|create|improve|run|activate|mark|off|on|disable|rollback|approve|decline|ship|transfer|digest|doctor|uninstall|builtins|anchor`).
  Approving, declining, pinning, disabling or rolling back entries, approving exports, marking, `off`/`on`,
  uninstalling and `policy scope set|unset` are reserved to the user (command, digest or shell): the tool refuses them, judged on the argv the binary
  would run (global `--text`/`--cwd` anywhere), and a `tool_call` guard refuses a `bash` call that runs one directly
  (same parser; a guard on the direct command, not a sandbox). `/rematch approve|decline <id>` typed without
  `--sha` opens that action's review dialog, whose Approve/Decline carry the reviewed snapshot hash.

Root is `$REMATCH_ROOT` or `~/.rematch`. Kill switch: `REMATCH=0` (only the tool and `/rematch` register)
or `<root>/off` (handlers stay registered but inert, so `/rematch on` resumes tracing immediately; skills apply from the
next omp start).
