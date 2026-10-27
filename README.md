<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/readme/wordmark-dark.svg">
    <img alt="rematch" src="docs/assets/readme/wordmark-light.svg" width="360">
  </picture>
</p>

<p align="center"><b>Let the agent improve its own harness from the sessions you were going to run anyway, and keep only what later sessions confirm.</b></p>

<p align="center">
  <a href="#what-you-can-do-with-it">What you can do with it</a> ·
  <a href="#thirty-second-start">Start</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#in-one-minute">In one minute</a> ·
  <a href="#how-a-reflection-works">How a reflection works</a> ·
  <a href="#what-it-counts">Counters</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="https://vncp.github.io/rematch/">Docs</a>
</p>

<p align="center">
  <img alt="Rust 1.85+" src="https://img.shields.io/badge/rust-1.85%2B-DEA584?logo=rust&logoColor=white">
  <img alt="omp 18.12" src="https://img.shields.io/badge/omp-18.12-171717">
  <img alt="Status: phase 1 in daily use" src="https://img.shields.io/badge/status-phase%201%20in%20daily%20use-orange">
  <a href="#license"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-lightgrey"></a>
</p>

## What you can do with it

- **Do nothing.** Work in omp as usual. Every session is traced. A session that ended with a redirect, a tool-error cluster, or a takeover produces one bounded proposal at the start of the next session, staged as `provisional`. Later sessions that match load it. Counters decide whether it stays.
- **Create a skill.** `/rematch create <name> --from sessions|task`. From three matching sessions, or from one reference run of a task you name. A description with neither becomes a `draft` that never loads until you activate it.
- **Improve a skill from history.** `/rematch improve <skill>`. Registers it, audits where it loaded, clusters the redirects and errors that happened with it in context, proposes unit-level edits, stages the new version beside the old one. Refuses with a count if fewer than five traced sessions used it, and offers the mechanical split instead.
- **Run a batch job with a verifier.** `/rematch run --task <id> --verifier <cmd>`. Reference rollout, two candidates, probe, compose, re-run; the verifier picks. Never a model's opinion of its own output.
- **See why.** `/rematch why`, `/rematch status`, `/rematch library`. Which entries were loaded for the last deliverable, what was staged, kept, retired, and the raw counts per entry.
- **Correct it or stop it.** `/rematch mark correction|preference|scope` overrides the classifier for the last turn. `/rematch off`, `REMATCH=0`, `/rematch disable <id>`, `/rematch rollback <skill>`, `rematch uninstall` are immediate.

## Harnesses

The binary, the ledger, lint, the counters and the entries are the same for every harness; an adapter only forwards what its harness exposes ([`docs/DESIGN.md`](docs/DESIGN.md) §18). Session ids carry the harness (`cc:<uuid>`, `codex:<id>`; omp's are bare), so one ledger holds all of them. omp is the reference; Claude Code runs the loop live through a plugin of hooks ([`adapters/claude-code`](adapters/claude-code/README.md)); Codex is read after the fact ([`adapters/codex`](adapters/codex/README.md)).

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

One directory, `~/.rematch/skills/`, delivers skills to all three harnesses: it links every kept entry and every provisional entry whose predicate matched a directory that was synced, so loading is the union of the repos you work in while counting stays with each session's own predicate ([Where skills load from](https://vncp.github.io/rematch/concepts/entries.html#where-skills-load-from), DESIGN §16 "Shared skills"). No model call ever runs inside a Claude Code or Codex session (Claude Code's reflection is a detached background process). Why each Claude Code gap cannot close: DESIGN §18.1 "What Claude Code cannot do".

## Thirty-second start

From a checkout of this repo, with omp 18.12 and a Rust toolchain:

```sh
scripts/install.sh            # cargo build --release · omp plugin link extension/ · ~/.local/bin/rematch
rematch import --history      # seed the ledger from your existing omp sessions (read-only)
rematch doctor --text         # omp version, resolved editor/classifier roles, extension linked, ledger schema
```

Then use omp. The first proposal arrives after the first session that ends with a redirect.

Optional, in `~/.omp/agent/config.yml`: `modelRoles.rematch-editor` for the strongest model you have and `modelRoles.rematch-classifier` for a fast one. Without them the editor falls back to `slow` → `default`, the classifier to `smol` → `tiny` → `default`.

## How it works

Every claim below carries the id of the decision it rests on; the evidence and the paper behind each id are in [`docs/research.md`](docs/research.md) §2.

