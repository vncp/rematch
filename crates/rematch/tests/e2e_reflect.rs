//! End-to-end: synthetic trace + pending file → `reflect --pending` with a scripted editor
//! (and through the fake omp) → staged entry → `match` → idempotence / canary → `count`.

mod common;

use std::process::Command;

use common::*;
use serde_json::{json, Value};

#[test]
fn reflect_stage_match_canary_count() {
    let env = Env::new();
    let s1 = "0192c4a0-0000-7000-8000-000000000001";
    env.write_trace(s1, &[], true);

    let d = env.run(&["reflect", "--pending", "--editor", &script("editor-ok.sh")]);
    let r = &d["reflections"][0];
    assert_eq!(r["outcome"], "staged", "{d}");
    assert_eq!(r["model_calls"], 2, "classifier + editor: {d}");
    assert_eq!(env.calls(), vec!["classifier", "editor"]);
    let id = r["id"].as_str().unwrap().to_string();

    // Staged package with the rematch frontmatter block.
    let skill = std::fs::read_to_string(env.staged_dir().join("SKILL.md")).unwrap();
    assert!(skill.contains("rematch:"), "{skill}");
    assert!(skill.contains(&format!("id: {id}")), "{skill}");
    assert!(env.staged_dir().join("rematch.json").is_file());
    assert!(!env.root.join("pending").join(format!("{s1}.json")).exists(), "pending consumed");

    // Ledger has the entry, with the derived predicate.
    let ls = env.run(&["library", "ls"]);
    let e = ls["entries"].as_array().unwrap().iter().find(|e| e["id"] == id.as_str()).expect("entry in ledger").clone();
    assert_eq!(e["status"], "provisional");
    assert_eq!(e["branch"], "payments");
    assert_eq!(e["predicate"]["file_exts"], json!(["rs"]));
    assert_eq!(e["origin"], json!({"kind": "session", "session_id": s1}));

    // match --cwd <repo> contributes the staging branch dir.
    let m = env.run(&["match", "--cwd", env.repo.to_str().unwrap()]);
    assert_eq!(m["skill_dirs"], json!([env.root.join("staging").join("payments")]));
    assert!(m["entries"].as_array().unwrap().iter().any(|x| x["id"] == id.as_str() && x["predicate_hit"] == true), "{m}");
    // … and nothing for an unrelated directory.
    let other = env.home.join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let m2 = env.run(&["match", "--cwd", other.to_str().unwrap()]);
    assert_eq!(m2["skill_dirs"], json!([]));

    // A second reflect is a no-op (pending consumed, sessions counted).
    let d2 = env.run(&["reflect", "--pending", "--editor", &script("editor-ok.sh")]);
    assert_eq!(d2["reflections"], json!([]), "{d2}");
    assert_eq!(d2["count"]["folded"], json!([]));
    assert_eq!(env.calls().len(), 2, "no further model calls");

    // Same trigger again while the canary is pending: refused before any editor call.
    let s2 = "0192c4a0-0000-7000-8000-000000000002";
    env.write_trace(s2, &[], true);
    let d3 = env.run(&["reflect", "--pending", "--editor", &script("editor-ok.sh")]);
    assert_eq!(d3["reflections"][0]["outcome"], "rate_limited", "{d3}");
    assert!(d3["reflections"][0]["reason"].as_str().unwrap().contains("pending canary"), "{d3}");
    assert_eq!(env.calls(), vec!["classifier", "editor", "classifier"], "only the batched classifier ran");

    // A later session with the entry loaded and a predicate hit increments matched.
    let s3 = "0192c4a0-0000-7000-8000-000000000003";
    env.write_trace(s3, &[&id], false);
    let c = env.run(&["count", "--editor", &script("editor-ok.sh")]);
    assert_eq!(c["folded"], json!([s3]), "{c}");
    assert_eq!(c["counter_rows"], 1, "{c}");
    let ls = env.run(&["library", "ls"]);
    let e = ls["entries"].as_array().unwrap().iter().find(|e| e["id"] == id.as_str()).unwrap().clone();
    assert_eq!(e["counters"]["matched"], 1, "{e}");
    assert_eq!(e["counters"]["recurrence"], 0);
    assert_eq!(e["counters"]["harm"], 0);

    // why shows the loaded entry and its predicate hit for that session.
    let w = env.run(&["library", "why", s3]);
    assert_eq!(w["loaded"][0]["id"], id.as_str());
    assert_eq!(w["loaded"][0]["predicate_hit"], true);

    // Digest reports the staged entry once.
    let g = env.run(&["digest", "--cwd", env.repo.to_str().unwrap()]);
    assert!(g["lines"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("staged skill run-tests-before-commit")), "{g}");
    let g2 = env.run(&["digest", "--cwd", env.repo.to_str().unwrap()]);
    assert!(!g2["lines"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("staged skill")), "{g2}");
}

