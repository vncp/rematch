# rematch manual

The short version: **use omp as usual.** rematch watches, proposes when you correct the agent, and keeps only what later sessions confirm. Everything below is optional.

- [Day to day](#day-to-day)
- [Reading what it tells you](#reading-what-it-tells-you)
- [Correcting it](#correcting-it)
- [Approving scripts and hooks](#approving-scripts-and-hooks)
- [Creating a skill on purpose](#creating-a-skill-on-purpose)
- [Improving a skill you already have](#improving-a-skill-you-already-have)
- [Learning from sessions before rematch was installed](#learning-from-sessions-before-rematch-was-installed)
- [Batch mode with a verifier](#batch-mode-with-a-verifier)
- [Subagents](#subagents)
- [Text for a repo's AGENTS.md](#text-for-a-repos-agentsmd)
- [Prompt and tool text for an omp fork](#prompt-and-tool-text-for-an-omp-fork)
- [Managing the library](#managing-the-library)
- [Turning it off, rolling back, uninstalling](#turning-it-off-rolling-back-uninstalling)
- [Where things are](#where-things-are)
- [Troubleshooting](#troubleshooting)

Every `/rematch …` command in omp is the same as `rematch …` in a terminal; use whichever is in front of you. Add `--text` in the terminal for readable output instead of JSON.

## Day to day

1. Work in omp. Nothing to do.
2. When you tell the agent it got something wrong (*"no, …"*, *"that's wrong"*, *"revert that"*, interrupting it, or three tool errors in a row), the session is flagged at exit.
3. Next time you open omp, a reflection runs in the background (≈20 s, up to 3 model calls). If it produces something, you see one line at the top of the session:

   ```
   staged skill src-lib-public-functions in my-repo: Use when adding or modifying public functions in src/lib.py.
   ```

   That skill is now **provisional**: it loads in sessions whose repo and task look like the one that produced it, and nowhere else.
4. Keep working. Each later session where it loaded is counted: did the same kind of correction happen again (recurrence), did something break (harm)? After 4 clean sessions it is **kept** and copied into `~/.omp/agent/skills/`. After 2 recurrences or 2 harms it is **retired**. You are never asked.

Costs: nothing during a session; the reflection runs between sessions on the fast/strong roles you configured (`rematch doctor --text` shows which).

## Reading what it tells you

| Ask | What you get |
|---|---|
| `/rematch status` | counts of entries by status, pending flags, whether it is off |
| `/rematch why` | for the last session: which entries were loaded, whether the predicate hit, the redirects it saw and how they were classified, the running counters |
| `/rematch library` | every entry with its raw counts: `provisional skill foo [repo] matched 2 · recurrence 0 · harm 0` |
| `/rematch digest` | what was staged, kept or retired since you last looked, plus anything waiting for approval |
| `rematch doctor --text` | omp version, resolved model roles, plugin linked, ledger state |

`why` is the one to read when something surprises you: it shows exactly why an entry was or was not loaded (`glob hit · predicate miss`) and what the classifier called your redirect.

## Correcting it

The classifier decides whether your redirect was a **correction** (the agent did it wrong), a **preference** (fine, but not how you like it), or a **scope** change (you changed your mind). Only corrections and preferences produce proposals. If it got that wrong, or missed a redirect entirely:

```
/rematch mark correction      # or preference | scope
```

This overrides the classifier for the last turn, flags the session, and the next reflection uses your label. You can also run it after the session: `rematch mark --session <id> --class correction` (ids are in `rematch library why`).

## Approving scripts and hooks

Skill *text* activates on its own. Anything executable does not:

- A proposed **script** is staged mode 0644 (not runnable). A proposed **hook** (a rule that blocks a tool call) is not installed.
- At session start the digest asks, once per item, with the diff: approve or decline. Decline retires it.
- From the terminal: `rematch library approve <id>` / `rematch library decline <id>`.

Approved scripts become 0755 in place; approved hooks are compiled to `~/.omp/agent/extensions/rematch-hooks/<id>.ts` and run on `tool_call`.

## Creating a skill on purpose

```
/rematch create <name> --from sessions                 # distil from ≥3 past sessions that match the name/description
/rematch create <name> --from task --task <task-id>    # one reference run of a registered task, then propose
/rematch create <name> --from description --description "…" [--files path …]
```

`--from description` produces a **draft**: visible in the library, never loaded. Activate it when you are willing to test it:

```
/rematch activate <name> --task <task-id>    # runs the task once with it, then provisional
/rematch activate <name> --unverified        # provisional on your say-so
```

## Improving a skill you already have

```
/rematch improve <skill>                # e.g. forge-portfolio-weighting
/rematch improve <skill> --split-only   # just the mechanical split, no model
```

What happens: the skill is registered as a provisional entry (its file is never moved), the sessions that read it are audited, and:

- fewer than 5 traced sessions used it → it refuses with the count and offers the split;
- over 8 KB → it is split mechanically into a router `SKILL.md` + `units/` + `facts/` and staged beside the original (omp shows the new one under the bare name, the original namespaced) until the counters decide;
- otherwise → one editor call proposes unit-level edits from the clustered redirects and tool-error clusters (imported sessions included: their excerpt is rebuilt from omp's session files), same lint, staged beside.

A pinned skill is refused.

## Learning from sessions before rematch was installed

```
rematch import --history                              # omp's session history → the ledger (no model)
rematch reflect --history [--since 2026-10-01] [--limit 10]
```

`reflect --history` treats every imported session that the extension would have flagged (a redirect, or a tool-error cluster) like a fresh one: it rebuilds the session's trace from omp's file, then runs the normal reflection (same rate limits, ≤ 3 model calls each). Newest first, at most `--limit` sessions per run (default 10); run it again for the next batch. A session is reflected once; the envelope lists each session's outcome.

## Batch mode with a verifier

For work that has a mechanical pass/fail (tests, a check script, a parity file, a receipt):

```
rematch task add <id> --repo <path> --prompt "…" --verifier "<cmd>" [--max-time 10m]
rematch run --task <id> --verifier "<cmd>" [--width 2]      # returns a job id, runs detached
rematch status <job-id>
```

The worker runs the task in a fresh git worktree with the current harness, proposes an invariant and a domain candidate, probes each, composes, re-runs, and lets the verifier pick. Four rollouts at the default width, ten at `--width 2`. The winner's lessons are staged **provisional**; they become kept only when the *next* run of that task passes with them loaded. The receipt is at `~/.rematch/runs/<task>/receipt.json`.

## Subagents

When the fix for a correction is "read this big thing and report the one line that matters" (a build log, a long diff, a data dump), the editor may propose a **subagent** (`kind: agent`) instead of a skill. It is an omp task agent: an `AGENT.md` with `name`, `description` (what the main agent sees when deciding to delegate) and optionally `model`, and a body that is the subagent's system prompt. It runs in its own context and returns text, so the big input never enters your session.

- **Provisional:** the entry lives in `~/.rematch/staging/<branch>/<name>/`, and `~/.rematch/agents/<name>.md` links to its `AGENT.md`. omp finds it through the plugin (`extension/agents` is a link to `~/.rematch/agents/`, made by `scripts/install.sh`). Unlike a skill it is offered in **every** session, because omp reads agents only from fixed places; it is counted only in sessions whose repo matches, like a skill.
- **Kept:** after 4 clean counted sessions it is copied to `~/.omp/agent/agents/<name>.md` (a file of yours with that name is saved first and comes back with `rollback` or `uninstall`) and the link is removed.
- **Retired, dormant, stale:** the link (or the kept copy) is removed; the entry is archived.
- A project or user agent with the same name wins over a provisional one (omp's precedence).
- `/rematch off` does not hide provisional agents; `library disable <id>` does.

## Text for a repo's AGENTS.md

`AGENTS.md` is always in context for everyone working in a repo, so rematch never writes it on your machine. An entry may carry an `AGENTS.fragment.md` (the editor proposes one for a rule that applies to every run in the repo). It reaches the repo only through a team branch:

```
rematch ship --scope team --entry <id> [--push --pr]
```

The branch adds the skill under `.omp/skills/<name>/` (without the fragment) and a section `## rematch: <name>` to `AGENTS.md` (created if the repo has none). Shipping a newer version replaces that section in place. Fragment headings start at `###`; the fragment counts toward the 8 KB always-on budget like skill text.

## Prompt and tool text for an omp fork

Per-model prompt text, tool descriptions and compaction defaults live in the omp source, so they ship to a fork checkout as a branch. Point an entry at the file it replaces once:

```
rematch library set-fork-path <id> src/prompts/tools/bash.md --repo ~/src/omp-fork
rematch ship --scope fork --entry <id>      # --repo defaults to the one recorded above
```

The file's text is `fork/<file name>` in the entry if it has one, else its `SKILL.md` body. `library ls --text` shows `fork:<path>` for such entries. The editor can propose the path itself (`fork_path`); the path must be relative to the fork without `..`.

## Managing the library

```
rematch library ls --text
rematch library pin <id>        # human-owned: never replaced or retired automatically
rematch library unpin <id>
rematch library disable <id>    # retire now (archived with its evidence; never deleted)
rematch library rollback <name> # restore the previous kept version (skill or agent), retire the current one
```

Sharing beyond your machine (all open a branch or a diff; nothing is merged for you):

```
rematch ship --scope team [--entry <id>…] [--push --pr]   # <repo>/.omp/skills/<name> (agents: .omp/agents/<name>.md) + AGENTS.md section, on a branch with evidence
rematch ship --scope fork [--repo <fork>]                  # fork_path entries, fingerprint + before/after numbers
rematch library set-fork-path <id> <path> [--repo <fork>]  # which fork file an entry replaces
rematch library export                                     # invariant entries, per-entry approval, to ~/.rematch/export
rematch transfer suggest | apply <id> --branch <b>         # invariant lessons across branches with overlapping facets
```

## Turning it off, rolling back, uninstalling

| | |
|---|---|
| `/rematch off` · `/rematch on` | stop/resume tracing and reflection immediately (`~/.rematch/off` marker) |
| `REMATCH=0 omp` | off for one process |
| `/rematch disable <id>` | retire one entry now |
| `/rematch rollback <skill>` | previous version back, immediately |
| `rematch uninstall` | prints what it would remove; `--yes` removes every file rematch wrote (root, hooks, kept entries, restoring predecessors) and nothing else |

## Where things are

```
~/.rematch/
  traces/            one JSONL per session (the only input to reflection)
  pending/           sessions flagged for the next reflection
  ledger.sqlite      sessions, entries, counters, proposals, receipts
  staging/<branch>/  provisional entries, loaded on match
  agents/            links to provisional agents (extension/agents points here)
  kept-agents/       entry dirs of kept agents
  drafts/ archive/   never loaded (archive is never deleted)
  improver/          editor.md, classifier.md, reflector.md, lint.toml
  jobs/ runs/ tasks/ batch mode
  off                kill-switch marker
~/.omp/agent/skills/<name>/              kept entries (predecessor in ~/.rematch/predecessors/)
~/.omp/agent/agents/<name>.md            kept agents (predecessor in ~/.rematch/predecessors/.agents/)
~/.omp/agent/extensions/rematch-hooks/   approved hooks
```

Roles, in `~/.omp/agent/config.yml`:

```yaml
modelRoles:
  rematch-editor: <provider>/<strongest model>      # falls back to slow → default
  rematch-classifier: <provider>/<fast model>       # falls back to smol → tiny → default
```

## Troubleshooting

- **Nothing ever gets staged.** `rematch status --text`: are sessions traced (count > 0)? If not, the extension is not loaded (`rematch doctor`, `omp plugin doctor`). If traced but never flagged, your redirects are not being pre-labelled; use `/rematch mark correction` once to see the loop work, and check `library why` for `prelabel`.
- **It staged something but a later session did not load it.** `rematch match --cwd .` shows the match decision; `library why <session>` shows `glob hit · predicate miss` when the task did not resemble the original (different file types or no overlapping terms). That is by design: the entry only counts where it would have mattered.
- **The reflection chose KEEP_BASE.** The editor decided no rule would prevent that redirect (usually a scope change). `library why` shows the class; mark it `correction` if you disagree.
- **A session ran reflection with the wrong model.** The editor refuses to proceed if the provider/model of any reply differs from the configured role; the pending file is kept and retried next start (3 attempts). `doctor --text` shows the resolved roles.
- **omp is not on PATH for the binary** (ssh, cron): set `OMP_BIN=/path/to/omp`.
