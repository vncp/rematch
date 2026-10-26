# omp-evolve

`omp-evolve` is an `omp` plugin that lets the agent improve its own harness from real sessions: it traces sessions, proposes skills, scripts, rules, agents, and hooks grounded in execution traces, stages them in `~/.omp/evolve/staging/` as `provisional`, and promotes or retires them by observed session counters.

## How You Use It

| Entry point | What you say or do | What happens |
| --- | --- | --- |
| **Nothing** | Work in `omp` as usual | Every session is traced into `~/.omp/evolve/traces/<session>.jsonl`. At the next session start, pending reflections run detached: sessions with a redirect, tool-error cluster, or takeover produce one bounded proposal staged as `provisional`. Later sessions whose trigger predicate matches load it; counters decide whether it stays. |
| **Session-start digest** | Open `omp` (or press `Alt+E`) | Shows one line per entry staged since last time, plus one-tap approval prompts (with diffs) for executable scripts, blocking hooks, capability-floor reviews, or cross-branch transfers. |
| **Create a skill** | `"Use omp-evolve to create a skill for X"` or `/evolve create <name> --from sessions\|task\|description` | Greenfield creation from matching traced sessions, a single reference rollout of a named task, or description-only (`draft` until activated). |
| **Improve a skill** | `"Based on session history, improve <skill> using omp-evolve"` or `/evolve improve <skill>` | Brownfield audit of where the skill loaded, mechanical package split when > 8 KB, and unit-level edits staged beside the existing version. |
| **Batch job with a verifier** | `/evolve run --task <id> --verifier <cmd>` | Runs a detached Harness² loop (reference rollout, invariant + domain candidates, probes, composition, deterministic verifier selection) and returns a `job_id`. |
| **Inspect** | `/evolve why`, `/evolve status`, `/evolve library` | Shows which entries loaded for the last deliverable and why, job progress, and raw counters (`matched`, `recurrence`, `harm`) per entry. |
| **Correct it** | `/evolve mark correction\|preference\|scope` | Overrides the redirect classifier for the last turn. |
| **Stop it** | `/evolve off`, `OMP_EVOLVE=0`, `/evolve disable <id>`, `/evolve rollback <skill>`, `omp-evolve uninstall` | Immediate kill switches, single-entry disable, predecessor rollback, or complete removal of all plugin state. |

## Install & Uninstall

```bash
# Build release binary, link extension into omp user scope, and install ~/.local/bin/omp-evolve shim
./scripts/install.sh

# Optional: import existing omp session history and/or compound-forge skills
omp-evolve import --history

# Uninstall plugin, remove ~/.omp/evolve state, and remove PATH shim
./scripts/uninstall.sh
```

To configure dedicated model roles in `~/.omp/agent/config.yml`:

```yaml
modelRoles:
  evolve-editor: <provider>/<strongest-model-id>
  evolve-classifier: <provider>/<fast-model-id>
```

## Three Entry Points

1. **Interactive passive loop (zero reruns):** Normal sessions are traced by `extension/index.ts`; flagged sessions are reflected on detached at the next `session_start` via a single `--no-tools` editor call, checked by static lint, staged as `provisional`, and evaluated by on-session counters (`Keep` at `matched >= 4, recurrence == 0, harm == 0`; `Retire` at `recurrence >= 2 || harm >= 2`).
2. **Explicit greenfield / brownfield commands (`/evolve create`, `/evolve improve`):** Distill new skills from matching traces or a single task rollout, or split and refine existing skills from traced usage history.
3. **Detached batch mode (`/evolve run --task <id> --verifier <cmd>`):** Multi-candidate exploration gated by a deterministic test or check command in isolated git worktrees.

## CLI Reference (`omp-evolve`)

Every subcommand prints a single JSON envelope on stdout (`{"ok":true,"operation":"<op>","data":{...}}` or `{"ok":false,"operation":"<op>","error":"..."}`), or human-readable output when passed `--text`.

