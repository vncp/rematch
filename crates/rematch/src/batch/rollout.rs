//! One rollout: materialize a harness, run omp with tools in the fresh worktree under the
//! task's time cap, keep its JSONL event stream, verify the stream (provider+model on
//! every assistant message) and let the verifier judge the worktree.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rematch_core::trace::{fold_trace, TraceRecord};
use rematch_core::types::{EntryId, SessionRecord, Verdict};
use serde::Serialize;

use super::substrate::{CandidateTree, Install, OmpSubstrate, Substrate};
use super::tasks::Task;
use super::verifier::{is_pass, Verifier};
use crate::app::AppCtx;
use crate::omp;
use crate::proc;

/// Grace beyond the task cap before the process group is killed (omp's own
/// `--max-time` ends the session first).
pub const KILL_GRACE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Rollout {
    pub label: String,
    pub seed: u64,
    pub install: Install,
    pub records: Vec<TraceRecord>,
    pub session: SessionRecord,
    /// `None` when the rollout ran without a verifier (`create --from task`).
    pub verdict: Option<Verdict>,
    /// Stream/process failure (rollout invalid ⇒ verdict Fail without the verifier).
    pub error: Option<String>,
    pub elapsed_ms: u64,
    pub killed: bool,
    /// sha256 of `events.jsonl` in the ledger's blob store.
    pub events_blob: String,
    /// Sealed paths the rollout itself changed before the verifier ran (`Task::seal_violations`).
    pub seal_violations: Vec<String>,
}

/// Behavioral summary recorded in job progress and the receipt.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct RolloutSummary {
    pub label: String,
    pub seed: u64,
    pub session_id: String,
    pub pass: bool,
    pub verdict: Option<Verdict>,
    pub tool_calls: u32,
    pub tool_errors: u32,
    pub error_clusters: u32,
    pub turns: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub elapsed_ms: u64,
    pub killed: bool,
    pub error: Option<String>,
    pub loaded: Vec<EntryId>,
    pub skills: Vec<String>,
    pub events_blob: String,
}

impl Rollout {
    pub fn summary(&self) -> RolloutSummary {
        RolloutSummary {
            label: self.label.clone(),
            seed: self.seed,
            session_id: self.session.id.clone(),
            pass: self.verdict.as_ref().is_some_and(is_pass),
            verdict: self.verdict.clone(),
            tool_calls: self.session.tool_calls,
            tool_errors: self.session.tool_errors,
            error_clusters: self.session.error_clusters,
            turns: self.session.turns,
            tokens_in: self.session.tokens_in,
            tokens_out: self.session.tokens_out,
            elapsed_ms: self.elapsed_ms,
            killed: self.killed,
            error: self.error.clone(),
            loaded: self.install.loaded.clone(),
            skills: self.install.skills.clone(),
            events_blob: self.events_blob.clone(),
        }
    }
}

/// `runs/<task>/<seed>/<label>/`
pub fn workspace(ctx: &AppCtx, task: &Task, seed: u64, label: &str) -> PathBuf {
    super::tasks::task_runs_dir(ctx, &task.id).join(seed.to_string()).join(label)
}

#[allow(clippy::too_many_arguments)]
pub fn run_rollout(
    ctx: &AppCtx,
    sub: &OmpSubstrate,
    omp_bin: &Path,
    tree: &CandidateTree,
    task: &Task,
    seed: u64,
    label: &str,
    verifier: Option<&dyn Verifier>,
) -> Result<Rollout> {
    let dest = workspace(ctx, task, seed, label);
    let install = sub.materialize(tree, &dest).with_context(|| format!("materialize {label}"))?;
    let call = sub.rollout_cmd(&install, task, seed)?;
    let (mut cmd, input) = omp::command(omp_bin, &omp::tools_argv(&call));
    cmd.current_dir(&install.worktree)
        .env("REMATCH_SEED", seed.to_string())
        // A rollout is not an interactive session: the plugin stays inert inside it.
        .env("REMATCH", "0");
    tracing_line(&format!("rollout {label} seed={seed}: {}", omp::selector(&call.model)));
    let b = proc::run_bounded_input(cmd, task.max_time() + KILL_GRACE, input.as_deref())?;
    std::fs::write(&install.events, &b.stdout).with_context(|| format!("write {}", install.events.display()))?;
    let events_blob = ctx.ledger.put_blob(b.stdout.as_bytes())?;

    let stream = omp::parse_events(&b.stdout).and_then(|ev| omp::verify_stream(ev, &call.model, false));
    let mut error = match &stream {
        Ok(_) => None,
        Err(e) => Some(format!("{e:#}")),
    };
    if b.killed {
        error.get_or_insert_with(|| format!("killed after {} s", (task.max_time() + KILL_GRACE).as_secs()));
    } else if b.code != Some(0) && error.is_none() {
        error = Some(format!("omp exited with {:?}: {}", b.code, rematch_core::features::head(b.stderr.trim(), 512)));
    }

    let mut records = sub.read_trajectory(&install.events)?;
    for r in &mut records {
        if let TraceRecord::SessionStart { loaded_entries, model, .. } = r {
            *loaded_entries = install.loaded.iter().map(rematch_core::trace::LoadedEntry::id).collect();
            *model = Some(call.model.clone());
        }
    }
    let session_id = records
        .iter()
        .find_map(|r| match r {
            TraceRecord::SessionStart { session_id, .. } if !session_id.is_empty() => Some(session_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| format!("batch-{}-{seed}-{label}", task.id));
    let session = fold_trace(&records, &session_id);

    // Check sealed paths before running the verifier so build artifacts created by the
    // verifier itself (`cargo check` writing `Cargo.lock` / `target/`) are not attributed to
    // the rollout.
    let seal_violations = task.seal_violations(&install.worktree);
    let verdict = match (&error, verifier) {
        (_, None) => None,
        (Some(e), Some(_)) if !b.killed => Some(Verdict::Fail { score: None, detail: format!("rollout invalid: {e}") }),
        // A time-capped rollout is judged on what it left in the worktree.
        (_, Some(v)) => Some(v.check(task, &install.worktree)?),
    };
    Ok(Rollout {
        label: label.to_string(),
        seed,
        install,
        records,
        session,
        verdict,
        error,
        elapsed_ms: b.elapsed_ms,
        killed: b.killed,
        events_blob,
        seal_violations,
    })
}

/// Progress lines go to stderr (the job log); stdout carries only the envelope.
pub fn tracing_line(s: &str) {
    eprintln!("[rematch batch] {s}");
}
