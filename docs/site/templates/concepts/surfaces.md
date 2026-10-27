---
title: The eight surfaces
description: The eight parts of a harness rematch can change, and what each needs before it activates.
editUrl: https://github.com/vncp/rematch/edit/main/docs/DESIGN.md
---

A harness has eight parts an improver can edit. rematch maps each to one omp component and gives each its own activation rule: text in your personal scope activates as provisional, anything executable waits for your approval, anything beyond your machine waits for a merged PR.

Who approves what, and how team and fork changes leave your machine: [the evidence boundary](/rematch/concepts/evidence-boundary/#scope-and-who-merges).

## Surface by surface

:::note[Generated]
From `docs/DESIGN.md` §16 (region `surfaces`) by `docs/site/scripts/gen.ts`. Edit the source, then `bun run gen`.
:::

<!-- include: docs/DESIGN.md#surfaces -->

## See also

- [Approve scripts and hooks](/rematch/guides/approve-scripts-and-hooks/) and [share to a team or fork](/rematch/guides/share/).
- [Entry layout](/rematch/reference/entry-layout/): the files each surface lives in.
- Why eight: [D3](/rematch/background/decisions/).
