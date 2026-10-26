//! The lake's indexes (ADR 001 L3): `lake index` with the ADR key table through a real
//! Bellwether binary, `lake query` planned by it and finished by rematch's residual
//! evaluation, the same rows from a full scan without an index, and the version pin.
//! Needs `REMATCH_BELLWETHER` (a built `bellwether`); skipped with a message otherwise.
#![cfg(feature = "lake")]

mod common;

use std::collections::BTreeSet;

use common::*;
use rematch_core::ledger::{JournalRow, Ledger};
use serde_json::{json, Value};

const SKIP: &str = "e2e_lake_index skipped: set REMATCH_BELLWETHER to a built bellwether binary \
                    (e.g. ~/repos/bellwether/target/release/bellwether)";

fn bellwether() -> Option<String> {
    std::env::var("REMATCH_BELLWETHER").ok().filter(|s| !s.is_empty())
}

/// 2026-10-20T13:00:00Z, the morning an entry is kept.
const T0: i64 = 1_792_501_200_000;
const MIN: i64 = 60_000;

fn ids(rows: &Value, col: &str) -> BTreeSet<String> {
    rows.as_array().unwrap().iter().map(|r| r[col].as_str().unwrap_or("<null>").to_string()).collect()
}

/// Four closed sessions (two ran `cargo test`, two `cargo build`) that loaded a kept
/// entry; an entry staged at 13:00 and kept at 13:10 the same day (two journaled writes,
/// ledger clock as written); three outcomes with facets. Then `lake export --all`.
/// Returns the loaded entry's id.
fn seed(env: &Env) -> String {
    let dir = env.agent.join("skills/ledger-helper");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: ledger-helper\ndescription: Use when editing the ledger module.\n---\n\nUse when editing the ledger module.\n",
    )
    .unwrap();
    let id = env.run(&["improve", "ledger-helper"])["entry"].as_str().unwrap().to_string();
    for i in 0..4 {
        let cmd = if i < 2 { "cargo test" } else { "cargo build" };
        env.write_records(
            &format!("k-{i}"),
            &[
                json!({"seq":0,"t":3_000 + i,"ev":"session_start","session_id":format!("k-{i}"),"cwd":env.repo,"repo":env.repo,
                       "model":{"provider":"anthropic","id":"claude-opus-5-5"},"loaded_entries":[id]}),
                json!({"seq":1,"t":3_001 + i,"ev":"input","text":"Edit the ledger module rounding helper","idle":true,"prelabel":"none"}),
                json!({"seq":2,"t":3_002 + i,"ev":"tool_result","tool":"bash","call_id":"c1","input":{"command":cmd},
                       "args_hash":"h","result_bytes":12,"is_error":false,"error_class":null,"content_head":"ok","skill_read":null,"path":null}),
                json!({"seq":3,"t":3_003 + i,"ev":"session_shutdown","reasons":[],"flagged":false}),
            ],
        );
    }
    let c = env.run(&["count", "--editor", &script("editor-ok.sh")]);
    assert_eq!(c["counter_rows"], 4, "{c}");

    let ledger = Ledger::open(&env.root.join("ledger.sqlite")).unwrap();
    let entry = ledger.journal_since(0).unwrap().into_iter().rfind(|r| r.table == "entries").unwrap();
    let first = ledger.journal_max_seq().unwrap() + 1;
    for (seq, (status, at)) in (first..).zip([("provisional", T0), ("kept", T0 + 10 * MIN)]) {
        let mut j = entry.json.clone();
        j["id"] = json!("e-transition");
        j["name"] = json!("transition-helper");
        j["status"] = json!(status);
        j["status_since"] = json!(at);
        j["updated_at"] = json!(at);
        ledger
            .apply_journal_row(&JournalRow {
                seq,
                known: at,
                machine: entry.machine.clone(),
                table: "entries".into(),
                row_key: "e-transition".into(),
                op: "upsert".into(),
                json: j,
            })
            .unwrap();
    }
    let facet = |k: &str, v: &str| rematch_core::Facet { key: k.into(), value: v.into() };
    for (oid, domain, facets) in [
        ("o1", "quant.options", vec![facet("mechanism", "carry"), facet("universe", "spx")]),
        ("o2", "quant.options", vec![facet("mechanism", "carry")]),
        ("o3", "quant.futures", vec![facet("universe", "es"), facet("venue", "cme"), facet("mechanism", "momentum")]),
    ] {
        let o = rematch_core::outcome::new_outcome_at(
            rematch_core::outcome::NewOutcome {
                session_id: "k-0".into(),
                model_family: rematch_core::Family("anthropic/claude-opus".into()),
                repo: env.repo.to_string_lossy().into_owned(),
                branch: "payments".into(),
                kind: rematch_core::OutcomeKind::Decision,
                subject: rematch_core::Subject { domain: domain.into(), entities: vec![] },
                claim: format!("claim {oid}"),
                params: [("id".to_string(), json!(oid))].into_iter().collect(),
                facets,
                evidence: vec![],
                result: rematch_core::OutcomeResult {
                    status: rematch_core::ResultStatus::Kept,
                    metrics: Default::default(),
                    kill_criterion: None,
                },
                links: vec![],
                provenance: Default::default(),
            },
            oid.into(),
            T0,
            entry.machine.clone(),
        );
        ledger.insert_outcome(&o).unwrap();
    }
    drop(ledger);
    env.run(&["lake", "export", "--all"]);
    id
}

