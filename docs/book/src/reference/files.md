# File locations

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
  improver/          editor.md, classifier.md, reflector.md, lint.toml, domain_rules.toml (domain inference table)
  policy.json        work/home export policy, skipped profiles, declared domains and caps (written by `rematch policy`; a link into profile/ once a profile exists)
  branches/<b>/      domain.yml: the branch's domain and facet keys
  profile/           your profile git repo (see Profile): kept and adopted skills/agents, hooks, config, edited improver files
  jobs/ runs/ tasks/ batch mode
  builtins.version   hash of the shipped built-in entries (installed once; retired ones stay retired)
  lake/              the journal and closed sessions as Parquet (see The lake); never read by a session
  lake-index/        Bellwether indexes for this machine (L3; never synced)
  off                kill-switch marker
~/.omp/agent/skills/<name>/              kept entries (predecessor in ~/.rematch/predecessors/); with a profile, links into ~/.rematch/profile/skills/
~/.omp/agent/agents/<name>.md            kept agents (predecessor in ~/.rematch/predecessors/.agents/); with a profile, links into ~/.rematch/profile/agents/
~/.omp/agent/extensions/rematch-hooks/   approved hooks (data-only modules: block, stuck, truncate, retrieve); with a profile, links into ~/.rematch/profile/hooks/
```

Model roles live in `~/.omp/agent/config.yml` or the profile's `config/roles.yml`: [Configuration](config.md#model-roles). The root moves with `REMATCH_ROOT`; the agent dir follows omp's profile resolution ([Environment variables](environment.md)).

## Everything under `REMATCH_ROOT`

DESIGN §1, included as-is:

{{#include ../../../DESIGN.md:files}}

---

Source: moved from docs/MANUAL.md ("Where things are"); docs/DESIGN.md §1 (included).
