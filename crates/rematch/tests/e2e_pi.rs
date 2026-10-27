//! End-to-end for Pi history (DESIGN §18.3): a handwritten Pi session with a negation redirect
//! → `import --history --harness pi` (Pi's agent dir from `REMATCH_PI_AGENT_DIR`, as the Pi
//! adapter exports it) → `reflect --history` with a scripted editor stages an entry →
//! `skills sync --harness pi` links it into the shared dir → a later clean Pi session whose
//! skill list offered the link imports with that entry loaded and `count` folds it. Also: omp's
//! `PI_CODING_AGENT_DIR` is never taken for Pi's under `--harness all`.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use serde_json::{json, Value};

const ID: &str = "019a1f00-1111-7222-8333-944455556666";

/// A Pi v3 session file `<agent>/sessions/--<cwd>--/<ts>_<id>.jsonl`: header, then `entries`
/// chained by `parentId`, one second apart from `t0`.
fn write_pi_session(agent: &Path, cwd: &Path, id: &str, t0: chrono::DateTime<chrono::Utc>, entries: &[Value]) -> PathBuf {
    let ts = |s: i64| (t0 + chrono::Duration::seconds(s)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let slug = format!("--{}--", cwd.to_string_lossy().trim_start_matches('/').replace('/', "-"));
    let dir = agent.join("sessions").join(slug);
    std::fs::create_dir_all(&dir).unwrap();
    let mut lines = vec![json!({"type": "session", "version": 3, "id": id, "timestamp": ts(0), "cwd": cwd})];
    let mut parent: Option<String> = None;
    for (i, m) in entries.iter().enumerate() {
        let eid = format!("{:08x}", i + 1);
        lines.push(json!({"type": "message", "id": eid, "parentId": parent, "timestamp": ts(i as i64 + 1), "message": m}));
        parent = Some(eid);
    }
    let path = dir.join(format!("{}_{id}.jsonl", ts(0).replace([':', '.'], "-")));
    std::fs::write(&path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
    path
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text, "timestamp": 1})
}

fn assistant(text: &str, calls: Vec<Value>, stop: &str) -> Value {
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    content.extend(calls);
    json!({"role": "assistant", "content": content, "api": "openai-completions", "provider": "openai", "model": "gpt-6.1-sol",
           "usage": {"input": 100, "output": 20, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 120,
                     "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
           "stopReason": stop, "timestamp": 1})
}

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"type": "toolCall", "id": id, "name": name, "arguments": args})
}

fn result(id: &str, name: &str, text: &str, is_error: bool) -> Value {
    json!({"role": "toolResult", "toolCallId": id, "toolName": name, "content": [{"type": "text", "text": text}], "isError": is_error, "timestamp": 1})
}

fn system_with_skill(location: &Path) -> Value {
    let skills = format!(
        "<available_skills>\n  <skill>\n    <name>run-tests-before-commit</name>\n    <description>Use when committing.</description>\n    <location>{}</location>\n  </skill>\n</available_skills>",
        location.display()
    );
    json!({"role": "system", "content": "", "sections": {"skills": skills}, "timestamp": 1})
}

fn import_pi(env: &Env, pi_agent: &Path) -> Value {
    let mut c = env.cmd(&["import", "--history", "--harness", "pi"]);
    c.env("REMATCH_PI_AGENT_DIR", pi_agent);
    let (v, code) = env.run_cmd(c);
    assert_eq!(code, 0, "{v}");
    v["data"]["history_pi"].clone()
}

