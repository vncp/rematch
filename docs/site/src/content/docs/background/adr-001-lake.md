---
title: "ADR 001: the lake"
description: Agent sessions as a queryable backend — the journal, Parquet per table, Bellwether keys.
editUrl: https://github.com/vncp/rematch/edit/main/docs/adr/001-lake.md
---

:::note[Generated]
From `docs/adr/001-lake.md`, in full, by `docs/site/scripts/gen.ts`. Edit the source, then `bun run gen`.
:::

Status: built (L1–L4), live on the author's history · 2026-10-26 · amends SPEC §"What is deliberately not in the plan" (the indexing line) · extends DESIGN §5 (ledger) and SPEC §"Session outcome records"

Revision 2, after an independent architecture review of revision 1 (findings recorded in §"Review"). The one change that resolved three of the five blockers is the **journal**: the ledger gains an append-only table of every write, and the lake is an export of the journal, never of current state. Keys are declared only for questions a reader in the loop asks today; every other Bellwether kind is listed as declined with the question that would justify it.

## Context

rematch's state is a SQLite ledger plus JSONL traces under `REMATCH_ROOT`. That is right for the live loop (sub-50 ms `match`, single-writer locks, idempotent folds) and wrong for the rest of what the spec anticipates: history across machines, decisions auditable *as they were made*, "the 5 TB stays queryable as events", and a research log that other tools can read. SPEC names the shape in Bellwether's vocabulary: *"`outcomes` is a table in the lake beside the session events, partitioned by date and sorted by `subject.domain`, with keys `exact:signature`, `composite:(domain, facet_key, facet_value)`, and `temporal:subject_id@ts`."*

Three facts constrain the design:

1. **The hot path never touches the lake.** `match`, `skills sync`, `trace-event`, `hook-check` run inside sessions with millisecond budgets. The lake is written detached and read only by historical commands.
2. **Bellwether indexes immutable, fingerprinted Parquet, one dataset root per schema.** A rewritten file is a different file; a root has one schema, one path template. So: one root per table, files closed once and never rewritten, the index outside any synced tree.
3. **The ledger overwrites.** `entries`, `outcomes`, `redirects` (classifier labels, later marks), `counters` (refolds) are current-state rows. A bitemporal lake cannot be exported from current state; it needs the write history.

## Decision

### The journal (ledger, SQLite)

A new table `journal(seq INTEGER PRIMARY KEY, known INTEGER, machine TEXT, table_name TEXT, row_key TEXT, op TEXT, json TEXT)`: every write the ledger performs to `sessions`, `redirects`, `entries`, `counters`, `proposals`, `receipts`, `outcomes` appends one row with the full record as written, in the same transaction. `known` is the ledger's clock at that write (ms UTC), never the exporter's. The journal is the only thing the lake exports; current-state tables stay as they are for the hot path. Retention: never trimmed (it is the history); its size is bounded by the same caps that bound the library.

### The lake (Parquet, one root per table)

```
REMATCH_ROOT/lake/
  manifest.json                     lake_version, bellwether_version_pinned, exporter watermark per machine {machine: journal seq}
  events/     {harness}/{day:date}/{session_id}.parquet        one file per closed session; day = session start (UTC)
  sessions/   {day:date}/{machine}-{seq:int}.parquet           journal rows of table sessions
  redirects/  {day:date}/{machine}-{seq:int}.parquet
  entries/    {day:date}/{machine}-{seq:int}.parquet           one row per entry write: full json + status, version, content_sha256, valid, known
  counters/   {day:date}/{machine}-{seq:int}.parquet
  proposals/  {day:date}/{machine}-{seq:int}.parquet
  receipts/   {day:date}/{machine}-{seq:int}.parquet
  outcomes/   {domain}/{day:date}/{machine}-{seq:int}.parquet
  blobs/      <sha256>                                          content-addressed (same flat layout as the ledger's blobs/)
REMATCH_ROOT/lake-index/<table>/    one Bellwether index root per table, built per machine, never synced
```

Rules: `day` for append tables is the export UTC day; `{seq}` is the journal `seq` of the run's first row, unpadded (Bellwether's `int` placeholder is digits only); a file is written once per export run and never touched again; partition columns are **not** stored inside the files (so footer stats can never disagree with the path); time columns are `Timestamp(ms, UTC)`; the exporter sets `statistics_truncate_length(None)`. `lake import --from <other lake>` copies partition files whose `(machine, seq)` is absent and merges `manifest.json` watermarks per machine; it never copies `lake-index/`.

### Tables and keys (only what a reader in the loop asks for)

