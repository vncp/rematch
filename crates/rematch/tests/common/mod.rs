//! Shared harness for the binary's integration tests: an isolated REMATCH_ROOT, agent dir
//! (with modelRoles), HOME and a fake git repo; runs the built binary and parses the
//! single JSON envelope it prints.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

pub const BIN: &str = env!("CARGO_BIN_EXE_rematch");

pub fn scripts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts")
}

pub fn script(name: &str) -> String {
    scripts().join(name).to_string_lossy().into_owned()
}

pub struct Env {
    pub _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub agent: PathBuf,
    pub home: PathBuf,
    pub repo: PathBuf,
    pub log: PathBuf,
}

impl Env {
    pub fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path().canonicalize().unwrap();
        let repo = t.join("work").join("payments");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let agent = t.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("config.yml"),
            "modelRoles:\n  smol: fakeprov/small-model\n  default: anthropic/claude-opus-5-5:high\n",
        )
        .unwrap();
        let home = t.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Env { root: t.join("rematch"), agent, home, repo, log: t.join("calls.log"), _tmp: tmp }
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("REMATCH_ROOT", &self.root)
            .env("PI_CODING_AGENT_DIR", &self.agent)
            .env("HOME", &self.home)
            .env("EDITOR_LOG", &self.log)
            .env("REMATCH_MACHINE", "test-host")
            .env_remove("REMATCH")
            .env_remove("OMP_BIN")
            .env_remove("REMATCH_IMPROVER")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_PLUGIN_ROOT")
            .env_remove("CODEX_HOME")
            .env_remove("REMATCH_BIN")
            .current_dir(&self.repo);
        c
    }

    pub fn run_cmd(&self, mut c: Command) -> (Value, i32) {
        let out = c.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "exactly one envelope line, got: {stdout}\nstderr: {}", String::from_utf8_lossy(&out.stderr));
        let v: Value = serde_json::from_str(lines[0]).unwrap();
        eprintln!("$ rematch … → {}", lines[0].chars().take(1500).collect::<String>());
        (v, out.status.code().unwrap_or(-1))
    }

    pub fn run(&self, args: &[&str]) -> Value {
        let (v, code) = self.run_cmd(self.cmd(args));
        assert_eq!(code, 0, "exit code for {args:?}: {v}");
        assert_eq!(v["ok"], true, "{args:?}: {v}");
        v["data"].clone()
    }

    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// Finished trace: one task input, edits, then a procedural negation redirect.
    pub fn write_trace(&self, sid: &str, loaded: &[&str], redirect: bool) {
        let repo = self.repo.to_string_lossy();
        let file = format!("{repo}/src/ledger.rs");
        let mut recs = vec![
            json!({"seq":0,"t":1_000,"ev":"session_start","session_id":sid,"cwd":repo,"repo":repo,"branch_hint":"payments",
                   "model":{"provider":"anthropic","id":"claude-opus-5-5"},"mode":"interactive","agent":{"kind":"main","name":"main"},
                   "omp_version":"18.12.6","loaded_entries":loaded,"hooks_loaded":[],"staging_dirs":[]}),
            json!({"seq":1,"t":1_001,"ev":"input","text":"Add the ledger reconciliation command to the payments crate","source":"interactive",
                   "idle":true,"after_deliverable":null,"prelabel":"none","skills_invoked":[]}),
            json!({"seq":2,"t":1_002,"ev":"tool_result","tool":"read","call_id":"c1","input":{"path":file},"args_hash":"h1","result_bytes":100,
                   "is_error":false,"error_class":null,"content_head":"pub fn reconcile() {}","skill_read":null,"path":file}),
            json!({"seq":3,"t":1_003,"ev":"tool_result","tool":"edit","call_id":"c2","input":{"path":file},"args_hash":"h2","result_bytes":10,
                   "is_error":false,"error_class":null,"content_head":"ok","skill_read":null,"path":file}),
            json!({"seq":4,"t":1_004,"ev":"turn_end","model":{"provider":"anthropic","id":"claude-opus-5-5"},
                   "usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0,"totalTokens":150,"cost":null},"stop_reason":"stop",
                   "text_head":"Added the reconcile command and committed it."}),
        ];
        if redirect {
            recs.push(json!({"seq":5,"t":1_005,"ev":"input","text":"No, run the tests before you commit; the reconcile tests fail",
                             "source":"interactive","idle":true,"after_deliverable":"Added the reconcile command and committed it.",
                             "prelabel":"negation","skills_invoked":[]}));
        } else {
            recs.push(json!({"seq":5,"t":1_005,"ev":"input","text":"Now extend the ledger reconciliation command with a dry run",
                             "source":"interactive","idle":true,"after_deliverable":"Added the reconcile command and committed it.",
                             "prelabel":"none","skills_invoked":[]}));
        }
        recs.extend([
            json!({"seq":6,"t":1_006,"ev":"tool_result","tool":"bash","call_id":"c3","input":{"command":"cargo test -p payments"},"args_hash":"h3",
                   "result_bytes":10,"is_error":false,"error_class":null,"content_head":"test result: ok","skill_read":null,"path":null}),
            json!({"seq":7,"t":1_007,"ev":"turn_end","model":{"provider":"anthropic","id":"claude-opus-5-5"},
                   "usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":null},"stop_reason":"stop","text_head":"Tests pass."}),
            json!({"seq":8,"t":1_008,"ev":"agent_end","turns":2,"is_terminal":true,"yielded":true}),
            json!({"seq":9,"t":1_009,"ev":"session_shutdown","reasons": if redirect { json!(["redirect:negation"]) } else { json!([]) },"flagged":redirect}),
        ]);
        let traces = self.root.join("traces");
        std::fs::create_dir_all(&traces).unwrap();
        let body: String = recs.iter().map(|r| format!("{r}\n")).collect();
        let path = traces.join(format!("{sid}.jsonl"));
        std::fs::write(&path, body).unwrap();
        if redirect {
            let pending = self.root.join("pending");
            std::fs::create_dir_all(&pending).unwrap();
            std::fs::write(
                pending.join(format!("{sid}.json")),
                json!({"session_id":sid,"flagged_at":1_010,"reasons":["redirect:negation"],"trace":path}).to_string(),
            )
            .unwrap();
        }
    }

    pub fn staged_dir(&self) -> PathBuf {
        self.root.join("staging").join("payments").join("run-tests-before-commit")
    }

    /// Finished trace in which the agent read `skill://<skill>`, with an optional
    /// correction (negation) redirect afterwards. No pending file.
    pub fn write_skill_trace(&self, sid: &str, skill: &str, input: &str, redirect: Option<&str>) {
        let repo = self.repo.to_string_lossy();
        let mut recs = vec![
            json!({"seq":0,"t":2_000,"ev":"session_start","session_id":sid,"cwd":repo,"repo":repo,"branch_hint":"payments",
                   "model":{"provider":"anthropic","id":"claude-opus-5-5"},"loaded_entries":[]}),
            json!({"seq":1,"t":2_001,"ev":"input","text":input,"idle":true,"prelabel":"none","skills_invoked":[]}),
            json!({"seq":2,"t":2_002,"ev":"tool_result","tool":"read","call_id":"c1","input":{"path":format!("skill://{skill}")},
                   "is_error":false,"skill_read":skill,"path":null}),
            json!({"seq":3,"t":2_003,"ev":"tool_result","tool":"edit","call_id":"c2","input":{"path":format!("{repo}/src/ledger.rs")},
                   "is_error":false,"path":format!("{repo}/src/ledger.rs")}),
            json!({"seq":4,"t":2_004,"ev":"turn_end","usage":{"input":10,"output":5},"stop_reason":"stop","text_head":"Done."}),
        ];
        if let Some(text) = redirect {
            recs.push(json!({"seq":5,"t":2_005,"ev":"input","text":text,"idle":true,"after_deliverable":"Done.","prelabel":"negation"}));
        }
        recs.push(json!({"seq":9,"t":2_009,"ev":"session_shutdown","reasons":[],"flagged":redirect.is_some()}));
        self.write_records(sid, &recs);
    }

    pub fn write_records(&self, sid: &str, recs: &[Value]) {
        let traces = self.root.join("traces");
        std::fs::create_dir_all(&traces).unwrap();
        let body: String = recs.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(traces.join(format!("{sid}.jsonl")), body).unwrap();
    }

    pub fn entry(&self, id: &str) -> Value {
        let ls = self.run(&["library", "ls"]);
        ls["entries"].as_array().unwrap().iter().find(|e| e["id"] == id).cloned().unwrap_or(Value::Null)
    }
}
