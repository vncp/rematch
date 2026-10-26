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
    let op = if name == "PreToolUse" { "hook-check" } else { "trace-event" };
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
    // legacy dir and asks for a re-scan.
    let o = event(&env, "SessionStart", json!({"source":"startup","model":"claude-opus-5-5"}));
    assert_eq!(o.json(), json!({"hookSpecificOutput":{"hookEventName":"SessionStart","reloadSkills":true}}));
    assert_eq!(std::fs::read_link(&link).unwrap(), env.root.join("skills"));
    assert!(!legacy.exists());

    let o = event(&env, "UserPromptSubmit", json!({"prompt":"Add the ledger reconciliation command to the payments crate"}));
    assert!(o.stdout.trim().is_empty(), "no retrieve hook approved: {:?}", o.stdout);
    let o = event(&env, "PreToolUse", json!({"tool_name":"Read","tool_input":{"file_path":file},"tool_use_id":"toolu_1"}));
    assert_eq!((o.code, o.stdout.trim()), (0, ""), "nothing approved, nothing blocks");
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
    let o = event(&env, "SessionEnd", json!({"reason":"prompt_input_exit"}));
    assert!(o.stdout.trim().is_empty());

    // The trace: DESIGN §3 records, seq = index.
    let recs = trace(&env);
    assert_eq!(
        evs(&recs),
        vec![
            "session_start", "input", "tool_result", "tool_result", "tool_result", "tool_result", "tool_result", "turn_end", "turn_end",
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
    assert_eq!(s0["loaded_entries"], json!([]));
    assert_eq!((recs[1]["prelabel"].as_str(), recs[1]["idle"].as_bool()), (Some("none"), Some(true)));
    assert_eq!((recs[2]["tool"].as_str(), recs[2]["path"].as_str()), (Some("read"), Some(file.as_str())));
    assert_eq!(recs[2]["content_head"], "pub fn reconcile() {}");
    assert_eq!(recs[3]["tool"], "edit");
    assert_eq!((recs[4]["tool"].as_str(), recs[4]["is_error"].as_bool()), (Some("bash"), Some(true)));
    assert!(recs[4]["content_head"].as_str().unwrap().starts_with("Exit code 101"));
    assert_eq!(recs[8]["text_head"], "Added the reconcile command and committed it.");
    assert_eq!(recs[8]["model"], json!({"provider":"anthropic","id":"claude-opus-5-5"}));
    assert_eq!(recs[8]["usage"]["input"], 100);
    assert_eq!((recs[9]["turns"].as_u64(), recs[9]["yielded"].as_bool()), (Some(2), Some(true)));
    assert_eq!((recs[10]["prelabel"].as_str(), recs[10]["idle"].as_bool()), (Some("negation"), Some(true)));
    assert_eq!(recs[10]["after_deliverable"], "Added the reconcile command and committed it.");
    assert_eq!(recs[12]["text_head"], "Tests pass now; committed after running them.");
    let shutdown = recs.last().unwrap();
    assert_eq!(shutdown["flagged"], true);
    assert_eq!(shutdown["reasons"], json!(["tool_error_cluster", "redirect:negation"]));

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

    // skills sync links the staged skill; the plugin sees it through skills/.
    let m = env.run(&["skills", "sync", "--harness", "claude-code", "--cwd", &repo]);
    let staged = env.staged_dir();
    assert_eq!((m["added"].clone(), m["links"].clone()), (json!(["run-tests-before-commit"]), json!({"run-tests-before-commit": staged})), "{m}");
    assert_eq!(m["dirs"], json!([env.root.join("skills")]));
    let linked = env.root.join("skills/run-tests-before-commit");
    assert_eq!(std::fs::read_link(&linked).unwrap(), staged);
    assert!(link.join("run-tests-before-commit/SKILL.md").is_file(), "readable through the plugin's skills/ link");
    let again = env.run(&["skills", "sync", "--harness", "claude-code", "--cwd", &repo]);
    assert_eq!((again["added"].clone(), again["pruned"].clone()), (json!([]), json!([])), "idempotent");

    // The next session in the repo loads it (and needs no re-scan: the links did not change).
    let next = "6e1d9b9f-2222-4d4d-9999-0123456789ab";
    let p = json!({"session_id":next,"transcript_path":"","cwd":repo,"hook_event_name":"SessionStart","source":"startup"});
    let o = run_hook(&env, "trace-event", &p.to_string(), &[]);
    assert_eq!((o.code, o.stdout.trim()), (0, ""));
    let s = trace_of(&env, &format!("cc:{next}"));
    assert_eq!(s[0]["loaded_entries"], json!([id]));
    assert_eq!(s[0]["staging_dirs"], json!([env.root.join("skills")]));

    // Elsewhere nothing matches: the link stays (union: a session in the repo may use it), and
    // the session does not count it.
    let other = env.home.join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let elsewhere = "7f2e0c0a-3333-4e4e-aaaa-0123456789ab";
    let p = json!({"session_id":elsewhere,"cwd":other,"hook_event_name":"SessionStart","source":"startup"});
    let o = run_hook(&env, "trace-event", &p.to_string(), &[]);
    assert_eq!((o.code, o.stdout.trim()), (0, ""));
    assert_eq!(std::fs::read_link(&linked).unwrap(), staged);
    assert_eq!(trace_of(&env, &format!("cc:{elsewhere}"))[0]["loaded_entries"], json!([]));

    // Retired: the link is pruned at once.
    env.run(&["library", "disable", &id]);
    assert!(std::fs::symlink_metadata(&linked).is_err());
}

#[test]
fn approved_stuck_hook_blocks_the_repeat() {
    let env = Env::new();
    let ls = env.run(&["builtins", "ls"]);
    let stuck = ls["builtins"].as_array().unwrap().iter().find(|x| x["name"] == "stuck-detector").unwrap()["id"].as_str().unwrap().to_string();
    env.run(&["library", "approve", &stuck]);

    event(&env, "SessionStart", json!({"source":"startup"}));
    let s0 = &trace(&env)[0];
    assert_eq!(s0["hooks_loaded"], json!([stuck]));
    assert!(s0["loaded_entries"].as_array().unwrap().contains(&json!(stuck)));
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
    env.run(&["library", "approve", &hook]);

    event(&env, "SessionStart", json!({"source":"startup"}));
    let s0 = &trace(&env)[0];
    assert!(!s0["loaded_entries"].as_array().unwrap().contains(&json!(hook)), "a retrieve hook credits through retrieval records");
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
