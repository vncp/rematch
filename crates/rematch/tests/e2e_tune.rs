//! Predicate tuning end to end (DESIGN §6 "Predicate tuning"): a user's skill registered by
//! `improve`, read in imported sessions its predicate missed → `tune --dry-run` proposes the
//! stems those sessions share, `tune` stages the sibling under the guards, the sibling is
//! counted on the next session while the parent keeps serving, `why` and `report` show both
//! versions; matched-but-unread sessions narrow inside `count`; pinned and built-in entries
//! are skipped; the ledger and the lake give the same signals.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use rematch_core::ledger::Ledger;
use rematch_core::types::Entry;
use serde_json::{json, Value};

const DAY_MS: i64 = 86_400_000;
const SKILL: &str = "ledger-helper";
const SKILL_MD: &str = "---\nname: ledger-helper\ndescription: Ledger rounding audits.\n---\n\nLedger rounding audits.\n\n## Laws\n\n- Round half to even before posting a ledger line.\n";

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn day(days_ago: i64) -> String {
    chrono::DateTime::from_timestamp_millis(now_ms() - days_ago * DAY_MS).unwrap().format("%Y-%m-%d").to_string()
}

fn skill_dir(env: &Env) -> PathBuf {
    env.agent.join("skills").join(SKILL)
}

/// A user's skill registered by `improve` before any session read it: its predicate comes
/// from its name and description alone (stems ledger, helper, round, audit; any repo, any
/// extension). Returns its entry id.
fn register(env: &Env) -> String {
    std::fs::create_dir_all(skill_dir(env)).unwrap();
    std::fs::write(skill_dir(env).join("SKILL.md"), SKILL_MD).unwrap();
    let r = env.run(&["improve", SKILL]);
    assert_eq!(r["registered"], true, "{r}");
    r["entry"].as_str().unwrap().to_string()
}

/// An imported omp session `days_ago` that read `skill://ledger-helper` and edited
/// `src/orderbook.py`, on a task its predicate does not name.
fn imported_read(env: &Env, sid: &str, days_ago: i64, task: &str) {
    let file = format!("{}/src/orderbook.py", env.repo.display());
    write_omp_session(env, sid, &day(days_ago), &[
        omp_user(task),
        omp_assistant("", vec![omp_call("c1", "read", json!({"path": format!("skill://{SKILL}")}))], "toolUse"),
        omp_result("c1", "read", "skill text", false),
        omp_assistant("", vec![omp_call("c2", "edit", json!({"path": file, "input": "x"}))], "toolUse"),
        omp_result("c2", "edit", "applied", false),
        omp_assistant("Done.", vec![], "stop"),
    ]);
}

/// The seeded root: the registered skill and three imported sessions that read it while its
/// stems missed. Returns the entry id.
fn seed(env: &Env) -> String {
    let id = register(env);
    imported_read(env, "rm-1", 4, "Fix the orderbook parser for the quote feed");
    imported_read(env, "rm-2", 3, "The orderbook parser drops a quote on reconnect");
    imported_read(env, "rm-3", 2, "Speed up the orderbook parser hot loop");
    env.run(&["import", "--history"]);
    id
}

/// A finished trace `minutes_ago` with `loaded` entries and one task input; `read` adds a
/// read of the skill.
fn trace(env: &Env, sid: &str, minutes_ago: i64, loaded: &[&str], input: &str, read: bool) {
    let t = now_ms() - minutes_ago * 60_000;
    let repo = env.repo.to_string_lossy();
    let mut recs = vec![
        json!({"seq":0,"t":t,"ev":"session_start","session_id":sid,"cwd":repo,"repo":repo,"branch_hint":"payments",
               "model":{"provider":"anthropic","id":"claude-opus-5-5"},"loaded_entries":loaded}),
        json!({"seq":1,"t":t + 1,"ev":"input","text":input,"idle":true,"prelabel":"none","skills_invoked":[]}),
    ];
    if read {
        recs.push(json!({"seq":2,"t":t + 2,"ev":"tool_result","tool":"read","call_id":"c1","input":{"path":format!("skill://{SKILL}")},
                         "is_error":false,"skill_read":SKILL,"path":null}));
    }
    recs.push(json!({"seq":3,"t":t + 3,"ev":"turn_end","usage":{"input":10,"output":5},"stop_reason":"stop","text_head":"Done."}));
    recs.push(json!({"seq":4,"t":t + 4,"ev":"session_shutdown","reasons":[],"flagged":false}));
    env.write_records(sid, &recs);
}

