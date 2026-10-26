//! `run --task <id> --verifier <cmd> [--width k] [--seed n]`: validate, then hand the
//! cycle to a detached `run-worker --job <id>` (DESIGN §2). Returns `{job_id}`.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use super::tasks::load_task;
use super::verifier::parse_spec;
use super::worker::{model_call_budget, rollout_budget, JobSpec, MAX_WIDTH};
use crate::app::AppCtx;
use crate::envelope::Usage;
use crate::jobs;
use crate::omp;

pub const JOB_KIND: &str = "run";

#[derive(Debug, Clone, clap::Args)]
pub struct RunArgs {
    #[arg(long)]
    pub task: String,
    /// Verifier spec (tests | parity:<produced>=<expected> | receipt:<path> | [script:]<cmd>);
    /// defaults to the task's verifier.
    #[arg(long)]
    pub verifier: Option<String>,
    /// Candidates per editor scope (rollouts: (k+1)² + [k>1]).
    #[arg(long, default_value_t = 1)]
    pub width: u32,
    /// Rollout seed (default: the task's first seed).
    #[arg(long)]
    pub seed: Option<u64>,
    /// Scripted editor command (tests): prompt on stdin, reply on stdout.
    #[arg(long, hide = true)]
    pub editor: Option<String>,
}

/// Validated job payload for `args`.
pub fn job_spec(ctx: &AppCtx, args: &RunArgs) -> Result<JobSpec> {
    if ctx.paths.is_off() {
        bail!("rematch is off (`on` to resume); batch runs stage entries and are refused");
    }
    let task = load_task(ctx, &args.task)?;
    if !task.repo.is_dir() {
        bail!("task {} repo {} is missing", task.id, task.repo.display());
    }
    let verifier = match args.verifier.clone().or_else(|| task.verifier.clone()) {
        Some(v) => v,
        None => bail!(Usage("batch mode requires a verifier: pass --verifier or define it with `task add --verifier`".into())),
    };
    parse_spec(&verifier).map_err(|e| Usage(format!("--verifier: {e:#}")))?;
    if args.width == 0 || args.width > MAX_WIDTH {
        bail!(Usage(format!("--width must be 1..={MAX_WIDTH}")));
    }
    Ok(JobSpec {
        task: task.id.clone(),
        verifier,
        width: args.width,
        seed: args.seed.unwrap_or_else(|| task.first_seed()),
        editor: args.editor.clone(),
        omp_bin: Some(omp::omp_bin()),
    })
}

pub fn cmd_run(ctx: &AppCtx, args: RunArgs) -> Result<Value> {
    let spec = job_spec(ctx, &args)?;
    let payload = serde_json::to_value(&spec)?;
    let argv = ["run-worker", "--job", jobs::JOB_ID_TOKEN].map(String::from);
    let job_id = jobs::spawn_detached(ctx, JOB_KIND, &argv, &payload)?;
    Ok(json!({
        "job_id": job_id,
        "task": spec.task,
        "verifier": spec.verifier,
        "width": spec.width,
        "seed": spec.seed,
        "rollouts_budget": rollout_budget(spec.width),
        "model_calls_budget": model_call_budget(spec.width),
    }))
}
