//! End-to-end worker tests: fake omp (`tests/scripts/fake-omp-batch.sh`), scripted editor
//! (`tests/scripts/batch-editor.sh`), a verifier that passes only when a candidate's skill
//! made the "agent" create a marker file, in a temp git repo.

use std::path::{Path, PathBuf};
use std::process::Command;

use rematch_core::ledger::{EntryFilter, Ledger};
use rematch_core::paths::RematchPaths;
use rematch_core::types::{Origin, Status};
use serde_json::{json, Value};

use super::tasks::{cmd_task, TaskAddArgs, TaskArgs, TaskCmd};
use super::worker::{cmd_run_worker, JobSpec, RunWorkerArgs};
use crate::app::{AppCtx, IMPROVER_FILES};

fn scripts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts")
}

struct Env {
    _tmp: tempfile::TempDir,
    ctx: AppCtx,
    repo: PathBuf,
}

fn sh(dir: &Path, cmd: &str) {
    let st = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).status().unwrap();
    assert!(st.success(), "{cmd}");
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("rematch");
    let agent = tmp.path().join("agent");
    let paths = RematchPaths::with_roots(&root, &agent);
    paths.ensure_dirs().unwrap();
    for (name, text) in IMPROVER_FILES {
        std::fs::write(paths.improver.join(name), text).unwrap();
    }
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("config.yml"), "modelRoles:\n  default: fake/solver-1\n").unwrap();
    let ledger = Ledger::open(&paths.ledger_file).unwrap();
    let repo = tmp.path().join("demo-repo");
    std::fs::create_dir_all(&repo).unwrap();
    sh(
        &repo,
        "git init -q . && git config user.email t@e.st && git config user.name tester && \
         printf 'echo build\\n' > build.sh && git add . && git commit -qm init",
    );
    let ctx = AppCtx { paths, ledger, now: rematch_core::ledger::now_ms(), cwd: tmp.path().to_path_buf(), text: false };
    Env { _tmp: tmp, ctx, repo }
}

fn add_task(e: &Env) {
    cmd_task(
        &e.ctx,
        TaskArgs {
            cmd: TaskCmd::Add(TaskAddArgs {
                id: "demo".into(),
                repo: e.repo.clone(),
                prompt: "Make the check pass in the build script".into(),
                verifier: Some("test -f .rematch-marker".into()),
                max_time: Some("30s".into()),
                tools: None,
                seeds: vec![7],
            }),
        },
    )
    .unwrap();
}

/// Writes `jobs/<id>.json` the way `run` does, then runs the worker inline.
fn run_worker(e: &Env, job: &str, verifier: &str) -> Value {
    run_worker_k(e, job, verifier, 1)
}

fn run_worker_k(e: &Env, job: &str, verifier: &str, width: u32) -> Value {
    let spec = JobSpec {
        task: "demo".into(),
        verifier: verifier.into(),
        width,
        seed: 7,
        editor: Some(format!("sh {}", scripts().join("batch-editor.sh").display())),
        omp_bin: Some(scripts().join("fake-omp-batch.sh")),
    };
    let mut rec = serde_json::to_value(&spec).unwrap();
    rec["id"] = json!(job);
    rec["kind"] = json!("run");
    rec["status"] = json!("running");
    std::fs::write(e.ctx.paths.jobs.join(format!("{job}.json")), serde_json::to_string(&rec).unwrap()).unwrap();
    cmd_run_worker(&e.ctx, RunWorkerArgs { job: job.into() }).unwrap()
}

fn job_json(e: &Env, job: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(e.ctx.paths.jobs.join(format!("{job}.json"))).unwrap()).unwrap()
}