fn ledger(env: &Env) -> Ledger {
    Ledger::open(&env.root.join("ledger.sqlite")).unwrap()
}

fn get(env: &Env, id: &str) -> Entry {
    ledger(env).get_entry(id).unwrap().unwrap()
}

fn row<'a>(out: &'a Value, id: &str) -> &'a Value {
    out["entries"].as_array().unwrap().iter().find(|e| e["id"] == id).unwrap_or_else(|| panic!("{id} not in {out}"))
}

fn lines(out: &Value) -> String {
    out["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect::<Vec<_>>().join("\n")
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect()
}

fn files(dir: &Path) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read_to_string(e.path()).unwrap()))
        .collect();
    v.sort();
    v
}

#[test]
fn read_but_missed_sessions_widen_the_predicate_through_a_staged_sibling() {
    let env = Env::new();
    let id = seed(&env);
    let parent = get(&env, &id);
    assert_eq!(parent.predicate.stems, vec!["ledger", "helper", "round", "audit"], "{:?}", parent.predicate);
    assert!(parent.predicate.file_exts.is_empty() && parent.predicate.repo_glob == "**");
    let skill_before = files(&skill_dir(&env));

    // dry run: signals and the proposal, nothing written
    let dry = env.run(&["tune", "--dry-run"]);
    println!("$ rematch tune --dry-run --text\n{}", lines(&dry));
    assert_eq!(dry["source"], "ledger");
    let e = row(&dry, &id);
    assert_eq!(strings(&e["signals"]["read_missed"]), vec!["rm-1", "rm-2", "rm-3"], "{e}");
    assert_eq!(e["signals"]["matched_unread"], json!([]));
    assert_eq!(strings(&e["proposal"]["added"]), vec!["orderbook", "parser", "quot"], "stems in ≥ 2 of the 3 sessions: {e}");
    assert_eq!(e["proposal"]["added_exts"], json!([]), "an empty extension list stays empty");
    assert_eq!(e["proposal"]["hits_after"], 3);
    assert_eq!(e["outcome"], "would_stage");
    assert!(lines(&dry).contains("ledger-helper v1: read-but-missed 3 · matched-but-unread 0 · matched-and-read 0 → predicate tune: +[orderbook, parser, quot] -[] from 3 session(s) → would stage v2"), "{}", lines(&dry));
    for b in dry["entries"].as_array().unwrap().iter().filter(|x| x["id"] != id.as_str()) {
        assert_eq!((b["outcome"].as_str(), b["reason"].as_str()), (Some("skipped"), Some("built-in entry")), "{b}");
    }
    assert!(get(&env, &id).last_tune_at.is_none(), "a dry run writes nothing");
    assert_eq!(env.run(&["library", "ls"])["entries"].as_array().unwrap().len(), dry["entries"].as_array().unwrap().len());

    // tune: the sibling is staged, the parent untouched but for last_tune_at
    let out = env.run(&["tune", "--entry", SKILL]);
    println!("$ rematch tune --entry {SKILL} --text\n{}", lines(&out));
    let staged = &row(&out, &id)["staged"];
    assert_eq!(row(&out, &id)["outcome"], "staged", "{out}");
    let v2_id = staged["id"].as_str().unwrap().to_string();
    let v2 = get(&env, &v2_id);
    assert_eq!((v2.name.as_str(), v2.version, v2.sibling_of.as_deref()), (SKILL, 2, Some(id.as_str())));
    assert_eq!(rematch_core::ledger::enum_str(&v2.status), "provisional");
    assert_eq!(v2.origin, rematch_core::types::Origin::Import { source: "predicate-tune".into() });
    assert_eq!(v2.status_note.as_deref(), Some("predicate tune: +[orderbook, parser, quot] -[] from 3 session(s)"));
    assert_eq!(v2.predicate.stems, vec!["ledger", "helper", "round", "audit", "orderbook", "parser", "quot"]);
    assert_eq!((v2.predicate.repo_glob.as_str(), v2.predicate.file_exts.len()), ("**", 0));
    let (h1, h2) = (rematch_core::predicate::predicate_hash(&parent.predicate), rematch_core::predicate::predicate_hash(&v2.predicate));
    assert_ne!(h1, h2, "a new predicate hash: a new canary");
    let tune = v2.tune.clone().unwrap();
    assert_eq!((tune.parent.as_str(), tune.parent_version, tune.direction.as_str()), (id.as_str(), 1, "widen"));
    assert_eq!(tune.evidence.read_missed, vec!["rm-1", "rm-2", "rm-3"]);
    assert_eq!(tune.evidence.source, "ledger");
    assert_eq!(v2.path, env.root.join("staging/shared").join(SKILL));
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(v2.path.join("rematch.json")).unwrap()).unwrap();
    assert_eq!(on_disk["tune"]["evidence"]["read_missed"], json!(["rm-1", "rm-2", "rm-3"]), "rematch.json carries the evidence");
    let l = ledger(&env);
    let proposals = l.list_proposals(Some(&v2_id), None).unwrap();
    assert_eq!(proposals.len(), 1);
    let blob: Value = serde_json::from_slice(&l.get_blob(&proposals[0].blob).unwrap().unwrap()).unwrap();
    assert_eq!((blob["decision"].as_str(), &blob["tune"]["evidence"]["read_missed"]), (Some("PREDICATE_TUNE"), &json!(["rm-1", "rm-2", "rm-3"])));
    let after = get(&env, &id);
    assert_eq!((after.predicate.clone(), after.status, after.version, after.content_sha256.clone()), (parent.predicate.clone(), parent.status, 1, parent.content_sha256.clone()));
    assert!(after.last_tune_at.is_some());
    assert_eq!(files(&skill_dir(&env)), skill_before, "the user's skill files are never written");
    assert_eq!(files(&v2.path).iter().find(|(n, _)| n == "SKILL.md").map(|(_, t)| t.contains("Round half to even")), Some(true));

    // within 7 days: skipped (by id: the name now resolves to the newest live version, v2)
    let again = env.run(&["tune", "--entry", &id]);
    let r = row(&again, &id);
    assert_eq!(r["outcome"], "skipped", "{again}");
    assert!(r["reason"].as_str().unwrap().starts_with("tuned ") && r["reason"].as_str().unwrap().contains("next tune is allowed after"), "{r}");

    // the next sessions: v2 counted where its predicate hits, the parent keeps serving
    trace(&env, "next-1", 30, &[&id, &v2_id], "Tune the orderbook parser quote handling", false);
    trace(&env, "next-2", 20, &[&id, &v2_id], "Audit the ledger rounding in the orderbook parser", false);
    env.run(&["count", "--editor", &script("editor-ok.sh")]);
    let ls = env.run(&["library", "ls"]);
    let entry = |i: &str| ls["entries"].as_array().unwrap().iter().find(|e| e["id"] == i).unwrap().clone();
    assert_eq!(entry(&v2_id)["counters"]["matched"], 2, "{ls}");
    assert_eq!(entry(&id)["counters"]["matched"], 1, "{ls}");
    assert_eq!((entry(&id)["status"].as_str(), entry(&v2_id)["status"].as_str()), (Some("provisional"), Some("provisional")));
    let why = env.run(&["library", "why", "next-1"]);
    let text = lines(&why);
    println!("$ rematch library why next-1 --text\n{text}");
    assert!(text.contains(&format!("  predicate tune v2 pending ({v2_id} · predicate tune: +[orderbook, parser, quot] -[] from 3 session(s) · matched 2 recurrence 0 harm 0)")), "{text}");
    let loaded = why["loaded"].as_array().unwrap();
    assert_eq!(loaded.iter().find(|x| x["id"] == id.as_str()).unwrap()["tune_pending"]["id"], v2_id.as_str());

    // report: both versions' with-arms side by side, one trigger for the lineage
    let rep = env.run(&["report"]);
    println!("$ rematch report --text\n{}", lines(&rep));
    let t = &rep["tunes"][0];
    assert_eq!((t["parent"]["id"].as_str(), t["tune"]["id"].as_str()), (Some(id.as_str()), Some(v2_id.as_str())), "{rep}");
    assert_eq!((t["v1"]["sessions"].as_u64(), t["v2"]["sessions"].as_u64()), (Some(1), Some(2)), "{t}");
    assert!(lines(&rep).contains("tune ledger-helper v1→v2 ("), "{}", lines(&rep));
    let triggers: Vec<&Value> = rep["triggers"].as_array().unwrap().iter().filter(|x| x["names"] == SKILL).collect();
    assert_eq!(triggers.len(), 1, "the sibling joins its parent's trigger: {rep}");
    assert_eq!(triggers[0]["with"]["sessions"], 2, "each session counted by the version it loaded: {}", triggers[0]);
}

