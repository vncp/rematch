#![cfg_attr(not(unix), allow(dead_code, unused_imports))] // helpers of Unix-only tests
//! Identity that survives machines (ADR 001 L1) end to end through the binary: a session
//! traced in another checkout of the same remote counts an entry whose predicate matches by
//! remote only, `why` shows the remote and the content sha, and `lake migrate` fills the
//! identity fields of pre-L1 rows once.

mod common;

use common::*;
use rematch_core::ledger::{EntryFilter, Ledger};
use serde_json::{json, Value};

fn ledger(env: &Env) -> Ledger {
    Ledger::open(&env.root.join("ledger.sqlite")).unwrap()
}

fn staged(env: &Env) -> String {
    let d = env.run(&[
        "create", "ledger-test-gate", "--from", "description", "--description", "Run the ledger tests before committing payment changes",
        "--editor", &script("editor-ok.sh"),
    ]);
    assert_eq!(d["outcome"], "staged", "{d}");
    env.run(&["activate", "ledger-test-gate", "--unverified"]);
    d["id"].as_str().unwrap().to_string()
}

/// A finished session in `repo` (cwd `repo/crates/ledger`) whose trace names `remote`.
fn session(env: &Env, sid: &str, repo: &str, remote: Option<&str>, loaded: Value) {
    env.write_records(
        sid,
        &[
            json!({"seq":0,"t":5_000,"ev":"session_start","session_id":sid,"cwd":format!("{repo}/crates/ledger"),"repo":repo,
                   "repo_remote":remote,"subpath":"crates/ledger","head_sha":"c".repeat(40),
                   "model":{"provider":"anthropic","id":"claude-opus-5-5"},"loaded_entries":loaded}),
            json!({"seq":1,"t":5_001,"ev":"input","text":"Tidy the ledger module","idle":true,"prelabel":"none"}),
            json!({"seq":2,"t":5_002,"ev":"turn_end","usage":{"input":10,"output":5},"stop_reason":"stop","text_head":"Done."}),
            json!({"seq":3,"t":5_003,"ev":"session_shutdown","reasons":[],"flagged":false,"files_changed":[["crates/ledger/src/lib.rs","d".repeat(64)]]}),
        ],
    );
}