The session is the evidence. The extension writes every input, tool result and turn to a trace file of its own, and at shutdown flags the session only if it carried a signal: a redirect, a cluster of tool errors, a takeover, or an explicit mark. No proposal exists without that trace (D1). Nothing runs inside the session. At the start of the next session the binary is spawned detached: it classifies the redirect, finds the moment, runs one editor scoped by triage to how the agent works or to what a practitioner knows (D5), lints the result for generalization (D15), and stages it as a `provisional` entry with a deterministic trigger predicate (D12) and a manifest naming the predicted fix and the predicted regression (D16). Sessions whose predicate matches load it from the staging path; there is no withheld arm (D11). For each matched session the binary counts whether the specific redirect class that motivated the entry recurred, and whether harm occurred (D10). Keep at zero recurrences over at least four matched sessions; retire at two recurrences or two harm events; dormant after twelve without a decision (D8, D13). Only failures are reflected on; the system never grades its own successes (D9).

---

<p align="center"><b>A harness-improvement loop that costs no reruns in interactive use. The real session is the reference rollout, the next matched session is the probe, and a reflection costs at most three model calls.</b></p>

rematch is an omp plugin: a TypeScript extension that omp loads, and a Rust binary the extension calls with a JSON envelope. The extension only records and toggles; the binary proposes and stages. The boundary between what a model may propose and what the system may write is enforced by record types, not by a prompt: proposals carry no score field, and nothing activates without a lint report and, for anything executable, a receipt (D24). Skill text activates in personal scope on its own. Anything executable (scripts, hooks) and anything beyond personal scope (team `.omp/`, your fork) waits for a one-tap approval or a merged PR (D4, D3). No fork of omp is required; the fork is one of the things it can propose changes to.

The loop design, the scoped editors, the preference for skills and scripts over prose, and the experience library come from Harness² (Xu et al., arXiv 2610.14224); the `improver/` prompts are derived from its Appendix H, with every delta listed in [`docs/research.md`](docs/research.md) §5. Compiled hooks for recurring corrections come from Trace (D4); the counters and caps from ACE, Evo-Harness and HCL (D13, D14); the trust boundary from bellwether (D24).

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/readme/loop-dark.svg">
    <img alt="The rematch loop. In the session: trace, flag, load, guard. Between sessions: classify, triage, propose, lint, stage, count and decide. An evidence boundary separates them: no proposal without a trace. You see a digest, approve or decline, mark, or turn it off." src="docs/assets/readme/loop-light.svg" width="100%">
  </picture>
</p>

## Contents

