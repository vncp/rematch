//! End-to-end for the Codex adapter (DESIGN §18): a handwritten Codex rollout with a
//! negation redirect → `import --history --harness codex` → `reflect --history` with a
//! scripted editor stages an entry → `skills sync --harness codex --cwd <repo>` links it
//! into the shared `REMATCH_ROOT/skills/`, which Codex reads through `~/.agents/skills/rematch`.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use serde_json::{json, Value};

const ID: &str = "01a15c11-dfc7-7ea3-b4af-620b25f469a0";

/// `<codex home>/sessions/2026/10/20/rollout-…-<id>.jsonl` from the fixture, cwd = the env repo.
fn write_rollout(codex_home: &Path, repo: &Path) -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-rollout.jsonl");
    let text = std::fs::read_to_string(fixture).unwrap().replace("__REPO__", &repo.to_string_lossy());
    let day = codex_home.join("sessions/2026/10/20");
    std::fs::create_dir_all(&day).unwrap();
    let path = day.join(format!("rollout-2026-10-20T10-00-00-{ID}.jsonl"));
    std::fs::write(&path, text).unwrap();
    // A reviewer (subagent) thread of the same day: imported, never reflected.
    let sub = json!({"timestamp": "2026-10-20T11:00:00.000Z", "type": "session_meta", "payload": {
        "id": "01a15c11-0000-7000-8000-000000000002", "timestamp": "2026-10-20T11:00:00.000Z", "cwd": repo,
        "cli_version": "0.163.0", "source": {"subagent": {"other": "guardian"}}, "model_provider": "openai",
        "parent_thread_id": ID}});
    let review = json!({"timestamp": "2026-10-20T11:00:01.000Z", "type": "event_msg", "payload": {"type": "item_completed",
        "item": {"type": "UserMessage", "id": "r1", "content": [{"type": "text", "text": "No, that's wrong: review again"}]}}});
    std::fs::write(day.join("rollout-2026-10-20T11-00-00-01a15c11-0000-7000-8000-000000000002.jsonl"), format!("{sub}\n{review}\n"))
        .unwrap();
    path
}

#[test]
fn codex_history_reflects_and_syncs() {
    let env = Env::new();
    let codex_home = env.home.join(".codex");
    write_rollout(&codex_home, &env.repo);
    let sid = format!("codex:{ID}");

    // Default dir = $CODEX_HOME/sessions.
    let mut c = env.cmd(&["import", "--history", "--harness", "codex"]);
    c.env("CODEX_HOME", &codex_home);
    let (v, code) = env.run_cmd(c);
    assert_eq!(code, 0, "{v}");
    let h = &v["data"]["history_codex"];
    assert_eq!((h["files"].clone(), h["new"].clone(), h["subagent_files"].clone()), (json!(2), json!(2), json!(1)), "{v}");

    let w = env.run(&["library", "why", &sid]);
    let s = &w["session"];
    assert_eq!((s["source"].as_str(), s["tool_errors"].as_u64(), s["turns"].as_u64()), (Some("import"), Some(1), Some(3)), "{w}");
    assert_eq!(s["model"], json!({"provider": "openai", "id": "gpt-6.1-sol"}), "{w}");
    assert_eq!(w["redirects"][0]["prelabel"], "negation", "{w}");

    // reflect --history works unchanged on the Codex session; the subagent thread is skipped.
    let editor = script("editor-ok.sh");
    let r = env.run(&["reflect", "--history", "--editor", &editor]);
    let hist = &r["history"];
    assert_eq!(hist["handed"], json!([sid]), "{r}");
    let staged = hist["sessions"].as_array().unwrap().iter().find(|x| x["session"] == sid).cloned().unwrap_or(Value::Null);
    assert_eq!((staged["outcome"].as_str(), staged["name"].as_str()), (Some("staged"), Some("run-tests-before-commit")), "{r}");
    let sub = hist["sessions"].as_array().unwrap().iter().find(|x| x["session"] != sid).cloned().unwrap_or(Value::Null);
    assert_eq!((sub["outcome"].as_str(), sub["reason"].as_str()), (Some("skipped"), Some("subagent")), "{r}");
    assert!(env.calls().len() <= 3, "≤ 3 model calls per reflection: {:?}", env.calls());
    let skill_dir = env.staged_dir();
    assert!(skill_dir.join("SKILL.md").is_file(), "{r}");

    let trace = std::fs::read_to_string(env.root.join(format!("traces/{sid}.jsonl"))).unwrap();
    let recs: Vec<Value> = trace.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!((recs[0]["ev"].as_str(), recs[0]["mode"].as_str()), (Some("session_start"), Some("import")));
    assert_eq!(recs[0]["session_id"], sid);
    assert_eq!(recs.last().unwrap()["reasons"], json!(["redirect:negation"]));
    let input = recs.iter().find(|x| x["prelabel"] == "negation").unwrap();
    assert_eq!(input["text"], "No, that's wrong: run the tests before you commit; the reconcile tests fail", "ambient block stripped");
    assert_eq!(input["after_deliverable"], "Added the reconcile command and committed it.");

    // Delivery: the provisional entry matches this repo → linked in the shared dir. An older
    // version's layout is migrated: its `~/.agents/skills/rematch → codex-skills` link is
    // re-pointed and its marked kept copy removed.
    let skills_root = env.home.join(".agents/skills");
    std::fs::create_dir_all(skills_root.join("old-kept")).unwrap();
    std::fs::write(skills_root.join("old-kept/.rematch-codex.json"), "{}").unwrap();
    std::fs::create_dir_all(skills_root.join("mine")).unwrap();
    std::fs::create_dir_all(env.root.join("codex-skills")).unwrap();
    std::os::unix::fs::symlink(env.root.join("codex-skills"), skills_root.join("rematch")).unwrap();
    let d = env.run(&["skills", "sync", "--harness", "codex", "--cwd", env.repo.to_str().unwrap()]);
    let link = env.root.join("skills/run-tests-before-commit");
    assert_eq!(std::fs::read_link(&link).unwrap(), skill_dir, "{d}");
    assert_eq!(d["added"], json!(["run-tests-before-commit"]), "{d}");
    assert_eq!(std::fs::read_link(skills_root.join("rematch")).unwrap(), env.root.join("skills"));
    assert_eq!(d["reached_from"], json!([skills_root.join("rematch")]), "{d}");
    assert!(skills_root.join("rematch/run-tests-before-commit/SKILL.md").is_file());
    assert!(!skills_root.join("old-kept").exists() && skills_root.join("mine").is_dir() && !env.root.join("codex-skills").exists());

    // Counting: a later, clean Codex session in the repo listed the synced skill to the
    // model (`## Skills`, through ~/.agents/skills/rematch); its import writes the trace and
    // `count` folds it into the entry's counters.
    let id2 = "01a15c11-0000-7000-8000-000000000003";
    let clean = clean_rollout(&env.repo, &skills_root, id2);
    let day = codex_home.join("sessions/2026/10/21");
    std::fs::create_dir_all(&day).unwrap();
    std::fs::write(day.join(format!("rollout-2026-10-21T10-00-00-{id2}.jsonl")), clean).unwrap();
    let mut c = env.cmd(&["import", "--history", "--harness", "codex"]);
    c.env("CODEX_HOME", &codex_home);
    let (v, code) = env.run_cmd(c);
    assert_eq!((code, v["data"]["history_codex"]["traces_written"].clone()), (0, json!(1)), "{v}");
    let entry_id = staged["id"].as_str().unwrap();
    let w2 = env.run(&["library", "why", &format!("codex:{id2}")]);
    assert_eq!(w2["loaded"][0]["id"], entry_id, "the offered skill is the session's loaded entry: {w2}");
    let counted = env.run(&["count", "--editor", &editor]);
    assert!(counted["folded"].as_array().unwrap().contains(&json!(format!("codex:{id2}"))), "{counted}");
    let e = env.entry(entry_id);
    assert_eq!(
        (e["counters"]["matched"].as_u64(), e["counters"]["recurrence"].as_u64(), e["counters"]["harm"].as_u64()),
        (Some(1), Some(0), Some(0)),
        "{e}"
    );

    // Another directory: no predicate hit, the link stays (union); Codex is told how to see
    // the dir when no link reaches it.
    let elsewhere = env.home.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::remove_file(skills_root.join("rematch")).unwrap();
    let d2 = env.run(&["skills", "sync", "--harness", "codex", "--cwd", elsewhere.to_str().unwrap()]);
    assert_eq!((d2["added"].clone(), d2["pruned"].clone(), d2["reached_from"].clone()), (json!([]), json!([]), json!([])), "{d2}");
    assert!(d2["lines"].to_string().contains("ln -s"), "{d2}");
    assert_eq!(std::fs::read_link(&link).unwrap(), skill_dir);
}

