# The lake and bitemporal history

The ledger answers today's questions in milliseconds; it cannot answer *what did the library look like when that session ran*, nor combine two machines, nor index years of sessions. [ADR 001](../design/adr-001-lake.md) adds both without touching the hot path: every ledger write is journaled with the ledger's own clock, the journal is exported to Parquet (one [Bellwether](https://github.com/vncp/bellwether) root per table, files written once), and Bellwether's keys answer the historical questions the loop actually asks — `asof --valid --known` on `entries` for status-at-the-time without lookahead, `exact:signature` and `list:facets` on `outcomes` for the spec's three lookups, `exact:tool_cmd` and `text:tool_cmd` on `events` for "which sessions ran this command" and "which ran a command with these words". Only keys a current reader needs are declared; the ADR lists every other kind with the question that would justify it. Status: L1–L4 built (journal, export, import, rebuild, index, query, the switched readers and `rematch report`); see [Status and limits](../status.md).

The ledger keeps current state; the lake keeps history. Every write the ledger makes is journaled with the ledger's own clock, and `rematch lake export` copies the journal, plus the trace of every closed session, into Parquet files under `~/.rematch/lake/` that are written once and never changed. Nothing in a session reads it: `match`, `skills sync` and the hooks never open it, so a slow disk or a large lake costs a session nothing. Design: [ADR 001](../design/adr-001-lake.md); file layout and columns: DESIGN §19.1.

## Bitemporal semantics

`known` is the origin machine's ledger write time. `asof --valid V --known K` on `entries` answers "what status did the ledger hold for this entry at K, effective at V" with no lookahead: a transition written at 14:00 is invisible to a question about 13:00 on the same day, which is the invariant Bellwether enforces for prices. The exporter writes the journal up to a watermark `seq`, not up to a day, so a reader is never missing rows the ledger already knew; the only lag is between the last export and now, and `lake doctor` prints it.

Readers that dedupe restatements: `counters` rows for the same `(entry_id, session_id)` keep the row with the greatest `known ≤ K`; `redirects` likewise per `(session_id, seq)`.

## Identity that survives machines

- `repo_remote`: `origin` (else the first remote) normalized to `host/org/repo` (scheme, user, `.git`, trailing `/` stripped; host lowercased; path lowercased on github.com/gitlab.com); no remote → `local:<sha256 of the toplevel path>`. Forks are different remotes by design. Monorepos: the predicate keeps `repo_glob` as the sub-path under the toplevel; a predicate is `(repo_remote, subpath glob, exts, stems)`. Worktrees share the remote: this is the point.
- `head_sha` at session start. `files_changed` (path, sha256) computed once at `session_shutdown`, bounded (≤ 50 files, none over 1 MB), else at count time from `tool_result.path`.
- `content_sha256` stored **on the entry** at stage/keep/version time (sha256 over the receipt's `files:[[path, sha256]]` form), never hashed per session. `loaded_entries` in the trace becomes `[{id, version, sha}]`; `SessionRecord` carries `loaded_entry_ids` and `loaded_entry_shas`; `fold_session` matches on `(id, version)`; the `retrieval` record keeps crediting by id. `visible_entries` records what the shared skills dir exposed (DESIGN §16) so `improve`'s audit can say what the model could see versus what counted.
- Row ids: `redirects` keyed by `(session_id, seq)` (no autoincrement in the lake); entry ids stay machine-local, and cross-machine aggregation is by `content_sha256`.

Commands, files and readers: [Querying history with the lake](../guides/lake.md). The decision record: [ADR 001](../design/adr-001-lake.md).

---

Source: README.md ("History as a backend"); docs/adr/001-lake.md ("Bitemporal semantics", "Identity that survives machines"); moved from docs/MANUAL.md ("The lake").
