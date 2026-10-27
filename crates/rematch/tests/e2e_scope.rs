// POSIX-only: unprivileged symlinks.
#![cfg(unix)]
//! A skill name scoped to one repo through the binary (`policy scope set|unset`): a profile
//! skill leaves every global location (`REMATCH_ROOT/skills`, `<agent_dir>/skills`, the Claude
//! Code plugin's `skills/`), is linked in the views of the repo's places only (another clone of
//! the same remote included), reaches Claude Code there as `SessionStart` context and Codex
//! through the repo's trials, and is global again after `unset`.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use common::*;
use serde_json::{json, Value};

fn git_repo(dir: &Path, url: &str) {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join(".git/config"), format!("[remote \"origin\"]\n\turl = {url}\n")).unwrap();
}

/// Claude Code's `SessionStart` at `cwd` (the plugin hook's `trace-event`), its reply's context.
fn cc_start(env: &Env, plugin: &Path, sid: &str, cwd: &Path) -> String {
    let mut c = env.cmd(&["trace-event", "--harness", "claude-code", "--hook-output"]);
    c.env("CLAUDE_PLUGIN_ROOT", plugin).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    let payload = json!({"session_id": sid, "transcript_path": "", "cwd": cwd, "hook_event_name": "SessionStart", "source": "startup"});
    child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let reply: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
    reply["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default().to_string()
}

fn link_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[test]
fn a_scoped_skill_reaches_only_its_repos_sessions_on_every_host() {
    let env = Env::new();
    git_repo(&env.repo, "git@github.com:Org/Bellwether.git");
    let clone = env.home.join("wt/bellwether");
    git_repo(&clone, "https://github.com/org/bellwether.git");
    let other = env.home.join("other");
    git_repo(&other, "git@github.com:org/rematch.git");
    // A profile skill (kept on some machine) and the rematch-made link omp loads it through.
    let src = env.root.join("profile/skills/bellwether-dev");
    std::fs::create_dir_all(env.root.join("profile/.git")).unwrap();
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), "---\nname: bellwether-dev\ndescription: Use when working on the bellwether crates.\n---\n# bellwether-dev\n").unwrap();
    let native = env.agent.join("skills/bellwether-dev");
    std::fs::create_dir_all(env.agent.join("skills")).unwrap();
    std::os::unix::fs::symlink(&src, &native).unwrap();
    let shared = env.root.join("skills");
    let plugin = env.home.join("plugin");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    let s = env.run(&["--cwd", other.to_str().unwrap(), "skills", "sync"]);
    assert!(s["links"].get("bellwether-dev").is_some() && shared.join("bellwether-dev").exists(), "unscoped: global {s}");

    let set = env.run(&["policy", "scope", "set", "bellwether-dev", env.repo.to_str().unwrap()]);
    assert_eq!(set["scope"], json!({"remote": "github.com/org/bellwether", "subpath": "**"}), "{set}");
    let withdrawn: Vec<String> = serde_json::from_value(set["withdrawn"].clone()).unwrap();
    assert!(withdrawn.contains(&shared.join("bellwether-dev").display().to_string()) && withdrawn.contains(&native.display().to_string()), "{set}");
    assert!(set["lines"].to_string().contains("keep the skill text they loaded"), "{set}");
    assert!(std::fs::symlink_metadata(&native).is_err() && std::fs::symlink_metadata(shared.join("bellwether-dev")).is_err());
    let policy: Value = serde_json::from_str(&std::fs::read_to_string(env.root.join("policy.json")).unwrap()).unwrap();
    assert_eq!(policy["scopes"]["bellwether-dev"]["remote"], "github.com/org/bellwether");
    assert!(env.run(&["policy", "show"])["lines"].to_string().contains("scope bellwether-dev → remote github.com/org/bellwether subpath ** only"));

    // omp/Pi/OpenCode views: another repo has none of it, both checkouts of the remote have it.
    let s = env.run(&["--cwd", other.to_str().unwrap(), "skills", "sync"]);
    assert!(s["links"].get("bellwether-dev").is_none() && s.get("scoped").is_none(), "{s}");
    for dir in [&env.repo, &clone] {
        let s = env.run(&["--cwd", dir.to_str().unwrap(), "skills", "sync"]);
        assert_eq!((s["links"]["bellwether-dev"].as_str(), s["scoped"].clone()), (src.to_str(), json!(["bellwether-dev"])), "{s}");
    }
    assert!(std::fs::symlink_metadata(shared.join("bellwether-dev")).is_err() && std::fs::symlink_metadata(&native).is_err(), "syncs never relink it globally");

    // Claude Code: the plugin's skills/ is the kept library without it; SessionStart in the repo
    // lists it with its view path, elsewhere not.
    let here = cc_start(&env, &plugin, "5d0c8a8e-1111-4c4c-8888-0123456789ab", &env.repo);
    assert!(here.contains("Skills of this repo (scoped here") && here.contains("- bellwether-dev: Use when working on the bellwether crates."), "{here}");
    assert!(!link_names(&plugin.join("skills")).contains(&"bellwether-dev".to_string()));
    let there = cc_start(&env, &plugin, "6e1d9b9f-2222-4d4d-9999-0123456789ab", &other);
    assert!(!there.contains("bellwether-dev"), "{there}");

    // Codex: the repo's trials carry it; another repo's do not.
    let c = env.run(&["--cwd", env.repo.to_str().unwrap(), "skills", "sync", "--harness", "codex"]);
    let trials: PathBuf = env.repo.join(".agents/skills/rematch-trials");
    assert!(trials.join("bellwether-dev/SKILL.md").is_file(), "{c}");
    env.run(&["--cwd", other.to_str().unwrap(), "skills", "sync", "--harness", "codex"]);
    assert!(!other.join(".agents/skills/rematch-trials/bellwether-dev").exists());

    // Unset: global again.
    let u = env.run(&["policy", "scope", "unset", "bellwether-dev"]);
    assert!(std::fs::read_link(shared.join("bellwether-dev")).is_ok() && std::fs::read_link(&native).is_ok(), "{u}");
    let s = env.run(&["--cwd", other.to_str().unwrap(), "skills", "sync"]);
    assert!(s["links"].get("bellwether-dev").is_some(), "{s}");
}
