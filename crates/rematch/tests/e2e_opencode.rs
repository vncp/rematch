//! End-to-end through the OpenCode adapter's binary side: the plugin's envelopes
//! (`trace-event --harness opencode`, `hook-check --harness opencode`) → `traces/oc:<id>…`,
//! segments, pending files, approved hooks; and OpenCode's SQLite store →
//! `import --history --harness opencode` → `reflect --history`. The real runtime (the pinned
//! `opencode` binary loading the plugin) is driven by `adapters/opencode/test/smoke.ts`.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use common::*;
use serde_json::{json, Value};

fn call(env: &Env, op: &str, payload: &Value) -> Value {
    let mut c = env.cmd(&[op, "--harness", "opencode"]);
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout} {}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(v["ok"], true, "{v}");
    v["data"].clone()
}

fn send(env: &Env, pid: u32, events: Vec<Value>) -> Vec<Value> {
    let payload = json!({"pid": pid, "directory": env.repo, "version": "1.19.4", "skills_paths": [env.root.join("skills")], "events": events});
    let r = call(env, "trace-event", &payload)["events"].as_array().unwrap().clone();
    for e in &r {
        assert!(e.get("error").is_none(), "{e}");
    }
    r
}

fn prompt(sid: &str, t: i64, text: &str) -> Value {
    json!({"kind": "prompt", "session": sid,
           "message": {"id": format!("msg_u{t}"), "role": "user", "sessionID": sid, "time": {"created": t}, "agent": "build",
                       "model": {"providerID": "mock", "modelID": "scripted"}},
           "parts": [{"type": "text", "text": text, "id": format!("prt_u{t}")}]})
}

fn tool(sid: &str, call: &str, name: &str, status: &str, input: Value, out: &str, exit: Option<i64>) -> Value {
    let mut state = json!({"status": status, "input": input, "time": {"start": 1, "end": 2}});
    if status == "error" {
        state["error"] = json!(out);
    } else {
        state["output"] = json!(out);
        state["metadata"] = json!({"exit": exit});
    }
    json!({"kind": "tool", "session": sid, "part": {"id": format!("prt_{call}"), "type": "tool", "tool": name, "callID": call, "sessionID": sid, "state": state}})
}

fn turn(sid: &str, t: i64, finish: &str, text: &str) -> Value {
    json!({"kind": "turn", "session": sid, "text": text,
           "info": {"id": format!("msg_a{t}"), "role": "assistant", "sessionID": sid, "modelID": "scripted", "providerID": "mock", "finish": finish,
                    "cost": 0, "tokens": {"input": 11, "output": 7, "reasoning": 0, "cache": {"read": 0, "write": 0}, "total": 18},
                    "time": {"created": t, "completed": t + 1}}})
}

fn idle(sid: &str) -> Value {
    json!({"kind": "idle", "session": sid})
}