#[test]
fn pi_history_reflects_syncs_and_counts() {
    let env = Env::new();
    let pi_agent = env.home.join(".pi/agent");
    let file = format!("{}/src/ledger.rs", env.repo.display());
    write_pi_session(&pi_agent, &env.repo, ID, chrono::Utc::now() - chrono::Duration::hours(1), &[
        user("Add the ledger reconciliation command to the payments crate"),
        assistant("", vec![call("c1", "edit", json!({"path": file, "oldText": "x", "newText": "pub fn reconcile() {}"}))], "toolUse"),
        result("c1", "edit", "applied", false),
        assistant("Added the reconcile command and committed it.", vec![], "stop"),
        user("No, that's wrong: run the tests before you commit; the reconcile tests fail"),
        assistant("Running the reconcile tests now.", vec![], "stop"),
    ]);
    let sid = format!("pi:{ID}");

    let h = import_pi(&env, &pi_agent);
    assert_eq!((h["files"].clone(), h["new"].clone(), h["subagent_files"].clone()), (json!(1), json!(1), json!(0)), "{h}");
    let w = env.run(&["library", "why", &sid]);
    let s = &w["session"];
    assert_eq!((s["source"].as_str(), s["turns"].as_u64()), (Some("import"), Some(3)), "{w}");
    assert_eq!(s["model"], json!({"provider": "openai", "id": "gpt-6.1-sol"}), "{w}");
    assert_eq!(w["redirects"][0]["prelabel"], "negation", "{w}");
    // Idempotent.
    let again = import_pi(&env, &pi_agent);
    assert_eq!((again["new"].clone(), again["updated"].clone()), (json!(0), json!(1)), "{again}");

    // reflect --history reads the Pi file back by its `pi:` id and stages an entry.
    let editor = script("editor-ok.sh");
    let r = env.run(&["reflect", "--history", "--editor", &editor]);
    assert_eq!(r["history"]["handed"], json!([sid]), "{r}");
    let staged = r["history"]["sessions"].as_array().unwrap().iter().find(|x| x["session"] == sid).cloned().unwrap_or(Value::Null);
    assert_eq!((staged["outcome"].as_str(), staged["name"].as_str()), (Some("staged"), Some("run-tests-before-commit")), "{r}");
    assert!(env.calls().len() <= 3, "≤ 3 model calls per reflection: {:?}", env.calls());
    let recs: Vec<Value> = std::fs::read_to_string(env.root.join(format!("traces/{sid}.jsonl")))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!((recs[0]["ev"].as_str(), recs[0]["mode"].as_str(), recs[0]["omp_version"].as_str()), (Some("session_start"), Some("import"), Some("pi")));
    assert_eq!(recs.last().unwrap()["reasons"], json!(["redirect:negation"]));
    let input = recs.iter().find(|x| x["prelabel"] == "negation").unwrap();
    assert_eq!(input["after_deliverable"], "Added the reconcile command and committed it.");

    // Delivery: the shared dir Pi's package returns from `resources_discover`.
    let d = env.run(&["skills", "sync", "--harness", "pi", "--cwd", env.repo.to_str().unwrap()]);
    let link = env.root.join("skills/run-tests-before-commit");
    assert_eq!(std::fs::read_link(&link).unwrap(), env.staged_dir(), "{d}");
    assert_eq!(d["harness"], "pi", "{d}");
    assert!(d["lines"].to_string().contains("resources_discover"), "{d}");

    // A clean Pi session after the entry exists, whose skill list offered the link: imported
    // with the entry loaded (version + sha from its rematch.json), traced for `count`.
    let id2 = "019a1f00-1111-7222-8333-000000000002";
    write_pi_session(&pi_agent, &env.repo, id2, chrono::Utc::now() + chrono::Duration::seconds(60), &[
        system_with_skill(&link.join("SKILL.md")),
        user("Add a dry run to the ledger reconciliation command; run the tests before you commit"),
        assistant("", vec![call("c1", "read", json!({"path": link.join("SKILL.md")}))], "toolUse"),
        result("c1", "read", "# run-tests-before-commit", false),
        assistant("", vec![call("c2", "edit", json!({"path": file, "oldText": "x", "newText": "pub fn dry_run() {}"}))], "toolUse"),
        result("c2", "edit", "applied", false),
        assistant("", vec![call("c3", "bash", json!({"command": "cargo test -p payments"}))], "toolUse"),
        result("c3", "bash", "test result: ok. 4 passed; 0 failed", false),
        assistant("Dry run added; tests pass.", vec![], "stop"),
    ]);
    let h2 = import_pi(&env, &pi_agent);
    assert_eq!(h2["traces_written"], json!(1), "{h2}");
    let entry_id = staged["id"].as_str().unwrap();
    let w2 = env.run(&["library", "why", &format!("pi:{id2}")]);
    assert_eq!(w2["loaded"][0]["id"], entry_id, "the offered skill is the session's loaded entry: {w2}");
    assert!(w2["session"]["skill_reads"].to_string().contains("run-tests-before-commit"), "{w2}");
    let counted = env.run(&["count", "--editor", &editor]);
    assert!(counted["folded"].as_array().unwrap().contains(&json!(format!("pi:{id2}"))), "{counted}");
    let e = env.entry(entry_id);
    assert_eq!((e["counters"]["matched"].as_u64(), e["counters"]["harm"].as_u64()), (Some(1), Some(0)), "{e}");
}

#[test]
fn omp_agent_dir_is_not_imported_as_pi() {
    let env = Env::new();
    // `env.agent` is omp's agent dir (the harness sets `PI_CODING_AGENT_DIR` to it); its sessions
    // are omp's and must not come back under `pi:` ids.
    redirected_session(&env, "0199aaaa-0000-7000-8000-000000000001", "2026-10-01");
    let v = env.run(&["import", "--history"]);
    assert_eq!(v["history"]["new"], json!(1), "{v}");
    assert!(v.get("history_pi").is_none(), "{v}");
    assert!(v["lines"].to_string().contains("is an omp sessions dir, skipped"), "{v}");
    let ls = env.run(&["status"]);
    assert!(!ls.to_string().contains("pi:0199aaaa"), "{ls}");

    // Explicitly, with Pi's dir named the way the adapter names it: imported as Pi.
    let pi_agent = env.home.join(".pi/agent");
    write_pi_session(&pi_agent, &env.repo, ID, chrono::Utc::now(), &[user("hello"), assistant("hi", vec![], "stop")]);
    let h = import_pi(&env, &pi_agent);
    assert_eq!(h["new"], json!(1), "{h}");
    assert_eq!(h["dir"], json!(pi_agent.join("sessions")), "{h}");
}
