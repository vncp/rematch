# Importer goldens

Anonymized real omp session JSONL files used only by the historical importer test
(`crates/evolve-core/tests/golden_import.rs`). Each `*.jsonl` here must import to the
counts recorded for it in `expected.json` (branch entries, family, turns, tool calls and
errors, error clusters, skill reads, files touched, exts, redirects, takeovers, tokens,
receipt-derived outcomes).

## How they were produced

omp v18.12.6, from `~/.omp/agent/sessions/`, with `omp anonymize <session.jsonl> -o <dir>`
(turn contents and error text redacted, paths/literals replaced by consistent mock tokens,
metadata such as provider/model, usage, stopReason, tool names and the entry tree kept),
then `<dir>/session.jsonl` copied here:

| Golden | Source session | Exercises |
| --- | --- | --- |
| `skill-read.jsonl` | `-repos/2026-10-26T00-43-27-949Z_01a175a9-….jsonl` | `read skill://<name>` → skill read; 3 turns; `github-copilot/gpt-6.1-sol-1m` |
| `bash-clone.jsonl` | `-repos-omp-evolve/2026-10-26T01-21-25-513Z_01a175cc-….jsonl` | one bash tool call; `anthropic/claude-fable-5-1` |
| `subagent-branch-compaction.jsonl` | `-repos-omp-evolve/2026-10-26T01-05-32-665Z_01a175bd-…/OmpApiMap.jsonl` | subagent (`parentSession`, `attribution: agent`), 4 compactions, an off-branch `model_usage` probe entry, 216 tool calls / 7 errors |

`omp anonymize` drops the live file's line-1 `title` slot and rewrites `skills/<name>/SKILL.md`
path segments, so title-slot handling and `.../skills/<name>/SKILL.md` inference are
covered by the unit tests in `crates/evolve-core/src/import.rs` instead.

## Adding or re-recording

1. `omp anonymize <session.jsonl> -o /tmp/anon && cp /tmp/anon/session.jsonl tests/goldens/<name>.jsonl`
2. Review the file before committing (anonymize keeps metadata).
3. `GOLDEN_BLESS=1 cargo test -p evolve-core --test golden_import` rewrites `expected.json`;
   review the diff, then run the test without `GOLDEN_BLESS`.
