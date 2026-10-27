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

<!-- include: docs/DESIGN.md#entry-layout -->
