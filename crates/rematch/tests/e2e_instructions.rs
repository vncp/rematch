// POSIX-only: sh editor fixtures, file modes and symlinks.
#![cfg(unix)]
//! Personal instructions end to end: an automatically reflected entry with an
//! `AGENTS.fragment.md` waits as an `instructions:<id>` action on every digest/status/library
//! call; an approval bound to the reviewed snapshot writes a rematch block into the personal
//! `<agent_dir>/AGENTS.md` (never the repo's), through a dotfiles symlink too; the next session's
//! trace credits it; the user's own text survives updates; an edited block is never overwritten
//! or removed, and a stale review of it decides nothing; off, disable, withdrawal and uninstall
//! remove only unchanged blocks and never claim a removal a failed write did not make; the
//! budget blocks what does not fit; a PR2 runtime approval is not a personal-file approval; two
//! omp profiles sharing one root keep their own files and approvals; the profile carries the
//! approved text to another machine and a withdrawal back, profile-wide.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use common::*;
use serde_json::{json, Value};

const NAME: &str = "crate-tests-first";
const MINE: &str = "# Mine\n\nAlways answer in English.\n";

fn on(env: &Env, machine: &str, args: &[&str]) -> Value {
    let mut c = env.cmd(args);
    c.env("REMATCH_MACHINE", machine);
    let (v, code) = env.run_cmd(c);
    assert_eq!((code, v["ok"].clone()), (0, json!(true)), "{machine} {args:?}: {v}");
    v["data"].clone()
}

/// One flagged session reflected by the fragment editor: a provisional entry with a fragment.
fn reflected(env: &Env) -> String {
    env.write_trace("0192c4a0-0000-7000-8000-0000000000a0", &[], true);
    let d = env.run(&["reflect", "--pending", "--editor", &script("editor-fragment.sh")]);
    let r = &d["reflections"][0];
    assert_eq!(r["outcome"], "staged", "{d}");
    r["id"].as_str().unwrap().to_string()
}

fn ids(v: &Value, key: &str) -> Vec<String> {
    v[key].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap().to_string()).collect()
}

/// The digest's approvable item `id`.
fn action(env: &Env, id: &str) -> Value {
    let g = env.run(&["digest", "--cwd", env.repo.to_str().unwrap()]);
    g["approvals"].as_array().unwrap().iter().find(|a| a["id"] == id).cloned().unwrap_or_else(|| panic!("{id} not offered: {g}"))
}

fn argv(a: &Value, which: &str) -> Vec<String> {
    serde_json::from_value(a["commands"][which].clone()).unwrap_or_else(|_| panic!("no {which}: {a}"))
}

/// Run an action's command as it was shown (possibly stale now): `(envelope, exit code)`.
fn run_shown(env: &Env, argv: &[String]) -> (Value, i32) {
    env.run_cmd(env.cmd(&argv.iter().map(String::as_str).collect::<Vec<_>>()))
}

fn refused_as_stale(env: &Env, argv: &[String]) {
    let (v, code) = run_shown(env, argv);
    assert_eq!(code, 1, "{v}");
    assert!(v["error"].as_str().unwrap().contains("changed since it was shown"), "{v}");
}