#[test]
fn lint_failure_triggers_one_retry() {
    let env = Env::new();
    let s1 = "0192c4a0-0000-7000-8000-0000000000a1";
    env.write_trace(s1, &[], true);
    let d = env.run(&["reflect", "--pending", "--editor", &script("editor-lint-fail.sh")]);
    let r = &d["reflections"][0];
    assert_eq!(r["outcome"], "staged", "{d}");
    assert_eq!(r["editor_attempts"], 2, "{d}");
    assert_eq!(r["model_calls"], 3, "classifier + editor + lint retry: {d}");
    assert_eq!(env.calls(), vec!["classifier", "editor", "editor"]);
    let skill = std::fs::read_to_string(env.staged_dir().join("SKILL.md")).unwrap();
    assert!(!skill.contains("https://"), "{skill}");
}

#[test]
fn keep_base_records_a_proposal_and_stages_nothing() {
    let env = Env::new();
    let s1 = "0192c4a0-0000-7000-8000-0000000000b1";
    env.write_trace(s1, &[], true);
    let d = env.run(&["reflect", "--pending", "--editor", &script("editor-keep-base.sh")]);
    let r = &d["reflections"][0];
    assert_eq!(r["outcome"], "keep_base", "{d}");
    assert!(r["proposal"].is_string());
    assert!(!env.staged_dir().exists());
    assert!(!env.root.join("pending").join(format!("{s1}.json")).exists());
    // only the shipped built-ins (unapproved hooks, `origin.source == "builtin"`) are in the library
    let ls = env.run(&["library", "ls"]);
    let staged: Vec<&Value> = ls["entries"].as_array().unwrap().iter().filter(|e| e["origin"]["source"] != "builtin").collect();
    assert!(staged.is_empty(), "{ls}");
}

#[test]
fn reflect_through_fake_omp_verifies_the_event_stream() {
    let env = Env::new();
    let s1 = "0192c4a0-0000-7000-8000-0000000000c1";
    env.write_trace(s1, &[], true);
    // The editor reply the fake omp prints (taken from the scripted editor's clean entry).
    let reply = Command::new("sh")
        .arg("-c")
        .arg(format!("echo '{{}}' | REMATCH_PROMPT=editor {}", script("editor-ok.sh")))
        .output()
        .unwrap();
    let reply_file = env.home.join("reply.txt");
    std::fs::write(&reply_file, &reply.stdout).unwrap();
    let omp_log = env.home.join("omp.log");
    let mut c = env.cmd(&["reflect", "--pending"]);
    c.env("OMP_BIN", script("fake-omp.sh")).env("FAKE_OMP_REPLY_FILE", &reply_file).env("FAKE_OMP_LOG", &omp_log);
    let (v, code) = env.run_cmd(c);
    assert_eq!(code, 0, "{v}");
    let r = &v["data"]["reflections"][0];
    assert_eq!(r["outcome"], "staged", "{v}");
    let calls = std::fs::read_to_string(&omp_log).unwrap();
    let calls: Vec<&str> = calls.lines().collect();
    assert_eq!(calls.len(), 2, "classifier + editor through omp: {calls:?}");
    assert!(calls[0].contains("--model fakeprov/small-model"), "classifier role → smol: {}", calls[0]);
    assert!(calls[1].contains("--model anthropic/claude-opus-5-5"), "editor role → default (thinking dropped): {}", calls[1]);
    for c in &calls {
        assert!(c.contains("--no-tools") && c.contains("--mode json") && c.contains("-p --"), "{c}");
    }
}