fn trace(env: &Env, sid: &str) -> Vec<Value> {
    let p = env.trace_path(sid);
    std::fs::read_to_string(p).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn evs(recs: &[Value]) -> Vec<&str> {
    recs.iter().map(|r| r["ev"].as_str().unwrap()).collect()
}

/// The pid of a process that has exited.
fn dead_pid() -> u32 {
    let mut c = std::process::Command::new("true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

#[test]
fn live_runs_segments_and_finalization() {
    let env = Env::new();
    let me = std::process::id();
    let main = "ses_main1";
    let file = format!("{}/src/ledger.rs", env.repo.display());

    // One run: typed prompt, a skill read through OpenCode's skill tool, a success, a non-zero
    // exit, a failed read, a rejected call; two model turns; the run goes idle.
    let r = send(
        &env,
        me,
        vec![
            prompt(main, 1_000, "Add the ledger reconciliation command"),
            tool(main, "c1", "skill", "completed", json!({"name": "run-tests-before-commit"}), "<skill_content>", None),
            tool(main, "c2", "bash", "completed", json!({"command": "cargo build"}), "ok", Some(0)),
            tool(main, "c3", "bash", "completed", json!({"command": "ls /nope"}), "ls: /nope: No such file", Some(1)),
            tool(main, "c4", "read", "error", json!({"filePath": file}), "File not found", None),
            turn(main, 1_100, "tool-calls", ""),
            turn(main, 1_200, "stop", "Added the reconcile command."),
            idle(main),
        ],
    );
    assert_eq!(r[0]["started"], "oc:ses_main1");
    let recs = trace(&env, "oc:ses_main1");
    assert_eq!(evs(&recs), ["session_start", "input", "tool_result", "tool_result", "tool_result", "tool_result", "turn_end", "turn_end", "agent_end"]);
    let s = &recs[0];
    assert_eq!((s["mode"].as_str(), s["omp_version"].as_str(), s["agent"]["kind"].as_str()), (Some("opencode"), Some("opencode 1.19.4"), Some("main")));
    assert_eq!(s["model"], json!({"provider": "mock", "id": "scripted"}));
    assert_eq!(recs[2]["skill_read"], "run-tests-before-commit");
    assert_eq!(recs.iter().filter(|r| r["is_error"] == true).count(), 2);
    assert_eq!(recs[5]["path"], file.as_str());
    assert_eq!(recs[7]["text_head"], "Added the reconcile command.");
    assert_eq!((recs[8]["turns"].as_u64(), recs[8]["yielded"].as_bool()), (Some(2), Some(true)));
    let owner = env.root.join("opencode/open/oc:ses_main1.json");
    assert!(owner.is_file(), "the process owns the open segment");

    // Replayed payloads (a plugin restart, a duplicate bus event) write nothing.
    send(&env, me, vec![prompt(main, 1_000, "Add the ledger reconciliation command"), tool(main, "c2", "bash", "completed", json!({"command": "cargo build"}), "ok", Some(0)), turn(main, 1_200, "stop", "Added the reconcile command.")]);
    assert_eq!(trace(&env, "oc:ses_main1").len(), recs.len());

    // Idle → a new prompt in the same process: the same segment, one more run.
    send(
        &env,
        me,
        vec![
            prompt(main, 2_000, "No, that's wrong: run the tests before you commit"),
            json!({"kind": "prompt", "session": main, "message": {"id": "msg_syn", "role": "user", "time": {"created": 2_001}},
                   "parts": [{"type": "text", "text": "Called the Read tool with the following input", "synthetic": true}]}),
            turn(main, 2_100, "stop", "Ran the tests first."),
            idle(main),
        ],
    );
    let recs = trace(&env, "oc:ses_main1");
    let inputs: Vec<&Value> = recs.iter().filter(|r| r["ev"] == "input").collect();
    assert_eq!(inputs.len(), 2, "a synthetic-only message is no prompt");
    assert_eq!((inputs[1]["prelabel"].as_str(), inputs[1]["idle"].as_bool()), (Some("negation"), Some(true)));
    assert_eq!(inputs[1]["after_deliverable"], "Added the reconcile command.");
    assert_eq!(evs(&recs).iter().filter(|e| **e == "session_start").count(), 1);

    // A subagent's events are not traced; a concurrent session keeps its own trace, and a
    // prompt delivered into its running run is a takeover.
    let r = send(&env, me, vec![json!({"kind": "prompt", "session": "ses_child", "parent": main, "message": {"time": {"created": 3}}, "parts": [{"type": "text", "text": "child work"}]})]);
    assert_eq!(r[0]["skipped"], "subagent");
    assert!(!env.trace_path("oc:ses_child").exists());
    send(
        &env,
        me,
        vec![
            prompt("ses_other", 5_000, "Write the migration for the payments table"),
            tool("ses_other", "o1", "bash", "completed", json!({"command": "make"}), "ok", Some(0)),
            turn("ses_other", 5_100, "tool-calls", ""),
            prompt("ses_other", 5_200, "Use the existing migration helper instead of a new file"),
        ],
    );
    let other = trace(&env, "oc:ses_other");
    assert_eq!(evs(&other), ["session_start", "input", "tool_result", "turn_end", "agent_end", "input"]);
    assert_eq!((other[4]["yielded"].as_bool(), other[5]["prelabel"].as_str(), other[5]["idle"].as_bool()), (Some(false), Some("interrupt"), Some(false)));
    assert_eq!(trace(&env, "oc:ses_main1").len(), recs.len(), "sessions never mix");

    // The process disposes the plugin: every segment it owned ends, once.
    let r = send(&env, me, vec![json!({"kind": "shutdown"})]);
    let done = r[0]["finalized"].as_array().unwrap();
    assert_eq!(done.len(), 2, "{r:?}");
    let recs = trace(&env, "oc:ses_main1");
    let last = recs.last().unwrap();
    assert_eq!((last["ev"].as_str(), last["flagged"].as_bool()), (Some("session_shutdown"), Some(true)));
    assert_eq!(last["reasons"], json!(["redirect:negation"]));
    let pending_file = env.root.join("pending").join(format!("{}.json", rematch_core::paths::id_file_stem("oc:ses_main1")));
    let pending: Value = serde_json::from_str(&std::fs::read_to_string(pending_file).unwrap()).unwrap();
    assert_eq!((pending["source"].as_str(), pending["session_id"].as_str()), (Some("opencode"), Some("oc:ses_main1")));
    let other = trace(&env, "oc:ses_other");
    assert_eq!(&evs(&other)[6..], ["agent_end", "session_shutdown"], "the open run ends before the shutdown");
    assert!(!owner.exists());
    assert_eq!(send(&env, me, vec![json!({"kind": "shutdown"})])[0]["finalized"], json!([]));

    // Continued later (another process): a new segment; the finished one is never appended to.
    let frozen = std::fs::read_to_string(env.trace_path("oc:ses_main1")).unwrap();
    let r = send(&env, 4242, vec![prompt(main, 9_000, "Now add a dry-run flag to the command"), turn(main, 9_100, "stop", "Done."), idle(main)]);
    assert_eq!(r[0]["started"], "oc:ses_main1:r2");
    assert_eq!(std::fs::read_to_string(env.trace_path("oc:ses_main1")).unwrap(), frozen);
    let r2 = trace(&env, "oc:ses_main1:r2");
    assert_eq!((r2[0]["mode"].as_str(), r2[0]["session_id"].as_str()), (Some("opencode:resume"), Some("oc:ses_main1:r2")));
    assert_eq!(evs(&r2), ["session_start", "input", "turn_end", "agent_end"]);

    // count folds each finished segment once; the open r2 waits.
    let c = env.run(&["count"]);
    let folded: Vec<&str> = c["folded"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(folded.contains(&"oc:ses_main1") && folded.contains(&"oc:ses_other") && !folded.contains(&"oc:ses_main1:r2"), "{c}");
    assert_eq!(env.run(&["count"])["folded"], json!([]));
}

#[test]
fn reconcile_settles_dead_and_quiet_segments() {
    let env = Env::new();
    let me = std::process::id();
    // A process that died mid-run (no dispose): the next start finalizes its segment.
    let gone = dead_pid();
    send(&env, gone, vec![prompt("ses_dead", 1_000, "Refactor the ledger module"), tool("ses_dead", "d1", "edit", "completed", json!({"filePath": "src/ledger.rs"}), "ok", None)]);
    assert!(env.root.join("opencode/open/oc:ses_dead.json").is_file());
    // A live owner that has been quiet for longer than the threshold, with no run going.
    let repo = env.repo.to_string_lossy();
    env.write_records(
        "oc:ses_quiet",
        &[
            json!({"seq":0,"t":1_000,"ev":"session_start","session_id":"oc:ses_quiet","cwd":repo,"repo":repo,"mode":"opencode"}),
            json!({"seq":1,"t":1_001,"ev":"input","text":"Rename the reconcile flag","idle":true,"prelabel":"none"}),
            json!({"seq":2,"t":1_002,"ev":"turn_end","stop_reason":"stop","text_head":"Renamed."}),
            json!({"seq":3,"t":1_003,"ev":"agent_end","turns":1,"is_terminal":true,"yielded":true}),
        ],
    );
    std::fs::write(env.root.join("opencode/open/oc:ses_quiet.json"), json!({"session": "ses_quiet", "pids": [me]}).to_string()).unwrap();
    // A live owner, recently active: left open.
    send(&env, me, vec![prompt("ses_live", 1_000, "Keep working on the ledger")]);

    let r = send(&env, 1, vec![json!({"kind": "reconcile"})]);
    let mut done: Vec<(String, String)> =
        r[0]["finalized"].as_array().unwrap().iter().map(|f| (f["session"].as_str().unwrap().into(), f["why"].as_str().unwrap().into())).collect();
    done.sort();
    assert_eq!(done, [("oc:ses_dead".to_string(), "owners gone".to_string()), ("oc:ses_quiet".to_string(), "quiet".to_string())]);
    let dead = trace(&env, "oc:ses_dead");
    assert_eq!(&evs(&dead)[dead.len() - 2..], ["agent_end", "session_shutdown"]);
    assert_eq!(dead[dead.len() - 2]["yielded"], false, "the run never finished");
    assert!(env.root.join("opencode/open/oc:ses_live.json").is_file());
    assert!(!evs(&trace(&env, "oc:ses_live")).contains(&"session_shutdown"));
    // The quiet session's next event opens its second segment.
    let r = send(&env, me, vec![prompt("ses_quiet", 50_000, "Back to the reconcile flag")]);
    assert_eq!(r[0]["started"], "oc:ses_quiet:r2");
}

#[test]
fn approved_hooks_block_through_hook_check() {
    let env = Env::new();
    let me = std::process::id();
    let check = |sid: &str, parent: Option<&str>, command: &str| {
        call(
            &env,
            "hook-check",
            &json!({"pid": me, "directory": env.repo, "session": sid, "parent": parent, "tool": "bash", "call_id": "k", "args": {"command": command}}),
        )
    };
    assert_eq!(check("ses_h", None, "cargo test")["block"], false, "nothing approved, nothing blocks");
    let ls = env.run(&["builtins", "ls"]);
    let stuck = ls["builtins"].as_array().unwrap().iter().find(|x| x["name"] == "stuck-detector").unwrap()["id"].as_str().unwrap().to_string();
    // The digest (the plugin's toast in the TUI) names the approvals and the user's commands.
    let d = &send(&env, me, vec![json!({"kind": "digest"})])[0]["digest"];
    let toast = d["toast"].as_str().unwrap();
    assert!(toast.contains(&format!("approve hook stuck-detector: /rematch-approve {stuck} · /rematch-decline {stuck}")), "{toast}");
    assert!(d["approvals"].as_array().unwrap().iter().any(|a| a["id"] == json!(stuck)), "{d}");
    env.run(&["library", "approve", &stuck]);
    let load = send(&env, me, vec![json!({"kind": "load"})]);
    assert_eq!(load[0]["hooks"], json!([{"id": stuck, "mode": "stuck", "tool": "*", "params": {"repeats": 3}}]), "{load:?}");

    send(&env, me, vec![prompt("ses_h", 1_000, "make the payments tests pass")]);
    for (i, c) in ["k1", "k2", "k3"].iter().enumerate() {
        assert_eq!(check("ses_h", None, "cargo test -p payments")["block"], false, "call {i}");
        send(&env, me, vec![tool("ses_h", c, "bash", "completed", json!({"command": "cargo test -p payments"}), "1 failed", Some(101))]);
    }
    let v = check("ses_h", None, "cargo test -p payments");
    assert_eq!((v["block"].as_bool(), v["hook_id"].as_str()), (Some(true), Some(stuck.as_str())), "{v}");
    let recs = trace(&env, "oc:ses_h");
    let b = recs.last().unwrap();
    assert_eq!((b["ev"].as_str(), b["hook_id"].as_str(), b["tool"].as_str()), (Some("tool_blocked"), Some(stuck.as_str()), Some("bash")));
    assert_eq!(b["args_hash"].as_str().unwrap().len(), 64);
    assert_eq!(recs[0]["hooks_loaded"], json!([stuck]), "an approved stuck hook loads");
    // Another call passes; a subagent (no trace) is never stuck-blocked.
    assert_eq!(check("ses_h", None, "cargo test -p ledger")["block"], false);
    assert_eq!(check("ses_child", Some("ses_h"), "cargo test -p payments")["block"], false);
    assert!(!env.trace_path("oc:ses_child").exists());

    // The kill switch: nothing is checked or written.
    env.run(&["off"]);
    assert_eq!(check("ses_h", None, "cargo test -p payments")["why"], "off");
    assert_eq!(send(&env, me, vec![prompt("ses_h", 9_000, "more")])[0]["off"], true);
}

/// OpenCode 1.19.4's tables, the columns the importer reads.
const SCHEMA: &str = "
    CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL DEFAULT 'global', parent_id TEXT, slug TEXT NOT NULL DEFAULT 's',
      directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
    CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
    CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);";

fn write_store(db: &Path, repo: &Path) {
    let c = rusqlite::Connection::open(db).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    let dir = repo.to_string_lossy();
    for (id, parent, t) in [("ses_hist", None, 1_000i64), ("ses_sub", Some("ses_hist"), 1_500)] {
        c.execute(
            "INSERT INTO session (id, parent_id, directory, title, version, time_created, time_updated) VALUES (?1, ?2, ?3, 't', '1.19.4', ?4, ?4)",
            rusqlite::params![id, parent, dir, t],
        )
        .unwrap();
    }
    let file = format!("{dir}/src/ledger.rs");
    let assistant = |finish: &str, t: i64| {
        json!({"role": "assistant", "agent": "build", "modelID": "claude-fable-5", "providerID": "anthropic", "finish": finish, "cost": 0.01,
               "tokens": {"input": 100, "output": 50, "reasoning": 0, "cache": {"read": 0, "write": 0}}, "time": {"created": t, "completed": t + 1}})
    };
    let messages: Vec<(&str, &str, i64, Value, Vec<Value>)> = vec![
        ("ses_hist", "msg_01", 1_001, json!({"role": "user", "agent": "build"}), vec![json!({"type": "text", "text": "Add the ledger reconciliation command to the payments crate"})]),
        (
            "ses_hist",
            "msg_02",
            1_002,
            assistant("tool-calls", 1_002),
            vec![
                json!({"type": "tool", "tool": "edit", "callID": "e1", "state": {"status": "completed", "input": {"filePath": file}, "output": "ok", "time": {"start": 1, "end": 1_003}}}),
                json!({"type": "tool", "tool": "bash", "callID": "e2", "state": {"status": "error", "input": {"command": "git commit -am x"}, "error": "rejected", "time": {"start": 1, "end": 1_004}}}),
            ],
        ),
        ("ses_hist", "msg_03", 1_005, assistant("stop", 1_005), vec![json!({"type": "text", "text": "Added the reconcile command and committed it."})]),
        ("ses_sub", "msg_s1", 1_501, json!({"role": "user", "agent": "general"}), vec![json!({"type": "text", "text": "No, that's wrong: review again"})]),
        (
            "ses_hist",
            "msg_04",
            2_000,
            json!({"role": "user", "agent": "build"}),
            vec![json!({"type": "text", "text": "No, that's wrong: run the tests before you commit; the reconcile tests fail"})],
        ),
        ("ses_hist", "msg_05", 2_001, assistant("stop", 2_001), vec![json!({"type": "text", "text": "Ran the tests; they fail in reconcile."})]),
    ];
    for (sid, mid, t, info, parts) in messages {
        c.execute("INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4)", rusqlite::params![mid, sid, t, info.to_string()]).unwrap();
        for (i, p) in parts.iter().enumerate() {
            c.execute("INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)", rusqlite::params![format!("prt_{mid}_{i}"), mid, sid, t, p.to_string()]).unwrap();
        }
    }
}

#[test]
fn opencode_history_imports_and_reflects() {
    let env = Env::new();
    let data = env.home.join(".local/share/opencode");
    std::fs::create_dir_all(&data).unwrap();
    let db = data.join("opencode.db");
    write_store(&db, &env.repo);
    let sid = "oc:ses_hist";

    // Default store = $XDG_DATA_HOME/opencode/opencode.db (here: $HOME/.local/share).
    let mut c = env.cmd(&["import", "--history", "--harness", "opencode"]);
    c.env_remove("XDG_DATA_HOME").env_remove("OPENCODE_DB");
    let (v, code) = env.run_cmd(c);
    assert_eq!(code, 0, "{v}");
    let h = &v["data"]["history_opencode"];
    assert_eq!((h["new"].clone(), h["failed"].clone()), (json!(2), json!(0)), "{v}");
    // Idempotent: a second import updates, never duplicates.
    let again = env.run(&["import", "--history", "--harness", "opencode", "--sessions", &data.to_string_lossy()]);
    assert_eq!((again["history_opencode"]["new"].clone(), again["history_opencode"]["updated"].clone()), (json!(0), json!(2)), "{again}");

    let w = env.run(&["library", "why", sid]);
    let s = &w["session"];
    assert_eq!((s["source"].as_str(), s["tool_errors"].as_u64(), s["turns"].as_u64()), (Some("import"), Some(1), Some(3)), "{w}");
    assert_eq!(s["model"], json!({"provider": "anthropic", "id": "claude-fable-5"}), "{w}");
    assert_eq!(w["redirects"][0]["prelabel"], "negation", "{w}");

    // The subagent's "No, that's wrong" was written by its parent agent: no input, no redirect.
    let sub = env.run(&["library", "why", "oc:ses_hist:agent-ses_sub"]);
    assert_eq!(sub["redirects"], json!([]), "{sub}");

    // reflect --history reads the session back from the store.
    let editor = script("editor-ok.sh");
    let r = env.run(&["reflect", "--history", "--editor", &editor]);
    let hist = &r["history"];
    assert_eq!(hist["handed"], json!([sid]), "{r}");
    let staged = hist["sessions"].as_array().unwrap().iter().find(|x| x["session"] == sid).cloned().unwrap_or(Value::Null);
    assert_eq!((staged["outcome"].as_str(), staged["name"].as_str()), (Some("staged"), Some("run-tests-before-commit")), "{r}");
    assert!(env.staged_dir().join("SKILL.md").is_file(), "{r}");
    let recs = trace(&env, sid);
    assert_eq!((recs[0]["mode"].as_str(), recs[0]["omp_version"].as_str()), (Some("import"), Some("opencode 1.19.4")));
    let input = recs.iter().find(|x| x["prelabel"] == "negation").unwrap();
    assert_eq!(input["after_deliverable"], "Added the reconcile command and committed it.");
}

#[test]
fn approved_retrieve_hook_adds_system_context_with_attribution() {
    let env = Env::new();
    let me = std::process::id();
    let d = env.run(&["create", "ledger-test-gate", "--from", "description", "--description", "Run the ledger tests before committing payment changes", "--editor", &script("editor-ok.sh")]);
    let pkg = d["id"].as_str().unwrap().to_string();
    env.run(&["activate", "ledger-test-gate", "--unverified"]);
    let unit = env.root.join("staging/payments/ledger-test-gate/units/reconcile.md");
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, "# Ledger reconciliation\n\nRun the reconciliation migration before the ledger tests.\n").unwrap();
    let ls = env.run(&["builtins", "ls"]);
    let hook = ls["builtins"].as_array().unwrap().iter().find(|x| x["name"] == "context-unit-retrieval").unwrap()["id"].as_str().unwrap().to_string();

    // Not approved: no context, nothing recorded.
    send(&env, me, vec![prompt("ses_r", 1_000, "Commit the ledger reconciliation changes after the tests")]);
    assert!(send(&env, me, vec![json!({"kind": "retrieve", "session": "ses_r"})])[0].get("context").is_none());
    send(&env, me, vec![json!({"kind": "shutdown"})]);

    env.run(&["library", "approve", &hook]);
    let load = send(&env, me, vec![json!({"kind": "load"})]);
    assert!(load[0]["hooks"].as_array().unwrap().iter().any(|h| h["mode"] == "retrieve"), "{load:?}");
    // Asked before the session's prompt reached the trace: no input yet, so nothing is decided
    // (the plugin caches nothing and asks again on the next request).
    let early = send(&env, me, vec![json!({"kind": "retrieve", "session": "ses_r2"})]);
    assert_eq!((early[0]["has_input"].as_bool(), early[0].get("context")), (Some(false), None), "{early:?}");
    // OpenCode never got the shared dir (no `skills.paths`): its packages' units are not added.
    let undelivered = json!({"pid": me, "directory": env.repo, "version": "1.19.4", "skills_paths": [],
        "events": [prompt("ses_r3", 3_000, "Commit the ledger reconciliation changes after the tests"), {"kind": "retrieve", "session": "ses_r3"}]});
    let u = call(&env, "trace-event", &undelivered);
    assert_eq!((u["events"][1]["has_input"].as_bool(), u["events"][1].get("context")), (Some(true), None), "{u}");
    assert!(!trace(&env, "oc:ses_r3").iter().any(|x| x["ev"] == "retrieval"));
    send(&env, me, vec![prompt("ses_r2", 2_000, "Commit the ledger reconciliation changes after the tests")]);
    let r = send(&env, me, vec![json!({"kind": "retrieve", "session": "ses_r2"})]);
    let ctx = r[0]["context"].as_str().unwrap();
    assert!(ctx.starts_with("[rematch] Units of skills loaded in this session") && ctx.contains("Run the reconciliation migration"), "{ctx}");
    // The same units again (the next model request): no second record.
    send(&env, me, vec![json!({"kind": "retrieve", "session": "ses_r2"})]);
    let recs = trace(&env, "oc:ses_r2");
    let rs: Vec<&Value> = recs.iter().filter(|x| x["ev"] == "retrieval").collect();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0]["entry_ids"], json!([hook]));
    assert_eq!((rs[0]["units"][0]["entry_id"].as_str(), rs[0]["units"][0]["path"].as_str()), (Some(pkg.as_str()), Some("units/reconcile.md")));
    assert!(recs[0]["hooks_loaded"].as_array().unwrap().contains(&json!(hook)));
    assert!(!recs[0]["loaded_entries"].as_array().unwrap().iter().any(|e| e["id"] == json!(hook)), "credited through retrieval");
    assert_eq!(recs.iter().filter(|x| x["ev"] == "input").count(), 1, "units are not an input");
}
