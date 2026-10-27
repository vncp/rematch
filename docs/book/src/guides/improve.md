# Improving a skill

**Brownfield.** `rematch improve <skill>` registers the existing skill as a `provisional` entry in place (the file is never moved), audits the sessions that read it and the redirects and errors that happened with it in context, and proposes unit-level edits from the clustered redirects. A skill over 8 KB is split first, mechanically and with no model: router, `units/` by heading, `facts/`, long tables and rejected-hypothesis lists into a ledger-notes unit. The new version is a sibling beside the old one; omp shows the sibling under the bare name and the original under its namespaced name until the counters decide. A `pinned` skill is never replaced.

```
/rematch improve <skill>                # e.g. forge-portfolio-weighting
/rematch improve <skill> --split-only   # just the mechanical split, no model
```

What happens: the skill is registered as a provisional entry (its file is never moved), the sessions that read it are audited, and:

- fewer than 5 traced sessions used it → it refuses with the count and offers the split;
- over 8 KB → it is split mechanically into a router `SKILL.md` + `units/` + `facts/` and staged beside the original (omp shows the new one under the bare name, the original namespaced) until the counters decide;
- otherwise → one editor call proposes unit-level edits from the clustered redirects and tool-error clusters (imported sessions included: their excerpt is rebuilt from omp's session files), same lint, staged beside.

A pinned skill is refused.

## Importing compound-forge skills

`rematch import --forge <path>` registers compound-forge's skills as brownfield entries with predicates from their trigger text, and its `learnings/*.md` as lessons with their evidence hash. With `--migrate` the skill trees are copied into `staging/forge/` (scripts non-executable) so the forge plugin and checkout can be removed; the forge's own meta-skill is left behind. This is how compound-forge was retired on both of the author's machines.

---

Source: README.md ("Create and improve"); moved from docs/MANUAL.md ("Improving a skill you already have"); docs/DESIGN.md §14 ("Binary behavior notes": `improve`, `import --forge`).