#[test]
fn fake_omp_with_wrong_model_fails_closed() {
    let env = Env::new();
    let s1 = "0192c4a0-0000-7000-8000-0000000000d1";
    env.write_trace(s1, &[], true);
    // A wrapper that answers with a different model than requested.
    let wrapper = env.home.join("liar-omp.sh");
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\nexec {} --model other/model \"$@\" | sed 's/\"provider\":\"anthropic\"/\"provider\":\"other\"/g'\n", script("fake-omp.sh")),
    )
    .unwrap();
    Command::new("chmod").arg("755").arg(&wrapper).status().unwrap();
    let mut c = env.cmd(&["reflect", "--pending"]);
    c.env("OMP_BIN", &wrapper);
    let (v, code) = env.run_cmd(c);
    assert_eq!(code, 0, "{v}");
    let r = &v["data"]["reflections"][0];
    assert_eq!(r["outcome"], "editor_failed_retry_later", "{v}");
    assert!(r["reason"].as_str().unwrap().contains("requested (no fallback)"), "{v}");
    assert!(env.root.join("pending").join(format!("{s1}.json")).exists(), "pending kept for a later attempt");
}

#[test]
fn mark_flags_pending_and_off_refuses_reflect() {
    let env = Env::new();
    let d = env.run(&["mark", "--session", "live-session", "--class", "preference"]);
    assert_eq!(d["class"], "preference");
    let p: Value = serde_json::from_str(&std::fs::read_to_string(env.root.join("pending/live-session.json")).unwrap()).unwrap();
    assert_eq!(p["reasons"], json!(["mark:preference"]));
    env.run(&["off"]);
    let (v, code) = env.run_cmd(env.cmd(&["reflect", "--pending"]));
    assert_eq!(code, 1);
    assert!(v["error"].as_str().unwrap().contains("off"), "{v}");
    let m = env.run(&["match", "--cwd", env.repo.to_str().unwrap()]);
    assert_eq!(m["off"], true);
    env.run(&["on"]);
    let (_, code) = env.run_cmd(env.cmd(&["bogus-subcommand"]));
    assert_eq!(code, 2);
}

#[test]
fn four_clean_matched_sessions_keep_the_entry() {
    let env = Env::new();
    env.write_trace("0192c4a0-0000-7000-8000-0000000000f0", &[], true);
    let d = env.run(&["reflect", "--pending", "--editor", &script("editor-ok.sh")]);
    let id = d["reflections"][0]["id"].as_str().unwrap().to_string();
    for i in 1..=4 {
        env.write_trace(&format!("0192c4a0-0000-7000-8000-0000000000f{i}"), &[&id], false);
    }
    let c = env.run(&["count", "--editor", &script("editor-ok.sh")]);
    assert_eq!(c["counter_rows"], 4, "{c}");
    let dec = &c["decisions"][0];
    assert_eq!(dec["decision"], "keep", "{c}");
    assert_eq!(dec["experience"]["source"], "reflector");
    let kept = env.agent.join("skills/run-tests-before-commit");
    let skill = std::fs::read_to_string(kept.join("SKILL.md")).unwrap();
    assert!(skill.contains("status: kept"), "{skill}");
    let x = std::fs::read_to_string(kept.join("EXPERIENCE.md")).unwrap();
    assert!(x.contains("### Lesson") && x.contains("matched 4 · recurrence 0 · harm 0"), "{x}");
    assert!(!env.staged_dir().exists(), "moved out of staging");
    let e = env.entry(&id);
    assert_eq!(e["status"], "kept");
    assert_eq!(e["counters"]["matched"], 4);
    // Kept entries load on every match.
    let m = env.run(&["match", "--cwd", env.repo.to_str().unwrap()]);
    assert!(m["entries"].as_array().unwrap().iter().any(|x| x["id"] == id.as_str() && x["status"] == "kept"), "{m}");
}
