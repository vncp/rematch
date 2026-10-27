# Configuration

What you can configure, one page each: the model roles (below), [`policy.json`](policy-json.md), the domain inference table [`domain_rules.toml`](domain-rules.md), and per-branch [`domain.yml`](domain-yml.md). The [lint rules](lint.md) and the improver prompts are files you may edit too.

## Model roles

Roles, in `~/.omp/agent/config.yml` (a profile's `config/roles.yml` answers first, per machine; see [Syncing machines with the profile](../guides/profile.md)):

```yaml
modelRoles:
  rematch-editor: <provider>/<strongest model>      # falls back to slow → default
  rematch-classifier: <provider>/<fast model>       # falls back to smol → tiny → default
```

**Model roles per machine** in `profile/config/roles.yml`, read before `~/.omp/agent/config.yml`:

```yaml
editor:                       # rematch-editor; also classifier, solver, or any modelRoles key
  default: anthropic/claude-opus-5-5
  Vincent-M5M.local: openai/gpt-5     # this hostname only
classifier: fakeprov/small-model      # every machine
```

`rematch doctor --text` shows `role rematch-editor: … (via profile:rematch-editor)`; `rematch profile status --text` lists this machine's roles.

Resolution, from DESIGN §0 and §14: the binary reads `modelRoles` from `config.yml` (the agent dir's, with `<repo>/.omp/config.yml` as overlay); `rematch-editor` falls back to `slow` → `default`, `rematch-classifier` to `smol` → `tiny` → `default`, and `rematch-solver` (batch rollouts) to `default`. The reflector uses the `rematch-editor` role. A reply from a different provider or model than the role names is a failed run, never a fallback (DESIGN §9). There is no judge role.

## Improver files

From DESIGN §1: `improver/{editor.md,composer.md,reflector.md,classifier.md,lint.toml,domain_rules.toml,omp-no-tools.yml}` are shipped in the binary, written to `~/.rematch/improver/` on first run, and refreshed on upgrade only if the installed copy still matches the previously shipped hash (`.shipped/<name>`). The system never amends them (D25). `REMATCH_IMPROVER=<dir>` makes the binary take them from that directory instead of the embedded copies (DESIGN §14, `app.rs`). With a profile, an improver file you edited is synced through `profile/improver/` (DESIGN §20).

---

Source: moved from docs/MANUAL.md ("Where things are", "Profile: one setup on every machine"); docs/DESIGN.md §0, §1, §9, §14, §20; README.md ("Models", "Architecture").
