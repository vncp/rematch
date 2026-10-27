# Sharing to a team or fork

Sharing beyond your machine (all open a branch or a diff; nothing is merged for you):

```
rematch ship --scope team [--entry <id>…] [--push --pr]   # <repo>/.omp/skills/<name> (agents: .omp/agents/<name>.md) + AGENTS.md section, on a branch with evidence
rematch ship --scope fork [--repo <fork>]                  # fork_path entries, fingerprint + before/after numbers
rematch library set-fork-path <id> <path> [--repo <fork>]  # which fork file an entry replaces
rematch library export                                     # invariant entries, per-entry approval, to ~/.rematch/export
rematch transfer suggest | apply <id> --branch <b>         # invariant lessons across branches with overlapping facets
```

Which scope needs what approval: [Scope and who merges](../concepts/evidence-boundary.md#scope-and-who-merges).

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

---

Source: moved from docs/MANUAL.md ("Managing the library", "Text for a repo's AGENTS.md", "Prompt and tool text for an omp fork"); docs/DESIGN.md §15.