/// Matched-but-unread sessions narrow the predicate inside `count` (after its decisions):
/// the stems only unread sessions carry go, two always stay.
#[test]
fn matched_but_unread_sessions_narrow_inside_count() {
    let env = Env::new();
    let id = register(&env);
    for i in 0..2 {
        trace(&env, &format!("read-{i}"), 90 - i, &[&id], "Check the ledger helper output", true);
    }
    for i in 0..4 {
        trace(&env, &format!("unread-{i}"), 60 - i, &[&id], "Ledger helper for rounding audits", false);
    }
    let c = env.run(&["count", "--editor", &script("editor-ok.sh")]);
    println!("$ rematch count --text\n{}", lines(&c));
    let t = &c["tune"]["entries"][0];
    assert_eq!((t["id"].as_str(), t["outcome"].as_str()), (Some(id.as_str()), Some("staged")), "{c}");
    assert_eq!((strings(&t["signals"]["matched_read"]).len(), strings(&t["signals"]["matched_unread"]).len()), (2, 4), "{t}");
    assert_eq!((t["proposal"]["direction"].as_str(), strings(&t["proposal"]["removed"])), (Some("narrow"), vec!["round".to_string(), "audit".into()]));
    let v2 = get(&env, t["staged"]["id"].as_str().unwrap());
    assert_eq!(v2.predicate.stems, vec!["ledger", "helper"], "never below two stems");
    assert_eq!(v2.status_note.as_deref(), Some("predicate tune: +[] -[round, audit] from 4 session(s)"));
    assert!(lines(&c).contains("tune: ledger-helper v1: read-but-missed 0 · matched-but-unread 4 · matched-and-read 2 → predicate tune: +[] -[round, audit] from 4 session(s) → staged v2"), "{}", lines(&c));
    // the parent was kept by the same count (6 clean matched sessions); its slot stays its own
    assert_eq!(rematch_core::ledger::enum_str(&get(&env, &id).status), "kept");
}

