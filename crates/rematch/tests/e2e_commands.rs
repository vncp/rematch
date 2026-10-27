// POSIX-only: sh fixtures, Unix file modes and unprivileged symlinks.
#![cfg(unix)]
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
    rematch_core::paths::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
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

#[test]
fn import_forge_migrate_copies_skills_into_staging_so_the_forge_can_go() {
    let env = Env::new();
    let forge = env.home.join("compound-forge");
    let ux = forge.join("skills/forge-ux");
    std::fs::create_dir_all(ux.join("scripts")).unwrap();
    std::fs::create_dir_all(ux.join("references")).unwrap();
    std::fs::write(ux.join("SKILL.md"), "---\nname: forge-ux\ndescription: UI gates. Use when reviewing user interfaces.\n---\n# forge-ux\n\nRun scripts/check.py.\n").unwrap();
    std::fs::write(ux.join("scripts/check.py"), "print('ok')\n").unwrap();
    std::fs::write(ux.join("references/gates.md"), "# gates\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(ux.join("scripts/check.py"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let meta = forge.join("skills/compound-forge");
    std::fs::create_dir_all(&meta).unwrap();
    std::fs::write(meta.join("SKILL.md"), "---\nname: compound-forge\ndescription: Run the forge tool.\n---\n# meta\n").unwrap();
    let before = listing(&forge);

    // Registered in place first (the common upgrade path), then migrated.
    env.run(&["import", "--forge", forge.to_str().unwrap()]);
    let d = env.run(&["import", "--forge", forge.to_str().unwrap(), "--migrate"]);
    let f = &d["forge"];
    assert_eq!(f["skills_registered"], 1, "{d}");
    assert!(f["skipped"].as_array().unwrap().iter().any(|s| s["name"] == "compound-forge"), "meta-skill not migrated: {d}");
    assert_eq!(listing(&forge), before, "forge checkout untouched");
    let staged = env.root.join("staging/forge/forge-ux");
    assert!(staged.join("SKILL.md").is_file() && staged.join("references/gates.md").is_file(), "tree copied");
    let mode = std::fs::metadata(staged.join("scripts/check.py")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644, "scripts staged non-executable until approved");
    assert!(std::fs::read_to_string(staged.join("SKILL.md")).unwrap().contains("rematch:"), "frontmatter block written");
    let e = env.entry(f["skills"][0]["id"].as_str().unwrap());
    assert_eq!(e["path"], json!(staged));
    assert_eq!(e["status"], "provisional");

    // Now the forge is gone: the skill still loads from staging for any repo.
    std::fs::remove_dir_all(&forge).unwrap();
    let m = env.run(&["match", "--cwd", env.repo.to_str().unwrap()]);
    assert_eq!(m["skill_dirs"], json!([env.root.join("staging/forge")]));
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

/// `create --from sessions --ran`: the references are the sessions whose bash commands have
/// every word, from the ledger's records, then from the lake's `events` (`text:tool_cmd`; a
/// scan without an index) once it is exported.
#[cfg(feature = "lake")]
#[test]
fn create_from_sessions_ran_picks_the_sessions_that_ran_the_command() {
    let env = Env::new();
    let repo = env.repo.to_string_lossy().to_string();
    for (i, cmd) in ["cd x && cargo test --workspace", "CARGO TEST -p ledger", "cargo build", "cargo   test", "npm test"].iter().enumerate() {
        let sid = format!("ran-{i}");
        env.write_records(
            &sid,
            &[
                json!({"seq":0,"t":4_000 + i,"ev":"session_start","session_id":sid,"cwd":repo,"repo":repo,"loaded_entries":[]}),
                json!({"seq":1,"t":4_001 + i,"ev":"input","text":"Fix the parser","idle":true,"prelabel":"none"}),
                json!({"seq":2,"t":4_002 + i,"ev":"tool_result","tool":"bash","call_id":"c1","input":{"command":cmd},"is_error":false}),
                json!({"seq":3,"t":4_003 + i,"ev":"session_shutdown","reasons":[],"flagged":false}),
            ],
        );
    }
    env.run(&["count"]);
    let create = |extra: &[&str]| {
        let mut a = vec!["create", "cargo-test-gate", "--from", "sessions", "--description", "gate on tests", "--editor"];
        let ed = script("editor-ok.sh");
        a.push(&ed);
        a.extend_from_slice(extra);
        env.run_cmd(env.cmd(&a))
    };
    let (v, code) = create(&["--ran", "cargo,test,nowhere"]);
    assert_eq!(code, 1, "{v}");
    assert!(v["error"].as_str().unwrap().contains("found 0 matching session(s) that ran cargo test nowhere in the ledger"), "{v}");
    let (v, code) = create(&["--ran", "&&"]);
    assert_eq!(code, 2, "no word: a usage error: {v}");

    env.run(&["lake", "export", "--all"]);
    let (v, code) = create(&["--ran", "Cargo,test"]);
    assert_eq!(code, 0, "{v}");
    let refs: std::collections::BTreeSet<&str> = v["data"]["references"].as_array().unwrap().iter().map(|r| r.as_str().unwrap()).collect();
    assert_eq!(refs, ["ran-0", "ran-1", "ran-3"].into(), "{v}");
    assert_eq!((v["data"]["source"].clone(), v["data"]["ran"].clone()), (json!("lake"), json!(["cargo", "test"])), "{v}");
}

/// The same `--ran` from the ledger alone (no lake).
#[test]
fn create_from_sessions_ran_reads_the_ledger_without_a_lake() {
    let env = Env::new();
    let repo = env.repo.to_string_lossy().to_string();
    for (i, cmd) in ["cd x && cargo test --workspace", "cargo test", "cargo test --doc", "cargo build"].iter().enumerate() {
        let sid = format!("led-{i}");
        env.write_records(
            &sid,
            &[
                json!({"seq":0,"t":5_000 + i,"ev":"session_start","session_id":sid,"cwd":repo,"repo":repo,"loaded_entries":[]}),
                json!({"seq":1,"t":5_001 + i,"ev":"tool_result","tool":"bash","call_id":"c1","input":{"command":cmd},"is_error":false}),
                json!({"seq":2,"t":5_002 + i,"ev":"session_shutdown","reasons":[],"flagged":false}),
            ],
        );
    }
    env.run(&["count"]);
    let d = env.run(&["create", "cargo-test-gate", "--from", "sessions", "--ran", "cargo,test", "--editor", &script("editor-ok.sh")]);
    assert_eq!((d["outcome"].clone(), d["source"].clone()), (json!("staged"), json!("ledger")), "{d}");
    let refs: std::collections::BTreeSet<&str> = d["references"].as_array().unwrap().iter().map(|r| r.as_str().unwrap()).collect();
    assert_eq!(refs, ["led-0", "led-1", "led-2"].into(), "{d}");
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
    assert_eq!(env.entry(r["entry"].as_str().unwrap())["path"], json!(rematch_core::paths::canonicalize(&big).unwrap()));
    let u = env.run(&["improve", "ledger-helper", "--editor", &script("editor-ok.sh")]);
    assert_eq!(u["outcome"], "staged", "{u}");
    assert_eq!(u["audit"]["traced"], 5);
    let sib = env.entry(u["staged"]["id"].as_str().unwrap());
    assert_eq!(sib["name"], "ledger-helper");
    assert_eq!(sib["status"], "provisional");
    assert!(PathBuf::from(u["staged"]["path"].as_str().unwrap()).join("SKILL.md").is_file());
    let base = env.entry(u["entry"].as_str().unwrap());
    assert_eq!(base["origin"], json!({"kind": "import", "source": "brownfield"}));
    assert_eq!(base["path"], json!(rematch_core::paths::canonicalize(&small).unwrap()));

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
                // Actual use: the skill body is read (a session that only listed it earns nothing).
                json!({"seq":2,"t":3_002 + i,"ev":"tool_result","tool":"read","call_id":"r","input":{"path":"skill://ledger-helper"},
                       "is_error":false,"skill_read":"ledger-helper"}),
                json!({"seq":3,"t":3_002 + i,"ev":"turn_end","usage":{"input":10,"output":5},"stop_reason":"stop","text_head":"Done."}),
                json!({"seq":4,"t":3_003 + i,"ev":"agent_end","turns":1,"is_terminal":true,"yielded":true}),
                json!({"seq":5,"t":3_003 + i,"ev":"session_shutdown","reasons":[],"flagged":false}),
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
    assert_eq!(e["path"], json!(rematch_core::paths::canonicalize(&dir).unwrap()), "files left in place");
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
    let r = env.approve(&id);
    assert_eq!((r["receipts"][0]["entry"].as_str(), r["approved"].clone()), (Some(id.as_str()), json!(["scripts"])), "{r}");
    assert_eq!(std::fs::metadata(&script_path).unwrap().permissions().mode() & 0o777, 0o755);
    let g2 = env.run(&["digest", "--cwd", env.repo.to_str().unwrap()]);
    assert!(g2["approvals"].as_array().unwrap().iter().all(|a| a["id"] != id.as_str()));
}

#[test]
fn disabling_a_foreign_entry_removes_its_compiled_hook() {
    let env = Env::new();
    let dir = env.agent.join("skills/ledger-helper");
    std::fs::create_dir_all(&dir).unwrap();
    let md = "---\nname: ledger-helper\ndescription: Use when editing the ledger module.\n---\n\nUse when editing the ledger module.\n";
    std::fs::write(dir.join("SKILL.md"), md).unwrap();
    let id = env.run(&["improve", "ledger-helper"])["entry"].as_str().unwrap().to_string();
    // a module for that id (synced from another machine's profile, or left behind)
    let hook = env.agent.join("extensions/rematch-hooks").join(format!("{id}.ts"));
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "export default { id: \"x\", mode: \"block\" };\n").unwrap();
    let d = env.run(&["library", "disable", &id]);
    assert_eq!(d["entry"]["status"], "retired", "{d}");
    assert!(!hook.exists(), "the compiled hook goes with the entry");
    assert_eq!(std::fs::read_to_string(dir.join("SKILL.md")).unwrap(), md, "a foreign entry's files stay");
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

/// `match` stays on the session hot path with the lake linked in (ADR 001: the hot path never
/// touches the lake). The release target is ≤ 50 ms; this runs the debug binary, so the bound
/// is 100 ms on the best of five runs after a warm-up (process start dominates).
#[test]
fn match_stays_fast_on_a_seeded_root() {
    let env = Env::new();
    for i in 0..12 {
        env.write_trace(&format!("0192c4a0-0000-7000-8000-0000000001{i:02}"), &[], false);
    }
    env.run(&["count"]);
    let d = env.run(&[
        "create", "ledger-test-gate", "--from", "description", "--description", "Run the ledger tests before committing payment changes",
        "--editor", &script("editor-ok.sh"),
    ]);
    assert_eq!(d["outcome"], "staged", "{d}");
    env.run(&["activate", "ledger-test-gate", "--unverified"]);
    let cwd = env.repo.to_str().unwrap().to_string();
    let m = env.run(&["match", "--cwd", &cwd]);
    assert!(!m["entries"].as_array().unwrap().is_empty(), "seeded: {m}");
    let best = (0..5)
        .map(|_| {
            let t = Instant::now();
            env.run(&["match", "--cwd", &cwd]);
            t.elapsed()
        })
        .min()
        .unwrap();
    assert!(best <= Duration::from_millis(100), "match took {best:?} (debug bound 100 ms; release target 50 ms)");
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
    assert_eq!(d["ledger"]["schema_version"], rematch_core::ledger::SCHEMA_VERSION);
    assert!(d["improver"]["files"].as_array().unwrap().iter().all(|f| f["present"] == true));
    std::fs::create_dir_all(env.home.join(".omp/plugins")).unwrap();
    std::fs::write(env.home.join(".omp/plugins/omp-plugins.lock.json"), r#"{"plugins":{"@vncp/rematch":{"enabled":true}}}"#).unwrap();
    let doctor = || {
        let mut c = env.cmd(&["doctor"]);
        c.env("OMP_BIN", script("fake-omp.sh"));
        env.run_cmd(c).0
    };
    let v = doctor();
    assert_eq!((v["data"]["extension"]["linked"].clone(), v["data"]["extension"]["loads"].clone()), (json!(true), json!(false)), "{v}");
    assert_eq!(v["data"]["ready"], false, "listed in the lock, no module: it cannot load: {v}");
    let modules = env.home.join(".omp/plugins/node_modules/@vncp");
    std::fs::create_dir_all(&modules).unwrap();
    std::os::unix::fs::symlink(repo_root().join("extension"), modules.join("rematch")).unwrap();
    let v = doctor();
    assert_eq!(v["data"]["harnesses"]["omp"]["extension"]["entries"], json!(["./index.ts"]), "{v}");
    assert_eq!(v["data"]["ready"], true, "{v}");
}

/// `doctor`'s harness block: each harness set up in turn (Claude Code's plugin link with a
/// scripted `claude plugin validate`, Codex's skill-root link, a profile with a launchd job,
/// the lake), a broken one making `ready: false`, and one text line per harness.
#[test]
fn doctor_reports_every_harness_and_ready() {
    let env = Env::new();
    let modules = env.home.join(".omp/plugins/node_modules/@vncp");
    std::fs::create_dir_all(&modules).unwrap();
    std::os::unix::fs::symlink(repo_root().join("extension"), modules.join("rematch")).unwrap();
    let bin = env.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = |name: &str, body: &str| {
        let p = bin.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        p
    };
    let claude_ok = fake("claude-ok", "echo 'Validation passed'");
    let claude_bad = fake("claude-bad", "echo 'x: invalid manifest' >&2; exit 1");
    let launchctl_no = fake("launchctl-no", "exit 113");
    let launchctl_yes = fake("launchctl-yes", "echo '{ \"Label\" = \"com.vncp.rematch.profile-sync\"; }'");
    let doctor = |claude: &Path, launchctl: &Path| {
        let mut c = env.cmd(&["doctor"]);
        c.env("OMP_BIN", script("fake-omp.sh")).env("CLAUDE_BIN", claude).env("LAUNCHCTL_BIN", launchctl).env_remove("REMATCH_BELLWETHER");
        let (v, code) = env.run_cmd(c);
        assert_eq!(code, 0, "{v}");
        v["data"].clone()
    };
    let text = |d: &Value| d["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect::<Vec<_>>();
    let status = |d: &Value, h: &str| d["harnesses"][h]["status"].as_str().unwrap().to_string();

    // Nothing but omp: the others are "not set up", never problems.
    let d = doctor(&claude_ok, &launchctl_yes);
    assert_eq!(d["ready"], true, "{d}");
    for (h, s) in [("omp", "ok"), ("claude-code", "not set up"), ("codex", "not set up"), ("pi", "not set up"), ("opencode", "not set up"), ("profile", "not set up"), ("lake", "not set up"), ("built-ins", "ok"), ("hooks", "not set up")] {
        assert_eq!(status(&d, h), s, "{h}: {d}");
    }
    let lines = text(&d);
    let firsts: Vec<&str> = lines.iter().take(9).map(|l| l.split(':').next().unwrap()).collect();
    assert_eq!(firsts, ["omp", "claude-code", "codex", "pi", "opencode", "profile", "lake", "built-ins", "hooks"], "one line per harness, first: {lines:#?}");
    assert_eq!(lines.last().unwrap(), "ready: true");

    // Claude Code's plugin, Codex's link, a profile, the lake.
    env.run(&["skills", "sync", "--cwd", env.repo.to_str().unwrap()]);
    let plugin = env.home.join("plugin");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(plugin.join("hooks")).unwrap();
    std::fs::write(plugin.join(".claude-plugin/plugin.json"), r#"{"name":"rematch"}"#).unwrap();
    std::fs::write(plugin.join("hooks/hooks.json"), r#"{"hooks":{}}"#).unwrap();
    std::os::unix::fs::symlink(env.root.join("skills"), plugin.join("skills")).unwrap();
    std::fs::create_dir_all(env.home.join(".claude/skills")).unwrap();
    std::os::unix::fs::symlink(&plugin, env.home.join(".claude/skills/rematch")).unwrap();
    std::fs::create_dir_all(env.home.join(".agents/skills")).unwrap();
    std::os::unix::fs::symlink(env.root.join("skills"), env.home.join(".agents/skills/rematch")).unwrap();
    env.run(&["profile", "init"]);
    env.run(&["profile", "install-launchd"]);
    #[cfg(feature = "lake")]
    env.run(&["lake", "export", "--all"]);
    let d = doctor(&claude_ok, &launchctl_yes);
    for h in ["omp", "claude-code", "codex", "profile", "built-ins"] {
        assert_eq!(status(&d, h), "ok", "{h}: {d}");
    }
    assert_eq!(d["harnesses"]["claude-code"]["validated"], true, "{d}");
    assert_eq!(d["harnesses"]["profile"]["launchd"]["loaded"], true, "{d}");
    #[cfg(feature = "lake")]
    assert_eq!((status(&d, "lake"), d["harnesses"]["lake"]["lag"].clone()), ("ok".to_string(), json!(0)), "{d}");
    assert_eq!(d["ready"], true, "{d}");
    eprintln!("doctor --text:\n{}", text(&d).join("\n"));

    // Broken: the plugin does not validate, the job is not loaded, Codex's link points elsewhere.
    std::fs::remove_file(env.home.join(".agents/skills/rematch")).unwrap();
    std::os::unix::fs::symlink(env.root.join("codex-skills"), env.home.join(".agents/skills/rematch")).unwrap();
    let d = doctor(&claude_bad, &launchctl_no);
    for h in ["claude-code", "codex", "profile"] {
        assert_eq!(status(&d, h), "problem", "{h}: {d}");
    }
    assert_eq!(d["ready"], false);
    let problems: Vec<&str> = d["problems"].as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect();
    assert_eq!(problems.len(), 3, "{problems:#?}");
    assert!(problems[0].contains("claude plugin validate") && problems[0].contains("invalid manifest"), "{problems:#?}");
    assert!(problems[2].contains("launchctl load -w"), "{problems:#?}");
    assert_eq!(text(&d).last().unwrap(), "ready: false (3 problem(s))");
}

/// `doctor`'s hooks block: a compiled module no live local entry owns is an orphan (a
/// problem); one that `profile/hooks/` also holds (approved on another machine) is
/// information only; an approved entry's module is live.
#[test]
fn doctor_reports_orphan_hook_modules() {
    let env = Env::new();
    let hooks = env.agent.join("extensions/rematch-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::create_dir_all(env.root.join("profile/hooks")).unwrap();
    let module = "export default { id: \"x\", mode: \"block\" };\n";
    std::fs::write(hooks.join("README.md"), "not a module\n").unwrap();
    std::fs::write(hooks.join("gone-entry.ts"), module).unwrap();
    std::fs::write(hooks.join("other-machine.ts"), module).unwrap();
    std::fs::write(env.root.join("profile/hooks/other-machine.ts"), module).unwrap();

    let d = env.run(&["doctor"]);
    let h = &d["harnesses"]["hooks"];
    assert_eq!(h["status"], "problem", "{d}");
    assert_eq!((h["live"].clone(), h["profile"].clone()), (json!([]), json!(["other-machine"])), "{h}");
    assert_eq!(h["orphan"], json!([hooks.join("gone-entry.ts")]), "{h}");
    let problems: Vec<&str> = d["problems"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    let want = format!("orphan hook module {}: no live entry; delete the file", hooks.join("gone-entry.ts").display());
    assert!(problems.contains(&want.as_str()), "{problems:#?}");
    assert!(!problems.iter().any(|p| p.contains("other-machine")), "a profile module is not a problem: {problems:#?}");

    // the orphan deleted and a built-in hook approved: live, profile, no problem
    std::fs::remove_file(hooks.join("gone-entry.ts")).unwrap();
    let g = env.run(&["digest", "--cwd", env.repo.to_str().unwrap()]);
    let stuck = g["approvals"].as_array().unwrap().iter().find(|a| a["name"] == "stuck-detector").expect("built-in offered")["id"].clone();
    env.approve(stuck.as_str().unwrap());
    let d = env.run(&["doctor"]);
    let h = &d["harnesses"]["hooks"];
    assert_eq!(h["status"], "ok", "{d}");
    assert_eq!((h["live"].clone(), h["profile"].clone(), h["orphan"].clone()), (json!([stuck]), json!(["other-machine"]), json!([])), "{h}");
    assert!(d["lines"].as_array().unwrap().iter().any(|l| l == "hooks: ok · 1 live · 1 from profile (inert here) · 0 orphan"), "{d}");
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

// ---- reflect --history / improve over imported omp sessions ----

#[test]
fn reflect_history_reflects_flagged_imported_sessions_once() {
    let env = Env::new();
    redirected_session(&env, "hist-new", "2026-10-20");
    redirected_session(&env, "hist-old", "2025-01-05");
    let d = env.run(&["import", "--history"]);
    assert_eq!(d["history"]["new"], 2, "{d}");
    assert_eq!(d["history"]["redirects"], 2, "{d}");

    // --since keeps the 2025 session out; the 2026 one is handed to the normal loop.
    let editor = script("editor-ok.sh");
    let r = env.run(&["reflect", "--history", "--since", "2026-01-01", "--editor", &editor]);
    let h = &r["history"];
    assert_eq!(h["handed"], json!(["hist-new"]), "{r}");
    assert_eq!(h["deferred"], 0);
    assert_eq!(h["sessions"][0]["outcome"], "staged", "{r}");
    assert_eq!(h["sessions"][0]["name"], "run-tests-before-commit");
    assert_eq!(r["count"]["folded"], json!(["hist-new"]), "{r}");
    assert!(env.calls().len() <= 3, "≤ 3 model calls per reflection: {:?}", env.calls());
    assert!(env.staged_dir().join("SKILL.md").is_file());
    assert!(!env.root.join("pending/hist-new.json").exists(), "pending consumed");
    let trace = std::fs::read_to_string(env.root.join("traces/hist-new.jsonl")).unwrap();
    let first: Value = serde_json::from_str(trace.lines().next().unwrap()).unwrap();
    assert_eq!((first["ev"].as_str(), first["mode"].as_str()), (Some("session_start"), Some("import")));
    let last: Value = serde_json::from_str(trace.lines().last().unwrap()).unwrap();
    assert_eq!(last["reasons"], json!(["redirect:negation"]), "{last}");

    // The ledger row stays the import's, counted once, with the redirect at its trace seq.
    let w = env.run(&["library", "why", "hist-new"]);
    assert_eq!(w["session"]["source"], "import", "{w}");
    assert_eq!(w["session"]["counted"], true);
    let red = &w["redirects"][0];
    assert_eq!(red["prelabel"], "negation", "{w}");
    let seq = red["seq"].as_u64().unwrap();
    let at: Value = serde_json::from_str(trace.lines().nth(seq as usize).unwrap()).unwrap();
    assert_eq!((at["ev"].as_str(), at["prelabel"].as_str()), (Some("input"), Some("negation")), "why's seq addresses the trace");
    assert!(w["lines"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("negation")), "{w}");

    // Idempotent: nothing to hand, nothing to count, no model call.
    let calls = env.calls().len();
    let r2 = env.run(&["reflect", "--history", "--since", "2026-01-01", "--editor", &editor]);
    assert_eq!(r2["history"]["handed"], json!([]), "{r2}");
    assert_eq!(r2["count"]["folded"], json!([]), "{r2}");
    assert_eq!(r2["reflections"], json!([]));
    assert_eq!(env.calls().len(), calls);

    // A re-import keeps the counted flag and the marker; still nothing to reflect.
    let d2 = env.run(&["import", "--history"]);
    assert_eq!(d2["history"]["updated"], 2, "{d2}");
    let w2 = env.run(&["library", "why", "hist-new"]);
    assert_eq!((w2["session"]["counted"].clone(), w2["redirects"][0]["seq"].clone()), (json!(true), json!(seq)), "{w2}");
    assert_eq!(env.run(&["count"])["folded"], json!([]));

    // --limit 0 defers the remaining flagged session instead of spending model calls.
    let r3 = env.run(&["reflect", "--history", "--limit", "0", "--editor", &editor]);
    assert_eq!((r3["history"]["handed"].clone(), r3["history"]["deferred"].clone()), (json!([]), json!(1)), "{r3}");
    assert_eq!(env.calls().len(), calls);
    let r4 = env.run(&["reflect", "--history", "--editor", &editor]);
    assert_eq!(r4["history"]["handed"], json!(["hist-old"]), "{r4}");
    // Rate limits apply to imported sessions as to live ones: same trigger predicate, pending canary.
    assert_eq!(r4["history"]["sessions"][0]["outcome"], "rate_limited", "{r4}");

    let (v, code) = env.run_cmd(env.cmd(&["reflect", "--history", "--since", "yesterday"]));
    assert_eq!(code, 2, "{v}");
}

#[test]
fn improve_builds_its_excerpt_from_imported_history() {
    let env = Env::new();
    let skill = env.agent.join("skills/ledger-helper");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: ledger-helper\ndescription: Use when editing the ledger module.\n---\n\nUse when editing the ledger module.\n").unwrap();
    let read = || omp_call("r", "read", json!({"path": "skill://ledger-helper"}));
    for i in 0..4 {
        write_omp_session(&env, &format!("imp-{i}"), &format!("2026-10-1{i}"), &[
            omp_user("Update the ledger module rounding"),
            omp_assistant("", vec![read()], "toolUse"),
            omp_result("r", "read", "skill body", false),
            omp_assistant("Done.", vec![], "stop"),
        ]);
    }
    // The newest session: three failing test runs in a row (a tool-error cluster), no redirect.
    let bash = |id: &str| omp_call(id, "bash", json!({"command": "cargo test -p ledger"}));
    write_omp_session(&env, "imp-err", "2026-10-19", &[
        omp_user("Update the ledger module rounding"),
        omp_assistant("", vec![read()], "toolUse"),
        omp_result("r", "read", "skill body", false),
        omp_assistant("", vec![bash("b1"), bash("b2"), bash("b3")], "toolUse"),
        omp_result("b1", "bash", "error[E0425]: cannot find value `rounding`", true),
        omp_result("b2", "bash", "error[E0425]: cannot find value `rounding`", true),
        omp_result("b3", "bash", "error[E0425]: cannot find value `rounding`", true),
        omp_assistant("The build fails.", vec![], "stop"),
    ]);
    env.run(&["import", "--history"]);
    assert!(!env.root.join("traces").exists() || std::fs::read_dir(env.root.join("traces")).unwrap().next().is_none(), "no traces: imported only");

    let dump = env.root.parent().unwrap().join("editor-input.txt");
    let editor = format!("tee -a '{}' | sh '{}'", dump.display(), script("editor-ok.sh"));
    let u = env.run(&["improve", "ledger-helper", "--editor", &editor]);
    assert_eq!(u["audit"]["imported"], 5, "{u}");
    assert_eq!(u["audit"]["error_clusters"], 1, "{u}");
    let text = std::fs::read_to_string(&dump).unwrap();
    let body = &text[text.find("<input>").unwrap() + 7..text.find("</input>").unwrap()];
    let input: Value = serde_json::from_str(body.trim()).unwrap();
    let excerpt = input["trace_excerpt"].as_array().unwrap();
    assert!(!excerpt.is_empty(), "excerpt from the imported session: {input}");
    assert!(excerpt.iter().any(|r| r["ev"] == "tool_result" && r["is_error"] == true && r["tool"] == "bash"), "{input}");
    let clusters = input["clusters"].as_array().unwrap();
    assert!(clusters.iter().any(|c| c["class"] == "error_cluster" && c["stems"] == "bash" && c["count"] == 1), "{input}");
    assert_eq!(input["redirect"]["class"], "error_cluster", "{input}");
}

#[test]
fn import_history_and_doctor_across_profiles_with_skip() {
    let env = Env::new();
    let omp_root = env.home.join(".omp");
    let default_agent = omp_root.join("agent");
    let work_agent = omp_root.join("profiles/work/agent");
    let skip_agent = omp_root.join("profiles/sandbox/agent");
    for dir in [&default_agent, &work_agent, &skip_agent] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("config.yml"),
            "modelRoles:\n  smol: fakeprov/small-model\n  default: anthropic/claude-opus-5-5:high\n",
        )
        .unwrap();
    }
    let msg = &[omp_user("Fix the payments rounding"), omp_assistant("Fixed.", vec![], "stop")];
    write_omp_session_in(&default_agent, &env.repo, "sess-default", "2026-10-10", msg);
    write_omp_session_in(&work_agent, &env.repo, "sess-work-1", "2026-10-11", msg);
    write_omp_session_in(&work_agent, &env.repo, "sess-work-2", "2026-10-12", msg);
    write_omp_session_in(&skip_agent, &env.repo, "sess-sandbox", "2026-10-13", msg);

    // 1. CLI --skip-profile skips `sandbox` while importing `default` and `work`.
    let mut c = env.cmd(&["import", "--history", "--skip-profile", "sandbox"]);
    c.env_remove("PI_CODING_AGENT_DIR");
    let (v, code) = env.run_cmd(c);
    assert_eq!(code, 0, "{v}");
    let h = &v["data"]["history"];
    assert_eq!(h["files"], 3, "{h}");
    assert_eq!(h["new"], 3, "{h}");
    assert_eq!(h["profiles"]["default"]["files"], 1, "{h}");
    assert_eq!(h["profiles"]["default"]["new"], 1, "{h}");
    assert_eq!(h["profiles"]["work"]["files"], 2, "{h}");
    assert_eq!(h["profiles"]["work"]["new"], 2, "{h}");
    assert!(h["profiles"].get("sandbox").is_none(), "{h}");
    assert_eq!(h["skipped_profiles"], json!(["sandbox"]), "{h}");

    // 2. policy.json `skip_profiles` also skips `sandbox` without the CLI flag, and doctor reports it.
    std::fs::write(env.root.join("policy.json"), r#"{"skip_profiles":["sandbox"]}"#).unwrap();
    let mut c2 = env.cmd(&["import", "--history"]);
    c2.env_remove("PI_CODING_AGENT_DIR").env("OMP_PROFILE", "work");
    let (v2, code2) = env.run_cmd(c2);
    assert_eq!(code2, 0, "{v2}");
    let h2 = &v2["data"]["history"];
    assert_eq!(h2["files"], 3, "{h2}");
    assert_eq!(h2["new"], 0, "{h2}");
    assert_eq!(h2["updated"], 3, "{h2}");
    assert_eq!(h2["skipped_profiles"], json!(["sandbox"]), "{h2}");

    // 3. Doctor under OMP_PROFILE=work resolves work's agent_dir, lists existing and skipped profiles.
    let mut doc = env.cmd(&["doctor"]);
    doc.env("OMP_BIN", script("fake-omp.sh"))
        .env("OMP_PROFILE", "work")
        .env("PI_CODING_AGENT_DIR", "/ignored/under/named/profile");
    let (dv, dcode) = env.run_cmd(doc);
    assert_eq!(dcode, 0, "{dv}");
    let dd = &dv["data"];
    assert_eq!(dd["paths"]["agent_dir"], json!(work_agent), "{dd}");
    assert_eq!(dd["paths"]["root"], json!(env.root), "{dd}");
    assert_eq!(dd["profiles"]["active"], "work", "{dd}");
    assert_eq!(dd["profiles"]["existing"], json!(["sandbox", "work"]), "{dd}");
    assert_eq!(dd["profiles"]["skipped"], json!(["sandbox"]), "{dd}");
    assert!(
        dd["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap().contains("profile: work") && l.as_str().unwrap().contains("skipped: sandbox")),
        "{dd}"
    );
}
