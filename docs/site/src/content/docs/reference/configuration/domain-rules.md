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

```toml title="improver/domain_rules.toml"
# Domain inference table (docs/DESIGN.md §6 "Domain"). Used only when neither
# policy.json `domains` nor the branch's `domain.yml` names the session's domain.
#
# Score per top-level domain = ext_weight × (distinct session extensions in its `[[ext]]`
# rules) + stem_weight × (distinct session stems in its `[[stem]]` rules). The best domain
# wins when its score ≥ min_score and beats the runner-up by ≥ min_margin; otherwise the
# session has no domain. A second level (`web.frontend`) is added only by a `[[stem]]`
# rule whose stems hit. Stems are compared Porter-stemmed (`animation` = `anim`).

ext_weight = 2
stem_weight = 1
min_score = 2
min_margin = 1

[[ext]]
domain = "web"
exts = ["tsx", "jsx", "css", "html", "vue", "svelte"]

[[ext]]
domain = "mobile"
exts = ["swift", "kt", "m", "dart"]

[[ext]]
domain = "3d"
exts = ["blend", "fbx", "gltf", "usd", "anim"]

[[ext]]
domain = "data"
exts = ["parquet", "sql", "ipynb", "csv"]

[[ext]]
domain = "backend"
exts = ["rs", "go", "c", "cpp", "java"]

[[stem]]
domain = "quant"
stems = ["backtest", "sharpe", "nbbo", "option", "strategi", "portfolio"]

[[stem]]
domain = "3d.animation"
stems = ["anim", "rig", "bake", "skeleton", "keyframe"]

[[stem]]
domain = "web.frontend"
stems = ["ui", "component", "css", "layout", "a11y"]

[[stem]]
domain = "mobile"
stems = ["ios", "android", "swiftui", "compose"]
```
