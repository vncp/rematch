# Counters and the kill criterion

## What it counts

Each entry carries three raw counters, with a per-family breakdown. Tokens and turns are recorded for `why` and never gated on, because they vary tenfold with task difficulty.

| Counter | Increments when | Decides |
|---|---|---|
| **matched** | the session's repo matched the glob, its file extensions overlapped, and at least two predicate stems appeared in the first five inputs or the files read | the denominator for every decision; dormant at 12 without one |
| **recurrence** | the specific redirect class that motivated the entry happened again while the entry was loaded | retire at 2 |
| **harm** | a new tool-error cluster, a takeover, or a user disable while the entry was loaded | retire at 2 |

Keep at `matched ≥ 4 · recurrence 0 · harm 0`. Four matched sessions with no recurrence is evidence that the entry did its one job, nothing more; that is why the status is `kept`, not `verified`. A model never rewrites counters.

## Decision rules

From DESIGN §7:

Per matched session (predicate hit while the entry was in `loaded_entries`): `recurrence += 1` if a redirect of the entry's `redirect_class` occurred (classifier output, or prelabel ∈ {revert, negation, mark} when class unknown); `harm += 1` if a new tool-error cluster, an `interrupt`, or a user `disable` happened. Tokens/turns recorded.

Decide: `matched ≥ 4 && recurrence == 0 && harm == 0 → Keep`; `recurrence ≥ 2 || harm ≥ 2 → Retire`; `matched ≥ 12 → Dormant`; `provisional for 30 days && matched < 3 → Dormant`; `Kept && harm ≥ 2 within 30 days → Retire`; else `Continue`. Per-family breakdown kept; if one family shows harm ≥ 1 and another has matched ≥ 2 with zero harm/recurrence, the entry is split (Phase 3): the harmed family gets a sibling proposal.

## Rate limits

From DESIGN §8:

Per predicate_hash: ≤1 admission per UTC day; no reflection while a provisional entry with the same predicate_hash exists (pending canary). Per branch: ≤10 provisional; 5 kept general + 5 kept per branch before a new candidate must merge/revise (near-duplicate by simhash(normalized text) Hamming ≤ 6 or identical script body ⇒ append evidence to existing entry). Per top-level domain (§6 "Domain"): ≤ 8 kept entries across every branch on this machine (`policy.json` `caps.kept_per_domain`, `rematch policy caps set kept_per_domain <n>`). ≤3 model calls per reflection (classifier batch, editor, one lint-retry).

## The kill criterion

**A `kept` skill from daily use.** The three built-in hooks reached `kept` (9/6/5 matched sessions, 0 recurrence, 0 harm) during the build itself; no skill has yet. The kill criterion in the spec is six weeks of daily use: if no entry reaches `kept` over at least four matched sessions and recurrence has not fallen on any trigger, stop at Phase 1 and keep stock omp with hand-written skills. The trace tooling and `why` stay useful either way.

`rematch report` computes it instead of leaving it to be eyeballed: see [Reading the report](../guides/report.md).

---

Source: README.md ("What it counts", "Status"); docs/DESIGN.md §7 and §8 (quoted); docs/research.md D10, D13, D18.
