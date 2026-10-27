# Entries and statuses

An entry is one item of the library, with its own trigger predicate, counters and status. Its kind is one of `skill`, `rule`, `script`, `agent`, `hook`, `fact` or `lesson` (DESIGN §4 `Kind`); its files are laid out as in [Entry layout](../reference/entry-layout.md).

## Statuses

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="../assets/lifecycle-dark.svg">
    <img alt="Entry statuses. Draft becomes provisional on a task run or --unverified. Provisional becomes kept at zero recurrences over at least four matched sessions with no harm, retired at two recurrences or two harm events, dormant after twelve matched sessions without a decision or thirty days with fewer than three matches, and back from dormant when the predicate hits again. Kept becomes stale when a fact source, the model family or the fork commit changed, or retired at two harm events in thirty days. Stale returns as a new canary or retires. Retired never leaves. Pinned is a flag." src="../assets/lifecycle-light.svg" width="100%">
  </picture>
</p>

| Status | Where it lives | Loaded |
|---|---|---|
| `draft` | `~/.rematch/drafts/<name>/` | never |
| `provisional` | `~/.rematch/staging/<branch>/<name>/` | on predicate match, via the skill paths; a subagent (`AGENT.md`) via the link `~/.rematch/agents/<name>.md` in every session, counted on match |
| `kept` | `~/.omp/agent/skills/<name>/` (subagents: `~/.omp/agent/agents/<name>.md`), predecessor in `predecessors/` | natively, every match |
| `stale` · `dormant` · `retired` | `~/.rematch/archive/<status>/<id>/` | never; never deleted |
| `pinned` | wherever it is | a flag; never replaced automatically |

Two notes from DESIGN §11: `pinned` is carried as a flag on the entry (status and location unchanged; automatic transitions refuse pinned entries; `disable` and `rollback` are user actions and proceed), and a kept entry replaced by a sibling becomes `dormant` (files in `predecessors/<name>/<version>/`) rather than `retired`, because `rollback` must be able to restore it and `retired` never leaves. Declining an approval retires the entry.

## Subagents

When the fix for a correction is "read this big thing and report the one line that matters" (a build log, a long diff, a data dump), the editor may propose a **subagent** (`kind: agent`) instead of a skill. It is an omp task agent: an `AGENT.md` with `name`, `description` (what the main agent sees when deciding to delegate) and optionally `model`, and a body that is the subagent's system prompt. It runs in its own context and returns text, so the big input never enters your session.

- **Provisional:** the entry lives in `~/.rematch/staging/<branch>/<name>/`, and `~/.rematch/agents/<name>.md` links to its `AGENT.md`. omp finds it through the plugin (`extension/agents` is a link to `~/.rematch/agents/`, made by `scripts/install.sh`). Unlike a skill it is offered in **every** session, because omp reads agents only from fixed places; it is counted only in sessions whose repo matches, like a skill. Claude Code gets a copy in the plugin's `agents/` at its next session start ([Claude Code](../get-started/install.md#claude-code)).
- **Kept:** after 4 clean counted sessions it is copied to `~/.omp/agent/agents/<name>.md` (a file of yours with that name is saved first and comes back with `rollback` or `uninstall`) and the link is removed.
- **Retired, dormant, stale:** the link (or the kept copy) is removed; the entry is archived.
- A project or user agent with the same name wins over a provisional one (omp's precedence; Claude Code also ranks plugin agents last).
- `/rematch off` does not hide provisional agents; `library disable <id>` does (in Claude Code, from its next session start).

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

---

Source: README.md ("Statuses"); moved from docs/MANUAL.md ("Subagents", "Where skills load from"); docs/DESIGN.md §4 (kinds), §11 (deviations 5–7), §16 ("Shared skills").