| Subcommand | Description |
| --- | --- |
| `doctor` | Check `omp` binary+version, resolved roles (`provider/model`), extension link status, ledger schema version, root paths, and kill-switch state |
| `import --history [--sessions <dir>] [--forge <path>]` | Import historical `omp` session JSONL into the ledger and optionally register `compound-forge` skills/learnings as `provisional` |
| `reflect --pending [--session <id>] [--editor <cmd>]` | Fold finished traces into counters/decisions, then run one reflection per pending flagged session respecting rate limits |
| `match --cwd <dir> [--inputs <json>]` | Evaluate deterministic predicates for `<dir>`; returns `{branch, repo, skill_dirs, entries, hooks, off}` |
| `digest --cwd <dir>` | Return session-start summary lines, pending script/hook approvals with diffs, capability-floor reviews, and transfer suggestions |
| `record --session <id> --event <json>` | Record an explicit event (such as `/evolve mark`) against a session |
| `create <name> --from sessions\|task\|description [--task <id>] [--branch <b>] [--description <text>] [--files <p>...]` | Create a greenfield skill package from matching sessions, a task rollout, or description (`draft`) |
| `improve <skill> [--split-only] [--branch <b>]` | Register, audit, mechanically split (> 8 KB), or propose unit-level improvements for an existing skill |
| `activate <name> [--task <id> \| --unverified]` | Promote a `draft` entry to `provisional` via a reference task run or explicit `--unverified` acceptance |
| `run --task <id> --verifier <cmd> [--width k] [--seed n]` | Start a detached batch-mode recursion job with a deterministic verifier; returns `{job_id}` |
| `run-worker --job <id>` | Internal detached worker entry point for `run` |
| `status [job]` | Inspect overall evolve status or poll a specific detached `job_id` |
| `library ls \| why [<session_id>] \| disable <id> \| rollback <skill> \| pin <id> \| unpin <id> \| approve <id> \| decline <id> \| export [--to <dir>] \| import-export [--from <dir>]` | Inspect and manage library entries, attributions, approvals, pins, rollbacks, and invariant exports |
| `task add <id> --repo <path> --prompt <text> [--verifier <cmd>] [--max-time 10m] [--tools ...]` | Register a repeatable batch task definition |
| `transfer suggest \| transfer apply <lesson_id> --branch <b>` | Suggest or apply kept `invariant` lessons across branches with overlapping facet structure |
| `ship --scope team\|fork [--repo <path>] [--entry <id>...]` | Prepare a branch/PR with evidence for team (`<repo>/.omp/`) or fork scope |
| `count [--session <id>]` | Fold uncounted finished traces into entry counters and run keep/retire/dormant decisions |
| `mark --session <id> --class correction\|preference\|scope` | Override the redirect classification for the latest turn in `<session>` |
| `off \| on` | Toggle the persistent kill switch (`$EVOLVE_ROOT/off`) |
| `uninstall [--yes]` | Remove all files written under `$EVOLVE_ROOT` and evolve-managed skills/hooks |

## Where State Lives

Root directory: `$OMP_EVOLVE_ROOT` (defaults to `~/.omp/evolve`). Agent directory: `$PI_CODING_AGENT_DIR` (defaults to `~/.omp/agent`).

| Path | Contents |
| --- | --- |
| `~/.omp/evolve/traces/<session_id>.jsonl` | Append-only extension event traces |
| `~/.omp/evolve/pending/<session_id>.json` | Flagged session markers written at shutdown and consumed by `reflect --pending` |
| `~/.omp/evolve/ledger.sqlite` + `blobs/<sha256>` | SQLite ledger (sessions, redirects, entries, counters, proposals, receipts, admissions, jobs, outcomes) + content-addressed blobs |
| `~/.omp/evolve/staging/<branch>/<name>/` | `provisional` entries loaded dynamically via `skills.customDirectories` when their predicate matches |
| `~/.omp/evolve/drafts/<name>/` | Description-only `draft` entries (never loaded until activated) |
| `~/.omp/evolve/archive/<status>/<id>/` | `retired`, `dormant`, and `stale` entries with preserved evidence |
| `~/.omp/evolve/predecessors/<name>/<version>/` | Copies of replaced `kept` entries retained for `/evolve rollback <skill>` |
| `~/.omp/evolve/improver/` | `editor.md`, `reflector.md`, `classifier.md`, `lint.toml`, `omp-no-tools.yml` |
| `~/.omp/evolve/jobs/`, `runs/`, `tasks/`, `branches/`, `export/` | Detached job state/logs, temporary batch worktrees, task definitions, per-branch `domain.yml` facets, and work↔home invariant exports |
| `~/.omp/agent/skills/<name>/` | `kept` skills copied into native agent discovery (`evolve:` frontmatter metadata) |
| `~/.omp/agent/extensions/evolve-hooks/<id>.ts` | Approved compiled `tool_call` verifiers |

## Kill Switches & Safety Controls

- **Environment variable:** Set `OMP_EVOLVE=0` to disable tracing, staging, and reflection for a process.
- **Persistent toggle:** Run `/evolve off` or `omp-evolve off` (creates `~/.omp/evolve/off`); re-enable with `/evolve on` or `omp-evolve on`.
- **Single-entry controls:** `/evolve disable <id>` retires an entry immediately; `/evolve rollback <skill>` restores the previous kept version; `/evolve pin <id>` prevents automatic replacement.
- **Approval gate:** Scripts are staged mode `0644` and hooks remain inert until explicitly approved via the session-start digest or `omp-evolve library approve <id>`.
- **Full removal:** `./scripts/uninstall.sh` or `omp-evolve uninstall --yes` removes every file written by the plugin and nothing else.

## Deviations from SPEC (Verified against `omp` v18.12.6)

1. `resources_discover` → factory-time `skills.customDirectories` override (same semantics, per process).
2. Batch isolation uses a `--config` overlay + fresh worktree instead of a private `PI_CODING_AGENT_DIR` (auth lives in the agent dir).
3. `omp plugin install @<org>/omp-evolve` → `scripts/install.sh` (`cargo build --release`, `omp plugin link extension/`, PATH shim). No registry publish.
4. "Related thing: embeddings, exploratory only" is not implemented: the binary holds no provider credentials; exact signature + MinHash are implemented.
