<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/readme/wordmark-dark.svg">
    <img alt="rematch" src="docs/assets/readme/wordmark-light.svg" width="360">
  </picture>
</p>

<p align="center"><b>Stop teaching your coding agent the same lesson every session.</b></p>

rematch turns corrections from your sessions into reusable skills, personal instructions
and guardrails, then tracks whether the entries are used cleanly or whether the same
problems recur. It changes the agent's harness—not model weights—and does not guarantee improvement.

<p align="center">
  <a href="#quick-start">Start</a> ·
  <a href="#harness-support">Harnesses</a> ·
  <a href="#everyday-commands">Commands</a> ·
  <a href="https://vncp.github.io/rematch/">Docs</a>
</p>

## Quick start

You need a Rust toolchain and **omp with working model access**. omp is the runtime
for rematch's editor and classifier calls, even when you work in Claude Code or
reflect on Codex history. The omp extension targets omp 18.12.

From a checkout of this repository:

```sh
scripts/install.sh
export PATH="$HOME/.local/bin:$PATH"
rematch doctor --text
```

The installer builds the binary, links the omp extension and puts a `rematch` shim
in `~/.local/bin`. Keep the checkout: the shim points to its release binary.
Then start omp and work normally. A correction can flag a session for reflection
in the background at the next session start; a proposal is not guaranteed.

On Windows, turn on Developer Mode first (Settings → System → For developers):
rematch's library is made of symlinks, which Windows lets a normal user create only
there. Then, in PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install.ps1
```

It does the same, with the shim `~\.local\bin\rematch.exe` (a symlink to the release
binary) and that directory added to your user `PATH`.

Optionally seed the ledger from existing omp sessions:

```sh
rematch import --history --harness omp
```

Model roles come from your omp configuration: `rematch-editor` falls back to
`slow` → `default`; `rematch-classifier` to `smol` → `tiny` → `default`.
See [installation](https://vncp.github.io/rematch/start/install/) and
[model roles](https://vncp.github.io/rematch/reference/configuration/model-roles/) for setup details.

## Harness support

The adapters share one binary, library and counters. Their integration is not identical:

| Harness | Workflow | Main limitation |
|---|---|---|
| **omp** | Extension traces live sessions, syncs a per-place skill view and starts background reflection. | Reference integration; model access must be configured in omp. |
| **Claude Code** | Plugin hooks trace sessions, sync kept skills and trial context, and start detached reflection through omp. | No output truncation or approval dialog; retrieval is once per prompt, and batch runs still use omp. |
| **Pi** | Native extension traces sessions, discovers the per-place skill view, and runs approved block, stuck, retrieval and truncation hooks. | Tested with Pi 1.4.0; no native subagent definitions. Reflection still uses omp. |
| **OpenCode** | Server plugin traces main sessions, exposes the per-place skill view and enforces approved runtime rules; native SQLite history can also be imported. | Tested with OpenCode 1.19.4, not v2. No generated agent profiles or approval dialog; reflection still uses omp. |
| **Codex** | Import session rollouts, reflect from a shell, then sync skills before launching Codex. | Post-hoc, skills-only integration: no live tracing, runtime hooks, personal instructions or subagent entries. |

For one Claude Code session, from this checkout after installing rematch:

```sh
claude --plugin-dir "$PWD/adapters/claude-code"
```

For Pi, run `pi install "$PWD/adapters/pi"` from this checkout. For OpenCode,
add the absolute path to `adapters/opencode` to your config's `plugin` array.
See the [Pi adapter](adapters/pi/README.md), [OpenCode adapter](adapters/opencode/README.md),
[Claude Code adapter](adapters/claude-code/README.md), [Codex setup](adapters/codex/README.md),
and the [full support matrix](https://vncp.github.io/rematch/reference/harnesses/) for installation and limits.

## The learning loop

1. **Record the problem.** Session traces supply the evidence. Corrections, tool-error
   clusters and takeovers can trigger reflection; clean sessions do not generate
   self-congratulatory proposals. ([D1, D9](docs/research.md#2-decisions-and-their-evidence))
2. **Propose a small change.** In a detached background process, a scoped editor proposes from the trace.
   Static lint checks catalog metadata (`description` ≤ 1 KB, rendered catalog ≤ 8 KB) and on-demand
   router size (`SKILL.md` body ≤ 8 KB) before staging `provisional` with a deterministic predicate.
   Interactive reflection uses at most three editor/classifier calls. ([D5, D6, D12, D15, D30](docs/research.md#2-decisions-and-their-evidence))
3. **Try it where it applies.** Kept entries live in the shared library (`~/.rematch/skills/`);
   provisional trials are exposed only through the matching place's view (`~/.rematch/views/<key>/`),
   so one repo's trials do not leak into another. Later sessions count only on a predicate hit,
   and catalog visibility alone is not evidence of use. ([D10, D12, D22, D30](docs/research.md#2-decisions-and-their-evidence))
4. **Keep or retire it.** Entries become `kept` through **accepted-in-use** (at least four clean
   completed sessions with actual relevant use: skill read/invocation, retrieval, hook evaluation/application,
   script run, agent call or approved instruction exposure) or **comparative support** (a bounded
   omp reference/candidate probe on an authorized replayable task). Neither is a causal proof of
   general improvement; two recurrences, two harm events or a regressed probe retire the entry. ([D8, D10, D13, D14, D31](docs/research.md#2-decisions-and-their-evidence))

**Three different things:** a skill can be **listed in the session's catalog** without being
**used by the model** (read, invoked or retrieved). It is **matched on a predicate hit** at the
recorded version (`matched` is an observational safety counter), and earns `used_clean` only when
actually used in a completed session with no recurrence or harm. Never read “listed” or “matched”
as “helped.” ([D10, D12, D22, D30, D31](docs/research.md#2-decisions-and-their-evidence))

For replayable tasks with a deterministic verifier, you can authorize **bounded automatic probes**
(one baseline and one candidate omp rollout per candidate version, ≤ 300 s each, 1 per reflect pass,
4 per 24 h, sealed editable paths) or run full [batch mode](https://vncp.github.io/rematch/guides/batch-with-a-verifier/).
Unavailable or declined probes never block an entry from qualifying through real use, and the
fixed verifier—never a model's self-score—decides. ([D6, D7, D17, D26, D31](docs/research.md#2-decisions-and-their-evidence))

## Everyday commands

Run these in a terminal; `--text` gives readable output instead of JSON.
In omp, `/rematch why` and `/rematch off` are convenient equivalents.

```sh
# Explain the last session here: loaded entries, personal instructions, hits and redirects.
rematch library why --text