fn lines(v: &Value) -> Vec<String> {
    v["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn session(env: &Env, sid: &str, id: &str, sha: &str) {
    let repo = env.repo.to_string_lossy();
    env.write_records(
        sid,
        &[
            json!({"seq":0,"t":5_000,"ev":"session_start","session_id":sid,"cwd":repo,"repo":repo,"branch_hint":"payments",
                   "model":{"provider":"anthropic","id":"claude-opus-5-5"},"loaded_entries":[],
                   "instructions_exposed":[{"id":id,"name":NAME,"sha":sha}]}),
            json!({"seq":1,"t":5_001,"ev":"input","text":"Commit the crate change after the tests","idle":true,"prelabel":"none"}),
            json!({"seq":2,"t":5_002,"ev":"turn_end","usage":{"input":10,"output":5},"stop_reason":"stop","text_head":"Done."}),
            json!({"seq":3,"t":5_003,"ev":"session_shutdown","reasons":[],"flagged":false}),
        ],
    );
}

/// Permissions bind (not running as root): a read-only dir refuses a new file.
fn permissions_bind(dir: &Path) -> bool {
    let probe = dir.join(".rematch-probe");
    let ok = std::fs::write(&probe, "").is_err();
    let _ = std::fs::remove_file(probe);
    ok
}

#[test]
fn reflected_text_waits_then_lands_in_the_personal_agents_md_only() {
    let env = Env::new();
    let personal = env.agent.join("AGENTS.md");
    std::fs::write(&personal, MINE).unwrap();
    std::fs::write(env.repo.join("AGENTS.md"), "# Payments\n").unwrap();
    let id = reflected(&env);
    let aid = format!("instructions:{id}");
    let cwd = env.repo.to_str().unwrap();

    // Pending on every digest (not consumed by digest.last), in status and in the library.
    for _ in 0..2 {
        let a = action(&env, &aid);
        assert_eq!((a["kind"].as_str(), a["surface"].as_str(), a["scope"].as_str(), a["effect"].as_str()),
                   (Some("instructions"), Some("system_prompt"), Some("personal"), Some("always_on")), "{a}");
        assert_eq!(a["target"], json!(personal), "{a}");
        assert!(a["diff"].as_str().unwrap().contains(&format!("+## rematch: {NAME}")), "{a}");
        let approve = argv(&a, "approve_argv");
        assert_eq!((approve[approve.len() - 2].as_str(), approve.last().unwrap()), ("--sha", &a["review_sha256"].as_str().unwrap().to_string()), "{a}");
        assert!(a["reload"].as_str().unwrap().contains("running session"), "{a}");
    }
    assert!(ids(&env.run(&["status"]), "actions").contains(&aid));
    let ls = env.run(&["library", "ls"]);
    assert!(ids(&ls, "actions").contains(&aid), "{ls}");
    assert_eq!(env.entry(&id)["instructions"], "awaiting approval");
    assert_eq!(env.run(&["library", "inspect", &aid])["action"]["id"], json!(aid));
    assert_eq!(read(&personal), MINE, "nothing written before the approval");

    // Only the full hash of the reviewed snapshot approves: not none, not a prefix, not the
    // fragment's own hash.
    let a = action(&env, &aid);
    let review = a["review_sha256"].as_str().unwrap();
    for sha in [None, Some("deadbeefdead"), Some(&review[..12]), a["sha256"].as_str()] {
        let mut args = vec!["library", "approve", aid.as_str()];
        args.extend(sha.map(|s| ["--sha", s]).into_iter().flatten());
        let (v, code) = env.run_cmd(env.cmd(&args));
        assert_eq!(code, 1, "{args:?}: {v}");
    }
    assert_eq!(read(&personal), MINE);
    let sha = a["sha256"].as_str().unwrap().to_string();
    env.approve(&aid);
    let text = read(&personal);
    assert!(text.starts_with(MINE), "{text}");
    assert!(text.contains(&format!("<!-- rematch:begin name={NAME} ")) && text.contains(&format!("## rematch: {NAME}\n\nRun `cargo test -p <crate>`")), "{text}");
    assert_eq!(read(&env.repo.join("AGENTS.md")), "# Payments\n", "the repo's AGENTS.md is untouched");
    let g = env.run(&["digest", "--cwd", cwd]);
    assert!(!ids(&g, "approvals").contains(&aid), "{g}");
    assert_eq!(g["instructions"]["exposed"][0]["id"], json!(id), "{g}");
    assert_eq!(env.entry(&id)["instructions"], "applied");
    let st = env.run(&["status"]);
    assert_eq!(ids(&st, "applied"), vec![format!("instructions-applied:{NAME}")], "applied blocks stay listed: {st}");
    assert!(lines(&st).iter().any(|l| l.contains("personal instructions applied in") && l.contains(NAME)), "{st}");

    // The next session's trace credits the exposure.
    let sid = "0192c4a0-0000-7000-8000-0000000000b1";
    session(&env, sid, &id, &sha);
    env.run(&["count", "--editor", &script("editor-fragment.sh")]);
    let w = env.run(&["library", "why", sid]);
    assert_eq!(w["instructions"]["exposed"][0]["id"], json!(id), "{w}");

    // The user's own text outside the block survives an update of the block; a revision changed
    // again after its review is refused.
    std::fs::write(&personal, read(&personal).replacen("Always answer in English.", "Always answer in English. Be brief.", 1)).unwrap();
    let path = env.entry(&id)["path"].as_str().unwrap().to_string();
    let fragment = Path::new(&path).join("AGENTS.fragment.md");
    let original = read(&fragment);
    std::fs::write(&fragment, format!("{original}Also run clippy.\n")).unwrap();
    let a = action(&env, &aid);
    assert!(a["summary"].as_str().unwrap().starts_with("update personal instructions"), "{a}");
    assert!(a["diff"].as_str().unwrap().contains("+Also run clippy."), "{a}");
    assert!(!read(&personal).contains("clippy"), "the approved text stays until the revision is approved");
    std::fs::write(&fragment, format!("{original}Also run clippy --fix.\n")).unwrap();
    refused_as_stale(&env, &argv(&a, "approve_argv"));
    assert!(!read(&personal).contains("clippy"));
    std::fs::write(&fragment, format!("{original}Also run clippy.\n")).unwrap();
    env.approve(&aid);
    let text = read(&personal);
    assert!(text.contains("Be brief.") && text.contains("Also run clippy.") && text.matches("rematch:begin").count() == 1, "{text}");

    // A user edit inside the block: never overwritten or removed, surfaced as a conflict.
    let edited = text.replace("Also run clippy.", "Also run clippy and fmt.");
    std::fs::write(&personal, &edited).unwrap();
    let off = env.run(&["off"]);
    assert_eq!(read(&personal), edited, "off leaves an edited block: {off}");
    let st = env.run(&["status"]);
    let conflict = format!("instructions-conflict:{NAME}");
    assert!(ids(&st, "actions").contains(&conflict), "{st}");
    env.run(&["on"]);
    assert_eq!(read(&personal), edited);
    let (v, code) = env.run_cmd(env.cmd(&["library", "disable", &id]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(read(&personal), edited, "disable leaves an edited block");

    // A conflict reviewed, then edited again: neither restore nor release goes through.
    let c = env.run(&["library", "inspect", &conflict])["action"].clone();
    assert!(c["diff"].as_str().unwrap().contains("clippy and fmt"), "{c}");
    let again = edited.replace("clippy and fmt", "clippy, fmt and doc");
    std::fs::write(&personal, &again).unwrap();
    refused_as_stale(&env, &argv(&c, "approve_argv"));
    refused_as_stale(&env, &argv(&c, "decline_argv"));
    assert_eq!(read(&personal), again);

    // Decline keeps the user's text as their own (markers dropped, never re-added).
    env.decline(&conflict);
    let mine = read(&personal);
    assert!(!mine.contains("<!-- rematch"), "{mine}");
    assert!(mine.starts_with("# Mine\n\nAlways answer in English. Be brief.\n") && mine.contains("Also run clippy, fmt and doc."), "{mine}");
    env.run(&["off"]);
    env.run(&["on"]);
    assert_eq!(read(&personal), mine);
    assert!(!ids(&env.run(&["status"]), "actions").contains(&conflict));
}

#[test]
fn off_disable_and_withdrawal_remove_only_the_unchanged_block() {
    let env = Env::new();
    let personal = env.agent.join("AGENTS.md");
    std::fs::write(&personal, MINE).unwrap();
    let id = reflected(&env);
    let aid = format!("instructions:{id}");
    env.approve(&aid);
    let applied = read(&personal);
    assert_ne!(applied, MINE);

    let off = env.run(&["off"]);
    assert_eq!(read(&personal), MINE, "off restores the bytes found");
    assert!(lines(&off).iter().any(|l| l.contains("running session")), "{off}");
    env.run(&["on"]);
    assert_eq!(read(&personal), applied, "on applies it again");

    // Withdrawing the applied text (its own action, bound like the rest); the entry stays.
    let wid = format!("instructions-applied:{NAME}");
    let w = env.run(&["library", "inspect", &wid])["action"].clone();
    assert_eq!((w["state"].as_str(), w["commands"]["approve_argv"].is_null()), (Some("applied"), true), "{w}");
    let d = env.decline(&wid);
    assert_eq!(read(&personal), MINE, "{d}");
    assert_eq!(env.entry(&id)["status"], "provisional");
    assert_eq!(env.entry(&id)["prompt_active"], json!(false), "the runtime reach goes too");
    assert!(!ids(&env.run(&["status"]), "actions").contains(&aid), "a withdrawn text is not offered again by itself");
}

#[test]
fn uninstall_removes_the_block_and_the_file_it_created() {
    // A personal file rematch created goes entirely.
    let env = Env::new();
    let personal = env.agent.join("AGENTS.md");
    let id = reflected(&env);
    env.approve(&format!("instructions:{id}"));
    assert!(personal.is_file());
    let plan = env.run(&["uninstall"]);
    assert!(personal.is_file(), "plan only: {plan}");
    assert!(lines(&plan).iter().any(|l| l.contains("would remove")), "{plan}");
    env.run(&["uninstall", "--yes"]);
    assert!(!personal.exists());

    // A file the user had keeps exactly its bytes.
    let env = Env::new();
    let personal = env.agent.join("AGENTS.md");
    std::fs::write(&personal, MINE).unwrap();
    let id = reflected(&env);
    env.approve(&format!("instructions:{id}"));
    env.run(&["uninstall", "--yes"]);
    assert_eq!(read(&personal), MINE);
}

#[test]
fn a_symlinked_agents_md_is_written_through_and_stays_a_link() {
    let env = Env::new();
    let dotfiles = env.home.join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    let real = dotfiles.join("AGENTS.md");
    std::fs::write(&real, MINE).unwrap();
    let personal = env.agent.join("AGENTS.md");
    std::os::unix::fs::symlink(&real, &personal).unwrap();
    let id = reflected(&env);
    env.approve(&format!("instructions:{id}"));
    let is_link = || std::fs::symlink_metadata(&personal).unwrap().file_type().is_symlink();
    assert!(is_link(), "the dotfiles link is kept");
    assert!(read(&real).starts_with(MINE) && read(&real).contains(&format!("## rematch: {NAME}")), "{}", read(&real));
    env.run(&["off"]);
    assert!(is_link());
    assert_eq!(read(&real), MINE, "the bytes found come back, through the link");
    env.run(&["on"]);
    env.run(&["uninstall", "--yes"]);
    assert!(is_link() && read(&real) == MINE, "uninstall never replaces the link or deletes the user's file");
}

#[test]
fn a_failed_write_is_never_reported_as_a_removal_and_stays_visible() {
    let env = Env::new();
    let personal = env.agent.join("AGENTS.md");
    std::fs::write(&personal, MINE).unwrap();
    let id = reflected(&env);
    env.approve(&format!("instructions:{id}"));
    let applied = read(&personal);
    std::fs::set_permissions(&env.agent, std::fs::Permissions::from_mode(0o555)).unwrap();
    if !permissions_bind(&env.agent) {
        std::fs::set_permissions(&env.agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        return; // running as root
    }
    let off = env.run(&["off"]);
    let out = lines(&off);
    assert_eq!(read(&personal), applied, "the write failed");
    assert!(!out.iter().any(|l| l.contains(": removed")), "no false removal: {off}");
    assert!(out.iter().any(|l| l.contains("was not updated") && l.contains("still holds") && l.contains("instructions-sync")), "{off}");
    let st = env.run(&["status"]);
    let sync = st["actions"].as_array().unwrap().iter().find(|a| a["id"] == "instructions-sync").cloned().unwrap_or_else(|| panic!("{st}"));
    assert!(sync["reason"].as_str().unwrap().contains("The last write failed"), "{sync}");
    assert!(st["instructions"]["error"].is_string(), "{st}");

    std::fs::set_permissions(&env.agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    env.approve("instructions-sync");
    assert_eq!(read(&personal), MINE, "off applied once the file is writable");
    let st = env.run(&["status"]);
    assert!(!ids(&st, "actions").contains(&"instructions-sync".to_string()) && st["instructions"]["error"].is_null(), "{st}");
}

#[test]
fn an_approval_that_would_overflow_the_budget_is_a_blocked_action() {
    let env = Env::new();
    let personal = env.agent.join("AGENTS.md");
    let id = reflected(&env);
    let aid = format!("instructions:{id}");
    let lint = env.root.join("improver/lint.toml");
    let shipped = std::fs::read_to_string(&lint).ok();
    std::fs::create_dir_all(lint.parent().unwrap()).unwrap();
    std::fs::write(&lint, "[limits]\nalways_on_bytes = 100\n").unwrap();
    let st = env.run(&["status"]);
    let a = st["actions"].as_array().unwrap().iter().find(|a| a["id"] == aid.as_str()).cloned().unwrap_or_else(|| panic!("{st}"));
    assert_eq!((a["state"].as_str(), a["commands"]["approve_argv"].is_null()), (Some("blocked"), true), "{a}");
    assert!(a["blocked"].as_str().unwrap().contains("budget"), "{a}");
    assert!(!ids(&env.run(&["digest", "--cwd", env.repo.to_str().unwrap()]), "approvals").contains(&aid));
    let review = a["review_sha256"].as_str().unwrap();
    let (v, code) = env.run_cmd(env.cmd(&["library", "approve", &aid, "--sha", review]));
    assert_eq!(code, 1, "{v}");
    assert!(!personal.exists());
    match shipped {
        Some(t) => std::fs::write(&lint, t).unwrap(),
        None => std::fs::remove_file(&lint).unwrap(),
    }
    env.approve(&aid);
    assert!(read(&personal).contains(&format!("## rematch: {NAME}")));
}

#[test]
fn a_pr2_runtime_approval_still_asks_before_the_personal_file() {
    use rematch_core::ledger::Ledger;
    use rematch_core::library::{self, Approval};
    use rematch_core::paths::RematchPaths;
    let env = Env::new();
    let personal = env.agent.join("AGENTS.md");
    let id = reflected(&env);
    // A PR2-era one-tap: the runtime insertion of these bytes, no personal-file receipt.
    let paths = RematchPaths::with_roots(&env.root, &env.agent);
    let ledger = Ledger::open(&paths.ledger_file).unwrap();
    library::approve(&ledger, &paths, &id, Approval::Prompt).unwrap();
    drop(ledger);
    env.run(&["skills", "sync"]);
    assert!(!personal.exists(), "a runtime approval never writes the personal file");
    assert_eq!(env.entry(&id)["prompt_active"], json!(true));

    let aid = format!("instructions:{id}");
    let a = action(&env, &aid);
    let reaches = a["reaches"].as_array().unwrap();
    assert_eq!((reaches[0]["host"].as_str(), reaches[0]["new"].as_bool()), (Some("omp"), Some(true)), "{a}");
    assert_eq!((reaches[1]["host"].as_str(), reaches[1]["new"].as_bool()), (Some("claude-code|pi|opencode"), Some(false)), "{a}");
    assert!(a["reason"].as_str().unwrap().contains("the personal file is new"), "{a}");
    env.approve(&aid);
    assert!(read(&personal).contains(&format!("## rematch: {NAME}")));
}

#[test]
fn two_agent_dirs_share_one_root_without_crossing_approvals() {
    let env = Env::new();
    let personal = env.agent.join("AGENTS.md");
    std::fs::write(&personal, MINE).unwrap();
    let work = env.agent.parent().unwrap().join("profiles/work/agent");
    std::fs::create_dir_all(&work).unwrap();
    let work_personal = work.join("AGENTS.md");
    std::fs::write(&work_personal, "").unwrap();
    let in_work = |args: &[&str]| -> (Value, i32) {
        let mut c = env.cmd(args);
        c.env("PI_CODING_AGENT_DIR", &work);
        env.run_cmd(c)
    };
    let run_work = |args: &[&str]| -> Value {
        let (v, code) = in_work(args);
        assert_eq!((code, v["ok"].clone()), (0, json!(true)), "{args:?}: {v}");
        v["data"].clone()
    };
    let id = reflected(&env);
    let aid = format!("instructions:{id}");
    env.approve(&aid);
    assert!(read(&personal).contains(&format!("## rematch: {NAME}")));

    // The other agent dir: untouched, and it asks on its own.
    run_work(&["off"]);
    run_work(&["on"]);
    assert_eq!(read(&work_personal), "", "an approval for one agent dir never writes another");
    let st = run_work(&["status"]);
    assert!(ids(&st, "actions").contains(&aid), "{st}");
    assert_eq!(st["instructions"]["target"], json!(work_personal), "{st}");

    let review = |id: &str, which: &str| argv(&run_work(&["library", "inspect", id])["action"], which);
    run_work(&review(&aid, "approve_argv").iter().map(String::as_str).collect::<Vec<_>>());
    assert!(read(&work_personal).contains(&format!("## rematch: {NAME}")));
    let wid = format!("instructions-applied:{NAME}");
    run_work(&review(&wid, "decline_argv").iter().map(String::as_str).collect::<Vec<_>>());
    assert!(work_personal.is_file() && read(&work_personal).is_empty(), "its own empty file stays (rematch did not create it)");
    assert!(read(&personal).starts_with(MINE) && read(&personal).contains(&format!("## rematch: {NAME}")), "a withdrawal there leaves this one");
}

#[test]
fn the_profile_carries_the_approved_text_and_a_withdrawal_profile_wide() {
    let a = Env::new();
    let b = Env::new();
    let remote_tmp = tempfile::tempdir().unwrap();
    let remote = rematch_core::paths::canonicalize(remote_tmp.path()).unwrap().join("profile.git");
    assert!(Command::new("git").args(["init", "--bare", "-q"]).arg(&remote).status().unwrap().success());
    let remote_s = remote.to_str().unwrap();

    let id = reflected(&a);
    let aid = format!("instructions:{id}");
    a.approve(&aid);
    on(&a, "machine-a", &["profile", "init", "--remote", remote_s]);
    let canonical = a.root.join("profile/instructions").join(format!("{NAME}.md"));
    assert!(read(&canonical).starts_with("Run `cargo test -p <crate>`"));
    let a_personal = a.agent.join("AGENTS.md");

    let b_personal = b.agent.join("AGENTS.md");
    std::fs::write(&b_personal, "# B notes\n").unwrap();
    let init = on(&b, "machine-b", &["profile", "init", "--remote", remote_s]);
    let text = read(&b_personal);
    assert!(text.starts_with("# B notes\n") && text.contains(&format!("## rematch: {NAME}")), "{init}\n{text}");
    let ls = on(&b, "machine-b", &["library", "ls"]);
    assert_eq!(ls["instructions"]["exposed"][0]["source"], "profile", "{ls}");
    assert!(ids(&ls, "actions").iter().all(|x| !x.starts_with("instructions:")), "nothing to approve again on B: {ls}");

    // B's applied profile-only block is inspectable and withdrawable, and says how far that goes.
    let wid = format!("instructions-applied:{NAME}");
    let w = on(&b, "machine-b", &["library", "inspect", &wid])["action"].clone();
    assert!(w["reason"].as_str().unwrap().contains("PROFILE-WIDE"), "{w}");
    assert!(w["reaches"].as_array().unwrap().iter().any(|r| r["scope"].as_str().unwrap().starts_with("profile-wide")), "{w}");
    on(&b, "machine-b", &argv(&w, "decline_argv").iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(read(&b_personal), "# B notes\n");
    let s = on(&b, "machine-b", &["profile", "sync"]);
    assert!(lines(&s).iter().any(|l| l.contains("profile-wide")), "{s}");
    assert!(!b.root.join("profile/instructions").join(format!("{NAME}.md")).exists());

    // A, which approved it, drops it too, and may approve it again.
    on(&a, "machine-a", &["profile", "sync"]);
    assert!(!a_personal.exists() || !read(&a_personal).contains(NAME), "A follows the withdrawal");
    assert!(ids(&on(&a, "machine-a", &["status"]), "actions").contains(&aid));
    std::thread::sleep(std::time::Duration::from_millis(5));
    a.approve(&aid);
    on(&a, "machine-a", &["profile", "sync"]);
    on(&b, "machine-b", &["profile", "sync"]);
    assert!(read(&b_personal).contains(&format!("## rematch: {NAME}")), "the later approval reaches B again");

    // Retired on A: the profile copy goes, and B's block with it.
    on(&a, "machine-a", &["library", "disable", &id]);
    on(&a, "machine-a", &["profile", "sync"]);
    assert!(!canonical.exists());
    on(&b, "machine-b", &["profile", "sync"]);
    assert_eq!(read(&b_personal), "# B notes\n");
}
