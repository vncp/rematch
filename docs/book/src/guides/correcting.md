# Correcting the classifier

The classifier decides whether your redirect was a **correction** (the agent did it wrong), a **preference** (fine, but not how you like it), or a **scope** change (you changed your mind). Only corrections and preferences produce proposals. If it got that wrong, or missed a redirect entirely:

```
/rematch mark correction      # or preference | scope
```

This overrides the classifier for the last turn, flags the session, and the next reflection uses your label. You can also run it after the session: `rematch mark --session <id> --class correction` (ids are in `rematch library why`).

## In Claude Code

`/rematch:rematch-mark correction` (or `preference`, `scope`) is `/rematch mark correction` in omp: it overrides the classifier for the last turn of this session. Only you can run it; Claude cannot invoke it.

## In Codex

Codex has no `/rematch` command. Either write the mark as a message in the Codex session
(`/rematch mark correction`; the importer reads it from the rollout like the extension would, if your
Codex front end sends the text instead of rejecting the unknown slash command) or, after the session:

```sh
rematch library why codex:<session id>        # ids of imported Codex sessions are codex:<rollout id>
rematch mark --session codex:<session id> --class correction
```

What a mark changes in the counters: a mark overrides the classifier and is never overridden ([The loop](../concepts/loop.md#how-a-reflection-works), "Classify"); the prelabel and flag rules are in the [trace schema](../reference/trace-schema.md).

---

Source: moved from docs/MANUAL.md ("Correcting it", "Claude Code"); adapters/codex/README.md ("Marking a redirect"); README.md ("How a reflection works").
