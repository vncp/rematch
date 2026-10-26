//! Batch task definitions (`tasks/<id>.json`, DESIGN §1) and `task add|ls`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app::{absolute, AppCtx};
use crate::envelope::Usage;

/// Rollout time cap when `task add` gets no `--max-time`.
pub const DEFAULT_MAX_TIME_SECS: u64 = 600;
/// Seed used when a task lists none.
pub const DEFAULT_SEED: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// Absolute path of the repository the task runs in.
    pub repo: PathBuf,
    pub prompt: String,
    /// Default verifier spec for `run` (see `verifier::parse_spec`).
    #[serde(default)]
    pub verifier: Option<String>,
    pub max_time_secs: u64,
    /// omp `--tools` list; `None` ⇒ omp's default tool set.
    #[serde(default)]
    pub tools: Option<String>,
    #[serde(default)]
    pub seeds: Vec<u64>,
    pub created_at: i64,
}

impl Task {
    pub fn max_time(&self) -> Duration {
        Duration::from_secs(self.max_time_secs.max(1))
    }

    pub fn first_seed(&self) -> u64 {
        self.seeds.first().copied().unwrap_or(DEFAULT_SEED)
    }
}

#[derive(Debug, Clone, clap::Args)]
pub struct TaskArgs {
    #[command(subcommand)]
    pub cmd: TaskCmd,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum TaskCmd {
    /// Define (or redefine) a batch task.
    Add(TaskAddArgs),
    /// List task definitions.
    Ls,
}

#[derive(Debug, Clone, clap::Args)]
pub struct TaskAddArgs {
    pub id: String,
    #[arg(long)]
    pub repo: PathBuf,
    #[arg(long)]
    pub prompt: String,
    #[arg(long)]
    pub verifier: Option<String>,
    /// Per-rollout time cap: `600`, `90s`, `10m`, `1h`.
    #[arg(long = "max-time")]
    pub max_time: Option<String>,
    /// omp `--tools` list for rollouts.
    #[arg(long)]
    pub tools: Option<String>,
    /// Rollout seed (repeatable); exported to rollouts and verifiers as `REMATCH_SEED`.
    #[arg(long = "seed")]
    pub seeds: Vec<u64>,
}

pub fn cmd_task(ctx: &AppCtx, args: TaskArgs) -> Result<Value> {
    match args.cmd {
        TaskCmd::Add(a) => cmd_task_add(ctx, a),
        TaskCmd::Ls => Ok(json!({ "tasks": list_tasks(ctx)? })),
    }
}

pub fn cmd_task_add(ctx: &AppCtx, a: TaskAddArgs) -> Result<Value> {
    validate_id(&a.id)?;
    let repo = absolute(&a.repo)?;
    if !repo.is_dir() {
        bail!(Usage(format!("--repo {} is not a directory", repo.display())));
    }
    if a.prompt.trim().is_empty() {
        bail!(Usage("--prompt is empty".into()));
    }
    if let Some(v) = &a.verifier {
        super::verifier::parse_spec(v).map_err(|e| Usage(format!("--verifier: {e:#}")))?;
    }
    let max_time_secs = match &a.max_time {
        Some(s) => parse_duration_secs(s).map_err(|e| Usage(format!("--max-time: {e:#}")))?,
        None => DEFAULT_MAX_TIME_SECS,
    };
    let task = Task {
        id: a.id,
        repo,
        prompt: a.prompt,
        verifier: a.verifier,
        max_time_secs,
        tools: a.tools.filter(|t| !t.trim().is_empty()),
        seeds: if a.seeds.is_empty() { vec![DEFAULT_SEED] } else { a.seeds },
        created_at: ctx.now,
    };
    save_task(ctx, &task)?;
    Ok(json!({ "task": task, "path": task_path(ctx, &task.id) }))
}

pub fn task_path(ctx: &AppCtx, id: &str) -> PathBuf {
    ctx.paths.tasks.join(format!("{id}.json"))
}

pub fn save_task(ctx: &AppCtx, task: &Task) -> Result<()> {
    std::fs::create_dir_all(&ctx.paths.tasks)?;
    let p = task_path(ctx, &task.id);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(task)?)?;
    std::fs::rename(&tmp, &p).with_context(|| format!("write {}", p.display()))
}

pub fn load_task(ctx: &AppCtx, id: &str) -> Result<Task> {
    validate_id(id)?;
    let p = task_path(ctx, id);
    let text = std::fs::read_to_string(&p).map_err(|_| anyhow!("no task {id:?} (define it with `task add`)"))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", p.display()))
}

pub fn list_tasks(ctx: &AppCtx) -> Result<Vec<Task>> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&ctx.paths.tasks) else { return Ok(out) };
    for ent in rd {
        let p = ent?.path();
        if p.extension().is_some_and(|e| e == "json") {
            let text = std::fs::read_to_string(&p)?;
            out.push(serde_json::from_str(&text).with_context(|| format!("parse {}", p.display()))?);
        }
    }
    out.sort_by(|a: &Task, b: &Task| a.id.cmp(&b.id));
    Ok(out)
}

/// Task ids name files and `runs/<task>/` directories.
pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || id.starts_with('.')
        || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!(Usage(format!("invalid task id {id:?} (use [A-Za-z0-9._-])")));
    }
    Ok(())
}

/// `600` | `90s` | `10m` | `1h` → seconds (> 0).
pub fn parse_duration_secs(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        _ => (s, 1),
    };
    let n: u64 = num.trim().parse().map_err(|_| anyhow!("bad duration {s:?}"))?;
    if n == 0 {
        bail!("duration must be > 0");
    }
    Ok(n * mult)
}

/// `runs/<task>/`
pub fn task_runs_dir(ctx: &AppCtx, task_id: &str) -> PathBuf {
    ctx.paths.runs.join(task_id)
}

/// Whether `p` is inside a git work tree (`git -C p rev-parse`).
pub fn is_git_repo(p: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(p)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration_secs("600").unwrap(), 600);
        assert_eq!(parse_duration_secs("90s").unwrap(), 90);
        assert_eq!(parse_duration_secs("10m").unwrap(), 600);
        assert_eq!(parse_duration_secs("1h").unwrap(), 3600);
        assert!(parse_duration_secs("0").is_err());
        assert!(parse_duration_secs("abc").is_err());
    }

    #[test]
    fn ids() {
        assert!(validate_id("fix-tests_1.a").is_ok());
        for bad in ["", "../x", "a/b", ".hidden", "a b"] {
            assert!(validate_id(bad).is_err(), "{bad}");
        }
    }
}
