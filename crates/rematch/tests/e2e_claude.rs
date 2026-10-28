// POSIX-only: sh fixtures, Unix file modes and unprivileged symlinks.
#![cfg(unix)]
//! End-to-end through the Claude Code adapter's own hook relay (`adapters/claude-code/scripts/
//! hook.sh`): synthetic Claude Code hook payloads → `traces/cc:<uuid>.jsonl` + the pending
//! flag → `reflect --pending` stages an entry → `skills sync --harness claude-code` links it
//! into the shared `REMATCH_ROOT/skills/`, which the plugin's `skills/` symlink exposes. Also
//! the approved `stuck` and `retrieve` hooks, the kill switch, and the never-fail rule.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::*;
use serde_json::{json, Value};

const UUID: &str = "5d0c8a8e-1111-4c4c-8888-0123456789ab";

fn hook_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../adapters/claude-code/scripts/hook.sh")
}

/// A plugin root outside the repo (the relay links `<root>/skills` on SessionStart).
fn plugin_root(env: &Env) -> PathBuf {
    let p = env.home.join("plugin");
    std::fs::create_dir_all(p.join(".claude-plugin")).unwrap();
    p
}

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim()).unwrap_or_else(|e| panic!("hook stdout is not JSON ({e}): {:?}", self.stdout))
    }
}

fn run_hook(env: &Env, op: &str, stdin: &str, extra_env: &[(&str, &str)]) -> Out {
    let mut c = Command::new("sh");
    c.arg(hook_script())
        .arg(op)
        .env("REMATCH_ROOT", &env.root)
        .env("PI_CODING_AGENT_DIR", &env.agent)
        .env("HOME", &env.home)
        .env("EDITOR_LOG", &env.log)
        .env("REMATCH_MACHINE", "test-host")
        .env("REMATCH_BIN", BIN)
        .env("CLAUDE_PLUGIN_ROOT", plugin_root(env))
        // No omp: SessionStart starts no reflection (`reflect_runs_detached_at_session_start`).
        .env("OMP_BIN", env.home.join("no-omp"))
        .env_remove("REMATCH")
        .env_remove("CLAUDE_CONFIG_DIR")
        .current_dir(&env.repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        c.env(k, v);
    }
    let mut child = c.spawn().unwrap();
    {
        use std::io::Write;
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    }
    let o = child.wait_with_output().unwrap();
    let out = Out {
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        code: o.status.code().unwrap_or(-1),
    };
    eprintln!("hook {op} → {} {:?} {:?}", out.code, out.stdout.trim(), out.stderr.trim());
    out
}

/// One hook event of the session: the common fields plus `fields`.
fn event(env: &Env, name: &str, fields: Value) -> Out {
    let mut p = json!({
        "session_id": UUID,
        "transcript_path": transcript(env),
        "cwd": env.repo,
        "hook_event_name": name,
        "permission_mode": "default",
    });
    p.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
    let op = match name {
        "PreToolUse" => "hook-check",
        "SessionStart" => "session-start",
        "SessionEnd" => "session-end",
        _ => "trace-event",
    };
    let out = run_hook(env, op, &p.to_string(), &[]);
    assert!(out.code == 0 || (op == "hook-check" && out.code == 2), "hook exit {}", out.code);
    out
}

fn transcript(env: &Env) -> PathBuf {
    env.home.join(".claude/projects/-repo").join(format!("{UUID}.jsonl"))
}