#[test]
fn pinned_entries_are_skipped() {
    let env = Env::new();
    let id = seed(&env);
    env.run(&["library", "pin", &id]);
    let out = env.run(&["tune", "--entry", SKILL]);
    let r = row(&out, &id);
    assert_eq!((r["outcome"].as_str(), r["reason"].as_str()), (Some("skipped"), Some("pinned (human-owned)")), "{r}");
    assert!(!r["proposal"].is_null(), "the signals and the proposal are still shown");
    assert!(get(&env, &id).last_tune_at.is_none());
}

/// The lake and the ledger journal give the same signals and the same proposal.
#[cfg(feature = "lake")]
#[test]
fn ledger_and_lake_sources_agree() {
    let env = Env::new();
    let id = seed(&env);
    let ledger_out = env.run(&["tune", "--dry-run", "--entry", SKILL]);
    env.run(&["lake", "export", "--all"]);
    let lake_out = env.run(&["tune", "--dry-run", "--entry", SKILL]);
    assert_eq!((ledger_out["source"].as_str(), lake_out["source"].as_str()), (Some("ledger"), Some("lake")));
    let strip = |out: &Value| {
        let mut r = row(out, &id).clone();
        r["signals"]["source"] = Value::Null;
        r["proposal"]["evidence"]["source"] = Value::Null;
        r
    };
    assert_eq!(strip(&ledger_out), strip(&lake_out));
    assert_eq!(strings(&row(&lake_out, &id)["signals"]["read_missed"]), vec!["rm-1", "rm-2", "rm-3"]);
}
