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
- [Claude Code](#claude-code)
- [Codex](#codex)
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

- A proposed **script** is staged mode 0644 (not runnable). A proposed **hook** is not installed, and until you approve it nothing of it loads or counts.
- At session start the digest asks, once per item, with the diff and the hook's pass condition: approve or decline. Decline retires it.
- From the terminal: `rematch library approve <id>` / `rematch library decline <id>`.

Approved scripts become 0755 in place; approved hooks are compiled to `~/.omp/agent/extensions/rematch-hooks/<id>.ts` (data only; the extension runs them). A hook has one of four modes:

| Mode | What it does |
|---|---|
| `block` | blocks a tool call whose input field matches a pattern, with the rule's message |
| `stuck` | blocks a call identical (same tool, same arguments) to the N calls right before it; any other call in between resets it |
| `truncate` | in the request sent to the model (never in the session file), tool outputs older than N user turns keep their first K characters and end with `…[rematch: truncated N chars, age K turns]` |
| `retrieve` | before each request, appends one hidden message with the units of your loaded skill packages that match what you just asked and the files just touched, within a byte budget |

### Built-in entries

Three hook entries ship with rematch and show up as approvals the first time the digest runs (branch `shared`, they match every session):

| Entry | Mode | Setting |
|---|---|---|
| `context-unit-retrieval` | `retrieve` | ≤ 6000 bytes, units sharing ≥ 2 stems with the request |
| `tool-output-truncation` | `truncate` | outputs older than 6 user turns keep 1200 characters |
| `stuck-detector` | `stuck` | the 4th identical call in a row is blocked |

They are ordinary entries: nothing runs until you approve one; once approved, it is counted in every session, retired after two sessions in which you redirected the agent (any classed correction, preference or scope redirect) or two with harm (error cluster, takeover, disable), and kept after four clean ones. Declining retires it for good. `rematch builtins ls` shows their status; `rematch builtins reinstall [name]` stages a retired one again.

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
rematch import --history [--harness omp|claude-code|codex|all] [--skip-profile <name>…]   # omp (default + profiles), Claude Code (~/.claude/projects) and Codex (~/.codex/sessions) history → the ledger (no model)
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

**Replacing a kept skill.** When a new version would replace a kept one (a sibling of it from `improve` or a per-family split, or a batch winner with the same name), rematch first re-runs the kept skill's own task once in a fresh worktree, with the new version loaded instead of the old. That task is the one the kept skill came from (or whose last run staged or loaded it), and it must have a verifier. If the verifier passes, the replacement goes ahead and the verdict is recorded in the new version's receipt. If it fails, the new version is retired with the note `anchor replay failed: …` and the kept skill stays. If there is no such task with a verifier, the new version's own clean sessions are the only check, as for any other entry. It is one run, not a test suite. To see what would happen without changing anything:

```
rematch anchor replay <id>     # runs the replay once and reports pass/fail; stages, keeps and retires nothing
```

## Subagents

When the fix for a correction is "read this big thing and report the one line that matters" (a build log, a long diff, a data dump), the editor may propose a **subagent** (`kind: agent`) instead of a skill. It is an omp task agent: an `AGENT.md` with `name`, `description` (what the main agent sees when deciding to delegate) and optionally `model`, and a body that is the subagent's system prompt. It runs in its own context and returns text, so the big input never enters your session.

- **Provisional:** the entry lives in `~/.rematch/staging/<branch>/<name>/`, and `~/.rematch/agents/<name>.md` links to its `AGENT.md`. omp finds it through the plugin (`extension/agents` is a link to `~/.rematch/agents/`, made by `scripts/install.sh`). Unlike a skill it is offered in **every** session, because omp reads agents only from fixed places; it is counted only in sessions whose repo matches, like a skill. Claude Code gets a copy in the plugin's `agents/` at its next session start ([Claude Code](#claude-code)).
- **Kept:** after 4 clean counted sessions it is copied to `~/.omp/agent/agents/<name>.md` (a file of yours with that name is saved first and comes back with `rollback` or `uninstall`) and the link is removed.
- **Retired, dormant, stale:** the link (or the kept copy) is removed; the entry is archived.
- A project or user agent with the same name wins over a provisional one (omp's precedence; Claude Code also ranks plugin agents last).
- `/rematch off` does not hide provisional agents; `library disable <id>` does (in Claude Code, from its next session start).

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
rematch library why <session> --asof 2026-10-20T13:05   # what the session loaded, with each entry's status and each redirect's class as they stood then
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

## Claude Code

The same binary, ledger and entries work in Claude Code through the plugin in `adapters/claude-code` (details and what it cannot do: [`DESIGN.md`](DESIGN.md) §18.1).

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

Provisional and kept skills appear as `rematch:<name>` skills through the shared skills directory (see [Where skills load from](#where-skills-load-from)): at session start the plugin syncs `~/.rematch/skills/` for the session's directory and points its own `skills/` there (created on first start; `rematch skills sync --harness claude-code --cwd <dir>` refreshes it by hand). Subagent entries appear as `rematch:<name>` agents: Claude Code reads plugin agents only from real files, so the same sync copies every provisional and kept agent into the plugin's `agents/` (an omp model role such as `@smol` is dropped, so the agent uses the session's model; a copy written at a session's start is counted from the next session; a file of yours there is never touched). Approved `block` and `stuck` hooks refuse tool calls; an approved `retrieve` hook adds matching units to your prompt; `truncate` hooks do not run in Claude Code. `rematch off` stops tracing and empties the shared skills directory at once. Upgrading from a version that linked skills into `~/.rematch/claude-skills/`: the first session start re-points the plugin's `skills/` and deletes that directory. To remove the plugin, delete the `~/.claude/skills/rematch` link; `rematch uninstall` removes `~/.rematch` and the agent copies.

Raise `SessionEnd`'s 1.5 s budget if large sessions end unflagged: `CLAUDE_CODE_SESSIONEND_HOOKS_TIMEOUT_MS=5000 claude`.

## Codex

Codex (CLI, IDE extension, Codex in the ChatGPT desktop app) gets the same entries and counters through its own files: rematch reads its session rollouts afterwards and puts skills where Codex looks for them. Nothing runs inside a Codex session; there are no hooks, no retrieval or truncation, and no live trace. Install steps and the full limitation list: [`adapters/codex/README.md`](../adapters/codex/README.md); format facts and the record mapping: [`DESIGN.md`](DESIGN.md) §18.2.

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

## Where skills load from

One directory, `~/.rematch/skills/`, holds a link `<name> → <entry dir>` for every skill entry that loads: every kept or pinned entry (its directory under `~/.omp/agent/skills/`), and every provisional entry whose predicate matched a directory that was synced (its `staging/<branch>/<name>/`). All three harnesses read it:

| Harness | Reads `~/.rematch/skills/` through | Synced for a directory by | You install |
|---|---|---|---|
| omp | `skills.customDirectories`, added by the extension for the process | the extension, at omp start (`skills sync --cwd`) | `scripts/install.sh` |
| Claude Code | the plugin's `skills/` link (made at the first session start) | the plugin's `SessionStart` hook | `ln -s /path/to/rematch/adapters/claude-code ~/.claude/skills/rematch` |
| Codex | `~/.agents/skills/rematch` | `rematch skills sync --harness codex --cwd "$PWD"` | `ln -s ~/.rematch/skills ~/.agents/skills/rematch` |

- **Loading is the union of what you work on.** A sync adds the provisional skills matching its directory and never removes one because this directory does not match it: a session in another repo may be using it. Each extra skill costs one description line in the system prompt.
- **Counting is not.** A session counts only the entries whose predicate matches its own directory, so a skill loaded from the shared directory in an unrelated repo never moves a counter.
- **Status changes apply at once.** Keeping an entry re-points its link to the kept directory; retiring, archiving or disabling one removes its link; `rematch off` removes every link and `rematch on` restores the kept ones. A session sees the directory as it was at its own start; a link removed while it runs dangles there.
- `rematch skills sync --cwd <dir>` runs the sync by hand and prints what it added and pruned. Directories in `~/.rematch/skills/` that are not links are never touched.
- `~/.rematch` is shared by every omp profile, and so is the directory: a skill kept under one profile's `skills/` is linked for sessions of every profile and harness.
- Subagents (`kind: agent`) are not skills: omp loads them (provisional ones through the extension's `agents/` link, kept ones from `~/.omp/agent/agents/`); Claude Code loads copies in its plugin's `agents/`, refreshed at each session start; Codex does not load them.

## The lake

The ledger keeps current state; the lake keeps history. Every write the ledger makes is journaled with the ledger's own clock, and `rematch lake export` copies the journal, plus the trace of every closed session, into Parquet files under `~/.rematch/lake/` that are written once and never changed. Nothing in a session reads it: `match`, `skills sync` and the hooks never open it, so a slow disk or a large lake costs a session nothing. Design: [ADR 001](adr/001-lake.md); file layout and columns: DESIGN §19.1.

```sh
rematch lake export            # journal rows since the last export + newly closed sessions (one bounded run)
rematch lake export --all      # drain everything in one run
rematch lake doctor --text     # watermarks, how far the export lags the ledger, files per table, events exported/missing/pending, stray files
rematch lake import --from /Volumes/laptop/.rematch     # merge another machine's lake (its root or its lake/ dir)
rematch lake rebuild --from ~/backup/lake               # a ledger from a lake, into a root that has none
rematch lake index                                      # Bellwether indexes for every table (needs a bellwether binary)
rematch lake query events --key expr:tool_cmd --values 'cargo test'          # the events of sessions that ran it
rematch lake query entries --key temporal2:entry_id --values <id> --valid 2026-10-20T13:05:00Z --known 2026-10-20T13:05:00Z
rematch lake query outcomes --key list:facets --values mechanism,universe --all
```

- **Export runs by itself.** The end of every `reflect --pending` exports one bounded run (at most 50 000 journal rows and 64 sessions); a second export with nothing new writes nothing. Run `lake export --all` after importing years of history.
- **Imported history is exported too.** A session imported from omp, Claude Code or Codex history has no trace file; its events file is built from the harness's own session file, the same way `reflect --history` reads it. If that file is gone, the session is recorded as missing in `lake/manifest.json` and skipped by later bounded runs; `lake export --all` tries it again. `lake doctor` prints `events: exported N, missing M, pending P`.
- **Where files live.** `lake/manifest.json` holds the per-machine watermarks (the last journal row in the lake) and which sessions have events (or are missing them); `lake/<table>/<day>/<machine>-<seq>.parquet` holds one export run of `sessions`, `redirects`, `entries`, `counters`, `proposals`, `receipts` (and `outcomes/<domain>/…`); `lake/events/<harness>/<day>/<session>.parquet` holds one closed session's trace; `lake/blobs/` the proposal replies and certificates those rows point at. Copy or sync the whole `lake/` directory freely; never sync `lake-index/` (per machine, L3).
- **Indexes are optional and local.** `lake index` asks an external `bellwether` binary (`--bellwether <path>`, else `$REMATCH_BELLWETHER`, else `PATH`) to index each table under `~/.rematch/lake-index/<table>/`, with the keys ADR 001 declares; run it after exports, or let `lake query` do it when it finds files the index has not seen. `lake query` answers one lookup (`exact:`/`expr:`/`list:` values, `--from/--to` windows, `--latest`, `--valid/--known` as-of) with the matching rows as JSON; with no binary or no index it reads every file instead and says `"indexed": false` and why. The rows are the same either way. The first index pins the Bellwether version in `lake/manifest.json`; another version is refused until `rematch lake index --repin` rebuilds the indexes (the lake files are not touched). `lake doctor` shows the pin, each table's index and missing keys, and the sessions that were counted with loaded entries but have no counters row. Keys and adaptations: DESIGN §19.2.
- **Two machines.** Each machine exports under its own name (`$REMATCH_MACHINE`, else the host name, fixed in the ledger at first use). `lake import --from <other>` copies the files you do not have and merges the watermarks; `lake doctor` then lists both machines. The other machine's rows are read-only here: they never enter this ledger.
- **Rebuild.** `lake rebuild` replays one machine's journal (yours unless `--machine`) into a fresh ledger: `library ls` and the counters come back exactly. It refuses a ledger that already holds anything; `--force` replaces it and keeps the old file as `ledger.sqlite.pre-rebuild-<ms>`. Traces, staging dirs and skills are files, not ledger rows: copy them with the root if you are moving machines.
- **Before the journal.** Rows that existed when the journal was added are journaled once as `snapshot` rows (their state at that moment, not their history); `lake doctor` shows that range.
- **After upgrading: `rematch lake migrate`.** Entries and sessions written before this version lack the identity that lets two machines (or two checkouts) recognize the same thing: the content hash of an entry and the repo's remote. `rematch lake migrate` fills them once: `content_sha256` on every entry, `repo_remote` + `subpath` on each entry whose repo is on this machine (its repo path stays as the fallback) and on each session whose repo is here. It reads files only, runs no model and no git, and a second run changes nothing. Until it runs, those entries match by repo path as before; afterwards a worktree or another clone of the same remote loads and counts them too.
- **Built without it.** The lake needs the cargo feature `lake` (on by default). `cargo build --no-default-features` (or `REMATCH_NO_LAKE=1 scripts/install.sh`) builds a binary without arrow; there every `lake` subcommand except `migrate` answers `built without the lake feature`.
- **Who reads it.** Historical commands read the lake when there is one and say so in their output (`"source": "lake"` or `"ledger"`): `reflect --history` and `improve` take a session's records from its events file (falling back to the trace or the harness transcript, so a deleted transcript no longer loses an exported session); `create --from sessions` searches every machine's `sessions`; `transfer suggest` reads every machine's outcome facets; `library why --asof` reads `entries` and `redirects`; `rematch report` (below) reads the lake alone. The first four read the ledger instead while the lake is behind it (`lake doctor` shows the lag; `lake export` closes it), so an export lag never hides a local write; the session-start digest never reads the lake.

## The kill-criterion report

```sh
rematch report --text                   # the last 6 weeks
rematch report --window 30d --text      # <n>w, <n>d or <n>h, ending now
rematch report --since 2026-09-15       # from that UTC day to now
```

SPEC's kill criterion, computed instead of eyeballed: after six weeks of daily use, has an entry reached `kept` over at least four matched sessions, and has recurrence fallen on a trigger the library covers? `report` answers both from the lake when there is one (every machine's history after `lake import`), else from this ledger's journal (`"source": "ledger"`); the output is the JSON envelope, `--text` prints the lines.

- **Kept over ≥ 4.** Every entry whose status at the window's end is `kept`, with the sessions in the window its counter rows matched (on any machine, by entry id or by content hash), their recurrence and harm.
- **Recurrence per trigger.** Entries are grouped by trigger predicate. Each session started in the window whose files and terms match the predicate falls in one of two arms: *with* (it loaded an entry of that trigger) or *before* (it loaded none). Each arm shows sessions, sessions where the trigger's redirect class recurred (the entry's motivating class; for a classless entry, a correction or preference), the redirects of that class, and the rate (recurred / sessions); then `Δ` = with − before, and `fell` when it is negative. A trigger is *covered* when its with-arm is not empty.
- **The verdict line.** `verdict: an entry reached kept over ≥ 4 matched sessions: yes (<names>)|no; recurrence fell on a covered trigger: yes (<triggers>)|no`. Built-in entries are listed but never count toward it.
- **As of the window's end.** Every row is read as its ledger held it then (a status or a redirect label written later does not count), so a report for a past window does not change when entries move on.

On the seed of `report`'s unit test (one trigger, both arms, after `lake export`; machine name shortened):

```
report · source lake (lag 0 journal row(s)) · machines laptop · window 2026-09-14 21:30 UTC → 2026-10-26 21:30 UTC (6w)
kept run-tests-before-commit: 4 matched session(s) in the window (≥ 4), recurrence 0, harm 0
trigger run-tests-before-commit [6675c0a0d803] class correction: with 0/4 recurred (0 redirect(s), rate 0.00) · before 2/4 recurred (2 redirect(s), rate 0.50) · Δ -0.50 fell
verdict: an entry reached kept over ≥ 4 matched sessions: yes (run-tests-before-commit); recurrence fell on a covered trigger: yes (run-tests-before-commit)
```

## Where things are

```
~/.rematch/
  traces/            one JSONL per session (the only input to reflection)
  pending/           sessions flagged for the next reflection
  ledger.sqlite      sessions, entries, counters, proposals, receipts
  staging/<branch>/  provisional entries, loaded through skills/ once a synced directory matched them (staging/shared/ holds the built-in hook entries)
  skills/            links to every skill entry that loads, read by omp, Claude Code and Codex (see Where skills load from)
  agents/            links to provisional agents (extension/agents points here)
  kept-agents/       entry dirs of kept agents
  drafts/ archive/   never loaded (archive is never deleted)
  improver/          editor.md, classifier.md, reflector.md, lint.toml
  jobs/ runs/ tasks/ batch mode
  builtins.version   hash of the shipped built-in entries (installed once; retired ones stay retired)
  lake/              the journal and closed sessions as Parquet (see The lake); never read by a session
  lake-index/        Bellwether indexes for this machine (L3; never synced)
  off                kill-switch marker
~/.omp/agent/skills/<name>/              kept entries (predecessor in ~/.rematch/predecessors/)
~/.omp/agent/agents/<name>.md            kept agents (predecessor in ~/.rematch/predecessors/.agents/)
~/.omp/agent/extensions/rematch-hooks/   approved hooks (data-only modules: block, stuck, truncate, retrieve)
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
- **Profiles (`omp --profile <name>` / `OMP_PROFILE`).** The agent directory follows omp's profile resolution (`~/.omp/profiles/<name>/agent` when `OMP_PROFILE` or `PI_PROFILE` is set, else `$PI_CODING_AGENT_DIR`, else `~/.omp/agent`; `PI_CONFIG_DIR` replaces `~/.omp`), while `~/.rematch` stays shared unless `REMATCH_ROOT` is set. `import --history` imports the default agent dir and every profile's sessions except profiles listed in `--skip-profile <name>` or `~/.rematch/policy.json` (`{"skip_profiles":["…"]}`). `rematch doctor --text` shows the resolved `agent_dir` and which profiles exist or are skipped.
