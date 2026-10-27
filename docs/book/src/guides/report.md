# Reading the report

```sh
rematch report --text                   # the last 6 weeks
rematch report --window 30d --text      # <n>w, <n>d or <n>h, ending now
rematch report --since 2026-09-15       # from that UTC day to now
```

SPEC's kill criterion, computed instead of eyeballed: after six weeks of daily use, has an entry reached `kept` over at least four matched sessions, and has recurrence fallen on a trigger the library covers? `report` answers both from the lake when there is one (every machine's history after `lake import`), else from this ledger's journal (`"source": "ledger"`); the output is the JSON envelope, `--text` prints the lines.

- **Kept over ≥ 4.** Every entry whose status at the window's end is `kept`, with the sessions in the window its counter rows matched (on any machine, by entry id or by content hash), their recurrence and harm.
- **Recurrence per trigger.** Entries are grouped by trigger predicate. Each session started in the window whose files and terms match the predicate (and whose domain fits the trigger's, see [Domains](../concepts/predicates-and-domains.md#domains)) falls in one of two arms: *with* (it loaded an entry of that trigger) or *before* (it loaded none). Each arm shows sessions, sessions where the trigger's redirect class recurred (the entry's motivating class; for a classless entry, a correction or preference), the redirects of that class, and the rate (recurred / sessions); then `Δ` = with − before, and `fell` when it is negative. A trigger is *covered* when its with-arm is not empty. Triggers are listed under their domain (`(none)` for a trigger without one), each domain headed by a summary line whose arms add up its triggers' (a session matching two triggers counts twice).
- **The verdict line.** `verdict: an entry reached kept over ≥ 4 matched sessions: yes (<names>)|no; recurrence fell on a covered trigger: yes (<triggers>)|no`. Built-in entries are listed but never count toward it.
- **As of the window's end.** Every row is read as its ledger held it then (a status or a redirect label written later does not count), so a report for a past window does not change when entries move on.

On the seed of `report`'s unit test (one trigger, both arms, after `lake export`; machine name shortened):

```
report · source lake (lag 0 journal row(s)) · machines laptop · window 2026-09-15 00:02 UTC → 2026-10-27 00:02 UTC (6w)
kept run-tests-before-commit: 4 matched session(s) in the window (≥ 4), recurrence 0, harm 0
domain (none): 2 trigger(s), 1 covered, 1 fell · with 0/4 recurred (0 redirect(s), rate 0.00) · before 2/4 recurred (2 redirect(s), rate 0.50)
  trigger run-tests-before-commit [6675c0a0d803] class correction: with 0/4 recurred (0 redirect(s), rate 0.00) · before 2/4 recurred (2 redirect(s), rate 0.50) · Δ -0.50 fell
verdict: an entry reached kept over ≥ 4 matched sessions: yes (run-tests-before-commit); recurrence fell on a covered trigger: yes (run-tests-before-commit)
```

With two domains in one monorepo (`web/` declared `web.frontend`, `ios/` declared `mobile`, a review skill learned in each, six sessions with the same words):

```
domain mobile: 1 trigger(s), 1 covered, 0 fell · with 2/3 recurred (2 redirect(s), rate 0.67) · before no sessions
  trigger ios-review [60b78f6a0c69] class correction|preference: with 2/3 recurred (2 redirect(s), rate 0.67) · before no sessions
domain web.frontend: 1 trigger(s), 1 covered, 0 fell · with 1/3 recurred (1 redirect(s), rate 0.33) · before no sessions
  trigger web-review [6f88fb35eae6] class correction|preference: with 1/3 recurred (1 redirect(s), rate 0.33) · before no sessions
```

With a profile, skills kept on another machine load here but are never counted here; `report` adds one line after its header, `profile-kept (not counted on this machine): N` (JSON `profile_kept`).

---

Source: moved from docs/MANUAL.md ("The kill-criterion report"); docs/DESIGN.md §2 (`report`), §19.3, §20.
