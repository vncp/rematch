# Environment variables

Every variable below is named in the docs or read by the code (file named where the docs are silent). Unset means the default in the second column.

## rematch

| Variable | Default | Effect | Where documented |
|---|---|---|---|
| `REMATCH_ROOT` | `~/.rematch` | the root of all rematch state; shared by every omp profile; one root per environment (home and work are separate libraries) | DESIGN §1; README "Architecture" |
| `REMATCH` | on | `REMATCH=0`: off for one process (the extension registers only the `rematch` tool and `/rematch`; the binary refuses reflect/stage) | DESIGN §1; MANUAL "Turning it off" |
| `REMATCH_BIN` | `target/release/rematch` → `target/debug/rematch` of the checkout → `PATH` | the binary the omp extension and the Claude Code plugin run | extension/README.md; adapters/claude-code/README.md |
| `OMP_BIN` | `omp` on `PATH` | the omp the binary calls for the editor, classifier and batch rollouts; also gates the Claude Code plugin's reflection | DESIGN §14 (`omp.rs`); MANUAL "Troubleshooting" |
| `REMATCH_MACHINE` | the host name | this machine's name in the lake and the profile; fixed in the ledger at first use | MANUAL "The lake"; DESIGN §13, §20 |
| `REMATCH_BELLWETHER` | `bellwether` on `PATH` | the Bellwether binary for `lake index` / `lake query` (`--bellwether` wins) | DESIGN §19.2 |
| `REMATCH_NO_LAKE` | `0` | `1`: `scripts/install.sh` builds with `--no-default-features` (no lake) | MANUAL "The lake"; scripts/install.sh |
| `REMATCH_IMPROVER` | embedded files | a directory to take the improver files from | DESIGN §14 (`app.rs`) |
| `REMATCH_EDITOR_TIMEOUT` | 180 s | seconds per model call | not in the docs; `crates/rematch/src/editor.rs` (default: DESIGN §9) |
| `CLAUDE_BIN` | `claude` on `PATH` | the Claude Code binary `doctor` runs `claude plugin validate` with | DESIGN §2 (`doctor`) |
| `LAUNCHCTL_BIN` | `launchctl` | the launchctl `doctor` checks the profile's launchd job with | DESIGN §2 (`doctor`) |
| `OMP_PLUGINS_DIR` | `<config root>/plugins` | where `doctor` and `improve` look for omp plugins | not in the docs; `crates/rematch/src/{doctor,improve}.rs` |

## omp profile resolution

rematch resolves the omp agent dir exactly as omp does; `~/.rematch` stays shared unless `REMATCH_ROOT` is set (DESIGN §0, §1; MANUAL "Troubleshooting").

| Variable | Effect |
|---|---|
| `PI_CONFIG_DIR` | replaces `~/.omp` as the config root |
| `OMP_PROFILE` | selects `<config root>/profiles/<name>/agent`; when defined, even empty or `default`, it suppresses `PI_PROFILE` |
| `PI_PROFILE` | the same, when `OMP_PROFILE` is not defined |
| `PI_CODING_AGENT_DIR` | the agent dir in the default profile (ignored for a named profile) |

## Harnesses

| Variable | Effect | Where documented |
|---|---|---|
| `CLAUDE_PLUGIN_ROOT` | the Claude Code plugin dir `skills sync --harness claude-code` links and copies into; else `<CLAUDE_CONFIG_DIR or ~/.claude>/skills/rematch` | DESIGN §2, §18.1 |
| `CLAUDE_CONFIG_DIR` | relocates `~/.claude` (transcripts for `import --history --harness claude-code`) | DESIGN §18.1 |
| `CLAUDE_CODE_SESSIONEND_HOOKS_TIMEOUT_MS` | raises Claude Code's 1.5 s `SessionEnd` budget (e.g. `5000`) | MANUAL "Claude Code" |
| `CODEX_HOME` | Codex's home (default `~/.codex`); history is read from `$CODEX_HOME/sessions` | DESIGN §18.2.1 |

## Set by rematch for child processes

| Variable | Set for | Where documented |
|---|---|---|
| `PI_OFFLINE=1`, `PI_SKIP_VERSION_CHECK=1`, `PI_TELEMETRY=0` | every `omp` the binary runs | DESIGN §9, §14 |
| `REMATCH=0`, `REMATCH_SEED` | batch rollouts | DESIGN §17 |
| `REMATCH_TASK`, `REMATCH_WORKTREE`, `REMATCH_REPO` | batch verifiers | DESIGN §17 |
| `REMATCH_JOB_ID` | detached jobs (stripped from their children) | DESIGN §14 |
| `REMATCH_ROLE`, `REMATCH_PROMPT` | a scripted `--editor <cmd>` (tests) | DESIGN §14 |

---

Source: docs/DESIGN.md §0, §1, §9, §13, §14, §17, §18, §19.2, §20; moved from docs/MANUAL.md ("The lake", "Claude Code", "Troubleshooting"); extension/README.md; adapters/claude-code/README.md; scripts/install.sh; `crates/rematch/src/{editor,doctor,improve}.rs` for the two variables the docs do not name.