/// Appends assistant entries (one API response each) to the transcript, `secs` after now.
fn assistant(env: &Env, secs: i64, id: &str, text: &str, stop: &str) {
    let ts = (chrono::Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let e = json!({"type":"assistant","timestamp":ts,"sessionId":UUID,"cwd":env.repo,"isSidechain":false,"version":"2.1.285",
        "message":{"model":"claude-opus-5-5","id":id,"role":"assistant","content":[{"type":"text","text":text}],"stop_reason":stop,
                   "usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}});
    let p = transcript(env);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut body = std::fs::read_to_string(&p).unwrap_or_default();
    body.push_str(&format!("{e}\n"));
    std::fs::write(&p, body).unwrap();
}

fn trace(env: &Env) -> Vec<Value> {
    trace_of(env, &format!("cc:{UUID}"))
}

fn trace_of(env: &Env, sid: &str) -> Vec<Value> {
    let p = env.root.join("traces").join(format!("{sid}.jsonl"));
    std::fs::read_to_string(p).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn sha2_hex(b: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(b))
}

fn evs(recs: &[Value]) -> Vec<String> {
    recs.iter().map(|r| r["ev"].as_str().unwrap().to_string()).collect()
}

fn bash_failure(env: &Env, call: &str) -> Out {
    event(
        env,
        "PostToolUseFailure",
        json!({"tool_name":"Bash","tool_input":{"command":"cargo test -p payments","description":"Run tests"},"tool_use_id":call,
               "error":"Exit code 101\ntest result: FAILED. 3 passed; 1 failed","is_interrupt":false,"duration_ms":900}),
    )
}

#[test]
fn hooks_trace_flag_reflect_and_link() {
    let env = Env::new();
    let repo = env.repo.to_string_lossy().into_owned();
    let file = format!("{repo}/src/ledger.rs");

    // An older version's layout: `claude-skills/` with the plugin's skills/ pointing there.
    let legacy = env.root.join("claude-skills");
    std::fs::create_dir_all(&legacy).unwrap();
    std::os::unix::fs::symlink(env.home.join("gone"), legacy.join("old")).unwrap();
    let link = plugin_root(&env).join("skills");
    std::os::unix::fs::symlink(&legacy, &link).unwrap();

    // SessionStart: the relay re-points the plugin's skills/ to the shared dir, removes the
    // legacy dir and asks for a re-scan; the digest names what was staged and what was applied
    // automatically (the four built-in data-only hooks), each with its undo command.
    let o = event(&env, "SessionStart", json!({"source":"startup","model":"claude-opus-5-5"}));
    let reply = o.json();
    assert_eq!(reply["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert_eq!(reply["hookSpecificOutput"]["reloadSkills"], true);
    let stuck = env.run(&["builtins", "ls"])["builtins"].as_array().unwrap().iter().find(|x| x["name"] == "stuck-detector").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let shown = reply["systemMessage"].as_str().unwrap();
    assert!(shown.starts_with("rematch digest\n- staged hook context-unit-retrieval in shared"), "{shown}");
    // Undone by name; the hook's module path (its id) is not in the line.
    assert!(shown.contains("applied automatically: hook stuck-detector (stuck) (data-only) — undo: `rematch library disable stuck-detector`"), "{shown}");
    assert!(!shown.contains(&stuck), "{shown}");
    assert!(!shown.contains("action(s) waiting for you"), "{shown}");
    let context = reply["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(context.contains(shown) && context.contains("never run `rematch library approve`"), "{context}");
    assert!(!context.contains("<approval id="), "a proposal's text never enters Claude's context: {context}");
    assert_eq!(std::fs::read_link(&link).unwrap(), env.root.join("skills"));
    assert!(!legacy.exists());

    let o = event(&env, "UserPromptSubmit", json!({"prompt":"Add the ledger reconciliation command to the payments crate"}));
    assert!(o.stdout.trim().is_empty(), "no loaded package units: {:?}", o.stdout);
    let o = event(&env, "PreToolUse", json!({"tool_name":"Read","tool_input":{"file_path":file},"tool_use_id":"toolu_1"}));
    assert_eq!((o.code, o.stdout.trim()), (0, ""), "read passes");
    event(
        &env,
        "PostToolUse",
        json!({"tool_name":"Read","tool_input":{"file_path":file},"tool_use_id":"toolu_1",
               "tool_response":{"type":"text","file":{"filePath":file,"content":"pub fn reconcile() {}"}}}),
    );
    event(
        &env,
        "PostToolUse",
        json!({"tool_name":"Edit","tool_input":{"file_path":file,"old_string":"{}","new_string":"{ todo!() }"},"tool_use_id":"toolu_2",
               "tool_response":{"filePath":file,"structuredPatch":[]}}),
    );
    for call in ["toolu_3", "toolu_4", "toolu_5"] {
        bash_failure(&env, call);
    }
    assistant(&env, 2, "msg_1", "Running the tests.", "tool_use");
    assistant(&env, 3, "msg_2", "Added the reconcile command and committed it.", "end_turn");
    event(&env, "Stop", json!({"stop_hook_active":false,"last_assistant_message":"Added the reconcile command and committed it."}));

    event(&env, "UserPromptSubmit", json!({"prompt":"No, that's wrong — run the tests before you commit; the reconcile tests fail"}));
    event(
        &env,
        "PostToolUse",
        json!({"tool_name":"Bash","tool_input":{"command":"cargo test -p payments --lib"},"tool_use_id":"toolu_6",
               "tool_response":{"stdout":"test result: ok. 4 passed; 0 failed","stderr":"","interrupted":false}}),
    );
    assistant(&env, 6, "msg_3", "Tests pass now; committed after running them.", "end_turn");
    event(&env, "Stop", json!({"stop_hook_active":false,"last_assistant_message":"Tests pass now; committed after running them."}));
    std::fs::write(&file, "pub fn reconcile() { todo!() }").unwrap();
    let o = event(&env, "SessionEnd", json!({"reason":"prompt_input_exit"}));
    assert!(o.stdout.trim().is_empty());

    // The trace: DESIGN §3 records, seq = index.
    let recs = trace(&env);
    assert_eq!(
        evs(&recs),
        vec![
            "session_start", "input", "hook_evaluated", "tool_result", "tool_result", "tool_result", "tool_result", "tool_result", "turn_end", "turn_end",
            "agent_end", "input", "tool_result", "turn_end", "agent_end", "session_shutdown",
        ],
        "{recs:#?}"
    );
    for (i, r) in recs.iter().enumerate() {
        assert_eq!(r["seq"], json!(i));
    }
    let s0 = &recs[0];
    assert_eq!(s0["session_id"], json!(format!("cc:{UUID}")));
    assert_eq!(s0["cwd"], json!(repo));
    assert_eq!(s0["repo"], json!(repo));
    assert_eq!(s0["model"], json!({"provider":"anthropic","id":"claude-opus-5-5"}));
    assert_eq!(s0["mode"], "claude-code:startup");
    assert_eq!(s0["loaded_entries"].as_array().unwrap().len(), 2, "stuck-detector and fast-check built-ins load at start: {s0}");
    assert!(s0["repo_remote"].as_str().unwrap().starts_with("local:"), "{s0}");
    assert_eq!(s0["subpath"], "");
    assert_eq!((recs[1]["prelabel"].as_str(), recs[1]["idle"].as_bool()), (Some("none"), Some(true)));
    assert_eq!(recs[2]["ev"], "hook_evaluated");
    assert_eq!((recs[3]["tool"].as_str(), recs[3]["path"].as_str()), (Some("read"), Some(file.as_str())));
    assert_eq!(recs[3]["content_head"], "pub fn reconcile() {}");
    assert_eq!(recs[4]["tool"], "edit");
    assert_eq!((recs[5]["tool"].as_str(), recs[5]["is_error"].as_bool()), (Some("bash"), Some(true)));
    assert!(recs[5]["content_head"].as_str().unwrap().starts_with("Exit code 101"));
    assert_eq!(recs[9]["text_head"], "Added the reconcile command and committed it.");
    assert_eq!(recs[9]["model"], json!({"provider":"anthropic","id":"claude-opus-5-5"}));
    assert_eq!(recs[9]["usage"]["input"], 100);
    assert_eq!((recs[10]["turns"].as_u64(), recs[10]["yielded"].as_bool()), (Some(2), Some(true)));
    assert_eq!((recs[11]["prelabel"].as_str(), recs[11]["idle"].as_bool()), (Some("negation"), Some(true)));
    assert_eq!(recs[11]["after_deliverable"], "Added the reconcile command and committed it.");
    assert_eq!(recs[13]["text_head"], "Tests pass now; committed after running them.");
    let shutdown = recs.last().unwrap();
    assert_eq!(shutdown["flagged"], true);
    assert_eq!(shutdown["reasons"], json!(["tool_error_cluster", "redirect:negation"]));
    let edited = sha2_hex(b"pub fn reconcile() { todo!() }");
    assert_eq!(shutdown["files_changed"], json!([["src/ledger.rs", edited]]), "the Edit's file, repo-relative");

    // The pending flag.
    let sid = format!("cc:{UUID}");
    let pending: Value =
        serde_json::from_str(&std::fs::read_to_string(env.root.join("pending").join(format!("{sid}.json"))).unwrap()).unwrap();
    assert_eq!(pending["session_id"], json!(sid));
    assert_eq!(pending["reasons"], json!(["tool_error_cluster", "redirect:negation"]));
    assert_eq!(pending["trace"], json!(env.root.join("traces").join(format!("{sid}.jsonl"))));

    // The ordinary loop takes it from here: count + reflect stage an entry.
    let d = env.run(&["reflect", "--pending", "--editor", &script("editor-ok.sh")]);
    let r = &d["reflections"][0];
    assert_eq!((r["session"].as_str(), r["outcome"].as_str()), (Some(sid.as_str()), Some("staged")), "{d}");
    let id = r["id"].as_str().unwrap().to_string();
    let row = env.run(&["library", "why", &sid]);
    assert!(row["lines"][0].as_str().unwrap().contains("claude-opus-5-5"), "{row}");

    // skills sync links the staged skill in this repo's view only; the plugin's skills/ (the
    // kept library every Claude Code session reads) does not list it: a SessionStart in this
    // repo names it in context instead, with its SKILL.md path.
    let m = env.run(&["skills", "sync", "--harness", "claude-code", "--cwd", &repo]);
    let staged = env.staged_dir();
    assert_eq!((m["added"].clone(), m["links"].clone()), (json!(["run-tests-before-commit"]), json!({"run-tests-before-commit": staged})), "{m}");
    assert!(std::fs::symlink_metadata(env.root.join("skills/run-tests-before-commit")).is_err(), "not in the kept library");
    assert!(!link.join("run-tests-before-commit").exists(), "not through the plugin's skills/");
    let again = env.run(&["skills", "sync", "--harness", "claude-code", "--cwd", &repo]);
    assert_eq!((again["added"].clone(), again["pruned"].clone()), (json!([]), json!([])), "idempotent");
    let start = event(&env, "SessionStart", json!({"source":"compact"})).json();
    let ctx = start["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default();
    assert!(ctx.contains("Skills on trial in this repo") && ctx.contains("- run-tests-before-commit: ") && ctx.contains("SKILL.md)"), "{start}");

    // The next session in the repo loads it (and needs no re-scan: the links did not change);
    // its digest names the new skill, once.
    let next = "6e1d9b9f-2222-4d4d-9999-0123456789ab";
    let p = json!({"session_id":next,"transcript_path":"","cwd":repo,"hook_event_name":"SessionStart","source":"startup"});
    let o = run_hook(&env, "session-start", &p.to_string(), &[]);
    assert_eq!(o.code, 0);
    let reply = o.json();
    assert!(reply["hookSpecificOutput"].get("reloadSkills").is_none(), "{reply}");
    let shown = reply["systemMessage"].as_str().unwrap();
    assert!(shown.contains("- staged skill run-tests-before-commit in payments") && !shown.contains("staged hook"), "{shown}");
    let s = trace_of(&env, &format!("cc:{next}"));
    let mirror: Value = serde_json::from_str(&std::fs::read_to_string(staged.join("rematch.json")).unwrap()).unwrap();
    let sha = mirror["content_sha256"].clone();
    assert!(sha.is_string(), "content_sha256 set at stage: {sha}");
    assert!(s[0]["loaded_entries"].as_array().unwrap().contains(&json!({"id": id, "version": 1, "sha": sha})), "versioned loaded_entries: {:?}", s[0]["loaded_entries"]);
    assert_eq!(s[0]["visible_entries"], json!([id]), "listed in this session's context");
    assert_eq!(s[0]["staging_dirs"], json!([env.root.join("skills")]), "the plugin mounts the kept library");
    assert!(s[0]["exposure_view"].as_str().unwrap().starts_with("payments-"), "{:?}", s[0]);

    // Elsewhere nothing matches: no trial in its context, nothing counted; the repo's view keeps it.
    let view = std::path::PathBuf::from(m["dirs"][0].as_str().unwrap());
    let other = env.home.join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let elsewhere = "7f2e0c0a-3333-4e4e-aaaa-0123456789ab";
    let p = json!({"session_id":elsewhere,"cwd":other,"hook_event_name":"SessionStart","source":"startup"});
    let o = run_hook(&env, "session-start", &p.to_string(), &[]);
    assert_eq!(o.code, 0);
    assert!(o.json()["hookSpecificOutput"].get("reloadSkills").is_none());
    assert!(!o.json()["systemMessage"].as_str().unwrap().contains("staged"), "nothing new since the last digest, approvals still wait");
    assert!(!o.json()["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default().contains("Skills on trial"), "{}", o.json());
    assert_eq!(std::fs::read_link(view.join("run-tests-before-commit")).unwrap(), staged);
    assert!(!trace_of(&env, &format!("cc:{elsewhere}"))[0]["loaded_entries"].as_array().unwrap().iter().any(|e| e["id"] == id.as_str()));
    // Retired: the link is pruned at once.
    env.run(&["library", "disable", &id]);
    assert!(std::fs::symlink_metadata(view.join("run-tests-before-commit")).is_err());
}

#[test]
fn approved_stuck_hook_blocks_the_repeat() {
    let env = Env::new();
    let ls = env.run(&["builtins", "ls"]);
    let stuck = ls["builtins"].as_array().unwrap().iter().find(|x| x["name"] == "stuck-detector").unwrap()["id"].as_str().unwrap().to_string();

    event(&env, "SessionStart", json!({"source":"startup"}));
    let s0 = &trace(&env)[0];
    assert!(s0["hooks_loaded"].as_array().unwrap().contains(&json!(stuck)), "{s0}");
    assert!(s0["loaded_entries"].as_array().unwrap().iter().any(|e| e["id"] == json!(stuck)));
    event(&env, "UserPromptSubmit", json!({"prompt":"make the payments tests pass"}));
    // Claude Code sends PreToolUse and PostToolUse(Failure) the same tool_input.
    let pre = json!({"tool_name":"Bash","tool_input":{"command":"cargo test -p payments","description":"Run tests"},"tool_use_id":"toolu_x"});
    for call in ["toolu_1", "toolu_2"] {
        assert_eq!(event(&env, "PreToolUse", pre.clone()).code, 0);
        bash_failure(&env, call);
    }
    assert_eq!(event(&env, "PreToolUse", pre.clone()).code, 0, "two identical calls ran: not yet stuck");
    bash_failure(&env, "toolu_3");

    // The fourth identical call: 3 identical calls ran right before it.
    let o = event(&env, "PreToolUse", pre.clone());
    assert_eq!(o.code, 2, "blocked");
    assert!(o.stdout.trim().is_empty());
    assert!(!o.stderr.trim().is_empty(), "the hook's message is the reason");
    let recs = trace(&env);
    let b = recs.last().unwrap();
    assert_eq!((b["ev"].as_str(), b["hook_id"].as_str(), b["tool"].as_str()), (Some("tool_blocked"), Some(stuck.as_str()), Some("bash")));
    assert_eq!(b["call_id"], "toolu_x");
    assert_eq!(b["args_hash"].as_str().unwrap().len(), 64);
    // A stuck rejection stays in the run: the same call is refused again.
    assert_eq!(event(&env, "PreToolUse", pre.clone()).code, 2);
    // Another command passes; a subagent (no trace of its own) is never stuck-blocked.
    let other = json!({"tool_name":"Bash","tool_input":{"command":"cargo test -p payments -- --nocapture"},"tool_use_id":"toolu_y"});
    assert_eq!(event(&env, "PreToolUse", other).code, 0);
    let mut sub = pre.clone();
    sub["agent_id"] = json!("a12");
    sub["agent_type"] = json!("Explore");
    assert_eq!(event(&env, "PreToolUse", sub).code, 0);
}

#[test]
fn approved_retrieve_hook_adds_context_once() {
    let env = Env::new();
    let d = env.run(&[
        "create",
        "ledger-test-gate",
        "--from",
        "description",
        "--description",
        "Run the ledger tests before committing payment changes",
        "--editor",
        &script("editor-ok.sh"),
    ]);
    let pkg = d["id"].as_str().unwrap().to_string();
    env.run(&["activate", "ledger-test-gate", "--unverified"]);
    let unit = env.root.join("staging/payments/ledger-test-gate/units/reconcile.md");
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, "# Ledger reconciliation\n\nRun the reconciliation migration before the ledger tests.\n").unwrap();
    let ls = env.run(&["builtins", "ls"]);
    let hook =
        ls["builtins"].as_array().unwrap().iter().find(|x| x["name"] == "context-unit-retrieval").unwrap()["id"].as_str().unwrap().to_string();
    event(&env, "SessionStart", json!({"source":"startup"}));
    let s0 = &trace(&env)[0];
    assert!(!s0["loaded_entries"].as_array().unwrap().iter().any(|e| e["id"] == json!(hook)), "a retrieve hook credits through retrieval records");
    // A request that shares stems with the unit but not with the package's trigger line
    // (committing changes to a Rust crate) gets nothing: units are gated by the trigger.
    let off = event(&env, "UserPromptSubmit", json!({"prompt":"fix the ledger reconciliation migration in src/ledger.rs"}));
    assert!(off.stdout.trim().is_empty(), "units injected without a trigger hit: {:?}", off.stdout);
    let prompt = json!({"prompt":"commit the ledger reconciliation migration change to the crate"});
    let o = event(&env, "UserPromptSubmit", prompt.clone());
    let ctx = o.json()["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string();
    assert!(ctx.contains("<unit skill=\"ledger-test-gate\" path=\"units/reconcile.md\">"), "{ctx}");
    assert!(ctx.contains("Run the reconciliation migration before the ledger tests."));
    let r = trace(&env).into_iter().find(|r| r["ev"] == "retrieval").expect("retrieval record");
    assert_eq!(r["entry_ids"], json!([hook]));
    assert_eq!(r["units"][0]["entry_id"], json!(pkg));
    assert_eq!(r["units"][0]["path"], "units/reconcile.md");

    // The same units again: already in Claude Code's conversation, not injected twice.
    event(&env, "Stop", json!({"last_assistant_message":"Fixed."}));
    let o = event(&env, "UserPromptSubmit", prompt);
    assert!(o.stdout.trim().is_empty(), "{:?}", o.stdout);
    assert_eq!(trace(&env).iter().filter(|r| r["ev"] == "retrieval").count(), 1);
}

#[test]
fn never_fails_a_hook_and_honors_the_kill_switch() {
    let env = Env::new();
    // Not JSON, no session id, unknown event, missing binary: exit 0, nothing on stdout.
    for stdin in ["not json", "{}", r#"{"session_id":"x","hook_event_name":"Notification"}"#] {
        let o = run_hook(&env, "trace-event", stdin, &[]);
        assert_eq!((o.code, o.stdout.trim()), (0, ""), "{stdin}");
        let o = run_hook(&env, "hook-check", stdin, &[]);
        assert_eq!((o.code, o.stdout.trim()), (0, ""), "{stdin}");
    }
    let missing = env.home.join("no-such-rematch");
    let o = run_hook(&env, "hook-check", "{}", &[("REMATCH_BIN", missing.to_str().unwrap())]);
    assert_eq!((o.code, o.stdout.trim()), (0, ""));
    // A path-like session id never becomes a file name.
    let o = run_hook(&env, "trace-event", &json!({"session_id":"../../x","hook_event_name":"SessionStart","cwd":env.repo}).to_string(), &[]);
    assert_eq!(o.code, 0);
    assert!(!env.root.join("x.jsonl").exists());

    env.run(&["off"]);
    event(&env, "SessionStart", json!({"source":"startup"}));
    event(&env, "UserPromptSubmit", json!({"prompt":"hello"}));
    assert!(trace(&env).is_empty(), "off: nothing is traced");
}

/// Appends a typed user entry to the transcript, stamped now.
fn user_entry(env: &Env, text: &str) {
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let e = json!({"type":"user","timestamp":ts,"sessionId":UUID,"cwd":env.repo,"isSidechain":false,"version":"2.1.285",
        "message":{"role":"user","content":[{"type":"text","text":text}]}});
    let p = transcript(env);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut body = std::fs::read_to_string(&p).unwrap_or_default();
    body.push_str(&format!("{e}\n"));
    std::fs::write(&p, body).unwrap();
}

#[test]
fn takeover_is_a_prompt_while_the_run_is_open() {
    let env = Env::new();
    let pause = || std::thread::sleep(std::time::Duration::from_millis(5));
    event(&env, "SessionStart", json!({"source":"startup"}));
    event(&env, "UserPromptSubmit", json!({"prompt":"Add the ledger reconciliation command to the payments crate"}));
    // Esc fires no hook, but the transcript records the abort: the agent loop had ended, so the
    // next prompt is no takeover (omp: `agent_end` fired before it).
    pause();
    user_entry(&env, "[Request interrupted by user]");
    pause();
    event(&env, "UserPromptSubmit", json!({"prompt":"Put the command in the cli module of the payments crate instead"}));
    // No Stop and no abort since that input: the prompt reached a running loop (queued while
    // Claude worked) — a takeover.
    event(&env, "UserPromptSubmit", json!({"prompt":"Also print a summary table when the reconciliation finishes"}));
    let recs = trace(&env);
    assert_eq!(evs(&recs), vec!["session_start", "input", "agent_end", "input", "agent_end", "input"], "{recs:#?}");
    assert_eq!(recs[2]["yielded"], false);
    assert_eq!((recs[3]["prelabel"].as_str(), recs[3]["idle"].as_bool()), (Some("none"), Some(true)));
    assert_eq!((recs[5]["prelabel"].as_str(), recs[5]["idle"].as_bool()), (Some("interrupt"), Some(false)));
}

#[test]
fn reflect_runs_detached_at_session_start() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let exe = |p: &Path, body: &str| {
        std::fs::write(p, body).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    let omp = env.home.join("omp");
    exe(&omp, "#!/bin/sh\nexit 0\n");
    // The binary, except that `reflect` records its arguments after 3 s.
    let seen = env.home.join("reflect.args");
    let bin = env.home.join("rematch-stub");
    exe(&bin, &format!("#!/bin/sh\nif [ \"$1\" = reflect ]; then sleep 3; echo \"$@\" > '{}'; exit 0; fi\nexec '{BIN}' \"$@\"\n", seen.display()));
    let p = json!({"session_id":UUID,"transcript_path":"","cwd":env.repo,"hook_event_name":"SessionStart","source":"startup"}).to_string();
    let t0 = std::time::Instant::now();
    let o = run_hook(&env, "session-start", &p, &[("OMP_BIN", omp.to_str().unwrap()), ("REMATCH_BIN", bin.to_str().unwrap())]);
    assert!(t0.elapsed() < std::time::Duration::from_millis(2500), "the hook does not wait for the reflection: {:?}", t0.elapsed());
    assert_eq!(o.code, 0);
    assert_eq!(evs(&trace(&env)), vec!["session_start"]);
    assert!(!seen.exists());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !seen.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(std::fs::read_to_string(&seen).unwrap().trim(), "reflect --pending", "outlived the hook");

    // Without omp nothing is started: a reflection's model calls would only fail.
    std::fs::remove_file(&seen).unwrap();
    exe(&bin, &format!("#!/bin/sh\nif [ \"$1\" = reflect ]; then echo \"$@\" > '{}'; exit 0; fi\nexec '{BIN}' \"$@\"\n", seen.display()));
    let o = run_hook(&env, "session-start", &p, &[("REMATCH_BIN", bin.to_str().unwrap())]);
    assert_eq!(o.code, 0);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(!seen.exists());
}

#[test]
fn provisional_agents_reach_their_repo_not_the_plugin() {
    let env = Env::new();
    let d = env.run(&[
        "create",
        "log-triage",
        "--from",
        "description",
        "--description",
        "triage long build logs for the failing step",
        "--editor",
        &script("editor-agent.sh"),
    ]);
    let id = d["id"].as_str().unwrap().to_string();
    env.run(&["activate", "log-triage", "--unverified"]);
    let agents = plugin_root(&env).join("agents");
    let copy = agents.join("log-triage.md");
    let sync = || {
        let mut c = env.cmd(&["skills", "sync", "--harness", "claude-code"]);
        c.env("CLAUDE_PLUGIN_ROOT", plugin_root(&env));
        let (v, code) = env.run_cmd(c);
        assert_eq!(code, 0, "{v}");
        v
    };
    // `editor-agent.sh` is a read-only agent (`tools: read, grep`), so it is auto-approved on
    // stage and never copied into the plugin's agents/ (every session on the machine reads it):
    // into this repo's ignored `.claude/agents/`.
    let local = env.repo.join(".claude/agents/log-triage.md");
    assert!(std::fs::symlink_metadata(&copy).is_err() && !local.exists(), "before SessionStart/sync: not copied to repo yet");

    // The first session copies it as a real file in Claude Code's agent format (omp's `@smol`
    // role and tool names do not carry over as they are). Claude Code read its agents before
    // the hook ran, so this session does not count it.
    event(&env, "SessionStart", json!({"source":"startup"}));
    assert!(std::fs::symlink_metadata(&copy).is_err(), "approved: still not in the plugin");
    assert!(std::fs::symlink_metadata(&local).unwrap().file_type().is_file());
    let exclude = std::fs::read_to_string(env.repo.join(".git/info/exclude")).unwrap_or_default();
    assert!(exclude.contains("/.claude/agents/log-triage.md"), "ignored locally, never .gitignore: {exclude}");
    let text = std::fs::read_to_string(&local).unwrap();
    assert!(text.starts_with("---\nname: log-triage\ndescription: Use for a long build or test log"), "{text}");
    assert!(text.contains("\ntools: Read, Grep\n") && !text.contains("model:") && !text.contains("rematch:"), "{text}");
    assert!(text.contains("reply with that step's name and its first error line"), "{text}");
    assert!(!trace(&env)[0]["loaded_entries"].as_array().unwrap().iter().any(|e| e["id"] == json!(id)));

    // The next session started with the copy in place: it is loaded, versioned like a skill.
    let next = "6e1d9b9f-2222-4d4d-9999-0123456789ab";
    let p = json!({"session_id":next,"transcript_path":"","cwd":env.repo,"hook_event_name":"SessionStart","source":"startup"});
    assert_eq!(run_hook(&env, "session-start", &p.to_string(), &[]).code, 0);
    let s = trace_of(&env, &format!("cc:{next}"));
    let loaded = s[0]["loaded_entries"].as_array().unwrap();
    assert!(loaded.iter().any(|e| e["id"] == json!(id) && e["version"] == 1 && e["sha"].is_string()), "{loaded:?}");

    // Another repo's session never sees it.
    let other = env.home.join("other-repo");
    std::fs::create_dir_all(other.join(".git")).unwrap();
    let mut c = env.cmd(&["skills", "sync", "--harness", "claude-code", "--cwd", other.to_str().unwrap()]);
    c.env("CLAUDE_PLUGIN_ROOT", plugin_root(&env));
    assert_eq!(env.run_cmd(c).1, 0);
    assert!(!other.join(".claude/agents/log-triage.md").exists());

    // An authored agent file of the same name is never overwritten; disabling removes ours.
    std::fs::write(agents.join("mine.md"), "---\nname: mine\ndescription: the user's\n---\nbody\n").unwrap_or(());
    env.run(&["library", "disable", &id]);
    sync();
    assert!(!local.exists(), "disabled: the repo-local copy goes");
}

#[test]
fn approved_fragments_reach_session_start_and_survive_compaction() {
    let env = Env::new();
    let d = env.run(&[
        "create",
        "crate-tests-first",
        "--from",
        "description",
        "--description",
        "run crate tests before committing",
        "--editor",
        &script("editor-fragment.sh"),
    ]);
    let id = d["id"].as_str().unwrap().to_string();
    env.run(&["activate", "crate-tests-first", "--unverified"]);
    let heading = "[rematch] Personal instructions (approved):";
    let context = |o: &Out| -> String {
        let v: Value = serde_json::from_str(o.stdout.trim()).unwrap_or(Value::Null);
        v["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default().to_string()
    };

    // Not approved: the digest offers it; no personal instructions.
    let o = event(&env, "SessionStart", json!({"source":"startup"}));
    assert!(!context(&o).contains(heading), "{}", context(&o));
    // The personal text is its own action, approved for the exact bytes shown.
    let g = env.run(&["digest", "--cwd", env.repo.to_str().unwrap()]);
    assert!(g["approvals"].as_array().unwrap().iter().any(|a| a["id"] == format!("instructions:{id}")), "offered: {g}");
    env.approve(&format!("instructions:{id}"));
    let fragment = std::fs::read_to_string(env.root.join("staging/payments/crate-tests-first/AGENTS.fragment.md")).unwrap();
    let want = format!("{heading}\n\n## rematch: crate-tests-first\n\n{}", fragment.trim());

    // A compaction drops the earlier context: the instructions come again, without a digest.
    let reply = event(&env, "SessionStart", json!({"source":"compact"})).json();
    let got = reply["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default().to_string();
    assert!(got.starts_with(&want), "personal instructions first (trial skills may follow): {reply}");
    assert!(reply.get("systemMessage").is_none(), "{reply}");
    // What Claude Code received whole is recorded, bound to the approved hash: once per start.
    let fsha = rematch_core::ledger::sha256_hex(fragment.as_bytes());
    let sent: Vec<Value> = trace(&env).into_iter().filter(|r| r["ev"] == "fragments_sent").collect();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["host"], "claude-code");
    assert_eq!(sent[0]["fragments"], json!([{"id": id, "name": "crate-tests-first", "sha": fsha, "via": "claude-code:session_start"}]));
    assert!(trace(&env).iter().all(|r| r["ev"] != "fragments_sent" || r["seq"].as_u64() > Some(0)), "after the session's start");

    // The next session: after the digest's context.
    let next = "6e1d9b9f-2222-4d4d-9999-0123456789ab";
    let p = json!({"session_id":next,"transcript_path":"","cwd":env.repo,"hook_event_name":"SessionStart","source":"startup"});
    let o = run_hook(&env, "session-start", &p.to_string(), &[]);
    assert!(context(&o).contains(&want), "{}", context(&o));
    let next_sent: Vec<Value> = trace_of(&env, &format!("cc:{next}")).into_iter().filter(|r| r["ev"] == "fragments_sent").collect();
    assert_eq!(next_sent.iter().map(|r| r["fragments"][0]["sha"].clone()).collect::<Vec<_>>(), vec![json!(fsha)], "{next_sent:?}");
    assert!(!env.repo.join("AGENTS.md").exists(), "the user's AGENTS.md is never written");
}

#[test]
fn post_tool_use_fast_check_returns_additional_context_with_diagnostic() {
    let env = Env::new();
    // Make env.repo a real Cargo package so `rematch check` detects Cargo.
    std::fs::write(
        env.repo.join("Cargo.toml"),
        "[package]\nname = \"payments\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let lib_rs = env.repo.join("src/lib.rs");
    std::fs::write(&lib_rs, "pub fn reconcile() -> String { \"bad\" }\n").unwrap();

    // Ensure fast-check hook is active (if not already auto-compiled, approve it).
    let fc = env
        .run(&["builtins", "ls"])["builtins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "fast-check")
        .cloned()
        .unwrap();
    let fc_id = fc["id"].as_str().unwrap().to_string();
    if fc["approved"] != true {
        let _ = env.run(&["library", "approve", &fc_id, "--what", "hook"]);
    }

    event(&env, "SessionStart", json!({"source":"startup"}));
    let file = lib_rs.to_string_lossy().into_owned();
    let o = event(
        &env,
        "PostToolUse",
        json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": file, "old_string": "x", "new_string": "y"},
            "tool_use_id": "toolu_edit_err",
            "tool_response": {"filePath": file, "structuredPatch": []},
        }),
    );
    let reply = o.json();
    assert_eq!(reply["hookSpecificOutput"]["hookEventName"], "PostToolUse", "{reply}");
    let ctx = reply["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default();
    assert!(
        ctx.contains("src/lib.rs:1:") && ctx.contains("E0308") && ctx.contains("to_string"),
        "expected cargo E0308 diagnostic + help in PostToolUse additionalContext: {ctx}"
    );
    let check_recs: Vec<Value> = trace(&env).into_iter().filter(|r| r["ev"] == "check_ran").collect();
    assert_eq!(check_recs.len(), 1, "{check_recs:?}");
    assert_eq!(check_recs[0]["hook_id"], fc_id);
    assert_eq!(check_recs[0]["kind"], "cargo");
    assert_eq!(check_recs[0]["ok"], false);
}