#[cfg(unix)] // POSIX sh fixtures
#[test]
fn a_worktree_session_matches_its_repos_entry_by_remote() {
    let env = Env::new();
    std::fs::write(env.repo.join(".git/config"), "[remote \"origin\"]\n\turl = git@github.com:Org/Payments.git\n").unwrap();
    let id = staged(&env);
    let mut e = ledger(&env).get_entry(&id).unwrap().unwrap();
    assert_eq!((e.predicate.repo_remote.as_deref(), e.predicate.subpath.as_deref()), (Some("github.com/org/payments"), Some("**")), "derive fills the remote");
    let sha = e.content_sha256.clone().expect("content_sha256 set at stage");
    // Only the remote can match: the glob names a path no session runs in, and no content rule.
    e.predicate.repo_glob = "/nowhere/else".into();
    e.predicate.file_exts.clear();
    e.predicate.stems.clear();
    ledger(&env).upsert_entry(&e).unwrap();

    let elsewhere = env.home.join("wt/payments").to_string_lossy().into_owned();
    let loaded = json!([{"id": id, "version": e.version, "sha": sha}]);
    session(&env, "wt-same", &elsewhere, Some("https://github.com/org/payments.git"), loaded.clone());
    session(&env, "wt-fork", &elsewhere, Some("git@github.com:fork/payments.git"), loaded.clone());
    session(&env, "wt-none", &elsewhere, None, loaded);
    // A session that loaded another version of the entry does not count it.
    session(&env, "old-version", &elsewhere, Some("github.com/org/payments"), json!([{"id": id, "version": e.version + 1}]));
    env.run(&["count"]);

    let c = ledger(&env).counter_rows(&id).unwrap();
    assert_eq!(c.iter().map(|r| r.session_id.as_str()).collect::<Vec<_>>(), vec!["wt-same"], "{c:?}");

    let w = env.run(&["library", "why", "wt-same"]);
    let s = &w["session"];
    assert_eq!((s["repo_remote"].as_str(), s["subpath"].as_str()), (Some("github.com/org/payments"), Some("crates/ledger")));
    assert_eq!(s["head_sha"], "c".repeat(40));
    assert_eq!(s["loaded_entry_shas"], json!([sha]));
    assert_eq!(s["files_changed"], json!([["crates/ledger/src/lib.rs", "d".repeat(64)]]));
    let l = &w["loaded"][0];
    assert_eq!((l["content_sha256"].as_str(), l["predicate_hit"].as_bool()), (Some(sha.as_str()), Some(true)));
    let text = w["lines"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect::<Vec<_>>().join("\n");
    assert!(text.contains("remote github.com/org/payments · subpath crates/ledger"), "{text}");
    assert!(text.contains(&format!("sha {}", &sha[..12])), "{text}");
    let none = env.run(&["library", "why", "wt-none"]);
    assert_eq!(none["loaded"][0]["predicate_hit"], false, "no remote on the session: the glob decides, and misses");

    // Every write above is in the journal.
    let j = ledger(&env).journal_since(0).unwrap();
    assert!(j.iter().any(|r| r.table == "counters" && r.row_key == format!("{id}#wt-same") && r.json["content_sha256"] == json!(sha)));
    assert!(j.iter().all(|r| r.machine == "test-host"));
}

#[cfg(unix)] // POSIX sh fixtures
#[test]
fn lake_migrate_fills_pre_l1_rows_once() {
    let env = Env::new();
    std::fs::write(env.repo.join(".git/config"), "[remote \"origin\"]\n\turl = https://GitHub.com/Org/Payments\n").unwrap();
    let id = staged(&env);
    let repo = env.repo.to_string_lossy().into_owned();
    session(&env, "old", &repo, None, json!([id]));
    env.run(&["count"]);
    // Make the rows look like they predate L1.
    let l = ledger(&env);
    let mut e = l.get_entry(&id).unwrap().unwrap();
    let want_sha = e.content_sha256.take().unwrap();
    e.predicate.repo_remote = None;
    e.predicate.subpath = None;
    l.upsert_entry(&e).unwrap();
    let mut s = l.get_session("old").unwrap().unwrap();
    s.repo_remote = None;
    s.subpath = None;
    l.upsert_session(&s).unwrap();
    drop(l);

    let m = env.run(&["lake", "migrate"]);
    assert_eq!((m["content_sha256"].as_u64(), m["predicates"].as_u64(), m["sessions"].as_u64()), (Some(1), Some(1), Some(1)), "{m}");
    let l = ledger(&env);
    let e = l.get_entry(&id).unwrap().unwrap();
    assert_eq!(e.content_sha256.as_deref(), Some(want_sha.as_str()), "the stage-time hash, recomputed");
    assert_eq!((e.predicate.repo_remote.as_deref(), e.predicate.subpath.as_deref()), (Some("github.com/org/payments"), Some("**")));
    assert_eq!(e.predicate.repo_glob, rematch_core::predicate::escape_glob(&repo), "the glob stays");
    let mirror: Value = serde_json::from_str(&std::fs::read_to_string(e.path.join("rematch.json")).unwrap()).unwrap();
    assert_eq!(mirror["content_sha256"], json!(want_sha), "rematch.json mirrors it");
    let s = l.get_session("old").unwrap().unwrap();
    assert_eq!((s.repo_remote.as_deref(), s.subpath.as_deref()), (Some("github.com/org/payments"), Some("crates/ledger")));
    let seq = l.journal_max_seq().unwrap();
    drop(l);

    let again = env.run(&["lake", "migrate"]);
    assert_eq!((again["entries"].as_u64(), again["sessions"].as_u64()), (Some(0), Some(0)), "idempotent: {again}");
    assert_eq!(ledger(&env).journal_max_seq().unwrap(), seq, "a second run writes nothing");
    assert_eq!(ledger(&env).list_entries(&EntryFilter::default()).unwrap().iter().filter(|e| e.content_sha256.is_none()).count(), 0);
}
