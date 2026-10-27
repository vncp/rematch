---
title: Lint rules
description: The static checks every staged entry passes, and the shipped lint.toml.
editUrl: https://github.com/vncp/rematch/edit/main/docs/DESIGN.md
---

Every staged entry passes lint. It is static and costs no model call; on an error the editor gets one retry with the findings. Why: [D15](/rematch/background/decisions/).

`improver/lint.toml` ships in the binary and is copied to `~/.rematch/improver/lint.toml` on first run. It is yours to edit; an upgrade refreshes it only while it still equals the previously shipped copy (DESIGN §1).

## Schema and rules

:::note[Generated]
From `docs/DESIGN.md` §12 (region `lint`) and `improver/lint.toml` by `docs/site/scripts/gen.ts`. Edit the source, then `bun run gen`.
:::

<!-- include: docs/DESIGN.md#lint -->

## The shipped `lint.toml`

<!-- include: improver/lint.toml lang=toml -->
