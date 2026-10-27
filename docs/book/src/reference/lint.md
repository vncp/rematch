# Lint rules

Lint is static and free: every staged entry passes it, and on an error the editor gets one retry with the findings (README, "How a reflection works"). The schema and the rules, DESIGN §12, included as-is:

{{#include ../../../DESIGN.md:lint}}

## The shipped `lint.toml`

`improver/lint.toml` as it ships in the binary and is copied to `~/.rematch/improver/lint.toml` on first run (yours to edit; an upgrade refreshes it only while it still equals the previously shipped copy, DESIGN §1):

```toml
{{#include ../../../../improver/lint.toml}}
```

---

Source: docs/DESIGN.md §12 (included), §1; improver/lint.toml (included); README.md ("How a reflection works", "Highlights").
