---
title: Entry layout
description: The files of one library entry, wherever it lives.
editUrl: https://github.com/vncp/rematch/edit/main/docs/DESIGN.md
---

Every entry is a directory. Where it lives depends on its status ([entries and statuses](/rematch/concepts/entries/)):

| Status | Directory |
|---|---|
| draft | `~/.rematch/drafts/<name>/` |
| provisional | `~/.rematch/staging/<branch>/<name>/` |
| kept | `~/.omp/agent/skills/<name>/` (with a profile: `~/.rematch/profile/skills/<name>/`) |
| stale, dormant, retired | `~/.rematch/archive/<status>/<id>/` |

An entry's `kind` is one of `Skill`, `Rule`, `Script`, `Agent`, `Hook`, `Fact`, `Lesson`; its status one of `Draft`, `Provisional`, `Kept`, `Stale`, `Dormant`, `Retired`, `Pinned` (DESIGN §4). The editor proposes entries in the reply shape of [DESIGN §9](/rematch/background/design/).

## Files

:::note[Generated]
From `docs/DESIGN.md` §4 (region `entry-layout`) by `docs/site/scripts/gen.ts`. Edit the source, then `bun run gen`.
:::

Entry directory layout (`<dir>` = staging/drafts/archive/kept path):
```
<dir>/SKILL.md            frontmatter: name, description, rematch: {id, status, branch, family, version, predicate_hash}
<dir>/AGENT.md            kind=Agent instead of SKILL.md: frontmatter name (= entry name), description, optional model/tools/…, rematch: {…}; body = the subagent's system prompt
<dir>/rematch.json        serde of Entry + manifest + predicate (the ledger row mirrors it; rematch.json is authoritative for files, ledger for counters)
<dir>/units/*.md  facts/*.md (each fact has `as_of:`)  scripts/* (mode 0644 until approved; 0755 after)
<dir>/hook.json           kind=Hook only: HookRule {mode (block|truncate|stuck|retrieve, default block), tool (`*` = any), field + regex (block), message, pass_condition, max_age_turns + keep_chars (truncate), repeats (stuck), budget_bytes + min_overlap (retrieve)}; unset mode parameters take the defaults 6, 1200, 3, 6000, 2 (`HookRule::params`)
<dir>/AGENTS.fragment.md  optional: system-prompt text. Personal: approved via `library approve instructions:<id> --sha <hash>`, which writes a marked `<!-- rematch:begin name=<name> ... -->` block into the user's personal `<agent_dir>/AGENTS.md` (omp) and enables matched-session runtime insertion in Claude Code, Pi and OpenCode (§16 "System prompt"); `<repo>/AGENTS.md` is never written here (only through `ship --scope team`)
<dir>/fork/<basename>     optional: the text `ship --scope fork` writes to `<fork>/<fork_path>`
<dir>/EXPERIENCE.md       written by the reflector after keep/retire (read-only to the system)
```
