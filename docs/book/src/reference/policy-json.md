# `policy.json`

`~/.rematch/policy.json` holds the work/home export policy, the omp profiles to skip on import, declared domains and caps (DESIGN §1). `rematch policy` is its writer: an atomic replace that keeps the other keys. With a profile, `policy.json` is a link to `profile/config/policy.json` (DESIGN §20). An absent file means the defaults.

## Keys

| Key | Shape | Meaning | Written by |
|---|---|---|---|
| `domains` | `[{"match": {"repo_remote": "...", "subpath": "..."}, "domain": "..."}]` | declared domains, first match wins; kept most specific first (with a subpath, then an exact remote, then globs) | `rematch policy domain set|unset` |
| `caps` | `{"kept_per_domain": <n>}` | kept entries per top-level domain across branches (default 8) | `rematch policy caps set kept_per_domain <n>` |
| `skip_profiles` | `["<profile>", …]` | omp profiles `import --history` skips, merged with `--skip-profile` | — |
| `environment` | `"work"` or `"home"` | missing ⇒ home; `library export` refuses at work unless `export_enabled` | — |
| `export_enabled` | bool | allows `library export` at work | — |

No `rematch policy` subcommand sets `skip_profiles`, `environment` or `export_enabled` (`rematch policy --help` lists `show`, `domain` and `caps`); the docs do not say how those keys are meant to be written.

## Example

On an empty root, `rematch policy domain set github.com/acme/web web.frontend` followed by `rematch policy caps set kept_per_domain 6` leaves:

```json
{
  "caps": {
    "kept_per_domain": 6
  },
  "domains": [
    {
      "domain": "web.frontend",
      "match": {
        "repo_remote": "github.com/acme/web"
      }
    }
  ]
}
```

and `rematch policy show --text` on a root without the file prints:

```
policy <root>/policy.json (absent)
domains: none declared
caps: kept_per_domain 8
```

How a declared domain is used: [Domains](../concepts/predicates-and-domains.md#domains).

---

Source: docs/DESIGN.md §1, §6 "Domain", §8, §15 (`library export`), §20; moved from docs/MANUAL.md ("Domains", "Troubleshooting"); `rematch policy --help`; the example ran `rematch` 0.1.0 against an empty `REMATCH_ROOT`.
