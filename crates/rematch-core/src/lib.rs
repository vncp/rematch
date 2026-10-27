//! rematch-core: trace model + parsers, redirect heuristics, predicates, lint,
//! library + counters, ledger (SQLite + blobs), package model. No omp invocation
//! lives here; the binary owns process spawning.

pub mod types;

// Owned by the Trace slice (DESIGN §3, §6 derivation, importer, outcomes):
pub mod trace; // own trace JSONL → TraceRecord stream + SessionRecord fold
pub mod import; // omp session JSONL → SessionRecord (+ skill reads, outcomes from receipts)
pub mod import_claude; // Claude Code ~/.claude/projects/<slug>/<uuid>.jsonl → TraceRecord stream
pub mod import_codex; // Codex ~/.codex/sessions/**/rollout-*.jsonl → TraceRecord stream
pub mod import_pi; // Pi <agent dir>/sessions/--<cwd>--/<ts>_<id>.jsonl → TraceRecord stream
pub mod import_opencode; // OpenCode opencode.db (SQLite) sessions → TraceRecord stream
pub mod redirect; // heuristics + classifier batch prompt building/parsing
pub mod features; // tokenization, stop words, Porter stemming, tf-idf stems, Jaccard
pub mod words; // Bellwether's `words` tokenizer: case fold + UAX #29 words (text keys, `create --ran`)
pub mod outcome; // Outcome signature/normalization, MinHash
pub mod family; // family_of(provider, model) → Family

// Owned by the Ledger slice (DESIGN §4, §5, §6 match, §7, §8):
pub mod paths; // REMATCH_ROOT / agent dir resolution
pub mod ledger; // SQLite schema + blobs
pub mod predicate; // derive / match (load-time and count-time)
pub mod library; // Entry lifecycle, status transitions, staging/kept/archive moves, rollback, pin
pub mod domain; // session/predicate work domain: policy.json, domain.yml, inference table
pub mod counters; // fold sessions into counters, Decision rules
pub mod lint; // lint.toml rules → LintReport
pub mod package; // SKILL.md frontmatter, package layout, mechanical split (>8 KB)
pub mod dedup; // simhash near-duplicate routing
pub mod experience; // EXPERIENCE.md writer/reader
pub mod rate; // admissions / caps
pub mod identity; // repo_remote normalization, subpath, HEAD (ADR 001)
pub mod profile; // REMATCH_ROOT/profile: layout, views linked into omp/rematch, roles.yml (DESIGN §20)

pub use types::*;
