# rematch extension

Plain TypeScript; omp loads `index.ts` with no build step (`package.json#omp.extensions`).
Install: `omp plugin link <repo>/extension` (see `scripts/install.sh`). Contract: `docs/DESIGN.md` §3 and §10.

What it does:

- **Factory**: resolves the binary (`$REMATCH_BIN` → `../target/release/rematch` → `../target/debug/rematch` → `PATH`),
  runs `rematch match --cwd <cwd>` (5 s), adds the returned `skill_dirs` to `skills.customDirectories`
  (runtime override, `[...configured, ...skill_dirs]`), and imports the returned hook modules. Any failure is logged
  through `pi.logger` and the session continues without staging.
- **Trace**: main sessions only (`ctx.agent.kind !== "sub"`) append to `<root>/traces/<session_id>.jsonl`
  (`session_start`, `input`, `tool_result`, `tool_blocked`, `turn_end`, `model_change`, `agent_end`, `mark`,
  `session_shutdown`). A flagged session (prelabel ≠ none, ≥3 errors in 6 consecutive tool results, or a mark)
  gets `<root>/pending/<session_id>.json` at shutdown. `session_switch`/`session_branch` rotate the trace.
- **Session start**: spawns `rematch reflect --pending` detached, then shows the digest (≤5 lines, then
  `+N more`) and one confirm per approval → `library approve|decline <id>`. `alt+e` shows the digest on demand.
- **Hooks**: `tool_call` runs each loaded hook's `test(input)`; a message blocks the call (a throwing hook blocks too).
- **Entry points**: tool `rematch {operation, arguments[]}` and `/rematch <sub> [args]`
  (`why|status|library|create|improve|run|activate|mark|off|on|disable|rollback|approve|decline|ship|transfer|digest|doctor|uninstall`).
  Approving entries and uninstalling are reserved to the user (command or digest), never the tool.

Root is `$REMATCH_ROOT` or `~/.rematch`. Kill switch: `REMATCH=0` (only the tool and `/rematch` register)
or `<root>/off` (handlers stay registered but inert, so `/rematch on` resumes tracing immediately; staging dirs
apply from the next omp start).