/// A clean Codex session started a minute from now (after the entry exists) whose skill list
/// names the synced link; the user asks for ledger work, tests pass, no redirect.
fn clean_rollout(repo: &Path, skills_root: &Path, id: &str) -> String {
    let t0 = chrono::Utc::now() + chrono::Duration::seconds(60);
    let ts = |s: i64| (t0 + chrono::Duration::seconds(s)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let body = format!(
        "## Skills\n### Skill roots\n- `r0` = `{}`\n### Available skills\n\
         - run-tests-before-commit: Use when committing changes to a Rust crate. (file: r0/rematch/run-tests-before-commit/SKILL.md)\n",
        skills_root.display()
    );
    let item = |s: i64, item: Value| json!({"timestamp": ts(s), "type": "event_msg", "payload": {"type": "item_completed", "item": item}});
    let ev = |s: i64, ty: &str| json!({"timestamp": ts(s), "type": "event_msg", "payload": {"type": ty}});
    [
        json!({"timestamp": ts(0), "type": "session_meta", "payload": {"id": id, "timestamp": ts(0), "cwd": repo,
            "cli_version": "0.163.0", "source": "vscode", "model_provider": "openai", "parent_thread_id": null}}),
        json!({"timestamp": ts(0), "type": "world_state", "payload": {"full": true, "state": {"host_skills": {"body": body}}}}),
        ev(1, "task_started"),
        json!({"timestamp": ts(1), "type": "turn_context", "payload": {"cwd": repo, "model": "gpt-6.1-sol"}}),
        item(2, json!({"type": "UserMessage", "id": "u1", "content": [{"type": "text",
            "text": "Add a dry run to the ledger reconciliation command; run the tests before you commit"}]})),
        item(3, json!({"type": "FileChange", "id": "f1", "status": "completed",
            "changes": {format!("{}/src/ledger.rs", repo.display()): {"type": "update"}}})),
        item(4, json!({"type": "CommandExecution", "id": "x1", "command": ["/bin/zsh", "-lc", "cargo test -p payments"],
            "status": "completed", "aggregated_output": "test result: ok. 4 passed; 0 failed", "exit_code": 0})),
        item(5, json!({"type": "AgentMessage", "id": "m1", "content": [{"type": "Text", "text": "Dry run added; tests pass."}], "phase": "final_answer"})),
        ev(6, "task_complete"),
    ]
    .iter()
    .map(|l| format!("{l}\n"))
    .collect()
}
