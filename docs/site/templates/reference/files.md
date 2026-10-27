---
title: File locations
description: Where rematch keeps its state, and which files omp and the other harnesses read.
editUrl: https://github.com/vncp/rematch/edit/main/docs/DESIGN.md
---

Everything rematch writes lives under `~/.rematch/`, plus the kept entries and approved hooks it places in omp's agent dir. The root moves with `REMATCH_ROOT`; the agent dir follows omp's profile resolution ([environment variables](/rematch/reference/environment/)).

## At a glance

```text
~/.rematch/
  traces/            one JSONL per session (the only input to reflection)
  pending/           sessions flagged for the next reflection
  ledger.sqlite      sessions, entries, counters, proposals, receipts
  staging/<branch>/  provisional entries (staging/shared/ holds the built-in hook entries)
  skills/            links to every skill entry that loads, read by omp, Claude Code and Codex
  agents/            links to provisional agents (extension/agents points here)
  kept-agents/       entry dirs of kept agents
  drafts/ archive/   never loaded (archive is never deleted)
  improver/          editor.md, classifier.md, reflector.md, lint.toml, domain_rules.toml
  policy.json        export policy, skipped profiles, declared domains and caps (written by rematch policy)
  branches/<b>/      domain.yml: the branch's domain and facet keys
  profile/           your profile git repo: kept and adopted skills/agents, hooks, config, edited improver files
  jobs/ runs/ tasks/ batch mode
  builtins.version   hash of the shipped built-in entries (installed once; retired ones stay retired)
  lake/              the journal and closed sessions as Parquet; never read by a session
  lake-index/        Bellwether indexes for this machine (never synced)
  off                kill-switch marker
~/.omp/agent/skills/<name>/              kept entries (predecessor in ~/.rematch/predecessors/)
~/.omp/agent/agents/<name>.md            kept agents (predecessor in ~/.rematch/predecessors/.agents/)
~/.omp/agent/extensions/rematch-hooks/   approved hooks (data-only modules: block, stuck, truncate, retrieve)
```

With a profile, the three `~/.omp/agent/` locations and `policy.json` are links into `~/.rematch/profile/` ([the profile](/rematch/concepts/profile/)). Model roles live in `~/.omp/agent/config.yml` or the profile's `config/roles.yml` ([model roles](/rematch/reference/configuration/model-roles/)).

## Everything under `REMATCH_ROOT`

:::note[Generated]
From `docs/DESIGN.md` §1 (region `files`) by `docs/site/scripts/gen.ts`. Edit the source, then `bun run gen`.
:::

<!-- include: docs/DESIGN.md#files -->
