---
title: domain_rules.toml
description: The table that infers a session's domain when nothing declares one.
editUrl: https://github.com/vncp/rematch/edit/main/improver/domain_rules.toml
---

The table rematch uses to infer a session's domain when neither `policy.json` nor the branch's `domain.yml` names one (rule 3 of [domains](/rematch/concepts/predicates-and-domains/#domains)).

| | |
|---|---|
| Ships in | the binary |
| Copied to | `~/.rematch/improver/domain_rules.toml` |
| Yours to edit | yes; an upgrade does not overwrite an edited copy |

## The shipped file

:::note[Generated]
From `improver/domain_rules.toml` by `docs/site/scripts/gen.ts`. Edit the source, then `bun run gen`.
:::

<!-- include: improver/domain_rules.toml lang=toml -->