Every row carries `machine`, `known` (origin ledger ms), journal `seq`. Columns are flat; lists are Arrow `List<Utf8>` (Bellwether refuses lists of structs). A key is declared only when a named reader below needs it; the rest of Bellwether's kinds are listed under "Declined" with the question that would justify each, so adding one later is a one-line change here, not a design change.

| Table | Grain | Keys | The reader that needs it |
|---|---|---|---|
| `events` | one trace record of a closed session; `tool_cmd` = a bash record's `input.command` (stored, derived at export) | `exact:tool_cmd,counts` · `text:tool_cmd,tokenizer=words` · `exact:path` · `exact:skill_read` | `create --from sessions` and `improve` ask "which sessions ran this command / touched this file / read this skill" over all imported history; `create --from sessions --ran cargo,test` asks "which sessions ran a command with these words" (`lake query events --key text:tool_cmd --has cargo,test` finds `cd x && cargo test --workspace`); a session's own excerpt is path resolution, no key |
| `sessions` | one row per session write | `exact:repo_remote,counts` · `exact:domain` · `list:loaded_entry_shas` · `list:exts` · `list:stems` · `range:started_at` | `rematch report` before-arm: sessions matching a predicate (`exts`/`stems` contains_all) in a window where the entry was **not** loaded; `create --from sessions` reference search; `exact:domain` (the session's work domain, DESIGN §6 "Domain"; an additive nullable column, null in files exported before it): `report`'s per-domain groups and a trigger's same-domain sessions ("which sessions of `quant.options` in this window") |
| `redirects` | one row per label write | `temporal:session_id,seq@known` · `exact:class` | `report` and `why --asof`: the class a redirect had as of a date (later marks are new rows) |
| `entries` | one row per entry write; `valid` = `status_since`, `known` = `updated_at` | `temporal2:entry_id@valid,known` · `exact:content_sha256` | `why --asof` and `reflect --history`: the status an entry had when session S loaded it; cross-machine identity by content |
| `counters` | one row per fold write; `ts` = session end; carries `content_sha256` | `temporal:content_sha256@ts` | `report` and `shared` promotion across machines: counts for one content over a window, deduped by greatest `known ≤ K` |
| `proposals` | one editor/composer reply | `exact:entry_id` | `why`: decision → reply → lint |
| `receipts` | one certificate | `exact:entry_id` | `why`: decision → evidence (D24: certificates live here, never in `proposals`) |
| `outcomes` | one row per outcome write; `valid` = `ts`, `known` = ledger `created_at` | `exact:signature` · `temporal2:subject_id@valid,known` · `list:facets` | SPEC's three lookups: same thing, status as of, related; `facets` holds `domain=<each prefix>`, `key=value` and bare `key` so `transfer suggest`'s "overlaps on two or more structural keys" is one `contains_all` over keys |

Declined, each with the question that would justify it (none is asked by a current reader):

| Kind | Would answer | Why not now |
|---|---|---|
| `text:` (B5) on inputs or tool output | free-text search of prose | no reader searches prose. (`text:tool_cmd,tokenizer=words` moved to the key table: `create --ran` is its reader; it replaced `expr:tool_cmd`, which was exact on the whole command string, so "sessions that ran cargo test" missed `cd … && cargo test`; Bellwether refuses an `expr` name equal to a stored column, so the exact key moved to the stored column too) |
| `agg:` (B7) sums per repo/family | tokens and errors per repo from the index alone | `report` gates on recurrence, never on tokens (D10); one `locate` + sum over a few thousand rows is fine |
| `join:` (B3) sessions ⋈ counters | the attribution anti-join (sessions that loaded content never folded) | `lake doctor` computes it in one pass; a pinned cross-manifest key is maintenance for a diagnostic |
| `sketch` (B2) MinHash over stems/facets | vocabulary overlap between branches | SPEC's MinHash lives in the ledger's `minhash` table already; revisit if `transfer suggest` needs it across machines |
| `bloom:` | nothing here is high-cardinality-but-approximate | every key column is exact or low-cardinality |
| `exact:` on `ev`, `family`, `kind`, `harness` | — | 5–10 values over one-row-group files prune nothing |
| `text:trigram`, further `range:`, `counts` on `events` | substring search; time windows on other columns; row counts per session | no reader; the footer already has the row count |

### Bitemporal semantics

`known` is the origin machine's ledger write time. `asof --valid V --known K` on `entries` answers "what status did the ledger hold for this entry at K, effective at V" with no lookahead: a transition written at 14:00 is invisible to a question about 13:00 on the same day, which is the invariant Bellwether enforces for prices. The exporter writes the journal up to a watermark `seq`, not up to a day, so a reader is never missing rows the ledger already knew; the only lag is between the last export and now, and `lake doctor` prints it.

Readers that dedupe restatements: `counters` rows for the same `(entry_id, session_id)` keep the row with the greatest `known ≤ K`; `redirects` likewise per `(session_id, seq)`.

### Identity that survives machines

- `repo_remote`: `origin` (else the first remote) normalized to `host/org/repo` (scheme, user, `.git`, trailing `/` stripped; host lowercased; path lowercased on github.com/gitlab.com); no remote → `local:<sha256 of the toplevel path>`. Forks are different remotes by design. Monorepos: the predicate keeps `repo_glob` as the sub-path under the toplevel; a predicate is `(repo_remote, subpath glob, exts, stems)`. Worktrees share the remote: this is the point.
- `head_sha` at session start. `files_changed` (path, sha256) computed once at `session_shutdown`, bounded (≤ 50 files, none over 1 MB), else at count time from `tool_result.path`.
- `content_sha256` stored **on the entry** at stage/keep/version time (sha256 over the receipt's `files:[[path, sha256]]` form), never hashed per session. `loaded_entries` in the trace becomes `[{id, version, sha}]`; `SessionRecord` carries `loaded_entry_ids` and `loaded_entry_shas`; `fold_session` matches on `(id, version)`; the `retrieval` record keeps crediting by id. `visible_entries` records what the shared skills dir exposed (DESIGN §16) so `improve`'s audit can say what the model could see versus what counted.
- Row ids: `redirects` keyed by `(session_id, seq)` (no autoincrement in the lake); entry ids stay machine-local, and cross-machine aggregation is by `content_sha256`.

### Commands

```
rematch lake export [--all]               journal → Parquet up to the current seq, per-run files; idempotent by watermark; no index
rematch lake index [<table>…]             bellwether index per table root with the keys above (+ join keys across roots); on demand only
rematch lake query <table> <lookup…> [--asof V --known K]   bellwether locate/latest/asof/aggregate → rows (JSON); residual predicates evaluated by rematch
rematch lake import --from <lake>         merge another machine's partitions and watermarks; never the index
rematch lake rebuild                      SQLite ledger from the journal partitions (entries json, sessions, counters, receipts, outcomes); admissions/jobs/minhash are rebuilt or recomputed
rematch lake doctor                       watermarks, lag, orphan partitions, index completeness per key, pinned Bellwether version
rematch report [--window 6w]              the kill criterion from the lake alone: kept entries over ≥ 4 matched sessions; recurrence per trigger, before-arm vs. with-entry
```

`lake export` runs at the end of `reflect --pending` (detached, bounded: a journal drain, never an index) and on demand. `lake index` runs only on demand or when `lake query` finds a root with files newer than its manifest. The ledger stays authoritative per machine; `lake rebuild` exists so "the ledger is derived" is a true sentence, but cross-machine use is read-only: `report`, `transfer suggest`, and `shared` promotion read `counters`/`entries` through `lake query`, never by merging ledgers.

### Readers, switched in L4

| Reader | Today | With the lake |
|---|---|---|
| `reflect --history`, `improve` excerpts | scan harness session files | `events/<harness>/<day>/<session>.parquet` by path |
| `create --from sessions`, stem corpus | every session row in SQLite | `sessions` `list:stems` contains_any |
| `transfer suggest` | ledger facet rows | `outcomes` `list:facets` contains_all on bare keys |
| `library why --asof` | current status | `entries` temporal2 + `counters` temporal |
| `rematch report` | — | `counters` window + `sessions` before-arm + `entries` asof |
| `rematch tune` (and its pass inside `count`) | — | `sessions` `range:started_at` window (`skill_reads` = the `events.skill_read` key's sessions, `visible_entries`, `loaded_entry_shas`, `stems`/`exts`) → read-but-missed / matched-but-unread per entry; a session's `retrieval` records from its `events` file by path; the ledger journal while the lake lags (DESIGN §6 "Predicate tuning") |
| a second machine | nothing | `lake import` → `report`/`transfer`/`promote` over both |

### Not in this decision

Embeddings (D28: exploratory; `exact:signature` is the gate, the ledger's MinHash the suggestion). A query engine in rematch (Bellwether plans row groups and returns a residual — `In`, `Between`, `NotOlderThan`, `LatestPerKey`, `ContainsAll`, text — which rematch evaluates per row with arrow-rs; it never re-plans). Replacing SQLite. Indexing inside a session. `reconcile` (nothing in the loop compares two datasets by key).

## Live run (2026-10-26)

On this machine's real root: `lake migrate` set 27 content hashes and 206 session remotes; `lake export --all` wrote 1,178 journal rows and 326 events files in 2.9 s (16 sessions with no source left recorded as missing); `lake index` built all 14 keys with Bellwether 0.1.0 in 0.8 s, pinned; `lake query events --key exact:skill_read --values bellwether-dev` planned 18 of 326 files, `exact:path src/lib.py` 5 of 326; `report` ran from the lake in 57 ms and showed the before-arm the spec asks for (`bellwether-dev`: before 2/92 recurred, with 0/1); `why --asof` distinguished "provisional as of 04:30" from "kept as of the 27th" for the same entry. One correct-but-unhelpful key: `expr:tool_cmd` is an exact match on the full command, and real commands are `cd … && cargo test …`; a `has`-style question on commands needs the declined `text:` key — recorded in the declined table rather than added.

## Consequences

- `arrow`/`parquet` become dependencies **behind a cargo feature `lake`**, on by default in release builds; the hot-path binary size and link time are checked in CI (`match` ≤ 50 ms stays a test). Bellwether is an external binary, optional: without it `lake export` still writes, `lake query` falls back to full-file reads.
- `temporal2` is a storage-version-1 sidecar that Bellwether's ADR 028 folds into a `bitemporal` layout next release: `manifest.json` pins the Bellwether version used, `lake doctor` refuses a mismatch, and `lake index` rebuilds under the new version (the lake files are untouched).
- The trace schema (DESIGN §3) gains `repo_remote`, `subpath`, `head_sha`, `files_changed`, versioned `loaded_entries`, `visible_entries`; the extension and both adapters emit them. The ledger gains `journal`, `content_sha256` on entries, `repo_remote`/`subpath` on sessions and predicates.
- SPEC §"deliberately not in the plan" is amended: the indexing layer is in, scoped to the kill-criterion report and cross-machine history, and justified by the spec's own outcome-records section.

## Build plan

| Step | Delivers | Done when |
|---|---|---|
| L1 | journal; `content_sha256` on entries; `repo_remote` + subpath on sessions/predicates (`match` tries both, glob fallback); versioned `loaded_entries` + `visible_entries`; `head_sha`, `files_changed` at shutdown | every ledger write journals; `why` shows remote/sha; a worktree session matches its repo's entries; 240+ tests still green; `match` ≤ 50 ms |
| L2 | `lake export`, `lake import`, `lake rebuild`, `lake doctor` (plain parquet writer, watermark per machine, per-run files, feature `lake`) | two exports are byte-identical; a second machine's partitions merge; `lake rebuild` from a lake reproduces `library ls` exactly on a fresh root |
| L3 | `lake index` (key table above), `lake query` with residual evaluation, version pin | `events` lookup for `cargo test` (`expr:tool_cmd`) returns the right sessions over the imported history; `asof` on `entries` gives the pre-transition status for a same-day question; `lake doctor` lists sessions with no counter row |
| L4 | readers switched; `rematch report` | the kill-criterion report (kept-over-4 and recurrence before/with, per trigger) runs from the lake alone, on either machine, over both machines' history |

## Review

Revision 1 was reviewed independently against SPEC, DESIGN, the research file, the ledger code and Bellwether's format/source. Blockers and how revision 2 answers them:

| Finding | Resolution |
|---|---|
| `known` was the exporter's time → wrong `asof` answers the same day | `known` = origin ledger write time, from the journal |
| no source for one-row-per-transition; ledger overwrites `entries`, `outcomes`, `redirects`, `counters` | the journal |
| closed-day partitions vs "readers switched to the lake alone" | per-run files up to a journal `seq`; no day cutoff |
| one `index/` root for eight tables; index inside the synced tree | one Bellwether root per table; `lake-index/` outside the lake, per machine |
| `list:loaded_entries` over a struct list is refused | two `List<Utf8>` columns (ids, shas) |
| "ledger is derived" with no rebuild; machine-local ids on `counters`; autoincrement redirect ids | `lake rebuild`; `content_sha256` on counters; `(session_id, seq)` keys |
| exporter + index inside the detached `reflect --pending` | export only (bounded drain); index on demand |
| `temporal2` unfrozen in Bellwether | pinned version in `manifest.json`, `doctor` check, index rebuildable |
| `rematch report` unspecified; before-arm needs `exts`/`stems`/classes on `sessions` | specified; columns added |
| SPEC §"not in the plan" contradiction; D28 misread | amended explicitly; gate = signature, MinHash stays in the ledger |
| `text:` on the wrong column; `words` serves `has`, not `contains` | `text:` dropped; `expr:tool_cmd` answers the command question exactly |
| arrow/parquet in the hot-path binary | cargo feature `lake`; `match` latency test |