fn worktrees(repo: &Path) -> usize {
    let out = Command::new("git").arg("-C").arg(repo).args(["worktree", "list"]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().count()
}

#[test]
fn batch_worker_k1_stages_winner_then_next_run_keeps_it() {
    let e = env();
    add_task(&e);

    let out = run_worker(&e, "job-1", "test -f .rematch-marker");
    // 4 rollouts at k=1: reference, invariant, domain, composed.
    assert_eq!(out["rollouts"], 4, "{out:#}");
    assert_eq!(out["model_calls"], 3, "{out:#}");
    assert_eq!(out["decision"], "EDIT", "{out:#}");
    assert_eq!(out["winner"], "domain-1", "{out:#}");

    let job = job_json(&e, "job-1");
    assert_eq!(job["status"], "done");
    assert_eq!(job["rollouts_done"], 4);
    let verdicts: Vec<(String, bool)> = job["verdicts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["label"].as_str().unwrap().to_string(), v["pass"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        verdicts,
        vec![
            ("reference".into(), false),
            ("invariant-1".into(), false),
            ("domain-1".into(), true),
            ("composed-1-1".into(), true),
        ]
    );

    // Winner staged provisional, origin Task, pending_next_run.
    let prov = e.ctx.ledger.list_entries(&EntryFilter { status: Some(Status::Provisional), ..Default::default() }).unwrap();
    assert_eq!(prov.len(), 1, "{prov:#?}");
    let staged = &prov[0];
    assert_eq!(staged.name, "check-marker-before-yield");
    assert!(staged.pending_next_run);
    assert_eq!(staged.origin, Origin::Task { task_id: "demo".into() });
    assert_eq!(staged.branch, "demo-repo");
    assert!(staged.path.starts_with(e.ctx.paths.staging.join("demo-repo")));
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(staged.path.join("rematch.json")).unwrap()).unwrap();
    assert_eq!(on_disk["pending_next_run"], true);
    assert_eq!(e.ctx.ledger.list_receipts(Some(&staged.id)).unwrap().len(), 1);

    // Receipt written; workspaces and worktrees removed; event streams kept as blobs.
    let runs = e.ctx.paths.runs.join("demo");
    let receipt: Value = serde_json::from_str(&std::fs::read_to_string(runs.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["rollouts"].as_array().unwrap().len(), 4);
    assert_eq!(receipt["budget"]["rollouts_planned"], 4);
    assert!(receipt["proposals"].as_array().unwrap().iter().all(|p| p["lint"]["ok"] == true), "{receipt:#}");
    assert!(!runs.join("7").exists(), "workspace left behind");
    assert_eq!(worktrees(&e.repo), 1, "git worktrees left behind");
    for r in receipt["rollouts"].as_array().unwrap() {
        let blob = e.ctx.ledger.get_blob(r["events_blob"].as_str().unwrap()).unwrap().unwrap();
        assert!(String::from_utf8_lossy(&blob).contains("\"agent_end\""));
    }

    // Next run of the same task: the reference loads the pending entry, passes ⇒ kept.
    let out2 = run_worker(&e, "job-2", "test -f .rematch-marker");
    assert_eq!(out2["rollouts"], 1, "score-less pass stops after the reference: {out2:#}");
    assert_eq!(out2["decision"], "KEEP_BASE");
    assert_eq!(out2["next_run"][0]["status"], "kept", "{out2:#}");
    let kept = e.ctx.ledger.get_entry(&staged.id).unwrap().unwrap();
    assert_eq!(kept.status, Status::Kept);
    assert!(!kept.pending_next_run);
    assert!(kept.path.starts_with(e.ctx.paths.skills_dir()));
    assert!(!kept.verified_on.is_empty());
    assert_eq!(worktrees(&e.repo), 1);
}

#[test]
fn batch_next_run_failure_retires_pending_entry() {
    let e = env();
    add_task(&e);
    run_worker(&e, "job-a", "test -f .rematch-marker");
    let staged = e.ctx.ledger.list_entries(&EntryFilter { status: Some(Status::Provisional), ..Default::default() }).unwrap();
    assert_eq!(staged.len(), 1);
    // A verifier the pending entry cannot satisfy.
    let out = run_worker(&e, "job-b", "test -f .never-created");
    assert_eq!(out["next_run"][0]["status"], "retired", "{out:#}");
    assert_eq!(out["decision"], "KEEP_BASE", "nothing passes ⇒ reference: {out:#}");
    assert_eq!(e.ctx.ledger.get_entry(&staged[0].id).unwrap().unwrap().status, Status::Retired);
    assert_eq!(out["rollouts"], 4);
}

#[test]
fn batch_reference_rollout_writes_trace() {
    let e = env();
    add_task(&e);
    let path = super::worker::reference_rollout(&e.ctx, "demo", &scripts().join("fake-omp-batch.sh")).unwrap();
    assert!(path.starts_with(e.ctx.paths.runs.join("demo/traces")), "{}", path.display());
    let recs = rematch_core::trace::read_trace(&path).unwrap();
    let sid = path.file_stem().unwrap().to_string_lossy().to_string();
    let s = rematch_core::trace::fold_trace(&recs, &sid);
    assert_eq!(s.tool_calls, 1);
    assert_eq!(s.tool_errors, 1);
    assert_eq!(s.turns, 2);
    assert_eq!(s.cwd, e.repo.to_string_lossy());
    assert_eq!(s.repo.as_deref(), Some(e.repo.to_string_lossy().as_ref()));
    assert_eq!(s.model.unwrap().id, "solver-1");
    assert!(!e.ctx.paths.runs.join("demo/7").exists(), "workspace left behind");
    assert_eq!(worktrees(&e.repo), 1);
}

#[test]
fn batch_worker_width2_spends_ten_rollouts() {
    let e = env();
    add_task(&e);
    let out = run_worker_k(&e, "job-w2", "test -f .rematch-marker", 2);
    // 1 reference + 4 candidates + 4 composed + 1 confirmation.
    assert_eq!(out["rollouts"], 10, "{out:#}");
    assert_eq!(out["model_calls"], 5, "4 editors + 1 composer: {out:#}");
    let receipt: Value =
        serde_json::from_str(&std::fs::read_to_string(e.ctx.paths.runs.join("demo/receipt.json")).unwrap()).unwrap();
    let labels: Vec<&str> = receipt["rollouts"].as_array().unwrap().iter().map(|r| r["label"].as_str().unwrap()).collect();
    assert_eq!(labels.len(), 10);
    assert_eq!(labels.iter().filter(|l| l.starts_with("composed-")).count(), 4, "{labels:?}");
    assert_eq!(labels.iter().filter(|l| l.starts_with("confirm-")).count(), 1, "{labels:?}");
    let composers: Vec<&str> = receipt["variants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["composer"].as_str())
        .collect();
    assert_eq!(composers.iter().filter(|c| **c == "editor").count(), 1, "{composers:?}");
    assert_eq!(composers.iter().filter(|c| **c == "deterministic merge").count(), 3, "{composers:?}");
    assert_eq!(out["decision"], "EDIT");
    assert!(!e.ctx.paths.runs.join("demo/7").exists());
    assert_eq!(worktrees(&e.repo), 1);
}
