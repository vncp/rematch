#![cfg_attr(not(unix), allow(dead_code, unused_imports))] // helpers of Unix-only tests
//! Context entries through the binary: `retrieve` over a staged package (ranking, budget,
//! loaded-only), the built-in hook entries (installed once, offered as approvals, inert
//! until approved), and the attribution loop (a `retrieval` trace record credits the
//! retrieve hook and the packages its units came from when `count` folds the session).

mod common;

use common::{script, Env};
use serde_json::{json, Value};

fn staged(env: &Env, name: &str, description: &str) -> String {
    let d = env.run(&["create", name, "--from", "description", "--description", description, "--editor", &script("editor-ok.sh")]);
    assert_eq!(d["outcome"], "staged", "{d}");
    env.run(&["activate", name, "--unverified"]);
    d["id"].as_str().unwrap().to_string()
}

fn write(p: &std::path::Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn paths(v: &Value) -> Vec<(String, u64)> {
    v["units"].as_array().unwrap().iter().map(|u| (u["path"].as_str().unwrap().to_string(), u["overlap"].as_u64().unwrap())).collect()
}

#[cfg(unix)] // POSIX sh fixtures
#[test]
fn retrieve_ranks_loaded_package_units_within_the_budget() {
    let env = Env::new();
    let id = staged(&env, "ledger-test-gate", "Run the ledger tests before committing payment changes");
    let dir = env.root.join("staging/payments/ledger-test-gate");
    write(&dir.join("units/reconcile.md"), "# Ledger reconciliation\n\nRun the reconciliation migration before the ledger tests.\n");
    write(&dir.join("units/short.md"), "# Ledger\n\nReconciliation first.\n");
    write(&dir.join("units/charts.md"), "# Charts\n\nPlots use the dark theme.\n");
    write(&dir.join("facts/schedule.md"), "---\nas_of: 2026-10-01\n---\n# Ledger schedule\n\nThe ledger reconciliation runs nightly.\n");
    let repo = env.repo.to_str().unwrap();
    // The package's trigger line (the scripted editor's: committing changes to a Rust crate)
    // must share a stem with the query; a query about the unit's words alone yields nothing.
    let q = "fix the ledger reconciliation migration in src/ledger.rs";
    let r = env.run(&["retrieve", "--cwd", repo, "--budget", "6000", "--query", q]);
    assert_eq!(paths(&r), vec![], "no trigger hit: {r}");
    let q = "commit the ledger reconciliation migration change to the crate";

    let r = env.run(&["retrieve", "--cwd", repo, "--budget", "6000", "--query", q]);
    assert_eq!(
        paths(&r),
        vec![("units/reconcile.md".into(), 3), ("units/short.md".into(), 2), ("facts/schedule.md".into(), 2)],
        "{r}"
    );
    let u0 = &r["units"][0];
    assert_eq!((u0["entry_id"].as_str(), u0["name"].as_str(), u0["title"].as_str()), (Some(id.as_str()), Some("ledger-test-gate"), Some("Ledger reconciliation")));
    let used: u64 = r["units"].as_array().unwrap().iter().map(|u| u["text"].as_str().unwrap().len() as u64).sum();
    assert_eq!(r["used_bytes"].as_u64(), Some(used));
    assert!(r["query_stems"].as_array().unwrap().contains(&json!("ledger")));

    // budget: only what fits, in rank order
    let first = r["units"][0]["text"].as_str().unwrap().len();
    let r = env.run(&["retrieve", "--cwd", repo, "--budget", &first.to_string(), "--query", q]);
    assert_eq!(paths(&r), vec![("units/reconcile.md".into(), 3)]);
    // min overlap
    let r = env.run(&["retrieve", "--cwd", repo, "--min-overlap", "3", "--query", q]);
    assert_eq!(paths(&r).len(), 1);

    // never a unit of an entry the caller did not load: `--entries` and the cwd's match
    let r = env.run(&["retrieve", "--cwd", repo, "--entries", "someone-else", "--query", q]);
    assert_eq!(r["units"], json!([]), "{r}");
    let r = env.run(&["retrieve", "--cwd", repo, "--entries", &format!("x,{id}"), "--query", q]);
    assert_eq!(paths(&r).len(), 3);
    let elsewhere = env.home.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let r = env.run(&["retrieve", "--cwd", elsewhere.to_str().unwrap(), "--query", q]);
    assert_eq!(r["units"], json!([]), "the payments package does not load elsewhere: {r}");

    // kill switch: nothing loads, nothing is retrieved
    env.run(&["off"]);
    let r = env.run(&["retrieve", "--cwd", repo, "--query", q]);
    assert_eq!(r["units"], json!([]));
}

#[test]
fn builtins_install_once_and_wait_for_approval() {
    let env = Env::new();
    let ls = env.run(&["builtins", "ls"]);
    let items = ls["builtins"].as_array().unwrap();
    let got: Vec<(&str, &str, &str)> =
        items.iter().map(|x| (x["name"].as_str().unwrap(), x["mode"].as_str().unwrap(), x["status"].as_str().unwrap())).collect();
    assert_eq!(
        got,
        vec![("context-unit-retrieval", "retrieve", "provisional"), ("tool-output-truncation", "truncate", "provisional"), ("stuck-detector", "stuck", "provisional")]
    );
    assert!(items.iter().all(|x| x["approved"] == false));
    let hook: Value = serde_json::from_str(
        &std::fs::read_to_string(env.root.join("staging/shared/tool-output-truncation/hook.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(hook["mode"], "truncate");
    assert_eq!((hook["max_age_turns"].as_u64(), hook["keep_chars"].as_u64()), (Some(6), Some(1200)));

    // unapproved: no shared dir, no hook module, not counted
    let m = env.run(&["match", "--cwd", env.repo.to_str().unwrap()]);
    assert_eq!((m["skill_dirs"].clone(), m["hooks"].clone(), m["entries"].clone()), (json!([]), json!([]), json!([])), "{m}");

    // offered once each in the digest; approving compiles a data-only module
    let g = env.run(&["digest", "--cwd", env.repo.to_str().unwrap()]);
    let approvals = g["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 3, "{g}");
    let stuck = approvals.iter().find(|a| a["name"] == "stuck-detector").unwrap();
    assert_eq!(stuck["kind"], "hook");
    assert!(stuck["diff"].as_str().unwrap().contains(r#"params: {"repeats":3},"#), "{stuck}");
    let id = stuck["id"].as_str().unwrap().to_string();
    env.run(&["library", "approve", &id]);
    let module = std::fs::read_to_string(env.agent.join("extensions/rematch-hooks").join(format!("{id}.ts"))).unwrap();
    assert!(module.contains(r#"mode: "stuck","#) && !module.contains("=>"), "{module}");
    let m = env.run(&["match", "--cwd", env.repo.to_str().unwrap()]);
    assert_eq!(m["skill_dirs"], json!([env.root.join("staging").join("shared")]));
    assert!(m["entries"].as_array().unwrap().iter().any(|e| e["id"] == id.as_str() && e["predicate_hit"] == true), "{m}");

    // idempotent across runs
    env.run(&["library", "ls"]);
    let ls = env.run(&["library", "ls"]);
    let n = ls["entries"].as_array().unwrap().iter().filter(|e| e["origin"]["source"] == "builtin").count();
    assert_eq!(n, 3);
}

/// A clean finished session in the payments repo: `loaded` at session start, then (when
/// given) a `retrieval` record the extension's retrieve hook wrote before the first turn.
fn context_session(env: &Env, sid: &str, loaded: &[&str], retrieval: Option<Value>) {
    let repo = env.repo.to_string_lossy();
    let file = format!("{repo}/src/ledger.rs");
    let mut recs = vec![
        json!({"ev":"session_start","session_id":sid,"cwd":repo,"repo":repo,"branch_hint":"payments",
               "model":{"provider":"anthropic","id":"claude-opus-5-5"},"mode":"interactive","agent":{"kind":"main","name":"main"},
               "omp_version":"18.12.6","loaded_entries":loaded,"hooks_loaded":[],"staging_dirs":[]}),
        json!({"ev":"input","text":"Fix the ledger reconciliation migration in src/ledger.rs","source":"interactive",
               "idle":true,"after_deliverable":null,"prelabel":"none","skills_invoked":[]}),
    ];
    recs.extend(retrieval);
    recs.extend([
        json!({"ev":"tool_result","tool":"read","call_id":"c1","input":{"path":file},"args_hash":"h1","result_bytes":100,
               "is_error":false,"error_class":null,"content_head":"pub fn reconcile() {}","skill_read":null,"path":file}),
        json!({"ev":"tool_result","tool":"edit","call_id":"c2","input":{"path":file},"args_hash":"h2","result_bytes":10,
               "is_error":false,"error_class":null,"content_head":"ok","skill_read":null,"path":file}),
        json!({"ev":"turn_end","model":{"provider":"anthropic","id":"claude-opus-5-5"},
               "usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0,"totalTokens":150,"cost":null},"stop_reason":"stop",
               "text_head":"Fixed the reconciliation migration."}),
        json!({"ev":"input","text":"Now add a dry run flag to the ledger reconciliation","source":"interactive","idle":true,
               "after_deliverable":"Fixed the reconciliation migration.","prelabel":"none","skills_invoked":[]}),
        json!({"ev":"turn_end","model":{"provider":"anthropic","id":"claude-opus-5-5"},
               "usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":null},"stop_reason":"stop","text_head":"Added."}),
        json!({"ev":"agent_end","turns":2,"is_terminal":true,"yielded":true}),
        json!({"ev":"session_shutdown","reasons":[],"flagged":false}),
    ]);
    for (i, r) in recs.iter_mut().enumerate() {
        r["seq"] = json!(i);
        r["t"] = json!(1_000 + i);
    }
    env.write_records(sid, &recs);
}

fn builtin(env: &Env, name: &str) -> Value {
    let ls = env.run(&["builtins", "ls"]);
    ls["builtins"].as_array().unwrap().iter().find(|x| x["name"] == name).cloned().unwrap()
}

fn count(env: &Env) -> Value {
    env.run(&["count", "--editor", &script("editor-ok.sh")])
}

#[cfg(unix)] // POSIX sh fixtures
#[test]
fn retrieval_record_credits_the_hook_and_the_package_through_count() {
    let env = Env::new();
    let pkg = staged(&env, "ledger-test-gate", "Run the ledger tests before committing payment changes");
    write(&env.root.join("staging/payments/ledger-test-gate/units/reconcile.md"), "# Ledger reconciliation\n\nRun the reconciliation migration before the ledger tests.\n");
    let hook = builtin(&env, "context-unit-retrieval")["id"].as_str().unwrap().to_string();
    env.run(&["library", "approve", &hook]);
    assert_eq!(builtin(&env, "context-unit-retrieval")["approved"], true);
    let retrieval = || {
        json!({"ev":"retrieval","entry_ids":[hook],"units":[{"entry_id":pkg,"path":"units/reconcile.md","bytes":80}],"bytes":80})
    };

    // Session 1: nothing loaded at start; the retrieve hook appended a unit of the package.
    let s1 = "0199d000-0000-7000-8000-000000000001".to_string();
    context_session(&env, &s1, &[], Some(retrieval()));
    let c = count(&env);
    assert_eq!(c["folded"].as_array().unwrap().len(), 1, "{c}");
    let ledger = rematch_core::paths::RematchPaths::with_roots(&env.root, &env.agent).ledger_file;
    let rows = |id: &str| -> Vec<(String, u32)> {
        let l = rematch_core::ledger::Ledger::open(&ledger).unwrap();
        let mut v: Vec<_> = l.counter_rows(id).unwrap().into_iter().map(|r| (r.session_id, r.matched)).collect();
        v.sort();
        v
    };
    assert_eq!(rows(&hook), vec![(s1.clone(), 1)], "the retrieve hook is credited by its retrieval record");
    assert_eq!(rows(&pkg), vec![(s1.clone(), 1)], "the package its unit came from is credited");
    assert_eq!(builtin(&env, "context-unit-retrieval")["counters"]["matched"], 1);

    // Session 2: the package loaded at start, no retrieval record ⇒ the hook is not credited.
    let s2 = "0199d000-0000-7000-8000-000000000002".to_string();
    context_session(&env, &s2, &[&pkg], None);
    count(&env);
    assert_eq!(rows(&hook), vec![(s1.clone(), 1)], "no retrieval record, no credit for the retrieve hook");
    assert_eq!(rows(&pkg), vec![(s1.clone(), 1), (s2.clone(), 1)]);

    // Sessions 3 and 4 (clean, retrieved again): four clean matched sessions keep the package.
    context_session(&env, "0199d000-0000-7000-8000-000000000003", &[], Some(retrieval()));
    context_session(&env, "0199d000-0000-7000-8000-000000000004", &[&pkg], Some(retrieval()));
    let c = count(&env);
    let d = c["decisions"].as_array().unwrap().iter().find(|d| d["entry"] == pkg.as_str()).cloned().unwrap_or(Value::Null);
    assert_eq!(d["decision"], "keep", "{c}");
    assert_eq!(env.entry(&pkg)["status"], "kept");
    assert_eq!(rows(&pkg).len(), 4);
    let h = builtin(&env, "context-unit-retrieval");
    assert_eq!((h["counters"]["matched"].as_u64(), h["status"].as_str()), (Some(3), Some("provisional")), "{h}");
}
