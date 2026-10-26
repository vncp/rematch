//! evolve-core: trace model + parsers, redirect heuristics, predicates, lint,
//! library + counters, ledger (SQLite + blobs), package model. No omp invocation
//! lives here; the binary owns process spawning.

pub mod types;

// Owned by the Trace slice (DESIGN §3, §6 derivation, importer, outcomes):
pub mod trace; // own trace JSONL → TraceRecord stream + SessionRecord fold
pub mod import; // omp session JSONL → SessionRecord (+ skill reads, outcomes from receipts)
pub mod redirect; // heuristics + classifier batch prompt building/parsing
pub mod features; // tokenization, stop words, Porter stemming, tf-idf stems, Jaccard
pub mod outcome; // Outcome signature/normalization, MinHash
pub mod family; // family_of(provider, model) → Family

// Owned by the Ledger slice (DESIGN §4, §5, §6 match, §7, §8):
pub mod paths; // EVOLVE_ROOT / agent dir resolution
pub mod ledger; // SQLite schema + blobs
pub mod predicate; // derive / match (load-time and count-time)
pub mod library; // Entry lifecycle, status transitions, staging/kept/archive moves, rollback, pin
pub mod counters; // fold sessions into counters, Decision rules
pub mod lint; // lint.toml rules → LintReport
pub mod package; // SKILL.md frontmatter, package layout, mechanical split (>8 KB)
pub mod dedup; // simhash near-duplicate routing
pub mod experience; // EXPERIENCE.md writer/reader
pub mod rate; // admissions / caps

pub use types::*;