- [What you can do with it](#what-you-can-do-with-it)
- [Harnesses](#harnesses)
- [Thirty-second start](#thirty-second-start)
- [How it works](#how-it-works)
- [In one minute](#in-one-minute)
- [Highlights](#highlights)
- [How a reflection works](#how-a-reflection-works)
- [What it counts](#what-it-counts)
- [Statuses](#statuses)
- [Create and improve](#create-and-improve)
- [Batch mode](#batch-mode)
- [Scope and who merges](#scope-and-who-merges)
- [Models](#models)
- [How it compares](#how-it-compares)
- [One profile, every machine](#one-profile-every-machine)
- [Architecture](#architecture)
- [Run it](#run-it)
- [Status](#status)
- [Docs](#docs)
- [License](#license)

## In one minute

1. **Work.** You ask for a function in `src/lib.py`. The agent adds it. You answer: *"That's wrong. Every public function here needs a Google-style docstring and must be exported in `__all__`."* The extension pre-labels the turn as a negation and flags the session at shutdown.
2. **Reflect.** Next session start, the binary classifies the redirect (`correction`, 0.86), triages it as a process failure about this workload's conventions, and runs the domain editor once with the trace excerpt. The reply passes lint and is staged with a predicate: this repo, `.py` files, the stems of the task turn.
3. **Keep or retire.** The next session that adds a function to `src/lib.py` loads the entry. In the scratch run behind the screenshots below the agent exported and documented the function unprompted; that is one session on a toy repository, not a measured gain. The session counts as `matched 1 · recurrence 0 · harm 0`. Three more like it and the entry is `kept`; two redirects of the same class and it is retired.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/readme/reflect-dark.svg">
    <img alt="Terminal: rematch reflect --pending reports the classifier's verdict, the triage, and that src-lib-public-functions was staged with lint ok after one model call; the staged SKILL.md shows the rematch frontmatter block and three checkable laws about __all__ and docstrings." src="docs/assets/readme/reflect-light.svg" width="100%">
  </picture>
</p>
<p align="center"><em>A reflection from a real session on a scratch repository, with gemini-4-argon as classifier and editor. Output condensed to the lines that matter.</em></p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/readme/why-dark.svg">
    <img alt="Terminal: rematch library why shows the next session with the entry loaded, glob hit and predicate hit, recurrence 0 harm 0, total matched 1; library ls lists the provisional entry with its raw counts; digest shows the staged line and no pending approvals." src="docs/assets/readme/why-light.svg" width="100%">
  </picture>
</p>
<p align="center"><em>The next matched session. <code>why</code> explains the load and the count; <code>ls</code> shows the counts raw.</em></p>

## Highlights

- **No proposal without a trace (D1).** Every entry cites the moment (`session_id`, `seq`) it came from. Editing a harness without an execution behind it degraded performance in Harness² (73.4 → 63.2 on code). A description-only skill is a `draft` that never loads until you run a task with it or accept it unverified.
- **No reruns in interactive use (D6, D7).** The real session is the reference and the next matched session is the probe. A reflection spends at most three model calls: one batched classifier call, one editor, one lint retry. There is no composer without probes and no model selector anywhere: a composer choosing between untested texts is blind selection, which produced Harness²'s worst regressions.
- **Kept by use, retired by counters (D8, D10).** The primary signal is recurrence of the specific redirect class that motivated the entry, not tokens, not turns, not a judge. Counts are raw and shown raw. The status is called `kept`, never `verified`: unvalidated model-written patches were net negative in Harness-R1 (21 of 39 lowered success).
- **Nothing executable without you (D4).** Scripts are staged mode 0644. Hooks are compiled only after approval. Both arrive as one-tap approvals in the session-start digest, with the diff. Compiled hooks are the form that raised compliance with user corrections from 55.0% to 70.1% in Trace; they are also the form that can act without being read.
- **Lint is static and free (D15).** No session ids, task paths or user names in the text; no string-match on the user's words; a trigger predicate and a trigger line on every entry; no URLs, network calls or destructive git; always-on text within 8 KB per branch; facts carry `as_of`. HarnessCompass's generalization gate alone recovered most of its gain.
- **A bad entry cannot cascade (D13).** One admission per trigger predicate per day, ten provisional entries per branch, no new reflection while a canary for the same trigger is pending, caps that force merges, and an archive that is never deleted. Every unbounded library in the literature peaked and then declined.
- **Triggers stay in their domain.** Sessions and triggers carry a domain (`web.frontend`, `mobile`, `quant.options`): declared with `rematch policy domain set`, named in a branch's `domain.yml`, else inferred without a model from file extensions and words. A review skill learned on web work never loads or counts on mobile or 3D work, at most 8 entries are kept per domain, and `report` groups its triggers by domain. See [Domains](https://vncp.github.io/rematch/concepts/predicates-and-domains.html#domains).
- **The library is appended, never rewritten (D14).** Entries are itemized units with their own counters; a model never rewrites the library or the improver prompts (D25). Monolithic rewrites collapsed a context from 18,282 tokens to 122 in ACE, below baseline.
- **The schema lives in Rust only (D24).** The extension forwards raw events and parses nothing. One static binary owns the ledger, the library, lint, staging, batch mode, PR output. It never holds provider credentials; model calls go through `omp -p` and every event's provider and model are verified. The model family is recorded on every entry and never gated on (D20).
- **Rollback and uninstall (D13).** Every kept entry keeps its predecessor. `uninstall` removes every file the plugin wrote and nothing else.
- **Expectations are modest by design (D18, D19).** Published harness gains were 54% overfitting on audit; the deployed-static regime this tool runs in showed low single digits for strong models and more for weak ones. The project has a kill criterion (see Status) and no gain has been measured on real use yet.
- **Predicates learn from reads, under the same counters (D12, D15).** A skill the model opened in three sessions its predicate missed gets the stems those sessions share; a predicate that matched four sessions where the skill was loaded and never read loses the stems only those carry. No model call, and never an edit in place: `rematch tune` (and every `count`) stages a provisional sibling with the new predicate, a new canary that the counters keep or retire, with its evidence session ids in `rematch.json`. Guards: one tune per entry per 7 days, never on pinned, built-in or hook entries, never a new repo, remote or `**`, never another domain, ≤ 10 stems, ≤ 8 extensions, ≥ 2 stems after narrowing, lint and rate limits unchanged; `report` shows both versions' with-arms so a wider predicate cannot win the verdict by matching more.

## How a reflection works

| Step | What happens | Enforced by |
|---|---|---|
| **Flag** | At shutdown the extension flags the session only if it carried a signal: a pre-labelled redirect (`interrupt`, `revert`, `negation`, `reprompt`), three tool errors in any six consecutive results, or `/rematch mark`. Clean sessions produce nothing. | extension; a 2-second budget, so it writes one small file |
| **Classify** | One batched call labels each redirect `correction`, `preference`, `scope` or `none`. A session whose only redirects are `scope` or `none` carries no signal and gets no editor call. A mark overrides the classifier and is never overridden. | binary (`reflect.rs`) |
| **Triage** | The first failure is either *process* (anything the agent produced with tools, tool-error clusters, takeovers, reverts, procedural text) or *content* (a corrected prose answer). Content failures are logged, not proposed on. Separately, the scope picks the editor: *domain* by default, *invariant* when the failure is about the agent's general way of working. | `trace::triage` |
| **Predicate** | Repo path, the extensions of files touched in the evidence window, and up to ten stems ranked by tf-idf over the task turn, the touched files and the redirect. The hash of this predicate keys the rate limits. | `predicate::derive` |
| **Propose** | One editor session through `omp -p --no-tools`, inputs in the prompt: the trace excerpt, the deliverable, the redirect, the current entry if any, matching `EXPERIENCE.md` lessons. The prompt treats the trace as data. The editor returns edits plus a manifest (evidence moment, predicted fix, predicted regression). `KEEP_BASE` is valid. There is no composer: with no probes, choosing between two untested texts would be blind selection. | `improver/editor.md`; `omp.rs` verifies provider and model on every event |
| **Lint** | Static, free, every rule in `improver/lint.toml`. On an error the editor gets one retry with the findings. | `lint::lint_entry` |
| **Stage** | Written to `staging/<branch>/<name>/` as `provisional` with the predicate, model family, originating session and manifest. Near-duplicates (simhash) route evidence to the existing entry instead. | `library::stage`, `dedup` |
| **Load** | At the next session start the extension has the binary link the entries matching the working directory into the shared skills directory, which it adds to omp's skill paths before discovery; `match` gives the entries and hooks the session counts. Every session is an on-session; there is no withheld arm. | extension factory; `library::sync_shared_skills`, `library::list_for_match` |
| **Count · decide** | For every finished trace: did the motivating redirect class recur while the entry was loaded; did a tool-error cluster, a takeover or a disable happen. Then the decision rule. After a keep or retire the reflector writes `EXPERIENCE.md` beside the entry; later proposals read matching ones. The editor prompt itself is never amended. | `counters::fold_session`, `counters::decide` |

## What it counts

Each entry carries three raw counters, with a per-family breakdown. Tokens and turns are recorded for `why` and never gated on, because they vary tenfold with task difficulty.

| Counter | Increments when | Decides |
|---|---|---|
| **matched** | the session's repo matched the glob, its file extensions overlapped, and at least two predicate stems appeared in the first five inputs or the files read | the denominator for every decision; dormant at 12 without one |
| **recurrence** | the specific redirect class that motivated the entry happened again while the entry was loaded | retire at 2 |
| **harm** | a new tool-error cluster, a takeover, or a user disable while the entry was loaded | retire at 2 |

Keep at `matched ≥ 4 · recurrence 0 · harm 0`. Four matched sessions with no recurrence is evidence that the entry did its one job, nothing more; that is why the status is `kept`, not `verified`. A model never rewrites counters.

## Statuses

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/readme/lifecycle-dark.svg">
    <img alt="Entry statuses. Draft becomes provisional on a task run or --unverified. Provisional becomes kept at zero recurrences over at least four matched sessions with no harm, retired at two recurrences or two harm events, dormant after twelve matched sessions without a decision or thirty days with fewer than three matches, and back from dormant when the predicate hits again. Kept becomes stale when a fact source, the model family or the fork commit changed, or retired at two harm events in thirty days. Stale returns as a new canary or retires. Retired never leaves. Pinned is a flag." src="docs/assets/readme/lifecycle-light.svg" width="100%">
  </picture>
</p>

| Status | Where it lives | Loaded |
|---|---|---|
| `draft` | `~/.rematch/drafts/<name>/` | never |
| `provisional` | `~/.rematch/staging/<branch>/<name>/` | on predicate match, via the skill paths; a subagent (`AGENT.md`) via the link `~/.rematch/agents/<name>.md` in every session, counted on match |
| `kept` | `~/.omp/agent/skills/<name>/` (subagents: `~/.omp/agent/agents/<name>.md`), predecessor in `predecessors/` | natively, every match |
| `stale` · `dormant` · `retired` | `~/.rematch/archive/<status>/<id>/` | never; never deleted |
| `pinned` | wherever it is | a flag; never replaced automatically |

## Create and improve

**Greenfield.** `rematch create <name> --from sessions` searches the ledger (imported history plus traces; every machine's sessions through the lake when there is one) for sessions whose features match the request; with three or more, the skill is distilled from what worked and what was redirected. `--from task --task <id>` runs the task once in batch mode (one rollout, no candidates) to obtain a trace; this is the only create that costs a rollout. `--from description` stages a `draft`; `activate <name> --task <id>` or `--unverified` makes it `provisional`. The result is a skill package: a router `SKILL.md` with the trigger line and the checkable laws, `units/` for procedures and pitfalls, `facts/` for measured values with `as_of`, `scripts/` for anything deterministic. Below about 8 KB the package collapses to one file.

**Brownfield.** `rematch improve <skill>` registers the existing skill as a `provisional` entry in place (the file is never moved), audits the sessions that read it and the redirects and errors that happened with it in context, and proposes unit-level edits from the clustered redirects. A skill over 8 KB is split first, mechanically and with no model: router, `units/` by heading, `facts/`, long tables and rejected-hypothesis lists into a ledger-notes unit. The new version is a sibling beside the old one; omp shows the sibling under the bare name and the original under its namespaced name until the counters decide. A `pinned` skill is never replaced.

`rematch import --forge <path>` registers compound-forge's skills as brownfield entries with predicates from their trigger text, and its `learnings/*.md` as lessons with their evidence hash. With `--migrate` the skill trees are copied into `staging/forge/` (scripts non-executable) so the forge plugin and checkout can be removed; the forge's own meta-skill is left behind. This is how compound-forge was retired on both of the author's machines.

## Batch mode

Where a verifier exists, the plugin spends rollouts the way Harness² does, per task:

1. Reference rollout with the current harness, in a fresh git worktree with a `--config` overlay that exposes only the candidate skill tree.
2. Invariant and domain candidates from the single editor, each probed once on the same task under the same seed.
3. A composer merges from the behavioral contrasts; the composed harness runs once more.
4. The verifier picks among reference, candidates and composed: a test command, a deterministic check script, a parity check against a known result, or a receipt. Ties go to the reference.
5. The winner is kept for that job. Its lessons enter the library as `provisional` with a `pending_next_run` flag; they become `kept` only after the next real run of that job passes with them loaded.

Budget: `rollouts = (k+1)² + [k>1]`, four at `k=1`, ten at `--width 2`; model calls `2k + 1`. `run` returns a job id and runs detached; `status <job>` polls. Workspaces are deleted after the receipt is written.

## Scope and who merges

| Scope | Activation | Examples |
|---|---|---|
| Personal, text | automatic as `provisional` | skill text, laws, facts, lessons, subagents |
| Personal, executable or blocking | one tap in the session-start digest, with the diff | scripts (0644 → 0755), compiled `tool_call` verifiers |
| Team (`<repo>/.omp/`, `<repo>/AGENTS.md`) | `ship --scope team`: a branch with the entry (skills in `.omp/skills/`, subagents in `.omp/agents/`, an `AGENTS.fragment.md` as a section of `AGENTS.md`) and an evidence commit; `--push --pr` opens the PR, never merges | entries whose predicate recurred across two or more repos, or that you list; the only way text reaches `AGENTS.md` |
| Fork | `ship --scope fork [--repo <fork>]`: the entry's `fork_path` (set with `library set-fork-path`), the built fingerprint and before/after counters in the PR body | per-model prompt text, tool descriptions, compaction defaults |
| Work → home | `library export`, per-entry approval with the diff; off by default at work (`policy.json`) | invariant entries only; lint with `transfer: true` blocks domain and workspace facts |

A branch is a repo or project. `shared` holds entries kept in two or more branches; promotion needs a second branch's canary and no new canary of its own. When the shared copy replaces a kept entry whose originating task has a verifier, that task is replayed once with the copy in place first; a failing replay retires the copy and the kept entry stays. `transfer suggest` lists kept invariant lessons not yet tried in a branch whose facet structure overlaps on two or more keys (`branches/<branch>/domain.yml`); a transfer is applied as `provisional` and earns its place through that branch's counters.

## Models

Every entry records the model family it was created under and keeps a per-family breakdown of its counters. Entries load regardless of family; gating would make them flicker as you switch models. When the counters diverge, harm on one family and clean on another, the entry is split and the harmed family gets its own sibling. A family whose recurrence or harm rises across several entries in one window appears as a review list in the digest, never as automatic retirements. The editor is the strongest configured model; the classifier a fast one; a different provider than configured is a failed run, never a fallback. There is no judge role.

## How it compares

| | Hand-written skills | compound-forge (SkillOpt / GEPA) | rematch |
|---|---|---|---|
| Where evidence comes from | your memory of what went wrong | a benchmark suite you maintain | the execution trace of the session that went wrong |
| Cost per improvement | your time | dozens to hundreds of rollouts | ≤ 3 model calls, 0 reruns (interactive); 4–10 rollouts with a verifier (batch) |
| What decides | you | validation score on held-out cases | recurrence of the motivating redirect over later matched sessions; a verifier in batch mode |
| What can activate on its own | everything you write | nothing (proposals are reviewed) | skill text in personal scope; never scripts, hooks, team or fork changes |
| Exit for a bad change | you notice | regression on the suite | counters retire it; rollback restores the predecessor; `off` stops everything |

**In short:** the loop (scoped editors, lint for generalization, an experience library, composition only where a verifier exists) comes from Harness²; the prompts are derived from its Appendix H, not verbatim (D5, D6; deltas in `docs/research.md` §5). What rematch adds is the zero-rerun interactive mode with a deterministic predicate and raw counters (D10, D12), and a boundary that lets the harness propose without being trusted (D24). The cost: a single user is not blinded, so the counters measure only the recurrence of a specific redirect class, something you would not fake; and nothing is ever called verified.

## History as a backend

The ledger answers today's questions in milliseconds; it cannot answer *what did the library look like when that session ran*, nor combine two machines, nor index years of sessions. [ADR 001](docs/adr/001-lake.md) adds both without touching the hot path: every ledger write is journaled with the ledger's own clock, the journal is exported to Parquet (one [Bellwether](https://github.com/vncp/bellwether) root per table, files written once), and Bellwether's keys answer the historical questions the loop actually asks — `asof --valid --known` on `entries` for status-at-the-time without lookahead, `exact:signature` and `list:facets` on `outcomes` for the spec's three lookups, `exact:tool_cmd` and `text:tool_cmd` on `events` for "which sessions ran this command" and "which ran a command with these words". Only keys a current reader needs are declared; the ADR lists every other kind with the question that would justify it. Status: L1–L4 built (journal, export, import, rebuild, index, query, the switched readers and `rematch report`); see Status below.

## One profile, every machine

> Live-verified 2026-10-26 between two Macs over a private GitHub repo: hand-written subagent prompts adopted on both (identical copies became links), a skill added on one machine appeared on the other at the next sync, a deletion propagated back, launchd syncs every 5 minutes on each.

What rematch keeps on one machine should load on the next. `rematch profile init --remote <git url> --adopt` turns `~/.rematch/profile/` into a git repo of your omp setup — kept skills and agents, the skills and agents you wrote by hand (adopted as pinned entries), approved hooks, `policy.json`, per-branch `domain.yml`, improver prompts you edited, and `config/roles.yml` for the model behind each role on each machine — and links every piece back where omp and rematch read it. Another machine runs `rematch profile init --remote <git url>` once; from then on each omp process syncs in the background at session start (commit, pull with rebase, relink, push; bounded, offline is fine), keeps land in the profile, and `rematch profile install-launchd` adds a timer if you want one. A file changed on two machines stops the sync with the tree clean and a digest line naming the file. The ledger, counters, traces, staging and credentials stay per machine; history travels through the lake. Details: [Syncing machines with the profile](https://vncp.github.io/rematch/guides/profile.html), [DESIGN §20](docs/DESIGN.md).

```
profile/
  skills/<name>/  agents/<name>.md  fragments/<name>.md  hooks/<id>.ts
  config/policy.json  config/roles.yml  config/domains/<branch>.yml
  improver/<edited file>  profile.json
```

## Architecture

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/readme/architecture-dark.svg">
    <img alt="Architecture. omp (stock or a fork) emits events to the extension, which writes traces and has the binary sync the shared skills directory before discovery. The Rust binary owns rematch-core, reflect and count, create and improve, batch and scope, and calls omp -p non-interactively for the editor, the classifier and batch rollouts. State lives under ~/.rematch; kept entries are copied into ~/.omp/agent/skills." src="docs/assets/readme/architecture-light.svg" width="100%">
  </picture>
</p>

| Unit | What it holds | Rule |
|---|---|---|
| `extension/` | `index.ts`: trace writer, pre-labels, the skill-path override, approved hook modules (`block` and `stuck` on `tool_call`, `truncate` and `retrieve` on `context`; the modules are data, the modes live here), the `rematch` tool and `/rematch` command, the digest; `agents/` (made by `install.sh`, gitignored): a link to `~/.rematch/agents/`, through which omp discovers provisional subagents | forwards raw events; parses nothing beyond the hook-module contract; no model calls; every handler fails open except `block`/`stuck` hooks |
| `adapters/` | `claude-code/`: a Claude Code plugin (`hooks/hooks.json` → `scripts/hook.sh`, eight `/rematch:rematch-*` commands; `skills/` → the shared skills dir and `agents/` copies, both made at `SessionStart` and gitignored); `codex/`: the `$rematch-status` skill and setup notes | forward hook payloads unchanged to `rematch trace-event` / `hook-check` (schema, matching, digest and hook verdicts stay in Rust); never fail a hook except by an intentional block; no model call inside the session (Claude Code starts `reflect --pending` detached) |
| `crates/rematch-core` | trace model and parsers (own trace, omp session JSONL), redirect heuristics and classifier batching, predicates, lint, library and counters, SQLite ledger with content-addressed blobs, package model and mechanical split, simhash dedup, EXPERIENCE.md, rate limits, outcome records | pure functions over paths; no process spawning, no network; 135 unit tests + 1 golden import test |
| `crates/rematch` | the CLI; `proc.rs` (process-group bounded execution), `omp.rs` (argv builders and per-event provider/model verification), `jobs.rs` (detached jobs), reflect/count, create/improve, import, digest, `skills sync` (the shared skills dir), doctor, uninstall, `retrieve`, built-in entries, `batch/`, `scope/` | one JSON envelope per invocation; never holds credentials; 60 unit + 43 end-to-end tests (239 cargo tests in all, plus 24 extension tests under `bun test`) |
| `improver/` | `editor.md` and `composer.md` (derived from Harness² Appendix H Prompts 1–3; deltas in `docs/research.md` §5), `reflector.md`, `classifier.md`, `lint.toml`, `omp-no-tools.yml` | copied to `~/.rematch/improver/`; refreshed by a new binary only when you have not edited them; never amended by the system (D25) |
| `~/.rematch/` | `traces/`, `pending/`, `ledger.sqlite` + `blobs/`, `staging/<branch>/`, `skills/` (the shared skill links every harness loads), `agents/` (links to provisional subagents), `kept-agents/`, `drafts/`, `archive/`, `predecessors/`, `jobs/`, `runs/`, `tasks/`, `branches/`, `export/`, `profile/` (the git-synced profile, when initialized), `off` | one root per environment (`REMATCH_ROOT`); home and work are separate libraries |
| `crates/rematch-lake` (cargo feature `lake`, default on) | the Parquet side of the lake: per-table Arrow schemas, the journal and trace writers, `export`/`import`/`rebuild`/`doctor`, `index`/`query` over an external Bellwether binary (arrow/parquet 59.3) | read only by historical commands (`rematch lake …`, `report`, the readers of DESIGN §19.3); `cargo build --no-default-features` drops it and arrow with it |
| `~/.rematch/lake/` (ADR 001, L1–L4) | the journal exported to Parquet, one Bellwether root per table, plus one events file per closed session; `lake-index/` per machine | history as a backend: `report`, `why --asof`, cross-machine `transfer`/`shared`; never on the in-session path |

The two-language split is kept on one rule: the schema lives in Rust and the extension forwards raw events. The verified omp facts behind every design choice, and the four places the build deviates from the spec because of them (no `resources_discover` callsite in omp, auth living in the agent dir, no registry publish, no embeddings without credentials), are in [`docs/DESIGN.md`](docs/DESIGN.md). The full product spec is [`docs/SPEC.md`](docs/SPEC.md).

## Run it

The checks every change goes through:

```sh
cargo test --workspace                        # 119 core · 54 binary · 32 end-to-end (scripted editors, fake omp)
cargo clippy --workspace --all-targets -- -D warnings
bun build extension/index.ts --no-bundle --outdir /tmp/rematch-ext-check
bun test extension                            # hook runtime: mode helpers + the factory against a fake host
```

The end-to-end tests run the built binary against a temporary root: a synthetic trace with a redirect is reflected through a scripted editor, staged, matched, counted through four clean sessions and kept into the agent skills dir; the lint-fail script exercises exactly one retry; a wrong-model reply fails closed; `import --history` runs against anonymized goldens; `improve` refuses, splits, stages a sibling and refuses a pinned skill; the batch worker completes a `k=1` cycle with a fake omp and keeps the winner on the next run; `ship` creates the branch and commit; `export` refuses at work by default.

What was run live against omp 18.12.6 before this README was written: the full interactive loop on a scratch repository (rpc-mode sessions with real redirects, gemini-4-argon editor and classifier, the entry loaded in the next session and counted), the forge import and the mechanical split of a 31 KB skill resolved as a package in a live session, one detached batch cycle with a script verifier, one team ship, and `/rematch` in the TUI.

## Status

Built: the interactive loop (Phase 1), create and improve with the mechanical split and the forge import (Phase 1.5), batch mode with worktree isolation and the next-run keep rule, context-hook unit retrieval, and tool-output truncation and stuck detection as entries under the same counters (three built-in hook entries that wait for your approval; Phase 2), team and fork shipping, work-to-home export, stale on drift, split per family, transfer suggestions and `shared` promotion (Phase 3), and omp profile resolution (`OMP_PROFILE` / `PI_PROFILE` / `PI_CONFIG_DIR` / `PI_CODING_AGENT_DIR`, multi-profile history import with `--skip-profile` and `policy.json` `skip_profiles`).

Harnesses: omp is the reference. Claude Code runs the loop live through `adapters/claude-code` (see [Harnesses](#harnesses)); checked with `claude -p --model haiku` (Claude Code 2.1.285) against a temporary root: `session_start`, `input`, `tool_result` (a failing `Bash` call as `is_error`), `turn_end` with the model, `agent_end` and `session_shutdown` traced; a negation in a resumed session pre-labelled, flagged and counted once, whole; `reflect --pending` started detached by the next `SessionStart` (it counted the session and attempted the editor, which the temporary root had no model role for); a prompt queued while a `Bash` call ran traced as `interrupt`; the digest with approval commands delivered at `SessionStart`; a provisional agent copied and listed by `claude plugin details`, then counted in the next session. Not yet observed live: the digest's display in an interactive session, an Esc abort (covered by an end-to-end test on the transcript format), and a Claude Code session reflected by a real editor model. Codex is post-hoc only.

Lake (ADR 001), L1–L4 built. L1: every ledger write is journaled with the ledger's own clock; entries carry `content_sha256`, sessions and predicates `repo_remote` + `subpath`, traces versioned `loaded_entries`. L2: `rematch lake export` writes the journal and every closed session's trace to Parquet under `~/.rematch/lake/` (per-run files, never rewritten — except that `lake index` upgrades an events file exported before the stored `tool_cmd` column, once —, byte-identical for the same journal; also at the end of every `reflect --pending`), `lake import` merges a second machine's lake, `lake rebuild` reproduces the ledger (`library ls` and counters exactly) on a fresh root, `lake doctor` reports watermarks, lag and stray files. L3: `rematch lake index` builds one Bellwether index per table under `~/.rematch/lake-index/` with the ADR 001 key table (all 15 specs accepted by bellwether 0.1.0, among them `exact:tool_cmd` and `text:tool_cmd,tokenizer=words` on the events' bash command; the append-table path template uses `{run_seq:int}` because Bellwether refuses a `{seq}` placeholder that disagrees with the stored per-row `seq`), pins the Bellwether version in `lake/manifest.json` (`--repin` to rebuild under another), and `rematch lake query <table> --key … --values …` (a text key: `--has word,word`, one `locate --has` per word, row groups intersected) plans through `locate|latest|asof`, evaluates the returned residual (in, between, not_older_than, latest_per_key, text, contains_any/all; any other kind refused by name) and returns rows as JSON — with no binary or no index it scans every file and says `indexed: false` with the same rows. Details: DESIGN §19.2. L4: `reflect --history` and `improve` read a session's records from its events file, `create --from sessions` and `transfer suggest` read every machine's `sessions` and `outcomes` (`create --ran cargo,test` the events' `text:tool_cmd`), `library why <session> --asof <ts>` shows each loaded entry's status and each redirect's class as they stood then, each naming its source (`lake` or `ledger`, the ledger while the lake lags it); `rematch report` computes the kill criterion — entries kept over ≥ 4 matched sessions, recurrence per trigger on sessions with an entry of it loaded against sessions without, and the verdict line — from the lake alone, or from the ledger's journal without one (DESIGN §19.3).

The lake is verified by the test suite on temporary roots and was run live on both of the author's machines on 2026-10-26: 1,178 journal rows and 326 session event files exported in 2.9 s, all keys indexed with Bellwether 0.1.0 in 0.8 s, `report` from the lake in 57 ms, `why --asof` giving the pre- and post-transition status for the same entry, and the M5's lake reading this Mac's history after `lake import` (details in ADR 001, "Live run").

Profile built: `rematch profile init|adopt|sync|status|install-launchd`, keeps and hook approvals written into `~/.rematch/profile/`, views linked into the agent dir, `roles.yml` ahead of `config.yml`, the background sync at omp session start, conflicts in the digest, `uninstall` keeping the repo. Verified by `crates/rematch/tests/e2e_profile.rs` on two temporary machines sharing a local bare remote (adopt, clone, a keep crossing machines, a conflict left clean, an unreachable remote reported offline in about 2 s, roles per machine, uninstall); not yet run between two real machines or against a hosted remote.

Not yet earned:

- **A `kept` skill from daily use.** The three built-in hooks reached `kept` (9/6/5 matched sessions, 0 recurrence, 0 harm) during the build itself; no skill has yet. The kill criterion in the spec is six weeks of daily use: if no entry reaches `kept` over at least four matched sessions and recurrence has not fallen on any trigger, stop at Phase 1 and keep stock omp with hand-written skills. The trace tooling and `why` stay useful either way.
- **The built-in context entries in real use.** All three were approved and fired in one live omp session on this machine (a `retrieval` record for a `bellwether-dev` unit, the stuck-detector blocking a 4th identical call); truncation is only observable in the outgoing request and is covered by the extension tests. None has been counted across real sessions yet.
- **A batch cycle where a candidate beats the reference.** The one live cycle had a reference that already passed, so both editors chose `KEEP_BASE`; the fake-omp tests cover the other branches.
- **A merged team PR and a `shared` promotion from real branches.** Both are exercised on scratch repositories only.
- **Linux.** The code is POSIX-portable (`setsid`, process groups, `sh` scripts) and every run above was macOS arm64. No Windows.
- **Embeddings for the "related thing" lookup.** Exact signature and MinHash are implemented; embeddings would need credentials the binary does not hold.

Open questions from the spec that the build did not settle are at the end of [`docs/SPEC.md`](docs/SPEC.md#open-questions).

## Docs

**[Documentation site](https://vncp.github.io/rematch/)** (source: [`docs/book/`](docs/book/src/SUMMARY.md), `mdbook build docs/book`) · [quick reference](docs/MANUAL.md) · [CLI reference](https://vncp.github.io/rematch/reference/cli.html) · [ADR 001: the lake](docs/adr/001-lake.md) · [research provenance](docs/research.md) · [design contract](docs/DESIGN.md) · [product spec](docs/SPEC.md) · [facet vocabulary (`domain.yml`)](docs/domain.yml.md) · [extension](extension/README.md) · [importer goldens](tests/goldens/README.md)

## License

MIT.
