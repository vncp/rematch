# `domain_rules.toml`

The table rematch uses to infer a session's domain when neither `policy.json` nor the branch's `domain.yml` names one ([Domains](../concepts/predicates-and-domains.md#domains), rule 3). It ships in the binary and is copied to `~/.rematch/improver/domain_rules.toml`; yours to edit, and an upgrade does not overwrite an edited copy. The shipped file:

```toml
{{#include ../../../../improver/domain_rules.toml}}
```

---

Source: improver/domain_rules.toml (included); docs/DESIGN.md §6 "Domain"; moved from docs/MANUAL.md ("Domains").
