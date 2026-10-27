---
title: Trace schema
description: The per-session trace file the extension or an adapter writes and only the binary reads.
editUrl: https://github.com/vncp/rematch/edit/main/docs/DESIGN.md
---

One JSONL file per session. The omp extension or a harness adapter writes it; only the binary reads it. A trace is the only input to reflection ([the evidence boundary](/rematch/concepts/evidence-boundary/)).

:::note[Generated]
From `docs/DESIGN.md` §3 (region `trace`) by `docs/site/scripts/gen.ts`. Edit the source, then `bun run gen`.
:::

<!-- include: docs/DESIGN.md#trace -->
