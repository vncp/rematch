//! The lake (ADR 001 L2): `lake export | import | rebuild | doctor` end to end through the
//! binary, plus the `--no-default-features` build without arrow.
#![cfg(feature = "lake")]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;
use rematch_lake::arrow::array::{Array, StringArray};
use rematch_lake::arrow::datatypes::{DataType, TimeUnit};
use rematch_lake::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::{json, Value};

/// Every file under `dir` (relative path → bytes).
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.strip_prefix(dir).unwrap().to_string_lossy().into_owned(), std::fs::read(&p).unwrap());
            }
        }
    }
    out
}

fn copy_tree(from: &Path, to: &Path) {
    for (rel, bytes) in files(from) {
        let dst = to.join(rel);
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::write(dst, bytes).unwrap();
    }
}

fn run_as(env: &Env, machine: &str, args: &[&str]) -> Value {
    let mut c = env.cmd(args);
    c.env("REMATCH_MACHINE", machine);
    let (v, code) = env.run_cmd(c);
    assert_eq!((code, &v["ok"]), (0, &json!(true)), "{args:?}: {v}");
    v["data"].clone()
}

/// A registered skill kept after four matched sessions (entries, sessions, counters,
/// receipts), plus a drafted entry through the scripted editor (a proposal and its blob).
/// Returns the registered entry's id.
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
        env.write_records(
            &format!("k-{i}"),
            &[
                json!({"seq":0,"t":3_000 + i,"ev":"session_start","session_id":format!("k-{i}"),"cwd":env.repo,"repo":env.repo,
                       "model":{"provider":"anthropic","id":"claude-opus-5-5"},"loaded_entries":[id]}),
                json!({"seq":1,"t":3_001 + i,"ev":"input","text":"Edit the ledger module rounding helper","idle":true,"prelabel":"none"}),
                json!({"seq":2,"t":3_002 + i,"ev":"tool_result","tool":"bash","call_id":"c1","input":{"command":"cargo test -p payments"},
                       "args_hash":"h","result_bytes":12,"is_error":false,"error_class":null,"content_head":"test result: ok","skill_read":null,"path":null}),
                json!({"seq":3,"t":3_003 + i,"ev":"turn_end","usage":{"input":10,"output":5},"stop_reason":"stop","text_head":"Done."}),
                json!({"seq":4,"t":3_004 + i,"ev":"session_shutdown","reasons":[],"flagged":false}),
            ],
        );
    }
    // An open session: no events file until it closes.
    env.write_records(
        "open-1",
        &[json!({"seq":0,"t":4_000,"ev":"session_start","session_id":"open-1","cwd":env.repo,"loaded_entries":[]})],
    );
    let c = env.run(&["count", "--editor", &script("editor-ok.sh")]);
    assert_eq!(c["counter_rows"], 4, "{c}");
    let d = env.run(&[
        "create", "ledger-test-gate", "--from", "description", "--description", "Run the ledger tests before committing payment changes",
        "--editor", &script("editor-ok.sh"),
    ]);
    assert_eq!(d["outcome"], "staged", "{d}");
    id
}

fn one_file(lake: &Path, table: &str) -> PathBuf {
    let found: Vec<PathBuf> =
        files(&lake.join(table)).keys().filter(|k| k.ends_with(".parquet")).map(|k| lake.join(table).join(k)).collect();
    assert_eq!(found.len(), 1, "{table}: {found:?}");
    found[0].clone()
}

fn field_names(path: &Path) -> (Vec<String>, Vec<rematch_lake::arrow::record_batch::RecordBatch>) {
    let r = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(path).unwrap()).unwrap();
    let names = r.schema().fields().iter().map(|f| f.name().clone()).collect();
    (names, r.build().unwrap().map(|b| b.unwrap()).collect())
}

fn strings<'a>(b: &'a rematch_lake::arrow::record_batch::RecordBatch, col: &str) -> &'a StringArray {
    b.column_by_name(col).unwrap().as_any().downcast_ref::<StringArray>().unwrap()
}

