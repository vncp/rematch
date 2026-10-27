# Entry layout

Every entry is a directory: in `staging/<branch>/<name>/` while provisional, in `drafts/` as a draft, under `~/.omp/agent/skills/<name>/` (with a profile: `~/.rematch/profile/skills/<name>/`) once kept, in `archive/<status>/<id>/` when stale, dormant or retired ([Statuses](../concepts/entries.md#statuses)). Its files, from DESIGN §4:

{{#include ../../../DESIGN.md:entry-layout}}

The `kind` of an entry is one of `Skill, Rule, Script, Agent, Hook, Fact, Lesson`, its status one of `Draft, Provisional, Kept, Stale, Dormant, Retired, Pinned` (DESIGN §4). The editor proposes entries in the reply shape of DESIGN §9.

---

Source: docs/DESIGN.md §4 (included), §9; README.md ("Statuses").
