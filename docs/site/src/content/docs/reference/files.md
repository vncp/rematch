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

```
traces/<session_id>.jsonl         extension / adapter trace (schema §3); Claude Code sessions `cc:<uuid>`, Codex `codex:<id>` (§18); on Windows the file name is the id through `paths::encode_file_stem` (`cc%3A<uuid>.jsonl`: `:` would address an NTFS stream), the id itself elsewhere
pending/<session_id>.json         {session_id, flagged_at, reasons:[...], trace}   written at shutdown; deleted by reflect; named like traces/
skills/<name>                     symlink → kept/pinned entry dir or profile/skills/<name>/ (kept library only, never provisional; §16)
views/<key>/skills/<name>         per-place view (`<branch>-<16hex sha256>`): kept library links + provisional entries matching that place (loaded by omp, Pi, OpenCode)
views/<key>/trials/<name>         provisional-only links matching a repo's top (reached by Codex through `<repo>/.agents/skills/rematch-trials`)
project-links.json                registry of repo-local items rematch wrote (`<repo>/.agents/skills/rematch-trials`, `<repo>/.claude/agents/<name>.md`), withdrawn on off/uninstall
personal-instructions/<16hex>.json  one state per personal `<agent_dir>/AGENTS.md` (16 hex of the sha256 of its path; every omp profile has its own agent dir under this one root): the target it describes, whether rematch created the file (removed only then, and only when empty after the last managed block goes) and its last failed write; the pre-target `personal-instructions.json` of earlier builds is never trusted and is deleted on the first save
.instructions.lock                OS lock held by every writer of a personal `AGENTS.md` and its state (§16 "Personal instructions")
probes/{records,suggestions}/     engine-owned comparative probe records (`<entry>.json`), replay suggestions, `declined-repos.json` and `.lock` (§7 "Comparative support")
ledger.sqlite  blobs/<sha256>     ledger (schema §5) + content-addressed blobs
staging/<branch>/<name>/          provisional entries (loaded through views/<key>/ once synced for that place; agents via agents/)
agents/<name>.md                  symlink → staging/<branch>/<name>/AGENT.md per approved provisional agent (`library approve`); omp hides other places' agents via `task.disabledAgents`; emptied on `off`
kept-agents/<name>/               entry dir of a kept agent (AGENT.md, rematch.json, EXPERIENCE.md); omp loads the copy ~/.omp/agent/agents/<name>.md
drafts/<name>/                    draft entries (never loaded)
archive/<status>/<id>/            retired|dormant|stale entries (never loaded, never deleted)
predecessors/<name>/<version>/    copies of replaced kept entries (rollback); agents: predecessors/.agents/<name>/<version>/ (version 0 = the user's own ~/.omp/agent/agents/<name>.md)
improver/{editor.md,composer.md,reflector.md,classifier.md,lint.toml,domain_rules.toml,omp-no-tools.yml}   shipped in the binary; written on first run and refreshed on upgrade only if the installed copy still matches the previously shipped hash (`.shipped/<name>`); EXPERIENCE.md files live next to entries
jobs/<job_id>.json  jobs/<job_id>.log      detached job state
runs/<task>/<seed>/<variant>/     batch workspaces {wt/, skills/, overlay.yml, events.jsonl} (deleted after receipt)
runs/<task>/receipt.json          last batch receipt;  runs/<task>/traces/<session>.jsonl  reference traces for `create --from task`
tasks/<task_id>.json              batch task definitions
branches/<branch>/domain.yml      facet vocabulary per branch; its `domain:` is the branch's work domain (§6 "Domain")
policy.json                       `environment`/`export_enabled` (§15 export), `skip_profiles`, `domains` and `caps` (§6 "Domain", §8); written only by `rematch policy`; with a profile a link to `profile/config/policy.json` (§20)
export/                           git-tracked work↔home export of invariant entries
profile/                          git working tree synced across machines (§20): skills/<name>/ (kept + adopted), agents/<name>.md, instructions/<name>.md (canonical approved personal instructions), fragments/<name>.md, hooks/<id>.ts, config/{policy.json,roles.yml,domains/<branch>.yml}, improver/<edited file>, profile.json; never removed by `uninstall`
.profile.lock  profile-sync.log  one `profile sync` at a time; the launchd job's output (§20)
builtins.version  .builtins.lock  hash of the shipped built-in entries (§10) and the install lock
lake/  lake-index/  .lake.lock   the lake (§19.1): journal + closed sessions as Parquet, written by `lake export`; Bellwether indexes per machine (L3, never synced); export/import/rebuild lock
off                               kill switch marker
```