#[test]
fn export_twice_writes_nothing_new_and_a_fresh_root_with_the_same_journal_writes_the_same_bytes() {
    let a = Env::new();
    let id = seed(&a);
    let lake = a.root.join("lake");

    let e1 = a.run(&["lake", "export"]);
    assert!(e1["rows"].as_u64().unwrap() > 0, "{e1}");
    let written: Vec<&str> = e1["files"].as_array().unwrap().iter().map(|f| f.as_str().unwrap()).collect();
    for t in ["sessions", "entries", "counters", "proposals"] {
        assert!(written.iter().any(|f| f.starts_with(&format!("{t}/"))), "{t} file: {written:?}");
    }
    assert!(written.iter().all(|f| f.contains("/test-host-")), "{written:?}");
    let events: Vec<&str> = e1["events"].as_array().unwrap().iter().map(|f| f.as_str().unwrap()).collect();
    assert_eq!(events.len(), 4, "closed sessions only: {events:?}");
    assert!(events.contains(&"events/omp/1970-01-01/k-0.parquet"), "{events:?}");
    assert!(!lake.join("events/omp/1970-01-01/open-1.parquet").exists());
    let manifest: Value = serde_json::from_slice(&std::fs::read(lake.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["lake_version"], 1);
    assert_eq!(manifest["bellwether_version_pinned"], Value::Null);
    assert_eq!(manifest["watermarks"]["test-host"], e1["watermark"]);
    let blob_files: Vec<String> = files(&lake.join("blobs")).into_keys().collect();
    assert!(!blob_files.is_empty(), "proposal blobs travel with the lake");

    let before = files(&lake);
    let e2 = a.run(&["lake", "export"]);
    assert_eq!((e2["rows"].clone(), e2["files"].clone(), e2["events"].clone()), (json!(0), json!([]), json!([])), "{e2}");
    assert_eq!(files(&lake), before, "a second export writes nothing");

    // The entries file reads back with the declared schema, partition columns outside it.
    let (names, batches) = field_names(&one_file(&lake, "entries"));
    assert_eq!(
        names,
        ["seq", "known", "machine", "op", "row_key", "entry_id", "status", "version", "content_sha256", "predicate_hash", "name", "branch", "valid", "json"]
    );
    let b = &batches[0];
    assert_eq!(b.schema().field_with_name("known").unwrap().data_type(), &DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())));
    let ids = strings(b, "entry_id");
    let row = (0..b.num_rows()).find(|&r| ids.value(r) == id).expect("the registered entry has a row");
    assert_eq!(strings(b, "name").value(row), "ledger-helper");
    assert_eq!(strings(b, "machine").value(row), "test-host");
    let j: Value = serde_json::from_str(strings(b, "json").value(row)).unwrap();
    assert_eq!(j["id"], id.as_str());
    assert!(!strings(b, "content_sha256").is_null(row), "L1 identity on the entry");

    let (names, batches) = field_names(&lake.join("events/omp/1970-01-01/k-0.parquet"));
    assert_eq!(
        names,
        ["seq", "t", "ev", "tool", "call_id", "tool_input", "args_hash", "result_bytes", "is_error", "path", "skill_read", "text",
         "prelabel", "text_head", "usage_json", "stop_reason", "reasons", "flagged", "extra"]
    );
    let b = &batches[0];
    assert_eq!(b.num_rows(), 5);
    assert_eq!(strings(b, "ev").value(2), "tool_result");
    assert_eq!(strings(b, "tool_input").value(2), r#"{"command":"cargo test -p payments"}"#);
    assert_eq!(strings(b, "extra").value(2), r#"{"content_head":"test result: ok","error_class":null}"#);

    // A fresh root holding the same journal (and traces) writes the same files, byte for byte.
    let b_env = Env::new();
    std::fs::create_dir_all(&b_env.root).unwrap();
    for f in ["ledger.sqlite", "ledger.sqlite-wal"] {
        if a.root.join(f).exists() {
            std::fs::copy(a.root.join(f), b_env.root.join(f)).unwrap();
        }
    }
    copy_tree(&a.root.join("traces"), &b_env.root.join("traces"));
    copy_tree(&a.root.join("blobs"), &b_env.root.join("blobs"));
    b_env.run(&["lake", "export"]);
    assert_eq!(files(&b_env.root.join("lake")), files(&lake), "same journal → byte-identical lake");
}

#[test]
fn a_second_machines_lake_imports_and_doctor_shows_both_watermarks() {
    let home = Env::new();
    seed(&home);
    home.run(&["lake", "export"]);

    let laptop = Env::new();
    laptop.write_trace("laptop-1", &[], false);
    run_as(&laptop, "laptop", &["count"]);
    let x = run_as(&laptop, "laptop", &["lake", "export"]);
    assert!(x["files"].as_array().unwrap().iter().all(|f| f.as_str().unwrap().contains("/laptop-")), "{x}");
    std::fs::create_dir_all(laptop.root.join("lake-index/entries")).unwrap();
    std::fs::write(laptop.root.join("lake-index/entries/index.bin"), b"never synced").unwrap();

    let i = home.run(&["lake", "import", "--from", laptop.root.to_str().unwrap()]);
    assert!(i["copied"]["sessions"].as_u64().unwrap() >= 1, "{i}");
    assert_eq!(i["copied"]["events"], 1, "{i}");
    assert!(home.root.join("lake/events/omp/1970-01-01/laptop-1.parquet").is_file());
    assert!(!home.root.join("lake-index").exists(), "the index never travels");

    let d = home.run(&["lake", "doctor"]);
    assert_eq!(d["machine"], "test-host");
    assert_eq!(d["watermarks"]["laptop"], x["watermark"], "{d}");
    assert!(d["watermarks"]["test-host"].as_i64().unwrap() > 0, "{d}");
    assert_eq!(d["lag"], 0, "{d}");
    assert_eq!(d["orphans"], json!([]), "{d}");
    assert_eq!(d["feature"], "lake");
    assert!(d["snapshot"].is_null(), "a ledger born with the journal has no pre-journal snapshot: {d}");

    let again = home.run(&["lake", "import", "--from", laptop.root.join("lake").to_str().unwrap()]);
    assert_eq!(again["copied"], json!({}), "{again}");
    // The home ledger's own export is untouched by the import (other machines' rows stay read-only).
    let e = home.run(&["lake", "export"]);
    assert_eq!(e["files"], json!([]), "{e}");
}

#[test]
fn rebuild_from_the_lake_reproduces_library_ls_on_a_fresh_root() {
    let a = Env::new();
    seed(&a);
    a.run(&["lake", "export"]);
    let ls_a = a.run(&["library", "ls"]);
    let max_a = a.run(&["lake", "doctor"])["journal_max_seq"].clone();

    let fresh = Env::new();
    let lake = a.root.join("lake");
    let r = fresh.run(&["lake", "rebuild", "--from", lake.to_str().unwrap()]);
    assert_eq!(r["machine"], "test-host", "{r}");
    assert_eq!(r["last_seq"], max_a, "{r}");
    assert!(r["blobs"].as_u64().unwrap() > 0, "{r}");
    let ls_b = fresh.run(&["library", "ls"]);
    assert_eq!(ls_b["entries"], ls_a["entries"], "entries and their counters");
    assert_eq!(ls_b["lines"], ls_a["lines"]);
    assert_eq!(fresh.run(&["lake", "doctor"])["journal_max_seq"], max_a, "the journal comes back with the state");

    let (v, code) = fresh.run_cmd(fresh.cmd(&["lake", "rebuild", "--from", lake.to_str().unwrap()]));
    assert_eq!(code, 1, "{v}");
    assert!(v["error"].as_str().unwrap().contains("not empty"), "{v}");
    let (v, code) = fresh.run_cmd(fresh.cmd(&["lake", "rebuild", "--from", lake.to_str().unwrap(), "--force", "--machine", "nobody"]));
    assert_eq!(code, 1, "{v}");
    assert!(v["error"].as_str().unwrap().contains("test-host"), "names the machines it has: {v}");
    let f = fresh.run(&["lake", "rebuild", "--from", lake.to_str().unwrap(), "--force"]);
    assert!(PathBuf::from(f["backup"].as_str().unwrap()).is_file(), "{f}");
    assert_eq!(fresh.run(&["library", "ls"])["entries"], ls_a["entries"]);
}

#[test]
fn reflect_pending_ends_with_a_bounded_export() {
    let env = Env::new();
    let sid = "0192c4a0-0000-7000-8000-0000000000f1";
    env.write_trace(sid, &[], true);
    let r = env.run(&["reflect", "--pending", "--editor", &script("editor-ok.sh")]);
    assert_eq!(r["reflections"][0]["outcome"], "staged", "{r}");
    let lake = &r["lake"];
    assert!(lake["rows"].as_u64().unwrap() > 0, "{r}");
    assert_eq!(lake["events"], json!([format!("events/omp/1970-01-01/{sid}.parquet")]), "{r}");
    assert!(lake["files"].as_array().unwrap().iter().any(|f| f.as_str().unwrap().starts_with("proposals/")), "the run's own reflection is in it: {r}");
    let again = env.run(&["reflect", "--pending", "--editor", &script("editor-ok.sh")]);
    assert_eq!((again["lake"]["files"].clone(), again["lake"]["events"].clone()), (json!([]), json!([])), "{again}");
}

/// Imported history has no trace files: its events come from the harness transcripts (the
/// converter `reflect --history` uses), a session whose transcript is gone is recorded as
/// missing once and retried only by `--all`, and `lake doctor` reports the three counts.
#[test]
fn imported_sessions_export_events_from_their_harness_transcripts() {
    let env = Env::new();
    let goldens = env.home.join("goldens");
    copy_tree(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/goldens"), &goldens);
    let d = env.run(&["import", "--history", "--sessions", goldens.to_str().unwrap()]);
    assert_eq!(d["history"]["new"], 3, "{d}");
    assert!(!env.root.join("traces").exists() || std::fs::read_dir(env.root.join("traces")).unwrap().next().is_none(), "no traces: imported only");
    let doc = env.run(&["lake", "doctor"]);
    assert!(doc["lines"].as_array().unwrap().iter().any(|l| l == "events: exported 0, missing 0, pending 3"), "{doc}");
    // One transcript disappears before the first export.
    let gone = goldens.join("skill-read.jsonl");
    let kept = std::fs::read(&gone).unwrap();
    std::fs::remove_file(&gone).unwrap();
    let (bash, skill, sub) = (
        "01a175cc-4c09-7506-813d-17c490009c1c",
        "01a175a9-8b4d-73e2-bf6f-abbb6b304887",
        "01a175c2-7805-768a-8e7c-016db4ef27cf",
    );
    let path = |sid: &str| format!("events/omp/2026-10-26/{sid}.parquet");

    let e1 = env.run(&["lake", "export"]);
    assert_eq!(e1["events"], json!([path(sub), path(bash)]), "session-id order: {e1}");
    assert_eq!(e1["events_missing"], json!([skill]), "{e1}");
    let lake = env.root.join("lake");
    let (_, batches) = field_names(&lake.join(path(bash)));
    let evs: Vec<String> =
        batches.iter().flat_map(|b| { let s = strings(b, "ev"); (0..b.num_rows()).map(|r| s.value(r).to_string()).collect::<Vec<_>>() }).collect();
    assert_eq!(evs.first().map(String::as_str), Some("session_start"), "{evs:?}");
    assert_eq!(evs.iter().filter(|e| *e == "tool_result").count(), 1, "the golden's one tool call: {evs:?}");
    assert_eq!(evs.iter().filter(|e| *e == "turn_end").count(), 2, "{evs:?}");
    let records = rematch_lake::events::read_records(&lake.join(path(bash))).unwrap();
    assert_eq!(records.len(), evs.len());
    let manifest: Value = serde_json::from_slice(&std::fs::read(lake.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["events_missing"], json!([skill]), "{manifest}");

    // Recorded once: a bounded run does not retry it, even with the transcript back.
    std::fs::write(&gone, &kept).unwrap();
    let e2 = env.run(&["lake", "export"]);
    assert_eq!((e2["events"].clone(), e2["events_missing"].clone(), e2["remaining_sessions"].clone()), (json!([]), json!([]), json!(0)), "{e2}");
    let doc = env.run(&["lake", "doctor"]);
    assert_eq!((doc["events_exported"].clone(), doc["events_missing"].clone(), doc["events_pending"].clone()), (json!(2), json!(1), json!(0)), "{doc}");
    assert!(doc["lines"].as_array().unwrap().iter().any(|l| l == "events: exported 2, missing 1, pending 0"), "{doc}");

    // `--all` retries it.
    let e3 = env.run(&["lake", "export", "--all"]);
    assert_eq!(e3["events"], json!([path(skill)]), "{e3}");
    let doc = env.run(&["lake", "doctor"]);
    assert!(doc["lines"].as_array().unwrap().iter().any(|l| l == "events: exported 3, missing 0, pending 0"), "{doc}");
    let manifest: Value = serde_json::from_slice(&std::fs::read(lake.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["events_missing"], json!([]), "{manifest}");
}

/// The binary builds without the `lake` feature, links no arrow, and says so when asked.
#[test]
fn builds_without_the_lake_feature() {
    let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let target = ws.join("target").join("no-lake");
    let out = Command::new(&cargo)
        .args(["build", "--offline", "-p", "rematch", "--no-default-features"])
        .env("CARGO_TARGET_DIR", &target)
        .current_dir(&ws)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let tree = Command::new(&cargo)
        .args(["tree", "--offline", "-p", "rematch", "--no-default-features", "-e", "normal", "--prefix", "none"])
        .current_dir(&ws)
        .output()
        .unwrap();
    let tree = String::from_utf8_lossy(&tree.stdout);
    assert!(tree.contains("rematch-core") && !tree.lines().any(|l| l.starts_with("arrow") || l.starts_with("parquet")), "{tree}");

    let env = Env::new();
    let mut c = Command::new(target.join("debug").join("rematch"));
    c.args(["lake", "doctor"])
        .env("REMATCH_ROOT", &env.root)
        .env("PI_CODING_AGENT_DIR", &env.agent)
        .env("HOME", &env.home)
        .env("REMATCH_MACHINE", "test-host")
        .current_dir(&env.repo);
    let (v, code) = env.run_cmd(c);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["error"], "built without the lake feature");
}
