#![cfg_attr(not(unix), allow(dead_code, unused_imports))] // helpers of Unix-only tests
//! End-to-end for the shared skills dir (DESIGN §16 "Shared skills"): `skills sync --cwd`
//! adds the provisional entries a cwd matches and never drops one because another cwd does
//! not match it; status changes re-point (keep) or prune (retire) at once; the kill switch
//! empties the dir.

mod common;

use common::*;
use serde_json::json;

#[cfg(unix)] // POSIX sh fixtures
#[test]
fn union_keep_retire_and_off() {
    let env = Env::new();
    let repo = env.repo.to_str().unwrap().to_string();
    let elsewhere = env.home.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let shared = env.root.join("skills");

    // Stage in the payments branch: nothing is linked until a matching cwd syncs.
    env.write_trace("0192c4a0-0000-7000-8000-0000000000f0", &[], true);
    let d = env.run(&["reflect", "--pending", "--editor", &script("editor-ok.sh")]);
    let id = d["reflections"][0]["id"].as_str().unwrap().to_string();
    let link = shared.join("run-tests-before-commit");
    assert!(std::fs::symlink_metadata(&link).is_err(), "{d}");

    let a = env.run(&["skills", "sync", "--cwd", &repo]);
    assert_eq!((a["added"].clone(), a["total"].clone(), a["dirs"].clone()), (json!(["run-tests-before-commit"]), json!(1), json!([shared])), "{a}");
    assert_eq!(std::fs::read_link(&link).unwrap(), env.staged_dir());

    // Another cwd matches nothing: the link stays (a session in the repo may be using it).
    let b = env.run(&["skills", "sync", "--cwd", elsewhere.to_str().unwrap(), "--harness", "omp"]);
    assert_eq!((b["added"].clone(), b["pruned"].clone(), b["total"].clone()), (json!([]), json!([]), json!(1)), "{b}");
    assert_eq!(std::fs::read_link(&link).unwrap(), env.staged_dir());

    // Keep (four clean matched sessions): the link follows the entry to its kept dir at once.
    for i in 1..=4 {
        env.write_trace(&format!("0192c4a0-0000-7000-8000-0000000000f{i}"), &[&id], false);
    }
    let c = env.run(&["count", "--editor", &script("editor-ok.sh")]);
    assert_eq!(c["decisions"][0]["decision"], "keep", "{c}");
    assert_eq!(std::fs::read_link(&link).unwrap(), env.agent.join("skills/run-tests-before-commit"));

    // A second provisional entry: linked for its cwd, pruned when retired.
    env.run(&["create", "ledger-test-gate", "--from", "description", "--description", "Run the ledger tests before committing payment changes", "--editor", &script("editor-ok.sh")]);
    env.run(&["activate", "ledger-test-gate", "--unverified"]);
    let gate = shared.join("ledger-test-gate");
    assert!(std::fs::symlink_metadata(&gate).is_err(), "activation matched no cwd");
    env.run(&["skills", "sync", "--cwd", &repo]);
    assert_eq!(std::fs::read_link(&gate).unwrap(), env.root.join("staging/payments/ledger-test-gate"));
    let gate_id = env.run(&["library", "ls"])["entries"].as_array().unwrap().iter().find(|e| e["name"] == "ledger-test-gate").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    env.run(&["library", "disable", &gate_id]);
    assert!(std::fs::symlink_metadata(&gate).is_err(), "retired → pruned");
    assert!(link.join("SKILL.md").is_file(), "the kept link stays");

    // Off empties the dir at once; on restores the kept link.
    env.run(&["off"]);
    assert!(std::fs::symlink_metadata(&link).is_err());
    let off = env.run(&["skills", "sync", "--cwd", &repo]);
    assert_eq!((off["off"].clone(), off["dirs"].clone(), off["total"].clone()), (json!(true), json!([]), json!(0)), "{off}");
    env.run(&["on"]);
    assert_eq!(std::fs::read_link(&link).unwrap(), env.agent.join("skills/run-tests-before-commit"));
}