# See what waits for you, then review one action in full (its approve/decline commands included).
rematch status --text
rematch library inspect <action-id> --text

# Create a skill from at least three matching sessions, guided by your description.
rematch create public-python-api --from sessions \
  --description "Document public Python functions and export them in __all__"

# Draft a skill from a description with an eval suite, check its trigger for free,
# then run it side by side against the baseline and promote the draft if it wins.
rematch create checkout-ux-review --from description \
  --description "Review checkout pages for accessibility" --eval-prompt "Review the checkout form"
rematch eval trigger checkout-ux-review --text
rematch eval run checkout-ux-review --activate --text   # report: ~/.rematch/evals/<name>/report.html

# Audit an existing skill and propose a revised sibling from its history.
rematch improve public-python-api

# Stop rematch; use `rematch on` to enable it again.
rematch off
```

`improve` needs at least five sessions that used the skill, including imported sessions,
before proposing an edit. Description-only creation produces a `draft`, not an automatically active skill.
See [create](https://vncp.github.io/rematch/guides/create-a-skill/),
[improve](https://vncp.github.io/rematch/guides/improve-a-skill/) and the
[CLI reference](https://vncp.github.io/rematch/reference/cli/) for other commands and flags.

## Scope and approvals

- **Skill, rule and fact text** activate automatically as scoped provisional trials; `status` and
  `library ls` list the active `trials`.
- **Personal instructions, subagents, scripts, runtime hooks, probe replays and exports** wait in
  a persistent action queue (`actions` in `digest`, `status` and `library ls`) until you decide.
  Inspect an action to see its target, reach and exact diff, then approve or decline it. Each decision
  is bound to the snapshot you reviewed by its full hash (`--sha`); the omp and Pi review dialog
  (`/rematch review`, `alt+e` in omp) supplies it, and Claude Code, OpenCode and `library inspect` print
  the commands with the hash in them. ([D32](docs/research.md#2-decisions-and-their-evidence))
- **Personal `AGENTS.md`, not a team PR:** an approved instruction is written as a marked block into your
  own `<agent_dir>/AGENTS.md`, which omp loads natively; Claude Code, Pi and OpenCode insert it at
  runtime in matching sessions instead. Your text outside the markers is never touched, an edited block
  becomes a conflict for you to resolve, and the always-on budget is rechecked on approval and on every
  sync. Applied blocks stay listed and can be withdrawn; with a profile, a withdrawal is profile-wide.
  Declining a text keeps the skill itself. ([D29, D32](docs/research.md#2-decisions-and-their-evidence))
- **Team and fork changes** (`ship-team:<id>`, `fork:<id>`) stay optional and prepare a local branch
  or PR; nothing is written to a repo's `AGENTS.md` by default or merged for you. ([D29, D32](docs/research.md#2-decisions-and-their-evidence))

Approval controls activation, not safe execution. Neither a script's non-executable file mode nor
a probe's fresh git worktree is an **OS sandbox**. Review proposed code and replay tasks before approving.
See [approvals](https://vncp.github.io/rematch/guides/approve-scripts-and-hooks/),
[automatic probes](https://vncp.github.io/rematch/guides/automatic-probes/) and
[D3, D4, D24, D29–D32](docs/research.md#2-decisions-and-their-evidence).

## Profiles across machines

An optional Git-backed profile makes `~/.rematch/profile/` the canonical home for your
**omp setup**: kept and adopted skills, agents, approved hooks, approved personal instructions
(`profile/instructions/<name>.md`), policy, model-role configuration and edited improver prompts. Use a remote you own and trust; this is
not a hosted sync service or a Claude Code/Codex configuration synchronizer.

Start with `rematch profile init --remote <git-url> --adopt` on the first machine,
then initialize the same remote without `--adopt` on another. omp syncs in the
background at session start; pulled changes may only load in the **next session**.
Credentials, session databases and the SQLite ledger are not synced in the profile.
See [sync machines](https://vncp.github.io/rematch/guides/sync-machines/) for conflicts,
manual sync and the optional macOS background timer.

## Architecture

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/readme/architecture-dark.svg">
    <img alt="The omp integration: extension events and skill discovery connect to the Rust binary, which owns reflection, the library and local state and calls omp for model work." src="docs/assets/readme/architecture-light.svg" width="100%">
  </picture>
</p>

The diagram shows the omp path. Adapters feed the same Rust core; model calls run
through `omp -p`. The extension records events and applies approved runtime policies;
the binary owns schemas, lint, staging and counters. Local state lives under `~/.rematch/`.
With profile sync enabled, kept content lives in the profile with linked views for omp.

The **lake is optional**: a Parquet/Bellwether backend for historical and cross-machine
queries, not a prerequisite for the learning loop or an in-session dependency.
See [architecture](https://vncp.github.io/rematch/background/architecture/) and
[query history](https://vncp.github.io/rematch/guides/query-history/) for the deeper design.

## Status and limits

The interactive loop, skill creation and revision, verifier-driven batch mode,
sharing and omp profile sync are implemented. That is not a measured real-use gain.
The project's daily-use success criterion is still pending; paper results explain
the design, not rematch's performance. The prompts are derived from Harness², not
verbatim copies ([provenance and changes](docs/research.md#5-porting-notes-what-omp-evolve-changes-in-these-prompts-and-why)).

Counters report matched sessions, actual clean uses, recurrence and harm—not a controlled causal
estimate of improvement; bounded probes test one authorized task and seed in omp, not general correctness.
Live model-backed runs documented so far are on macOS arm64. Native CI checks the Windows
omp path and Linux; POSIX shell fixtures remain Unix-only. Pi/OpenCode host runtime
compatibility on Windows is not yet certified.
See [status and limits](https://vncp.github.io/rematch/background/status/) for evidence and open gaps.

## Docs

- [Start here](https://vncp.github.io/rematch/start/install/) · [First correction](https://vncp.github.io/rematch/start/first-correction/)
- [Daily use](https://vncp.github.io/rematch/guides/day-to-day/) · [CLI reference](https://vncp.github.io/rematch/reference/cli/)
- [Research and decision rationale](docs/research.md) · [Design contract](docs/DESIGN.md) · [Product spec](docs/SPEC.md)
