//! Golden importer test: every `tests/goldens/*.jsonl` (anonymized real omp sessions) must
//! import to the counts recorded in `tests/goldens/expected.json`.
//! Re-record after adding a golden: `GOLDEN_BLESS=1 cargo test -p rematch-core --test golden_import`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rematch_core::import::{extract_outcomes, read_session, session_record, trace_records_of};
use rematch_core::trace::TraceRecord;
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Expected {
    id: String,
    parent_id: Option<String>,
    branch_entries: usize,
    family: Option<String>,
    turns: u32,
    tool_calls: u32,
    tool_errors: u32,
    error_clusters: u32,
    skill_reads: Vec<String>,
    files_touched: usize,
    exts: Vec<String>,
    redirects: usize,
    takeovers: u32,
    tokens_in: u64,
    tokens_out: u64,
    outcomes: usize,
    /// Synthesized extension trace (`trace_records_of`).
    trace_inputs: usize,
    trace_tool_results: usize,
    trace_turn_ends: usize,
    trace_flagged: bool,
}

fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/goldens")
}

#[test]
fn goldens_import_to_stable_counts() {
    let dir = goldens_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("tests/goldens exists")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    files.sort();
    assert!(
        files.len() >= 2,
        "need at least two goldens, found {}",
        files.len()
    );

    let mut actual = BTreeMap::new();
    for path in &files {
        let session = read_session(path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
        let r = session_record(&session);
        // Importing twice is byte-for-byte stable.
        assert_eq!(
            r,
            session_record(&read_session(path).unwrap()),
            "{}",
            path.display()
        );
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let trace = trace_records_of(&session);
        assert_eq!(trace, trace_records_of(&read_session(path).unwrap()), "{name}: synthesis is stable");
        let count = |ev: &str| trace.iter().filter(|r| r.ev() == ev).count();
        let trace_flagged = match trace.last() {
            Some(TraceRecord::SessionShutdown { flagged, .. }) => *flagged,
            other => panic!("{name}: trace must end with session_shutdown, got {other:?}"),
        };
        assert_eq!(trace_flagged, r.flagged, "{name}: import row and trace flag by one rule");
        actual.insert(
            name,
            Expected {
                id: r.id.clone(),
                parent_id: session.parent_id(),
                branch_entries: session.entries.len(),
                family: r.family.as_ref().map(|f| f.0.clone()),
                turns: r.turns,
                tool_calls: r.tool_calls,
                tool_errors: r.tool_errors,
                error_clusters: r.error_clusters,
                skill_reads: r.skill_reads.clone(),
                files_touched: r.files_touched.len(),
                exts: r.exts.clone(),
                redirects: r.redirects.len(),
                takeovers: r.takeovers,
                tokens_in: r.tokens_in,
                tokens_out: r.tokens_out,
                outcomes: extract_outcomes(&session, &r).len(),
                trace_inputs: count("input"),
                trace_tool_results: count("tool_result"),
                trace_turn_ends: count("turn_end"),
                trace_flagged,
            },
        );
    }

    let expected_path = dir.join("expected.json");
    if std::env::var_os("GOLDEN_BLESS").is_some() {
        std::fs::write(
            &expected_path,
            serde_json::to_string_pretty(&actual).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    let expected: BTreeMap<String, Expected> = serde_json::from_str(
        &std::fs::read_to_string(&expected_path).expect("expected.json present"),
    )
    .unwrap();
    assert_eq!(
        expected.keys().collect::<Vec<_>>(),
        actual.keys().collect::<Vec<_>>(),
        "every golden needs an expected.json entry (GOLDEN_BLESS=1 to record)"
    );
    for (name, want) in &expected {
        assert_eq!(&actual[name], want, "golden {name}");
    }
}
