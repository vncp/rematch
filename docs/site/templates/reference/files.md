---
title: File locations
description: Where rematch keeps its state, and which files omp and the other harnesses read.
editUrl: https://github.com/vncp/rematch/edit/main/docs/DESIGN.md
---

Everything rematch writes lives under `~/.rematch/`, plus the kept entries, approved hooks and approved personal-instruction blocks it places in omp's agent dir, and the git-excluded repo-local trial views for Claude Code and Codex. The root moves with `REMATCH_ROOT`; the agent dir follows omp's profile resolution ([environment variables](/rematch/reference/environment/)).

## At a glance

```text
~/.rematch/
  traces/                     one JSONL per session (the only input to reflection)
  pending/                    sessions flagged for the next reflection
  ledger.sqlite               sessions, entries, counters, proposals, receipts
  staging/<branch>/           provisional entries (staging/shared/ holds the built-in hook entries)
  skills/                     links to kept/pinned skill entries and profile kept sources (never provisional)
  views/<key>/skills/         per-place view: kept links + provisional entries matching that place (omp, Pi, OpenCode)
  views/<key>/trials/         provisional-only links matching a repo's top (Codex repo trial link target)
  project-links.json          registry of repo-local items rematch wrote (withdrawn on off/uninstall)
  personal-instructions/      per personal AGENTS.md: whether rematch created it, its last failed write
  probes/{records,suggestions}/ engine-owned comparative probe records, replay suggestions, declined-repos.json
  agents/                     links to approved provisional agents (extension/agents points here; omp scopes via disabledAgents)
  kept-agents/                entry dirs of kept agents
  drafts/ archive/            never loaded (archive is never deleted)
  improver/                   editor.md, classifier.md, reflector.md, lint.toml, domain_rules.toml
  policy.json                 export policy, skipped profiles, declared domains and caps (written by rematch policy)
  branches/<b>/               domain.yml: the branch's domain and facet keys
  profile/                    your profile git repo: kept/adopted skills and agents, instructions/, hooks, config, edited improver files
  jobs/ runs/ tasks/          batch mode and authorized probe tasks
  builtins.version            hash of the shipped built-in entries (installed once; retired ones stay retired)
  lake/                       the journal and closed sessions as Parquet; never read by a session
  lake-index/                 Bellwether indexes for this machine (never synced)
  off                         kill-switch marker
~/.omp/agent/AGENTS.md                   personal context file (approved blocks between <!-- rematch:begin/end --> markers)
~/.omp/agent/skills/<name>/              kept entries (predecessor in ~/.rematch/predecessors/)
~/.omp/agent/agents/<name>.md            kept agents (predecessor in ~/.rematch/predecessors/.agents/)
~/.omp/agent/extensions/rematch-hooks/   approved hooks (data-only modules: block, stuck, truncate, retrieve)
```

With a profile, the `~/.omp/agent/{skills,agents,extensions/rematch-hooks}` locations and `policy.json` are links into `~/.rematch/profile/`, and `profile/instructions/<name>.md` syncs approved personal instructions into each machine's `<agent_dir>/AGENTS.md` ([the profile](/rematch/concepts/profile/)). Model roles live in `~/.omp/agent/config.yml` or the profile's `config/roles.yml` ([model roles](/rematch/reference/configuration/model-roles/)).

## Everything under `REMATCH_ROOT`

:::note[Generated]
From `docs/DESIGN.md` §1 (region `files`) by `docs/site/scripts/gen.ts`. Edit the source, then `bun run gen`.
:::

<!-- include: docs/DESIGN.md#files -->
