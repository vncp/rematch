# The profile

> Live-verified 2026-10-26 between two Macs over a private GitHub repo: hand-written subagent prompts adopted on both (identical copies became links), a skill added on one machine appeared on the other at the next sync, a deletion propagated back, launchd syncs every 5 minutes on each.

What rematch keeps on one machine should load on the next. `rematch profile init --remote <git url> --adopt` turns `~/.rematch/profile/` into a git repo of your omp setup — kept skills and agents, the skills and agents you wrote by hand (adopted as pinned entries), approved hooks, `policy.json`, per-branch `domain.yml`, improver prompts you edited, and `config/roles.yml` for the model behind each role on each machine — and links every piece back where omp and rematch read it. Another machine runs `rematch profile init --remote <git url>` once; from then on each omp process syncs in the background at session start (commit, pull with rebase, relink, push; bounded, offline is fine), keeps land in the profile, and `rematch profile install-launchd` adds a timer if you want one. A file changed on two machines stops the sync with the tree clean and a digest line naming the file. The ledger, counters, traces, staging and credentials stay per machine; history travels through the lake. Details: [Syncing machines with the profile](../guides/profile.md), [DESIGN §20](../design/design.md).

```
profile/
  skills/<name>/  agents/<name>.md  fragments/<name>.md  hooks/<id>.ts
  config/policy.json  config/roles.yml  config/domains/<branch>.yml
  improver/<edited file>  profile.json
```

**Per machine, never in the profile:** the ledger and its journal, the lake and its indexes, `staging/` (provisional entries), `drafts/`, `archive/`, `predecessors/`, traces, pending files, jobs, credentials and omp's own config (concrete model selectors only through `roles.yml`), and inside `skills/<name>/` the ledger mirror `rematch.json` (absolute paths, this ledger's state) and the reflector's `EXPERIENCE.md`. History is the lake's (§19), not the profile's.

**Kept sources.** `library::sync_shared_skills` adds every `profile/skills/<name>/` with a `SKILL.md` (`profile::kept_sources`) to `REMATCH_ROOT/skills/` for names no ledger entry holds, so entries kept on another machine and adopted skills load like this machine's kept entries. They are files, not ledger entries, on machines that did not keep them: they load but are counted only by the machine whose ledger holds the entry, and a hook linked from another machine runs only where its entry is known (built-in entries have per-machine ids and are approved per machine).

Setting it up, syncing, roles per machine and conflicts: [Syncing machines with the profile](../guides/profile.md). Full contract: DESIGN §20.

---

Source: README.md ("One profile, every machine"); docs/DESIGN.md §20 (quoted).
