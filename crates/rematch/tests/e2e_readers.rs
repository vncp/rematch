#![cfg_attr(not(unix), allow(dead_code, unused_imports))] // helpers of Unix-only tests
//! ADR 001 L4 through the binary: `reflect --history` and `improve` read a session's records
//! from the lake's `events/` by path, and `rematch report` reads the lake when there is one;
//! each agrees with today's path on the same seed and names its source.
#![cfg(feature = "lake")]

mod common;

use common::*;
use serde_json::{json, Value};

/// Paths under the env's temp dir as `<T>`, and the hashes derived from them (`local:`
/// remotes, tool-call `args_hash`) as `<H>`, so two envs' outputs compare.
fn norm_text(env: &Env, s: &str) -> String {
    let s = s.replace(env.root.parent().unwrap().to_str().unwrap(), "<T>");
    let s = regex::Regex::new("local:[0-9a-f]{64}").unwrap().replace_all(&s, "local:<H>").into_owned();
    regex::Regex::new(r#""args_hash":"[0-9a-f]{64}""#).unwrap().replace_all(&s, r#""args_hash":"<H>""#).into_owned()
}

fn norm(env: &Env, v: &Value) -> String {
    norm_text(env, &v.to_string())
}

/// Imported, flagged history; `lake`: exported, then the omp session file removed so only
/// the lake still holds its records. Returns the `reflect --history` result and the trace
/// it synthesized.
fn reflect_history(lake: bool) -> (Env, Value, String) {
    let env = Env::new();
    let file = redirected_session(&env, "hist-new", "2026-10-20");
    env.run(&["import", "--history"]);
    if lake {
        env.run(&["lake", "export", "--all"]);
        assert!(env.root.join("lake/events/omp/2026-10-20/hist-new.parquet").is_file());
        std::fs::remove_file(&file).unwrap();
    }
    let r = env.run(&["reflect", "--history", "--editor", &script("editor-ok.sh")]);
    let trace = norm_text(&env, &std::fs::read_to_string(env.root.join("traces/hist-new.jsonl")).unwrap());
    (env, r, trace)
}

#[cfg(unix)] // POSIX sh fixtures
#[test]
fn reflect_history_reads_the_lake_events_and_agrees_with_the_transcript() {
    let (_a, ledger, trace_a) = reflect_history(false);
    let (b, lake, trace_b) = reflect_history(true);
    for (r, src) in [(&ledger, "ledger"), (&lake, "lake")] {
        let h = &r["history"];
        assert_eq!((h["handed"].clone(), h["source"].clone()), (json!(["hist-new"]), json!(src)), "{r}");
        assert_eq!(h["sessions"][0]["outcome"], "staged", "{r}");
    }
    assert_eq!(lake["history"]["sources"], json!({"lake": 1, "ledger": 0}));
    assert_eq!(trace_a, trace_b, "the same records from the lake as from the transcript");
    assert_eq!(ledger["history"]["sessions"][0]["name"], lake["history"]["sessions"][0]["name"]);

    // `report` on the same root: the lake, with the verdict line.
    let rep = b.run(&["report"]);
    assert_eq!(rep["source"], "lake", "{rep}");
    assert!(rep["lines"].as_array().unwrap().last().unwrap().as_str().unwrap().starts_with("verdict: an entry reached kept over ≥ 4 matched sessions: "), "{rep}");
}

/// Four clean imported sessions that read the skill, then one with a tool-error cluster.
fn improve(lake: bool) -> (Env, Value, Value) {
    let env = Env::new();
    let skill = env.agent.join("skills/ledger-helper");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: ledger-helper\ndescription: Use when editing the ledger module.\n---\n\nUse when editing the ledger module.\n").unwrap();
    let read = || omp_call("r", "read", json!({"path": "skill://ledger-helper"}));
    let mut files = Vec::new();
    for i in 0..4 {
        files.push(write_omp_session(&env, &format!("imp-{i}"), &format!("2026-10-1{i}"), &[
            omp_user("Update the ledger module rounding"),
            omp_assistant("", vec![read()], "toolUse"),
            omp_result("r", "read", "skill body", false),
            omp_assistant("Done.", vec![], "stop"),
        ]));
    }
    let bash = |id: &str| omp_call(id, "bash", json!({"command": "cargo test -p ledger"}));
    files.push(write_omp_session(&env, "imp-err", "2026-10-19", &[
        omp_user("Update the ledger module rounding"),
        omp_assistant("", vec![read()], "toolUse"),
        omp_result("r", "read", "skill body", false),
        omp_assistant("", vec![bash("b1"), bash("b2"), bash("b3")], "toolUse"),
        omp_result("b1", "bash", "error[E0425]: cannot find value `rounding`", true),
        omp_result("b2", "bash", "error[E0425]: cannot find value `rounding`", true),
        omp_result("b3", "bash", "error[E0425]: cannot find value `rounding`", true),
        omp_assistant("The build fails.", vec![], "stop"),
    ]));
    env.run(&["import", "--history"]);
    if lake {
        env.run(&["lake", "export", "--all"]);
        for f in &files {
            std::fs::remove_file(f).unwrap();
        }
    }
    let dump = env.root.parent().unwrap().join("editor-input.txt");
    let editor = format!("tee -a '{}' | sh '{}'", dump.display(), script("editor-ok.sh"));
    let u = env.run(&["improve", "ledger-helper", "--editor", &editor]);
    let text = std::fs::read_to_string(&dump).unwrap();
    let body = &text[text.find("<input>").unwrap() + 7..text.find("</input>").unwrap()];
    let input: Value = serde_json::from_str(body.trim()).unwrap();
    (env, u, input)
}

#[cfg(unix)] // POSIX sh fixtures
#[test]
fn improve_excerpt_reads_the_lake_events_and_agrees_with_the_transcripts() {
    let (a, ledger, in_a) = improve(false);
    let (b, lake, in_b) = improve(true);
    assert_eq!((ledger["source"].as_str(), lake["source"].as_str()), (Some("ledger"), Some("lake")), "{ledger}\n{lake}");
    assert_eq!(lake["sources"], json!({"lake": 1, "ledger": 0}), "{lake}");
    assert!(in_b["trace_excerpt"].as_array().unwrap().iter().any(|r| r["ev"] == "tool_result" && r["is_error"] == true), "{in_b}");
    for k in ["trace_excerpt", "clusters", "redirect", "scope"] {
        assert_eq!(norm(&a, &in_a[k]), norm(&b, &in_b[k]), "{k}");
    }
}

#[test]
fn report_falls_back_to_the_ledger_without_a_lake() {
    let env = Env::new();
    let r = env.run(&["report", "--window", "2w"]);
    assert_eq!((r["source"].as_str(), r["window"]["spec"].as_str()), (Some("ledger"), Some("2w")), "{r}");
    let lines = r["lines"].as_array().unwrap();
    assert!(lines[0].as_str().unwrap().starts_with("report · source ledger · machines "), "{r}");
    assert_eq!(lines.last().unwrap(), "verdict: an entry reached kept over ≥ 4 matched sessions: no; recurrence fell on a covered trigger: no");
    let (v, code) = env.run_cmd(env.cmd(&["report", "--window", "6x"]));
    assert_eq!((code, v["ok"].clone()), (1, json!(false)), "{v}");
    env.run(&["lake", "export"]);
    let r = env.run(&["report", "--since", "2026-10-01"]);
    assert_eq!((r["source"].as_str(), r["window"]["since"].as_str()), (Some("lake"), Some("2026-10-01")), "{r}");
}
