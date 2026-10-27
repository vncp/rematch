# `branches/<branch>/domain.yml`

Each branch (a repo or project; `library::branch_for` names it) can register its facet
vocabulary in `REMATCH_ROOT/branches/<branch>/domain.yml`. Outcome records use these keys
in their `facets` (SPEC §Session outcome records). Cross-domain transfer reads them
(`rematch transfer suggest`, `scope::transfer`).

```yaml
domain: <dotted subject path>   # prefix-seekable: quant.options, repo.payments, 3d.animation
facets: [<key>, <key>, ...]     # structural facet keys of this branch
```

- `domain` (optional): the dotted `subject.domain` prefix this branch's outcomes use. It is
  also the branch's work domain (DESIGN §6 "Domain"): a session in the branch takes it unless
  `policy.json` declares one for its remote (which wins), and a `transfer apply` copy into
  the branch takes it as its trigger's domain. Lowercase dotted segments of `[a-z0-9_-]`;
  run `rematch lake migrate` after changing it to re-decide recorded sessions.
- `facets`: the structural keys. Order does not matter; keys are compared exactly after trimming.

**Transfer rule.** `transfer suggest` lists a kept lesson tagged `invariant` from
branch A for branch B when A's and B's facet keys share at least two keys
(`MIN_OVERLAP = 2`) and B has no entry with the same name, a `transfers_from` link to
the lesson, or near-duplicate text. If a branch has no `domain.yml`, or the file lists
no `facets`, its keys are the distinct facet keys of its outcome records in the
ledger (`outcome_facets` joined to `outcomes.branch`). `transfer apply <lesson>
--branch <b>` stages a provisional copy in B with lint `transfer: true`, which blocks
domain lessons and `kind: fact` entries. Workspace `facts/` are dropped from the copy.

## Examples

Quant research (`branches/quant/domain.yml`):

```yaml
domain: quant.options
facets: [mechanism, universe, horizon, data_snapshot]
```

Application repo (`branches/payments/domain.yml`):

```yaml
domain: repo.payments
facets: [module, change_type, test_scope]
```

3D production (`branches/exercise-rigs/domain.yml`):

```yaml
domain: 3d.animation
facets: [rig, exercise, bake]
```

These three share no keys, so none of them receives the others' lessons. A second
quant branch with `facets: [mechanism, horizon, strike]` overlaps `quant` on
`mechanism` and `horizon` (two keys), so kept invariant lessons from `quant` are
suggested for it.
