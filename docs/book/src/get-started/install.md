# Install

## Harnesses

The binary, the ledger, lint, the counters and the entries are the same for every harness; an adapter only forwards what its harness exposes ([`docs/DESIGN.md`](../design/design.md) §18). Session ids carry the harness (`cc:<uuid>`, `codex:<id>`; omp's are bare), so one ledger holds all of them. omp is the reference; Claude Code runs the loop live through a plugin of hooks ([`adapters/claude-code`](https://github.com/vncp/rematch/blob/main/adapters/claude-code/README.md)); Codex is read after the fact ([`adapters/codex`](https://github.com/vncp/rematch/blob/main/adapters/codex/README.md)).

| Capability | omp | Claude Code | Codex |
|---|---|---|---|
| Trace: inputs, tool results, errors, turns, model | ✓ | ✓ (turns and model read from the transcript at `Stop`) | partial (after the fact, from `~/.codex/sessions` at import) |
| Redirect pre-labels + `mark` | ✓ | ✓ (`/rematch:rematch-mark`) | partial (at import; `rematch mark --session codex:<id>`) |
| Takeover (input while the agent runs) | ✓ | ✓ (a prompt delivered into the running turn; an Esc abort read from the transcript) | partial (at import: steers, prompts after an aborted turn) |
| Flag + pending reflection | ✓ | ✓ (at `SessionEnd`, 1.5 s budget) | partial (`reflect --history`) |
| Provisional skill loading | ✓ | ✓ (plugin `skills/` → the shared dir, synced at `SessionStart`) | partial (shared dir; `skills sync` by hand; visible in every session) |
| Kept skill loading | ✓ | ✓ | ✓ |
| Loaded entries for counting | ✓ | ✓ | partial (reconstructed at import from the rollout's skill list) |
| Subagent entries | ✓ | ✓ (copied into the plugin's `agents/` at `SessionStart`) | — |
| `block` hooks | ✓ | ✓ (`PreToolUse`, exit 2) | — |
| `stuck` hooks | ✓ | partial (completed calls only; not in subagents) | — |
| `retrieve` (unit retrieval) | ✓ (before every request) | partial (once per prompt, when the units change) | — |
| `truncate` (tool-output truncation) | ✓ | — (no hook rewrites earlier tool results) | — |
| Digest + one-tap approvals | ✓ | partial (digest at `SessionStart`; approve with `/rematch:rematch-approve <id>`, no dialog) | partial (`$rematch-status` skill) |
| `/rematch` commands | ✓ (all, plus the `rematch` tool) | partial (status, why, mark, library, approve, decline, off, on; create/improve in a shell) | partial (`$rematch-status` skill) |
| Reflection trigger (`reflect --pending`) | ✓ (spawned at session start) | ✓ (spawned detached at `SessionStart`; needs omp for the model calls) | — (`rematch reflect --history`) |
| Batch mode (`run`) | ✓ | — | — (plan: DESIGN §18.2.4) |
| History import | ✓ | ✓ | ✓ |

Install, one line each (details in each adapter's README):

- **omp:** `scripts/install.sh` (build, `omp plugin link extension/`, `~/.local/bin/rematch`).
- **Claude Code:** `ln -s <repo>/adapters/claude-code ~/.claude/skills/rematch` (or `claude --plugin-dir <repo>/adapters/claude-code` for one session).
- **Codex:** `ln -s ~/.rematch/skills ~/.agents/skills/rematch`, then `rematch skills sync --harness codex --cwd <repo>` before starting Codex.

One directory, `~/.rematch/skills/`, delivers skills to all three harnesses: it links every kept entry and every provisional entry whose predicate matched a directory that was synced, so loading is the union of the repos you work in while counting stays with each session's own predicate ([Where skills load from](../concepts/entries.md#where-skills-load-from), DESIGN §16 "Shared skills"). No model call ever runs inside a Claude Code or Codex session (Claude Code's reflection is a detached background process). Why each Claude Code gap cannot close: DESIGN §18.1 "What Claude Code cannot do".

## omp

From a checkout of this repo, with omp 18.12 and a Rust toolchain:

```sh
scripts/install.sh            # cargo build --release · omp plugin link extension/ · ~/.local/bin/rematch
rematch import --history      # seed the ledger from your existing omp sessions (read-only)
rematch doctor --text         # omp version, resolved editor/classifier roles, extension linked, ledger schema
```

Then use omp. The first proposal arrives after the first session that ends with a redirect.

Optional, in `~/.omp/agent/config.yml`: `modelRoles.rematch-editor` for the strongest model you have and `modelRoles.rematch-classifier` for a fast one. Without them the editor falls back to `slow` → `default`, the classifier to `smol` → `tiny` → `default`.

`scripts/install.sh` runs `cargo build --release` (`--locked` when `Cargo.lock` exists), links `extension/agents` → `$REMATCH_ROOT/agents` (default `~/.rematch/agents`, for provisional subagents), runs `omp plugin link extension --scope user`, writes the shim `~/.local/bin/rematch` (which execs `target/release/rematch` of the checkout), runs `rematch doctor`, and prints the optional model roles. `REMATCH_NO_LAKE=1 scripts/install.sh` builds without the cargo feature `lake` (no arrow/parquet); every `rematch lake` subcommand except `migrate` then answers "built without the lake feature".

## Claude Code

The same binary, ledger and entries work in Claude Code through the plugin in `adapters/claude-code` (details and what it cannot do: [`DESIGN.md`](../design/design.md) §18.1).

**Install** (nothing is installed for you; Claude Code 2.1.285 or later, `rematch` built with `cargo build --release` or on `PATH`, or `REMATCH_BIN` set):

```sh
claude --plugin-dir /path/to/rematch/adapters/claude-code          # this session only
ln -s /path/to/rematch/adapters/claude-code ~/.claude/skills/rematch # every session (loads as rematch@skills-dir)
```

Then work as usual. Each session is traced to `~/.rematch/traces/cc:<session id>.jsonl` by the plugin's hooks; a session with a redirect, a tool-error cluster or a mark is flagged when it ends. A prompt you send while Claude is still working counts as a takeover (an interrupt); a prompt after Esc does not, the run had ended.

At the next session start, as in omp, the plugin runs `rematch reflect --pending` in the background (it needs omp on `PATH` or `OMP_BIN`, which makes the model calls; without omp, run `rematch reflect --pending` where omp is installed) and shows the digest: what was staged, kept or retired since the last digest, and every approval waiting, each with the command that grants it. There is no approval dialog in Claude Code: run `/rematch:rematch-approve <id>` (or `/rematch:rematch-decline <id>`, or `rematch library approve|decline <id>` in a terminal). Claude sees the digest too, with each hook's diff, so you can ask it what an approval does; it is told not to approve for you.

| In Claude Code | Same as |
|---|---|
| `/rematch:rematch-status` | `rematch status` + the skills and agents synced for this directory (`rematch skills sync --harness claude-code`) |
| `/rematch:rematch-why [cc:<id>]` | `rematch library why` (default: the latest session here) |
| `/rematch:rematch-library` | `/rematch library` in omp: every entry with its raw counters |
| `/rematch:rematch-mark correction` | `/rematch mark correction` in omp: overrides the classifier for the last turn of this session |
| `/rematch:rematch-approve <id>` · `/rematch:rematch-decline <id>` | the digest's approve/decline dialog in omp |
| `/rematch:rematch-off` · `/rematch:rematch-on` | `/rematch off` · `/rematch on` |

Only you can run `mark`, `approve`, `decline`, `off` and `on` (Claude cannot invoke them). Everything else (`create`, `improve`, `run`, `disable`, `rollback`, …) is the `rematch` CLI in a terminal.

Provisional and kept skills appear as `rematch:<name>` skills through the shared skills directory (see [Where skills load from](../concepts/entries.md#where-skills-load-from)): at session start the plugin syncs `~/.rematch/skills/` for the session's directory and points its own `skills/` there (created on first start; `rematch skills sync --harness claude-code --cwd <dir>` refreshes it by hand). Subagent entries appear as `rematch:<name>` agents: Claude Code reads plugin agents only from real files, so the same sync copies every provisional and kept agent into the plugin's `agents/` (an omp model role such as `@smol` is dropped, so the agent uses the session's model; a copy written at a session's start is counted from the next session; a file of yours there is never touched). Approved `block` and `stuck` hooks refuse tool calls; an approved `retrieve` hook adds matching units to your prompt; `truncate` hooks do not run in Claude Code. `rematch off` stops tracing and empties the shared skills directory at once. Upgrading from a version that linked skills into `~/.rematch/claude-skills/`: the first session start re-points the plugin's `skills/` and deletes that directory. To remove the plugin, delete the `~/.claude/skills/rematch` link; `rematch uninstall` removes `~/.rematch` and the agent copies.

Raise `SessionEnd`'s 1.5 s budget if large sessions end unflagged: `CLAUDE_CODE_SESSIONEND_HOOKS_TIMEOUT_MS=5000 claude`.

`~/.claude/plugins/` is Claude Code's marketplace cache and settings only list `enabledPlugins` by `name@marketplace`; a marketplace install would copy the plugin and drop the `skills/` symlink, so use one of the two routes above. A `--plugin-dir` copy named `rematch` takes precedence over the `skills-dir` one for that session. Check the load with `claude --plugin-dir /path/to/rematch/adapters/claude-code plugin details rematch`: 8 hooks, under Skills the eight commands plus every linked skill, under Agents the rematch subagents.

To remove: delete the `~/.claude/skills/rematch` link (or stop passing `--plugin-dir`). `rematch uninstall` removes `~/.rematch` and the agent copies in `agents/`.

## Codex

Codex (CLI, IDE extension, Codex in the ChatGPT desktop app) gets the same entries and counters through its own files: rematch reads its session rollouts afterwards and puts skills where Codex looks for them. Nothing runs inside a Codex session; there are no hooks, no retrieval or truncation, and no live trace. Install steps and the full limitation list: [`adapters/codex/README.md`](https://github.com/vncp/rematch/blob/main/adapters/codex/README.md); format facts and the record mapping: [`DESIGN.md`](../design/design.md) §18.2.

```sh
rematch import --history --harness codex       # ~/.codex/sessions (or $CODEX_HOME/sessions) → the ledger, read-only
rematch reflect --history                      # flagged Codex sessions are reflected like omp ones
ln -s ~/.rematch/skills ~/.agents/skills/rematch          # once: Codex sees rematch's skills (the shared directory)
rematch skills sync --harness codex --cwd "$PWD"          # before starting Codex in a repo: adds the skills that match it
```

- **Ids.** Imported Codex sessions are `codex:<rollout id>`: `rematch library why codex:<id>`, `rematch mark --session codex:<id> --class correction`.
- **What loads.** Codex reads the shared skills directory `~/.rematch/skills/` through the one link above (it scans `~/.agents/skills` recursively and follows directory links): every kept skill, and every provisional skill whose predicate matched a directory you synced, from any harness. Codex loads every skill under its skill roots in every session, so a linked skill is visible everywhere until it is retired; it is counted only where its predicate hits. Upgrading from a version that used `~/.rematch/codex-skills/` and copies marked `.rematch-codex.json`: the first `skills sync --harness codex` re-points your `~/.agents/skills/rematch` link and removes those copies.
- **Counting.** A rollout records the skill list Codex gave the model; the import turns the rematch skills in it into the session's loaded entries. Clean sessions are counted by the next `rematch count` (or `reflect`), flagged ones when `reflect --history` handles them. Import after you worked: a session that was never imported never counts.
- **Reviewer threads** (Codex's auto-review subagents) are imported but never reflected.
- **Optional:** `$rematch-status` (`adapters/codex/skills/rematch-status`, symlinked into `~/.agents/skills/`) runs the sync, the digest and approvals from inside Codex, each command after your approval.

It adds the provisional skills whose predicate matches the repo, keeps every kept skill linked,
and removes links of entries that were retired; links other repos added stay (they are counted
only where they match). It also says when no link reaches the shared directory. To make it
automatic, a shell function in your shell rc does it on every launch:

```sh
codex() { rematch skills sync --harness codex --cwd "$PWD" >/dev/null; command codex "$@"; }

Uninstalling and turning rematch off are in [Day to day](../guides/day-to-day.md#turning-it-off-rolling-back-uninstalling); where each harness reads its skills from is in [Entries and statuses](../concepts/entries.md#where-skills-load-from).

---

Source: README.md ("Harnesses", "Thirty-second start"); scripts/install.sh; moved from docs/MANUAL.md ("Claude Code", "Codex"); adapters/claude-code/README.md ("Install"); adapters/codex/README.md ("Install"); contracts in docs/DESIGN.md §18.1 and §18.2.