fn query(env: &Env, bw: &str, args: &[&str]) -> Value {
    let mut a = vec!["lake", "query"];
    a.extend_from_slice(args);
    a.extend_from_slice(&["--bellwether", bw]);
    env.run(&a)
}

/// The same query without the table's index: a full scan with the same rows.
fn unindexed(env: &Env, bw: &str, table: &str, args: &[&str]) -> Value {
    let idx = env.root.join("lake-index").join(table);
    let off = env.root.join("lake-index").join(format!("{table}.off"));
    std::fs::rename(&idx, &off).unwrap();
    let mut a = vec![table];
    a.extend_from_slice(args);
    let v = query(env, bw, &a);
    std::fs::rename(&off, &idx).unwrap();
    assert_eq!(v["indexed"], false, "{v}");
    assert!(v["reason"].as_str().unwrap().contains("no index"), "{v}");
    v
}

fn version(bw: &str) -> String {
    let out = std::process::Command::new(bw).arg("--version").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().trim_start_matches("bellwether").trim().to_string()
}

#[test]
fn index_then_query_events_entries_and_outcomes_planned_by_bellwether() {
    let Some(bw) = bellwether() else {
        eprintln!("{SKIP}");
        return;
    };
    let env = Env::new();
    let id = seed(&env);
    let ix = env.run(&["lake", "index", "--bellwether", &bw]);
    let indexed: BTreeSet<&str> =
        ix["tables"].as_array().unwrap().iter().filter(|t| t["indexed"] == true).map(|t| t["table"].as_str().unwrap()).collect();
    for t in ["events", "sessions", "entries", "counters", "outcomes"] {
        assert!(indexed.contains(t), "{t}: {ix}");
    }
    let manifest: Value = serde_json::from_slice(&std::fs::read(env.root.join("lake/manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["bellwether_version_pinned"], json!(version(&bw)), "pinned on first index");
    assert!(!env.root.join("lake/lake-index").exists() && env.root.join("lake-index/events/manifest.json").is_file());

    // events: the sessions that ran `cargo test`, through expr:tool_cmd.
    let ev = query(&env, &bw, &["events", "--key", "expr:tool_cmd", "--values", "cargo test"]);
    assert_eq!(ev["indexed"], true, "{ev}");
    assert_eq!(ids(&ev["rows"], "session_id"), BTreeSet::from(["k-0".into(), "k-1".into()]), "{ev}");
    assert_eq!(ev["plan"]["residual"]["op"], "expr_in", "{ev}");
    assert_eq!(ev["plan"]["planned"]["files"], 2, "the index planned only the two files holding the command: {ev}");
    let scan = unindexed(&env, &bw, "events", &["--key", "expr:tool_cmd", "--values", "cargo test"]);
    assert_eq!(scan["rows"], ev["rows"], "a scan returns the same rows");

    // entries: the status as of a same-day instant, with no lookahead.
    let asof = |valid: i64, known: i64| {
        let (v, k) = (valid.to_string(), known.to_string());
        query(&env, &bw, &["entries", "--key", "temporal2:entry_id", "--values", "e-transition", "--valid", &v, "--known", &k])
    };
    let before = asof(T0 + 5 * MIN, T0 + 5 * MIN);
    assert_eq!(before["indexed"], true, "{before}");
    assert_eq!(before["count"], 1, "{before}");
    assert_eq!(before["rows"][0]["status"], "provisional", "the status before the 13:10 transition: {before}");
    let after = asof(T0 + 15 * MIN, T0 + 15 * MIN);
    assert_eq!(after["rows"][0]["status"], "kept", "{after}");
    let unknown_yet = asof(T0 + 15 * MIN, T0 + 5 * MIN);
    assert_eq!(unknown_yet["rows"][0]["status"], "provisional", "a transition not yet known is invisible: {unknown_yet}");
    let ops: Vec<&str> =
        before["plan"]["residual"]["all"].as_array().unwrap().iter().map(|p| p["op"].as_str().unwrap()).collect();
    assert_eq!(ops, ["in", "between", "between", "not_older_than", "not_older_than", "latest_per_key"], "{before}");
    let (v, k) = ((T0 + 5 * MIN).to_string(), (T0 + 5 * MIN).to_string());
    let scan = unindexed(&env, &bw, "entries", &["--key", "temporal2:entry_id", "--values", "e-transition", "--valid", &v, "--known", &k]);
    assert_eq!(scan["rows"], before["rows"]);

    // outcomes: list:facets contains_all over two bare keys.
    let fa = query(&env, &bw, &["outcomes", "--key", "list:facets", "--values", "mechanism,universe", "--all"]);
    assert_eq!(fa["indexed"], true, "{fa}");
    assert_eq!(ids(&fa["rows"], "id"), BTreeSet::from(["o1".into(), "o3".into()]), "{fa}");
    assert_eq!(fa["plan"]["residual"]["op"], "contains_all", "{fa}");
    assert_eq!(fa["rows"][0]["domain"], "quant.futures", "the path's partition columns are row columns: {fa}");
    let scan = unindexed(&env, &bw, "outcomes", &["--key", "list:facets", "--values", "mechanism,universe", "--all"]);
    assert_eq!(scan["rows"], fa["rows"]);

    // counters: the newest fold per content, and doctor's attribution check.
    let ent = query(&env, &bw, &["entries", "--key", "temporal2:entry_id", "--values", &id, "--latest"]);
    assert_eq!(ent["indexed"], true, "{ent}");
    let sha = ent["rows"][0]["content_sha256"].as_str().unwrap().to_string();
    let newest = query(&env, &bw, &["counters", "--key", "temporal:content_sha256", "--values", &sha, "--latest"]);
    assert_eq!(newest["indexed"], true, "{newest}");
    assert_eq!(ids(&newest["rows"], "session_id"), BTreeSet::from(["k-3".into()]), "the newest fold of that content: {newest}");
    let window = query(&env, &bw, &["counters", "--key", "temporal:content_sha256", "--values", &sha, "--from", "3004", "--to", "3005"]);
    assert_eq!(ids(&window["rows"], "session_id"), BTreeSet::from(["k-1".into(), "k-2".into()]), "{window}");
    let scan = unindexed(&env, &bw, "counters", &["--key", "temporal:content_sha256", "--values", &sha, "--from", "3004", "--to", "3005"]);
    assert_eq!(scan["rows"], window["rows"]);
    let doc = env.run(&["lake", "doctor", "--bellwether", &bw]);
    assert_eq!(doc["index"]["sessions_without_counters"], json!([]), "{doc}");
    assert_eq!(doc["index"]["index_refused"], Value::Null, "{doc}");
    for t in doc["index"]["indexes"].as_array().unwrap().iter().filter(|t| t["indexed"] == true) {
        assert!(t["keys"].as_array().unwrap().iter().all(|k| k["complete"] == true), "{t}");
        assert_eq!(t["untracked"], json!([]), "{t}");
    }
}

#[test]
fn a_new_export_is_indexed_before_the_next_query_and_another_version_is_refused_until_repin() {
    let Some(bw) = bellwether() else {
        eprintln!("{SKIP}");
        return;
    };
    let env = Env::new();
    seed(&env);
    env.run(&["lake", "index", "events", "--bellwether", &bw]);
    env.write_records(
        "k-9",
        &[
            json!({"seq":0,"t":9_000,"ev":"session_start","session_id":"k-9","cwd":env.repo,"repo":env.repo,"loaded_entries":[]}),
            json!({"seq":1,"t":9_001,"ev":"tool_result","tool":"bash","call_id":"c1","input":{"command":"cargo test"},"is_error":false}),
            json!({"seq":2,"t":9_002,"ev":"session_shutdown","reasons":[],"flagged":false}),
        ],
    );
    env.run(&["lake", "export", "--all"]);
    let ev = query(&env, &bw, &["events", "--key", "expr:tool_cmd", "--values", "cargo test"]);
    assert_eq!((&ev["indexed"], &ev["reindexed"]), (&json!(true), &json!(true)), "{ev}");
    assert_eq!(ids(&ev["rows"], "session_id"), BTreeSet::from(["k-0".into(), "k-1".into(), "k-9".into()]), "{ev}");

    // Another pinned version: index refuses, query scans (same rows), --repin rebuilds.
    let mp = env.root.join("lake/manifest.json");
    let mut m: Value = serde_json::from_slice(&std::fs::read(&mp).unwrap()).unwrap();
    m["bellwether_version_pinned"] = json!("0.0.0-other");
    std::fs::write(&mp, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    let (v, code) = env.run_cmd(env.cmd(&["lake", "index", "--bellwether", &bw]));
    assert_ne!(code, 0, "{v}");
    assert!(v.to_string().contains("--repin"), "{v}");
    let scan = query(&env, &bw, &["events", "--key", "expr:tool_cmd", "--values", "cargo test"]);
    assert_eq!(scan["indexed"], false, "{scan}");
    assert!(scan["reason"].as_str().unwrap().contains("0.0.0-other"), "{scan}");
    assert_eq!(scan["rows"], ev["rows"]);
    let doc = env.run(&["lake", "doctor", "--bellwether", &bw]);
    assert!(doc["index"]["index_refused"].as_str().unwrap().contains("--repin"), "{doc}");
    let re = env.run(&["lake", "index", "--repin", "--bellwether", &bw]);
    assert_eq!(re["repinned"], true, "{re}");
    let m: Value = serde_json::from_slice(&std::fs::read(&mp).unwrap()).unwrap();
    assert_eq!(m["bellwether_version_pinned"], json!(version(&bw)));
    assert_eq!(query(&env, &bw, &["events", "--key", "expr:tool_cmd", "--values", "cargo test"])["indexed"], true);
}

#[test]
fn without_a_binary_queries_scan_and_say_so() {
    let env = Env::new();
    seed(&env);
    let mut c = env.cmd(&["lake", "query", "events", "--key", "exact:path", "--values", "/nowhere"]);
    c.env_remove("REMATCH_BELLWETHER").env("PATH", "/nonexistent");
    let (v, code) = env.run_cmd(c);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["indexed"], false, "{v}");
    assert!(v["data"]["reason"].as_str().unwrap().contains("no bellwether binary"), "{v}");
}
