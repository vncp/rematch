//! Binary commands beyond the reflect loop: import (history goldens, forge), create
//! (description → draft → activate, sessions), improve (refusal, split, unit-level),
//! approvals, detached jobs, doctor, uninstall.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

#[test]
fn import_history_goldens_is_idempotent() {
    let env = Env::new();
    let dir = repo_root().join("tests/goldens");
    let d = env.run(&["import", "--history", "--sessions", dir.to_str().unwrap()]);
    let h = &d["history"];
    assert_eq!(h["files"], 3, "{d}");
    assert_eq!(h["new"], 3);
    assert_eq!(h["failed"], 0);
    assert!(h["with_skill_reads"].as_u64().unwrap() >= 1, "{d}");
    let d2 = env.run(&["import", "--history", "--sessions", dir.to_str().unwrap()]);
    assert_eq!(d2["history"]["new"], 0);
    assert_eq!(d2["history"]["updated"], 3);
}

fn listing(dir: &Path) -> Vec<(String, u64)> {
    let mut v = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                v.push((p.to_string_lossy().into_owned(), std::fs::metadata(&p).unwrap().len()));
            }
        }
    }
    v.sort();
    v
}

#[test]
fn import_forge_registers_without_touching_the_checkout() {
    let env = Env::new();
    let forge = env.home.join("compound-forge");
    for (name, desc) in [("forge-alpha", "Ledger reconciliation workflow. Use when reconciling payment ledgers."), ("forge-beta", "Animation baking. Use when baking clips.")] {
        std::fs::create_dir_all(forge.join("skills").join(name)).unwrap();
        std::fs::write(forge.join("skills").join(name).join("SKILL.md"), format!("---\nname: {name}\ndescription: {desc}\n---\n# {name}\n\n{desc}\n")).unwrap();
    }
    std::fs::create_dir_all(forge.join("learnings")).unwrap();
    let lesson = "# Keep integrity separate from vocabulary\n\nMeasured: separating them halved review churn.\n";
    std::fs::write(forge.join("learnings/2026-10-21-keep-integrity-separate.md"), lesson).unwrap();
    std::fs::create_dir_all(forge.join("evals/suite")).unwrap();
    std::fs::write(forge.join("evals/suite/case.json"), "{}").unwrap();
    let before = listing(&forge);

    let d = env.run(&["import", "--forge", forge.to_str().unwrap()]);
    let f = &d["forge"];
    assert_eq!(f["skills_registered"], 2, "{d}");
    assert_eq!(f["lessons_registered"], 1, "{d}");
    assert_eq!(listing(&forge), before, "forge checkout untouched");
    let alpha = f["skills"].as_array().unwrap().iter().find(|s| s["name"] == "forge-alpha").unwrap();
    assert_eq!(alpha["path"], json!(forge.join("skills/forge-alpha")));
    assert!(alpha["stems"].as_array().unwrap().iter().any(|s| s == "ledger"), "{alpha}");
    let e = env.entry(alpha["id"].as_str().unwrap());
    assert_eq!(e["status"], "provisional");
    assert_eq!(e["branch"], "forge");
    assert_eq!(e["origin"], json!({"kind": "import", "source": "forge"}));
    assert_eq!(e["counters"]["matched"], 0);

    let l = &f["lessons"][0];
    let expect = format!("{:x}", sha2_digest(lesson.as_bytes()));
    assert_eq!(l["evidence_sha256"], expect.as_str());
    let ej: Value = serde_json::from_str(
        &std::fs::read_to_string(env.root.join("lessons/forge").join(l["name"].as_str().unwrap()).join("rematch.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(ej["evidence_sha256"], expect.as_str());
    assert_eq!(ej["kind"], "lesson");

    // Idempotent.
    let d2 = env.run(&["import", "--forge", forge.to_str().unwrap()]);
    assert_eq!(d2["forge"]["skills_registered"], 0);
    assert_eq!(d2["forge"]["skills_existing"], 2);
    assert_eq!(d2["forge"]["lessons_existing"], 1);

    // Brownfield registrations add no staging dir and lessons never appear as loaded entries.
    let m = env.run(&["match", "--cwd", env.repo.to_str().unwrap()]);
    assert_eq!(m["skill_dirs"], json!([]));
    assert!(m["entries"].as_array().unwrap().iter().all(|x| x["name"] != l["name"]), "{m}");
}

fn sha2_digest(b: &[u8]) -> impl std::fmt::LowerHex {
    use sha2::Digest;
    sha2::Sha256::digest(b)
}

#[test]
fn create_description_is_a_draft_until_activated() {
    let env = Env::new();
    let d = env.run(&[
        "create", "ledger-test-gate", "--from", "description", "--description", "Run the ledger tests before committing payment changes",
        "--editor", &script("editor-ok.sh"),
    ]);
    assert_eq!(d["outcome"], "staged", "{d}");
    assert_eq!(d["status"], "draft");
    let id = d["id"].as_str().unwrap().to_string();
    let draft = env.root.join("drafts/ledger-test-gate");
    let skill = std::fs::read_to_string(draft.join("SKILL.md")).unwrap();
    assert!(skill.contains("name: ledger-test-gate"), "user's name wins: {skill}");
    let m = env.run(&["match", "--cwd", env.repo.to_str().unwrap()]);
    assert_eq!(m["skill_dirs"], json!([]), "drafts never load");
    let (v, code) = env.run_cmd(env.cmd(&["activate", "ledger-test-gate"]));
    assert_eq!(code, 2, "needs --task or --unverified: {v}");
    let a = env.run(&["activate", "ledger-test-gate", "--unverified"]);
    assert_eq!(a["entry"]["status"], "provisional");
    assert!(env.root.join("staging/payments/ledger-test-gate/SKILL.md").is_file());
    assert!(!draft.exists());
    assert_eq!(env.entry(&id)["status"], "provisional");
}

#[test]
fn create_from_sessions_needs_three_references() {
    let env = Env::new();
    for i in 0..2 {
        env.write_skill_trace(&format!("ref-{i}"), "unrelated", "Reconcile the ledger balances for the payments service", None);
    }
    env.run(&["count"]);
    let (v, code) = env.run_cmd(env.cmd(&[
        "create", "ledger-reconcile", "--from", "sessions", "--description", "reconcile ledger balances", "--editor", &script("editor-ok.sh"),
    ]));
    assert_eq!(code, 1);
    assert!(v["error"].as_str().unwrap().contains("found 2 matching session"), "{v}");
    env.write_skill_trace("ref-2", "unrelated", "Reconcile ledger balances after the import", Some("No, the balances must match the bank export"));
    env.run(&["count", "--editor", &script("editor-ok.sh")]);
    let d = env.run(&[
        "create", "ledger-reconcile", "--from", "sessions", "--description", "reconcile ledger balances", "--editor", &script("editor-ok.sh"),
    ]);
    assert_eq!(d["outcome"], "staged", "{d}");
    assert_eq!(d["references"].as_array().unwrap().len(), 3);
    assert!(env.root.join("staging/payments/ledger-reconcile/SKILL.md").is_file());
    let e = env.entry(d["id"].as_str().unwrap());
    assert_eq!(e["evidence"], 3, "one evidence moment per reference: {e}");
    assert!(matches!(e["origin"]["kind"].as_str(), Some("session")));
    // --from task without a defined task fails through the batch slice.
    let (v, code) = env.run_cmd(env.cmd(&["create", "other-skill", "--from", "task", "--task", "no-such-task"]));
    assert_eq!(code, 1, "{v}");
}

#[test]
fn improve_refuses_splits_and_proposes_a_sibling() {
    let env = Env::new();
    // Large skill, no sessions: refusal offers the split; --split-only stages it beside.
    let big = env.agent.join("skills/fpw");
    std::fs::create_dir_all(&big).unwrap();
    let md = std::fs::read_to_string(repo_root().join("crates/rematch-core/tests/fixtures/forge-portfolio-weighting/SKILL.md")).unwrap();
    std::fs::write(big.join("SKILL.md"), md.replace("name: forge-portfolio-weighting", "name: fpw")).unwrap();
    let r = env.run(&["improve", "fpw"]);
    assert_eq!(r["refused"], true, "{r}");
    assert_eq!(r["traced_sessions"], 0);
    assert_eq!(r["offer"], "split");
    let s = env.run(&["improve", "fpw", "--split-only"]);
    assert!(s["split"] == true || s["lint"].is_object(), "{s}");
    if s["split"] == true {
        let staged = PathBuf::from(s["staged"]["path"].as_str().unwrap());
        assert!(staged.join("SKILL.md").is_file());
        assert!(staged.join("units").is_dir(), "{s}");
        assert_eq!(s["staged"]["sibling_of"], s["entry"]);
    }
    assert!(big.join("SKILL.md").is_file(), "original stays in place");

    // Small skill read in 5 traced sessions with redirects: unit-level sibling.
    let small = env.agent.join("skills/ledger-helper");
    std::fs::create_dir_all(&small).unwrap();
    std::fs::write(small.join("SKILL.md"), "---\nname: ledger-helper\ndescription: Use when editing the ledger module.\n---\n\nUse when editing the ledger module.\n").unwrap();
    for i in 0..5 {
        env.write_skill_trace(&format!("lh-{i}"), "ledger-helper", "Update the ledger module rounding", Some("No, run the ledger tests first"));
    }
    env.run(&["count", "--editor", &script("editor-ok.sh")]);
    assert!(big.join("SKILL.md").is_file(), "count passes never move a registered skill");
    assert_eq!(env.entry(r["entry"].as_str().unwrap())["path"], json!(big.canonicalize().unwrap()));
    let u = env.run(&["improve", "ledger-helper", "--editor", &script("editor-ok.sh")]);
    assert_eq!(u["outcome"], "staged", "{u}");
    assert_eq!(u["audit"]["traced"], 5);
    let sib = env.entry(u["staged"]["id"].as_str().unwrap());
    assert_eq!(sib["name"], "ledger-helper");
    assert_eq!(sib["status"], "provisional");
    assert!(PathBuf::from(u["staged"]["path"].as_str().unwrap()).join("SKILL.md").is_file());
    let base = env.entry(u["entry"].as_str().unwrap());
    assert_eq!(base["origin"], json!({"kind": "import", "source": "brownfield"}));
    assert_eq!(base["path"], json!(small.canonicalize().unwrap()));

    // Pinned skills are never replaced.
    env.run(&["library", "pin", u["entry"].as_str().unwrap()]);
    let (v, code) = env.run_cmd(env.cmd(&["improve", "ledger-helper", "--editor", &script("editor-ok.sh")]));
    assert_eq!(code, 1);
    assert!(v["error"].as_str().unwrap().contains("pinned"), "{v}");
}

#[test]
fn counters_keep_a_registered_skill_without_moving_it() {
    let env = Env::new();
    let dir = env.agent.join("skills/ledger-helper");
    std::fs::create_dir_all(&dir).unwrap();
    let md = "---\nname: ledger-helper\ndescription: Use when editing the ledger module.\n---\n\nUse when editing the ledger module.\n";
    std::fs::write(dir.join("SKILL.md"), md).unwrap();
    let r = env.run(&["improve", "ledger-helper"]);
    assert_eq!(r["refused"], true);
    let id = r["entry"].as_str().unwrap().to_string();
    for i in 0..4 {
        env.write_records(
            &format!("k-{i}"),
            &[
                json!({"seq":0,"t":3_000 + i,"ev":"session_start","session_id":format!("k-{i}"),"cwd":env.repo,"repo":env.repo,
                       "model":{"provider":"anthropic","id":"claude-opus-5-5"},"loaded_entries":[id]}),
                json!({"seq":1,"t":3_001 + i,"ev":"input","text":"Edit the ledger module rounding helper","idle":true,"prelabel":"none"}),
                json!({"seq":2,"t":3_002 + i,"ev":"turn_end","usage":{"input":10,"output":5},"stop_reason":"stop","text_head":"Done."}),
                json!({"seq":3,"t":3_003 + i,"ev":"session_shutdown","reasons":[],"flagged":false}),
            ],
        );
    }
    let c = env.run(&["count", "--editor", &script("editor-ok.sh")]);
    assert_eq!(c["counter_rows"], 4, "{c}");
    let d = c["decisions"].as_array().unwrap().iter().find(|d| d["entry"] == id.as_str()).expect("decision").clone();
    assert_eq!(d["decision"], "keep", "{c}");
    assert_eq!(d["experience"]["source"], "reflector");
    let e = env.entry(&id);
    assert_eq!(e["status"], "kept");
    assert_eq!(e["path"], json!(dir.canonicalize().unwrap()), "files left in place");
    assert_eq!(std::fs::read_to_string(dir.join("SKILL.md")).unwrap(), md, "SKILL.md untouched");
    assert!(!dir.join("rematch.json").exists() && !dir.join("EXPERIENCE.md").exists());
    let x = std::fs::read_to_string(env.root.join("brownfield").join(&id).join("EXPERIENCE.md")).unwrap();
    assert!(x.contains("Run the crate's tests"), "{x}");
    assert!(x.contains("matched 4 · recurrence 0 · harm 0"), "{x}");
}

#[test]
fn scripts_wait_for_the_one_tap_approval() {
    let env = Env::new();
    let d = env.run(&["create", "crate-test-gate", "--from", "description", "--description", "test gate for crates", "--editor", &script("editor-script.sh")]);
    assert_eq!(d["outcome"], "staged", "{d}");
    let id = d["id"].as_str().unwrap().to_string();
    env.run(&["activate", "crate-test-gate", "--unverified"]);
    let script_path = env.root.join("staging/payments/crate-test-gate/scripts/check.sh");
    assert_eq!(std::fs::metadata(&script_path).unwrap().permissions().mode() & 0o777, 0o644);
    let g = env.run(&["digest", "--cwd", env.repo.to_str().unwrap()]);
    let a = g["approvals"].as_array().unwrap().iter().find(|a| a["id"] == id.as_str()).expect("approval offered").clone();
    assert_eq!(a["kind"], "scripts");
    assert!(a["diff"].as_str().unwrap().contains("cargo test"), "{a}");
    let r = env.run(&["library", "approve", &id]);
    assert_eq!(r["receipt"]["entry"], id.as_str());
    assert_eq!(std::fs::metadata(&script_path).unwrap().permissions().mode() & 0o777, 0o755);
    let g2 = env.run(&["digest", "--cwd", env.repo.to_str().unwrap()]);
    assert!(g2["approvals"].as_array().unwrap().iter().all(|a| a["id"] != id.as_str()));
}

#[test]
fn detached_reflect_reports_through_status() {
    let env = Env::new();
    env.write_trace("0192c4a0-0000-7000-8000-0000000000e1", &[], true);
    let d = env.run(&["reflect", "--pending", "--detach", "--editor", &script("editor-ok.sh")]);
    let job = d["job_id"].as_str().unwrap().to_string();
    let t = Instant::now();
    let st = loop {
        let s = env.run(&["status", &job]);
        if s["status"] != "running" && s["status"] != "starting" {
            break s;
        }
        assert!(t.elapsed() < Duration::from_secs(30), "job never finished: {s}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(st["status"], "done", "{st}");
    assert_eq!(st["kind"], "reflect");
    assert_eq!(st["result"]["data"]["reflections"][0]["outcome"], "staged", "{st}");
    assert!(env.root.join(format!("jobs/{job}.log")).is_file());
    assert!(env.staged_dir().join("SKILL.md").is_file());
    let all = env.run(&["status"]);
    assert!(all["jobs"].as_array().unwrap().iter().any(|j| j["id"] == job.as_str()));
}

#[test]
fn doctor_reports_omp_roles_and_link() {
    let env = Env::new();
    let mut c = env.cmd(&["doctor"]);
    c.env("OMP_BIN", script("fake-omp.sh"));
    let (v, code) = env.run_cmd(c);
    assert_eq!(code, 0);
    let d = &v["data"];
    assert_eq!(d["omp"]["version"], "omp/0.0.0-fake");
    assert_eq!(d["roles"]["rematch-editor"]["selector"], "anthropic/claude-opus-5-5");
    assert_eq!(d["roles"]["rematch-editor"]["via"], "default");
    assert_eq!(d["roles"]["rematch-classifier"]["via"], "smol");
    assert_eq!(d["extension"]["linked"], false);
    assert_eq!(d["ledger"]["schema_version"], 1);
    assert!(d["improver"]["files"].as_array().unwrap().iter().all(|f| f["present"] == true));
    std::fs::create_dir_all(env.home.join(".omp/plugins")).unwrap();
    std::fs::write(env.home.join(".omp/plugins/omp-plugins.lock.json"), r#"{"plugins":{"@vncp/rematch":{"enabled":true}}}"#).unwrap();
    let mut c = env.cmd(&["doctor"]);
    c.env("OMP_BIN", script("fake-omp.sh"));
    let (v, _) = env.run_cmd(c);
    assert_eq!(v["data"]["extension"]["linked"], true);
    assert_eq!(v["data"]["healthy"], true, "{v}");
}

#[test]
fn uninstall_prints_the_plan_then_removes_the_root() {
    let env = Env::new();
    env.run(&["doctor"]);
    let p = env.run(&["uninstall"]);
    assert_eq!(p["applied"], false);
    assert!(p["plan"]["remove"].as_array().unwrap().iter().any(|x| x == &json!(env.root)), "{p}");
    assert!(env.root.exists());
    let y = env.run(&["uninstall", "--yes"]);
    assert_eq!(y["applied"], true);
    assert!(!env.root.exists());
}
