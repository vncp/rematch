# Creating a skill

**Greenfield.** `rematch create <name> --from sessions` searches the ledger (imported history plus traces; every machine's sessions through the lake when there is one) for sessions whose features match the request; with three or more, the skill is distilled from what worked and what was redirected. `--from task --task <id>` runs the task once in batch mode (one rollout, no candidates) to obtain a trace; this is the only create that costs a rollout. `--from description` stages a `draft`; `activate <name> --task <id>` or `--unverified` makes it `provisional`. The result is a skill package: a router `SKILL.md` with the trigger line and the checkable laws, `units/` for procedures and pitfalls, `facts/` for measured values with `as_of`, `scripts/` for anything deterministic. Below about 8 KB the package collapses to one file.

```
/rematch create <name> --from sessions                 # distil from ≥3 past sessions that match the name/description
/rematch create <name> --from task --task <task-id>    # one reference run of a registered task, then propose
/rematch create <name> --from description --description "…" [--files path …]
```

`--from description` produces a **draft**: visible in the library, never loaded. Activate it when you are willing to test it:

```
/rematch activate <name> --task <task-id>    # runs the task once with it, then provisional
/rematch activate <name> --unverified        # provisional on your say-so
```

## References by the commands they ran

`rematch create cargo-test-gate --from sessions --ran cargo,test` takes as references the sessions whose bash commands contain every word (case-folded, whole words: `cd x && cargo test --workspace` matches, `cargo build` does not), best overlap with the name/description first. Reads the lake's events (indexed with `rematch lake index`, else a scan) when the lake is current, else every recorded session. Still needs 3 sessions.

---

Source: README.md ("Create and improve"); moved from docs/MANUAL.md ("Creating a skill on purpose"); docs/DESIGN.md §2 (`create --ran`), §14 ("Binary behavior notes": `create --from sessions`), §19.2.
