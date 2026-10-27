#![cfg_attr(not(unix), allow(dead_code, unused_imports))] // helpers of Unix-only tests
//! Harness surfaces beyond skills, through the binary: subagent entries (staging link →
//! kept copy in the agent dir → retire), `AGENTS.md` fragments through the team branch,
//! and the fork tool-guidance path set with `library set-fork-path`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{script, Env};
use rematch_core::package::SkillDoc;
use serde_json::{json, Value};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

fn init_repo(dir: &Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    for (p, c) in files {
        std::fs::write(dir.join(p), c).unwrap();
        git(dir, &["add", p]);
    }
    git(dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
}

/// `create <name> --from description` with a scripted editor, then `activate --unverified`.
fn staged(env: &Env, name: &str, description: &str, editor: &str) -> String {
    let d = env.run(&["create", name, "--from", "description", "--description", description, "--editor", &script(editor)]);
    assert_eq!(d["outcome"], "staged", "{d}");
    env.run(&["activate", name, "--unverified"]);
    d["id"].as_str().unwrap().to_string()
}

/// Four clean finished sessions that loaded `id` and match its predicate: `count` keeps it.
fn keep_by_counters(env: &Env, id: &str, input: &str) {
    for i in 0..4 {
        let sid = format!("keep-{i}");
        env.write_records(
            &sid,
            &[
                json!({"seq":0,"t":3_000 + i,"ev":"session_start","session_id":sid,"cwd":env.repo,"repo":env.repo,
                       "model":{"provider":"anthropic","id":"claude-opus-5-5"},"loaded_entries":[id]}),
                json!({"seq":1,"t":3_001 + i,"ev":"input","text":input,"idle":true,"prelabel":"none"}),
                json!({"seq":2,"t":3_002 + i,"ev":"turn_end","usage":{"input":10,"output":5},"stop_reason":"stop","text_head":"Done."}),
                json!({"seq":3,"t":3_003 + i,"ev":"session_shutdown","reasons":[],"flagged":false}),
            ],
        );
    }
    let c = env.run(&["count", "--editor", &script("editor-agent.sh")]);
    let d = c["decisions"].as_array().unwrap().iter().find(|d| d["entry"] == id).cloned().unwrap_or(Value::Null);
    assert_eq!(d["decision"], "keep", "{c}");
}

fn doc(p: &Path) -> SkillDoc {
    SkillDoc::parse(&std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
}

#[cfg(unix)] // POSIX sh fixtures
#[test]
fn agent_entries_load_through_the_plugin_root_then_the_agent_dir() {
    let env = Env::new();
    let id = staged(&env, "log-triage", "triage long build logs for the failing step", "editor-agent.sh");
    let dir = env.root.join("staging/payments/log-triage");
    assert!(dir.join("AGENT.md").is_file() && !dir.join("SKILL.md").exists(), "an agent is AGENT.md + rematch.json");

    // provisional: <root>/agents/<name>.md → the staged AGENT.md (extension/agents links to <root>/agents)
    let link = env.root.join("agents/log-triage.md");
    assert_eq!(std::fs::read_link(&link).unwrap(), dir.join("AGENT.md"));
    let d = doc(&link);
    assert_eq!(d.get_str("name"), Some("log-triage"), "omp requires name");
    assert!(d.get_str("description").is_some_and(|s| s.starts_with("Use for a long build")), "omp requires description");
    assert_eq!(d.get_str("model"), Some("@smol"));
    assert_eq!(d.get_str("tools"), Some("read, grep"));
    assert_eq!(d.rematch().unwrap().id, id);
    assert!(d.body.contains("first failing step"));

    // match lists it (so the trace's loaded_entries carries it) without a staging dir
    let m = env.run(&["match", "--cwd", env.repo.to_str().unwrap()]);
    assert_eq!(m["skill_dirs"], json!([]), "{m}");
    let me = m["entries"].as_array().unwrap().iter().find(|e| e["id"] == id.as_str()).cloned().expect("agent listed");
    assert_eq!((me["kind"].as_str(), me["status"].as_str(), me["predicate_hit"].as_bool()), (Some("agent"), Some("provisional"), Some(true)));

    // keep: AGENT.md copied to <agent_dir>/agents/<name>.md, the staging link gone
    let input = "Triage the long build logs: find the failing step in the log";
    keep_by_counters(&env, &id, input);
    let kept = env.agent.join("agents/log-triage.md");
    let k = doc(&kept);
    assert_eq!((k.get_str("name"), k.rematch().map(|b| b.status)), (Some("log-triage"), Some(rematch_core::Status::Kept)));
    assert!(std::fs::symlink_metadata(&link).is_err(), "staging link removed on keep");
    let e = env.entry(&id);
    assert_eq!(e["status"], "kept");
    assert_eq!(e["path"], json!(env.root.join("kept-agents/log-triage")));
    let m = env.run(&["match", "--cwd", env.repo.to_str().unwrap()]);
    assert!(m["entries"].as_array().unwrap().iter().any(|e| e["id"] == id.as_str() && e["kind"] == "agent" && e["status"] == "kept"));

    // disable a kept agent: omp's copy leaves the agent dir
    env.run(&["library", "disable", &id]);
    assert!(!kept.exists());

    // retire a provisional agent: the staging link goes with it
    let other = staged(&env, "other-triage", "triage long build logs elsewhere", "editor-agent.sh");
    let link2 = env.root.join("agents/other-triage.md");
    assert_eq!(doc(&link2).get_str("name"), Some("other-triage"), "the user's name wins in AGENT.md too");
    env.run(&["library", "disable", &other]);
    assert!(std::fs::symlink_metadata(&link2).is_err(), "retired agents unload");
    assert_eq!(env.entry(&other)["status"], "retired");

    // uninstall removes the link dir with the root
    let p = env.run(&["uninstall"]);
    assert!(p["plan"]["remove"].as_array().unwrap().iter().any(|x| x == &json!(env.root.join("agents"))), "{p}");
}

#[cfg(unix)] // POSIX sh fixtures
#[test]
fn team_ship_appends_the_agents_md_fragment_and_keeps_it_out_of_the_skill_dir() {
    let env = Env::new();
    init_repo(&env.repo, &[("AGENTS.md", "# Payments\n\nBe precise about money.\n")]);
    let id = staged(&env, "crate-tests-first", "run crate tests before committing", "editor-fragment.sh");
    let staged_dir = env.root.join("staging/payments/crate-tests-first");
    assert!(staged_dir.join("AGENTS.fragment.md").is_file());
    let s = env.run(&["ship", "--scope", "team", "--entry", &id, "--repo", env.repo.to_str().unwrap()]);
    let shipped = &s["shipped"][0];
    let branch = shipped["branch"].as_str().unwrap();
    assert!(shipped["files"].as_array().unwrap().iter().any(|f| f == "AGENTS.md"), "{s}");
    let files = git(&env.repo, &["ls-tree", "-r", "--name-only", branch]);
    assert!(files.lines().any(|f| f == ".omp/skills/crate-tests-first/SKILL.md"), "{files}");
    assert!(!files.contains(".omp/skills/crate-tests-first/AGENTS.fragment.md"), "the fragment reaches only AGENTS.md: {files}");
    let agents = git(&env.repo, &["show", &format!("{branch}:AGENTS.md")]);
    assert!(agents.starts_with("# Payments\n\nBe precise about money.\n\n## rematch: crate-tests-first\n\n"), "{agents}");
    assert!(agents.contains("Run `cargo test -p <crate>` for every crate you touched"));
    assert!(agents.contains("### Why"));
    assert_eq!(agents.matches("## rematch: crate-tests-first").count(), 1);
    assert_eq!(std::fs::read_to_string(env.repo.join("AGENTS.md")).unwrap(), "# Payments\n\nBe precise about money.\n", "checkout untouched");
}

#[cfg(unix)] // POSIX sh fixtures
#[test]
fn set_fork_path_then_ship_fork_writes_the_file() {
    let env = Env::new();
    let fork: PathBuf = env.root.parent().unwrap().join("omp-fork");
    init_repo(&fork, &[]);
    let id = staged(&env, "crate-tests-first", "run crate tests before committing", "editor-fragment.sh");
    // the editor proposed a fork_path; set-fork-path overrides it and records the fork
    assert_eq!(env.entry(&id)["fork_path"], "docs/prompts/team.md");
    let (v, code) = env.run_cmd(env.cmd(&["library", "set-fork-path", &id, "../escape.md"]));
    assert_eq!(code, 1, "{v}");
    env.run(&["library", "set-fork-path", &id, "src/prompts/system.md", "--repo", fork.to_str().unwrap()]);
    let rj: Value = serde_json::from_str(
        &std::fs::read_to_string(env.root.join("staging/payments/crate-tests-first/rematch.json")).unwrap(),
    )
    .unwrap();
    assert_eq!((rj["fork_path"].as_str(), rj["fork_repo"].as_str()), (Some("src/prompts/system.md"), fork.to_str()));
    let out = env.cmd(&["library", "ls", "--text"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("crate-tests-first [payments]") && text.contains("fork:src/prompts/system.md"), "{text}");

    // no --repo: the recorded fork_repo is the target
    let s = env.run(&["ship", "--scope", "fork", "--entry", &id]);
    let branch = s["shipped"][0]["branch"].as_str().unwrap().to_string();
    let file = git(&fork, &["show", &format!("{branch}:src/prompts/system.md")]);
    assert!(file.starts_with("Use when committing changes to a Rust crate."), "SKILL.md body: {file}");
    assert!(!file.contains("name: crate-tests-first"), "frontmatter stripped");
    assert_eq!(env.entry(&id)["fork_repo"], json!(fork));
}
