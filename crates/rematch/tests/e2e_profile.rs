//! End-to-end for the profile (DESIGN §20): two machines (two REMATCH_ROOTs and agent dirs)
//! share a local bare git remote. A adopts its kept entry and hand-written skill/agent into the
//! profile; B clones it and sees them linked; B's keep lands in the profile and reaches A; a
//! conflicting edit is reported (tree clean, digest line); an unreachable remote fails open
//! within the timeout; `roles.yml` picks the editor per machine; uninstall keeps `profile/`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

const A: &str = "machine-a";
const B: &str = "machine-b";

fn on(env: &Env, machine: &str, args: &[&str]) -> Value {
    let mut c = env.cmd(args);
    c.env("REMATCH_MACHINE", machine);
    let (v, code) = env.run_cmd(c);
    assert_eq!((code, v["ok"].clone()), (0, json!(true)), "{machine} {args:?}: {v}");
    v["data"].clone()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn link(p: &Path) -> PathBuf {
    std::fs::read_link(p).unwrap_or_else(|e| panic!("{} is not a link: {e}", p.display()))
}

fn lines(v: &Value) -> Vec<String> {
    v["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect()
}

/// Reflect one flagged session (the scripted editor proposes `name`), then fold four clean
/// sessions that loaded it: `count` keeps it.
fn keep(env: &Env, machine: &str, editor: &str, sid_base: &str) -> String {
    env.write_trace(&format!("{sid_base}0"), &[], true);
    let d = on(env, machine, &["reflect", "--pending", "--editor", editor]);
    let id = d["reflections"][0]["id"].as_str().unwrap_or_else(|| panic!("{d}")).to_string();
    for i in 1..=4 {
        env.write_trace(&format!("{sid_base}{i}"), &[&id], false);
    }
    let c = on(env, machine, &["count", "--editor", editor]);
    assert_eq!(c["decisions"][0]["decision"], "keep", "{c}");
    id
}

#[test]
fn two_machines_share_one_profile() {
    let a = Env::new();
    let b = Env::new();
    let remote_tmp = tempfile::tempdir().unwrap();
    let remote = remote_tmp.path().canonicalize().unwrap().join("profile.git");
    let out = Command::new("git").args(["init", "--bare", "-q"]).arg(&remote).output().unwrap();
    assert!(out.status.success());
    let remote_s = remote.to_str().unwrap();

    // --- A, before the profile: a kept entry, a hand-written skill and agent, a policy file.
    keep(&a, A, &script("editor-ok.sh"), "0192c4a0-0000-7000-8000-00000000a0");
    let kept_a = a.agent.join("skills/run-tests-before-commit");
    assert!(std::fs::symlink_metadata(&kept_a).unwrap().is_dir(), "kept natively before the profile");
    std::fs::create_dir_all(a.agent.join("skills/hand-notes")).unwrap();
    std::fs::write(a.agent.join("skills/hand-notes/SKILL.md"), "---\nname: hand-notes\ndescription: Use when writing notes.\n---\n\nKeep notes short.\n").unwrap();
    std::fs::create_dir_all(a.agent.join("agents")).unwrap();
    std::fs::write(a.agent.join("agents/reviewer.md"), "---\nname: reviewer\ndescription: Reviews a diff.\n---\n\nReview the diff.\n").unwrap();
    on(&a, A, &["policy", "caps", "set", "kept_per_domain", "3"]);

    // --- A: init --remote --adopt moves everything into the profile and pushes.
    let init = on(&a, A, &["profile", "init", "--remote", remote_s, "--adopt"]);
    eprintln!("A init lines: {:#?}", lines(&init));
    let pa = a.root.join("profile");
    let adopted: Vec<(String, String)> = init["adopted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (x["name"].as_str().unwrap().to_string(), x["as"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(
        adopted,
        vec![("run-tests-before-commit".into(), "kept".into()), ("hand-notes".into(), "pinned".into()), ("reviewer".into(), "file".into())],
        "{init}"
    );
    assert_eq!((init["sync"]["pushed"].clone(), init["sync"]["offline"].clone()), (json!(true), json!(false)), "{init}");
    assert_eq!(link(&kept_a), pa.join("skills/run-tests-before-commit"));
    assert_eq!(link(&a.agent.join("skills/hand-notes")), pa.join("skills/hand-notes"));
    assert_eq!(link(&a.agent.join("agents/reviewer.md")), pa.join("agents/reviewer.md"));
    assert_eq!(link(&a.root.join("skills/run-tests-before-commit")), pa.join("skills/run-tests-before-commit"));
    assert_eq!(link(&a.root.join("skills/hand-notes")), pa.join("skills/hand-notes"));
    assert_eq!(link(&a.root.join("policy.json")), pa.join("config/policy.json"));
    assert!(pa.join("skills/run-tests-before-commit/rematch.json").is_file(), "the ledger mirror stays local");
    assert_eq!(git(&pa, &["status", "--porcelain"]), "", "rematch.json is ignored");
    let tracked = git(&pa, &["ls-files"]);
    assert!(!tracked.contains("rematch.json") && tracked.contains("skills/hand-notes/SKILL.md"), "{tracked}");
    let ls = on(&a, A, &["library", "ls"]);
    let notes = ls["entries"].as_array().unwrap().iter().find(|e| e["name"] == "hand-notes").unwrap().clone();
    assert_eq!((notes["status"].clone(), notes["pinned"].clone()), (json!("pinned"), json!(true)), "{notes}");
    let kept = ls["entries"].as_array().unwrap().iter().find(|e| e["name"] == "run-tests-before-commit").unwrap().clone();
    assert_eq!(kept["path"], json!(pa.join("skills/run-tests-before-commit")), "{kept}");
    // The policy writer replaces the profile's file, not the link.
    on(&a, A, &["policy", "caps", "set", "kept_per_domain", "4"]);
    assert_eq!(link(&a.root.join("policy.json")), pa.join("config/policy.json"));
    assert!(std::fs::read_to_string(pa.join("config/policy.json")).unwrap().contains('4'));

    // An approved hook lands in the profile, linked from rematch-hooks/.
    let builtins = on(&a, A, &["builtins", "ls"]);
    let hook_id = builtins["builtins"].as_array().unwrap().iter().find(|x| x["name"] == "stuck-detector").unwrap()["id"].as_str().unwrap().to_string();
    on(&a, A, &["library", "approve", &hook_id]);
    let hook_file = format!("{hook_id}.ts");
    assert_eq!(link(&a.agent.join("extensions/rematch-hooks").join(&hook_file)), pa.join("hooks").join(&hook_file));

    // roles.yml: the editor per machine.
    std::fs::write(pa.join("config/roles.yml"), format!("editor:\n  default: anthropic/claude-opus-5-5\n  {B}: openai/gpt-5\n")).unwrap();
    let s = on(&a, A, &["profile", "sync"]);
    assert_eq!(s["pushed"], true, "{s}");

    // --- B: clone and sync; A's entries are linked into B's shared dir and agent dir.
    let init_b = on(&b, B, &["profile", "init", "--remote", remote_s]);
    eprintln!("B init lines: {:#?}", lines(&init_b));
    assert_eq!(init_b["cloned"], true, "{init_b}");
    let pb = b.root.join("profile");
    let sync_b = on(&b, B, &["profile", "sync"]);
    eprintln!("B sync lines: {:#?}", lines(&sync_b));
    for name in ["run-tests-before-commit", "hand-notes"] {
        assert_eq!(link(&b.root.join("skills").join(name)), pb.join("skills").join(name), "{sync_b}");
        assert_eq!(link(&b.agent.join("skills").join(name)), pb.join("skills").join(name));
    }
    assert_eq!(link(&b.agent.join("agents/reviewer.md")), pb.join("agents/reviewer.md"));
    assert_eq!(link(&b.agent.join("extensions/rematch-hooks").join(&hook_file)), pb.join("hooks").join(&hook_file));
    assert_eq!(link(&b.root.join("policy.json")), pb.join("config/policy.json"));
    let skills_b = on(&b, B, &["skills", "sync", "--cwd", b.repo.to_str().unwrap()]);
    assert_eq!(skills_b["profile_sources"], 2, "{skills_b}");
    let pj: Value = serde_json::from_str(&std::fs::read_to_string(pb.join("profile.json")).unwrap()).unwrap();
    assert_eq!(pj, json!({"version": 1, "machines": [A, B]}));

    // B has no ledger entry for A's two skills: listed as kept on A, never counted on B.
    let index: Value = serde_json::from_str(&std::fs::read_to_string(pb.join(".index.json")).unwrap()).unwrap();
    assert_eq!(index["skills"]["hand-notes"]["machine"], A, "{index}");
    assert_eq!(git(&pb, &["status", "--porcelain"]), "", ".index.json is per machine (ignored)");
    let ls_b = on(&b, B, &["library", "ls"]);
    let kept_b: Vec<(String, String)> = ls_b["profile_kept"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| (k["name"].as_str().unwrap().to_string(), k["from"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(kept_b, vec![("hand-notes".into(), A.into()), ("run-tests-before-commit".into(), A.into())], "{ls_b}");
    let line = lines(&ls_b).into_iter().find(|l| l.contains("run-tests-before-commit")).unwrap();
    eprintln!("B library ls: {line}");
    assert!(line.starts_with("kept (profile, from machine-a) "), "{line}");
    assert!(on(&a, A, &["library", "ls"])["profile_kept"].as_array().unwrap().is_empty(), "A's ledger holds both");
    let report_b = on(&b, B, &["report"]);
    assert!(lines(&report_b).contains(&"profile-kept (not counted on this machine): 2".to_string()), "{report_b}");
    assert_eq!(report_b["profile_kept"], 2);
    assert_eq!(on(&a, A, &["report"])["profile_kept"], 0);

    // roles.yml resolves the editor per machine, ahead of config.yml.
    let doctor_b = on(&b, B, &["doctor"]);
    assert_eq!(doctor_b["roles"]["rematch-editor"]["selector"], "openai/gpt-5", "{doctor_b}");
    assert_eq!(doctor_b["roles"]["rematch-editor"]["via"], "profile:rematch-editor");
    let doctor_a = on(&a, A, &["doctor"]);
    assert_eq!(doctor_a["roles"]["rematch-editor"]["selector"], "anthropic/claude-opus-5-5", "{doctor_a}");
    assert_eq!(doctor_a["roles"]["rematch-classifier"]["via"], "smol", "unlisted roles still come from config.yml");

    // --- B keeps a new entry: written into B's profile; after both sync, A has it.
    let other = b.home.join("editor-other.sh");
    std::fs::write(
        &other,
        format!(
            "#!/bin/sh\nset -eu\ninput=$(cat)\n. '{}'\ncase \"${{REMATCH_PROMPT:-editor}}\" in\n  classifier) classify_all correction ;;\n  reflector) reflector_reply ;;\n  *) clean_edit | sed 's/run-tests-before-commit/check-ledger-before-commit/g' ;;\nesac\n",
            scripts().join("editor-common.sh").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&other, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    keep(&b, B, other.to_str().unwrap(), "0192c4a0-0000-7000-8000-00000000b0");
    let new_b = pb.join("skills/check-ledger-before-commit");
    assert!(new_b.join("SKILL.md").is_file(), "the keep writes into the profile");
    assert_eq!(link(&b.agent.join("skills/check-ledger-before-commit")), new_b);
    assert_eq!(link(&b.root.join("skills/check-ledger-before-commit")), new_b);
    let s = on(&b, B, &["profile", "sync"]);
    assert_eq!(s["pushed"], true, "{s}");
    let s = on(&a, A, &["profile", "sync"]);
    eprintln!("A sync lines: {:#?}", lines(&s));
    assert_eq!(s["pulled"], true, "{s}");
    assert_eq!(link(&a.agent.join("skills/check-ledger-before-commit")), pa.join("skills/check-ledger-before-commit"));
    assert_eq!(link(&a.root.join("skills/check-ledger-before-commit")), pa.join("skills/check-ledger-before-commit"));
    let ls_a = on(&a, A, &["library", "ls"]);
    assert_eq!(ls_a["profile_kept"][0]["name"], "check-ledger-before-commit", "{ls_a}");
    assert_eq!(ls_a["profile_kept"][0]["from"], B);
    // B's `why` of its own session lists A's skills it could see and did not count.
    let why_b = on(&b, B, &["library", "why", "0192c4a0-0000-7000-8000-00000000b01"]);
    let names: Vec<&str> = why_b["profile_kept"].as_array().unwrap().iter().map(|k| k["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["hand-notes", "run-tests-before-commit"], "{why_b}");
    assert!(lines(&why_b).iter().any(|l| l.starts_with("kept (profile, from machine-a) hand-notes: linked into the shared skills dir, not counted")), "{why_b}");

    // --- A conflicting edit on both: B's sync reports it, aborts the rebase, digest shows it.
    std::fs::write(pa.join("skills/hand-notes/SKILL.md"), "---\nname: hand-notes\ndescription: Use when writing notes.\n---\n\nA: keep notes in bullets.\n").unwrap();
    std::fs::write(pb.join("skills/hand-notes/SKILL.md"), "---\nname: hand-notes\ndescription: Use when writing notes.\n---\n\nB: keep notes in prose.\n").unwrap();
    assert_eq!(on(&a, A, &["profile", "sync"])["pushed"], true);
    let c = on(&b, B, &["profile", "sync"]);
    eprintln!("B conflict sync lines: {:#?}", lines(&c));
    assert_eq!((c["conflicts"].clone(), c["pushed"].clone(), c["offline"].clone()), (json!(["skills/hand-notes/SKILL.md"]), json!(false), json!(false)), "{c}");
    assert_eq!(git(&pb, &["status", "--porcelain"]), "", "tree clean after the aborted rebase");
    assert!(!pb.join(".git/rebase-merge").exists() && !pb.join(".git/rebase-apply").exists());
    assert!(std::fs::read_to_string(pb.join("skills/hand-notes/SKILL.md")).unwrap().contains("B: keep notes in prose."));
    let digest = on(&b, B, &["digest", "--cwd", b.repo.to_str().unwrap()]);
    let line = lines(&digest).into_iter().find(|l| l.starts_with("profile: 1 conflict(s)")).unwrap_or_else(|| panic!("{digest}"));
    eprintln!("B digest: {line}");
    assert!(line.contains("skills/hand-notes/SKILL.md"), "{line}");
    assert_eq!(on(&b, B, &["profile", "status"])["conflicts"], json!(["skills/hand-notes/SKILL.md"]));

    // --- The remote unreachable (a transport that hangs): offline within the timeout, the
    // session goes on (match, digest), the known conflicts stay.
    let real_url = git(&pb, &["remote", "get-url", "origin"]);
    git(&pb, &["remote", "set-url", "origin", "ssh://nohost.invalid/profile.git"]);
    git(&pb, &["config", "core.sshCommand", "sh -c 'sleep 30'"]);
    let t = Instant::now();
    let off = on(&b, B, &["profile", "sync", "--timeout", "2"]);
    let took = t.elapsed();
    eprintln!("B offline sync ({} ms): {:#?}", took.as_millis(), lines(&off));
    assert_eq!(off["offline"], true, "{off}");
    assert!(took < Duration::from_secs(8), "bounded by --timeout (+ kill grace): {took:?}");
    let m = on(&b, B, &["match", "--cwd", b.repo.to_str().unwrap()]);
    assert_eq!(m["profile_present"], true, "{m}");
    assert!(lines(&on(&b, B, &["digest", "--cwd", b.repo.to_str().unwrap()])).iter().any(|l| l.starts_with("profile: 1 conflict(s)")));
    git(&pb, &["config", "--unset", "core.sshCommand"]);
    git(&pb, &["remote", "set-url", "origin", &real_url]);

    // --- launchd: the plist runs a full sync; written, not loaded.
    let l = on(&b, B, &["profile", "install-launchd", "--interval", "300"]);
    let plist = b.home.join("Library/LaunchAgents/com.vncp.rematch.profile-sync.plist");
    let text = std::fs::read_to_string(&plist).unwrap();
    eprintln!("plist:\n{text}");
    assert!(text.contains("<string>profile</string>\n\t\t<string>sync</string>\n\t</array>") && text.contains("<integer>300</integer>"), "{text}");
    assert_eq!((l["loaded"].clone(), l["load"].clone()), (json!(false), json!(format!("launchctl load -w {}", plist.display()))));

    // --- Uninstall on A: the profile stays, the links go, hand-written items come back as files.
    let u = on(&a, A, &["uninstall", "--yes"]);
    assert_eq!(u["kept"], json!([pa]), "{u}");
    assert!(pa.join(".git").is_dir() && pa.join("skills/hand-notes/SKILL.md").is_file());
    assert!(!a.root.join("ledger.sqlite").exists() && !a.root.join("skills").exists() && !a.root.join("policy.json").exists());
    assert!(std::fs::symlink_metadata(&kept_a).is_err(), "the kept link is removed");
    let notes_md = a.agent.join("skills/hand-notes");
    assert!(std::fs::symlink_metadata(&notes_md).unwrap().is_dir(), "restored as a directory");
    assert!(std::fs::read_to_string(notes_md.join("SKILL.md")).unwrap().contains("A: keep notes in bullets."));
    assert!(std::fs::symlink_metadata(a.agent.join("agents/reviewer.md")).unwrap().is_file());
    assert!(std::fs::symlink_metadata(a.agent.join("extensions/rematch-hooks")).is_err());
}

#[test]
fn sync_without_a_profile_fails_and_status_says_so() {
    let env = Env::new();
    let (v, code) = env.run_cmd(env.cmd(&["profile", "sync"]));
    assert_eq!((code, v["ok"].clone()), (1, json!(false)));
    assert!(v["error"].as_str().unwrap().contains("profile init"), "{v}");
    assert_eq!(env.run(&["profile", "status"])["present"], false);
    assert_eq!(env.run(&["match", "--cwd", env.repo.to_str().unwrap()])["profile_present"], false);
}

#[test]
fn local_profile_without_remote_commits_and_adopts_one_path() {
    let env = Env::new();
    let d = env.run(&["profile", "init"]);
    assert_eq!((d["created"].clone(), d["remote"].clone(), d["sync"]["offline"].clone()), (json!(true), json!(null), json!(false)), "{d}");
    let p = env.root.join("profile");
    assert!(!git(&p, &["log", "--oneline"]).is_empty());
    // Other harnesses' files stay out: a Codex skill root, Claude Code's dir and its
    // instructions file, anywhere.
    for (rel, is_dir) in [(".agents/skills/codex-only", true), (".claude/skills/cc-only", true), (".claude/CLAUDE.md", false), ("work/CLAUDE.md", false)] {
        let f = env.home.join(rel);
        if is_dir {
            std::fs::create_dir_all(&f).unwrap();
            std::fs::write(f.join("SKILL.md"), "---\nname: x\ndescription: Use for x.\n---\n\nX.\n").unwrap();
        } else {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, "# instructions\n").unwrap();
        }
        let (v, code) = env.run_cmd(env.cmd(&["profile", "adopt", f.to_str().unwrap()]));
        assert_eq!(code, 1, "{rel}: {v}");
        let e = v["error"].as_str().unwrap();
        assert!(e.contains("omp skills and agents only") && e.contains("CLAUDE.md, settings"), "{rel}: {e}");
        assert!(std::fs::symlink_metadata(&f).unwrap().file_type().is_dir() == is_dir, "{rel} left in place");
    }
    let skill = env.home.join("notes/skills/scratch");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: scratch\ndescription: Use for scratch work.\n---\n\nScratch.\n").unwrap();
    let a = env.run(&["profile", "adopt", skill.to_str().unwrap()]);
    assert_eq!(a["adopted"]["as"], "pinned", "{a}");
    assert_eq!(link(&skill), p.join("skills/scratch"));
    assert_eq!(link(&env.agent.join("skills/scratch")), p.join("skills/scratch"));
    let (again, code) = env.run_cmd(env.cmd(&["profile", "adopt", skill.to_str().unwrap()]));
    assert_eq!(code, 1, "{again}");
    let s = env.run(&["profile", "sync"]);
    assert!(s["committed"].is_string(), "{s}");
    let st = env.run(&["profile", "status"]);
    assert_eq!((st["skills"].clone(), st["dirty"].clone(), st["machines"].clone()), (json!(1), json!([]), json!(["test-host"])), "{st}");
}
