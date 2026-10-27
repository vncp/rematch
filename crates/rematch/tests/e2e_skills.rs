#![cfg_attr(not(unix), allow(dead_code, unused_imports))] // helpers of Unix-only tests
//! End-to-end for scoped exposure (DESIGN §16 "Shared skills"): `REMATCH_ROOT/skills/` is the
//! kept library every session loads; a provisional entry is linked only in the view of the
//! places its predicate matches (`views/<key>/skills/`), so two repos' sessions never list each
//! other's trials and no sync of one place prunes another's; status changes re-point or prune
//! at once; the kill switch empties the shared dir and every view.

mod common;

use common::*;
use serde_json::json;

#[cfg(unix)] // POSIX sh fixtures
#[test]
fn views_scope_trials_and_the_kept_library_is_shared() {
    let env = Env::new();
    let repo = env.repo.to_str().unwrap().to_string();
    let elsewhere = env.home.join("elsewhere");
    std::fs::create_dir_all(elsewhere.join(".git")).unwrap();
    let shared = env.root.join("skills");

    // Stage in the payments branch: nothing is linked until a matching cwd syncs.
    env.write_trace("0192c4a0-0000-7000-8000-0000000000f0", &[], true);
    let d = env.run(&["reflect", "--pending", "--editor", &script("editor-ok.sh")]);
    let id = d["reflections"][0]["id"].as_str().unwrap().to_string();
    let name = "run-tests-before-commit";
    assert!(std::fs::symlink_metadata(shared.join(name)).is_err(), "{d}");

    // The repo's session: its view links the trial; the shared dir does not.
    let a = env.run(&["skills", "sync", "--cwd", &repo, "--harness", "omp"]);
    let view = std::path::PathBuf::from(a["dirs"][0].as_str().unwrap());
    assert!(view.starts_with(env.root.join("views")), "{a}");
    assert_eq!((a["added"].clone(), a["total"].clone(), a["provisional"].clone()), (json!([name]), json!(1), json!([id])), "{a}");
    assert_eq!(std::fs::read_link(view.join(name)).unwrap(), env.staged_dir());
    assert!(std::fs::symlink_metadata(shared.join(name)).is_err(), "never in the shared dir while provisional");

    // Another repo's session, interleaved: its own view, without the trial; the first view is untouched.
    let b = env.run(&["skills", "sync", "--cwd", elsewhere.to_str().unwrap(), "--harness", "omp"]);
    let other = std::path::PathBuf::from(b["dirs"][0].as_str().unwrap());
    assert_ne!(other, view);
    assert_eq!((b["total"].clone(), b["links"].clone()), (json!(0), json!({})), "{b}");
    assert_eq!(std::fs::read_link(view.join(name)).unwrap(), env.staged_dir(), "the other place's sync pruned nothing here");

    // Pinned by the user: the kept library, shared by every place's view.
    env.run(&["library", "pin", &id]);
    let p = env.run(&["skills", "sync", "--cwd", elsewhere.to_str().unwrap(), "--harness", "omp"]);
    assert_eq!(std::fs::read_link(shared.join(name)).unwrap(), env.staged_dir(), "{p}");
    assert_eq!(std::fs::read_link(other.join(name)).unwrap(), env.staged_dir(), "{p}");
    env.run(&["library", "unpin", &id]);
    env.run(&["skills", "sync", "--cwd", elsewhere.to_str().unwrap(), "--harness", "omp"]);
    assert!(std::fs::symlink_metadata(other.join(name)).is_err(), "unpinned: back to its own place");

    // A second trial: linked for its cwd, pruned from its view when retired.
    env.run(&["create", "ledger-test-gate", "--from", "description", "--description", "Run the ledger tests before committing payment changes", "--editor", &script("editor-ok.sh")]);
    env.run(&["activate", "ledger-test-gate", "--unverified"]);
    env.run(&["skills", "sync", "--cwd", &repo]);
    let gate = view.join("ledger-test-gate");
    assert_eq!(std::fs::read_link(&gate).unwrap(), env.root.join("staging/payments/ledger-test-gate"));
    let gate_id = env.run(&["library", "ls"])["entries"].as_array().unwrap().iter().find(|e| e["name"] == "ledger-test-gate").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    env.run(&["library", "disable", &gate_id]);
    assert!(std::fs::symlink_metadata(&gate).is_err(), "retired → pruned from every view at once");

    // Off empties every view at once; on and the next sync restore this place's trial.
    env.run(&["off"]);
    assert!(std::fs::symlink_metadata(view.join(name)).is_err());
    let off = env.run(&["skills", "sync", "--cwd", &repo]);
    assert_eq!((off["off"].clone(), off["dirs"].clone(), off["total"].clone()), (json!(true), json!([]), json!(0)), "{off}");
    env.run(&["on"]);
    env.run(&["skills", "sync", "--cwd", &repo]);
    assert_eq!(std::fs::read_link(view.join(name)).unwrap(), env.staged_dir());
}

/// The always-on catalog the session's view costs is reported, split from the on-demand bodies.
#[cfg(unix)]
#[test]
fn sync_reports_the_catalog_budget() {
    let env = Env::new();
    env.write_trace("0192c4a0-0000-7000-8000-0000000000e0", &[], true);
    env.run(&["reflect", "--pending", "--editor", &script("editor-ok.sh")]);
    let s = env.run(&["skills", "sync", "--cwd", env.repo.to_str().unwrap(), "--harness", "omp"]);
    let c = &s["catalog"];
    assert_eq!(c["limit"], 8192, "{s}");
    let entry = &c["entries"][0];
    assert!(entry["catalog"].as_u64().unwrap() < 300 && entry["body"].as_u64().unwrap() > 0, "{c}");
    assert_eq!(c["on_demand_body_bytes"], entry["body"], "{c}");
    assert!(s["lines"].to_string().contains("always-on catalog"), "{s}");
}
