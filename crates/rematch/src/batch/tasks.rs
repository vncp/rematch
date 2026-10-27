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
    /// Repo-relative paths a rollout may change: the task's solution scope. Every other path
    /// (tracked or new, not git-ignored) is sealed, and verifier inputs ([`is_protected`])
    /// stay sealed even when listed. Empty ⇒ no comparison of this task can count.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editable: Vec<String>,
    /// The user's standing approval for engine-run bounded probes of this exact definition
    /// (`crate::probes`). Bound to [`Task::fingerprint`]: a redefined task needs a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_probe: Option<ReplayAuthorization>,
}

/// Approval to replay a task automatically (one baseline + one candidate rollout per
/// candidate version, `crate::probes` caps) with exactly its repo, prompt, verifier, tools,
/// time cap and seeds. A rollout runs in a fresh git worktree of the repo's HEAD; that is
/// isolation from the active checkout, not an OS sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayAuthorization {
    pub approved_at: i64,
    /// [`Task::fingerprint`] at approval.
    pub task_sha256: String,
    /// `probe:<suggestion id>` the user approved.
    pub action: String,
    /// sha256 of the full snapshot the user reviewed (`probes`: task, entry, seal, caps).
    #[serde(default)]
    pub snapshot_sha256: String,
}

impl Task {
    pub fn max_time(&self) -> Duration {
        Duration::from_secs(self.max_time_secs.max(1))
    }

    pub fn first_seed(&self) -> u64 {
        self.seeds.first().copied().unwrap_or(DEFAULT_SEED)
    }

    /// sha256 of what an authorization covers: repo, prompt, verifier, tools, time cap,
    /// seeds, editable paths.
    pub fn fingerprint(&self) -> String {
        let v = json!({
            "repo": self.repo, "prompt": self.prompt, "verifier": self.verifier, "tools": self.tools,
            "max_time_secs": self.max_time_secs, "seeds": self.seeds, "editable": self.editable,
        });
        rematch_core::ledger::sha256_hex(v.to_string().as_bytes())
    }

    /// Paths a rollout's judge depends on and no rollout may change even when declared
    /// editable: any file the verifier spec references that exists in the task repo (scripts,
    /// fixtures, a parity verifier's expected file), test code (test/spec dirs, `*_test.*`,
    /// `test_*`, `*.test.*`, `*.spec.*`, `tests.rs`, `test.rs`), test/build configuration and
    /// shell scripts. A referenced path absent from the repo (a marker the run must create)
    /// is the run's output and may be declared editable.
    pub fn is_protected(&self, path: &str) -> bool {
        let base = path.rsplit('/').next().unwrap_or(path);
        let lower = path.to_ascii_lowercase();
        let named = verifier_paths(self.verifier.as_deref().unwrap_or(""))
            .iter()
            .any(|p| p == path && (self.repo.join(p).is_file() || Path::new(p).extension().is_some_and(|x| {
                matches!(x.to_str(), Some("py" | "sh" | "js" | "mjs" | "cjs" | "ts" | "rb" | "pl"))
            })));
        named
            || lower.split('/').any(|seg| matches!(seg, "test" | "tests" | "spec" | "specs" | "__tests__" | "scripts"))
            || base.contains("_test.")
            || base.starts_with("test_")
            || base.contains(".test.")
            || base.contains(".spec.")
            || base.ends_with(".sh")
            || matches!(
                base,
                "tests.rs" | "test.rs" | "conftest.py" | "Makefile" | "makefile" | "Cargo.toml" | "Cargo.lock" | "package.json"
                    | "pyproject.toml" | "setup.cfg" | "setup.py" | "pytest.ini" | "tox.ini" | "go.mod" | "go.sum"
                    | "bunfig.toml" | "build.rs"
            )
    }

    /// Paths a rollout may write: declared editable and not protected, plus a parity
    /// verifier's produced file and a receipt verifier's receipt (the run's declared output).
    fn may_change(&self, path: &str) -> bool {
        let spec = self.verifier.as_deref().unwrap_or("").trim();
        let output = spec
            .strip_prefix("parity:")
            .and_then(|r| r.split('=').next())
            .or_else(|| spec.strip_prefix("receipt:"))
            .map(|p| p.trim().trim_start_matches("./"));
        output == Some(path) || (self.editable.iter().any(|e| e == path) && !self.is_protected(path))
    }

    /// Sealed paths a rollout changed in its worktree (`git status --porcelain -z`, both
    /// sides of a rename or copy, untracked included, ignored excluded). Non-empty ⇒ the
    /// run's verdict is no comparison evidence.
    pub fn seal_violations(&self, worktree: &Path) -> Vec<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["-c", "core.quotepath=off", "status", "--porcelain=v1", "-z", "--untracked-files=all"])
            .output();
        let Some(out) = out.ok().filter(|o| o.status.success()) else {
            return vec!["(git status failed: the worktree cannot be checked)".into()];
        };
        porcelain_z_paths(&out.stdout).into_iter().filter(|p| !self.may_change(p)).collect()
    }

    /// What the seal covers, for receipts and actions.
    pub fn seal_scope(&self) -> String {
        if self.editable.is_empty() {
            return if self.read_only() {
                format!(
                    "read-only task (tools {}): every repo path (tracked or new, not git-ignored) is sealed; any workspace change yields refused/incomplete evidence (verifier output files excepted). Git-ignored paths and anything outside the worktree are not checked; this is not an OS sandbox",
                    self.tools.as_deref().unwrap_or("")
                )
            } else {
                "no editable paths declared and the tools can write: comparisons of this task cannot count".into()
            };
        }
        format!(
            "editable: {}; every other repo path (tracked or new, not git-ignored) is sealed, and verifier inputs (files named in the verifier, test code, test/build config, scripts) stay sealed even if listed. A rollout changing a sealed path yields refused/incomplete evidence. Git-ignored paths and anything outside the worktree are not checked; this is not an OS sandbox",
            self.editable.join(", ")
        )
    }

    /// Explicitly read-only tools (`read`, `grep`, `glob` only). `None` (omp's default tool
    /// set) is not read-only.
    pub fn read_only(&self) -> bool {
        self.tools.as_deref().is_some_and(|t| {
            let tools: Vec<&str> = t.split(',').map(str::trim).filter(|x| !x.is_empty()).collect();
            !tools.is_empty() && tools.iter().all(|x| matches!(x.to_ascii_lowercase().as_str(), "read" | "grep" | "glob"))
        })
    }

    /// Whether the judge can be sealed: declared editable paths, or an empty allowlist under
    /// explicitly read-only tools (everything sealed).
    pub fn sealable(&self) -> bool {
        !self.editable.is_empty() || self.read_only()
    }

    /// An authorization that still matches this definition.
    pub fn authorized(&self) -> bool {
        self.auto_probe.as_ref().is_some_and(|a| a.task_sha256 == self.fingerprint())
    }
}

/// Paths of `git status --porcelain=v1 -z` output: `XY <path>\0`, renames and copies add
/// `<orig path>\0` (both returned).
pub fn porcelain_z_paths(out: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(f) = fields.next() {
        if f.len() < 4 {
            continue;
        }
        paths.push(String::from_utf8_lossy(&f[3..]).into_owned());
        if matches!(f[0], b'R' | b'C') || matches!(f[1], b'R' | b'C') {
            if let Some(orig) = fields.next() {
                paths.push(String::from_utf8_lossy(orig).into_owned());
            }
        }
    }
    paths
}

/// Repo-relative paths a verifier spec names: shell-style words (quotes removed), `script:`
/// and `parity:`/`receipt:` prefixes stripped, `=`-separated parity sides split.
pub fn verifier_paths(spec: &str) -> Vec<String> {
    let spec = spec.trim();
    let body = spec
        .strip_prefix("script:")
        .or_else(|| spec.strip_prefix("parity:"))
        .or_else(|| spec.strip_prefix("receipt:"))
        .unwrap_or(spec);
    let mut words: Vec<String> = Vec::new();
    let (mut cur, mut quote) = (String::new(), None::<char>);
    for c in body.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, c) if c.is_whitespace() || matches!(c, '=' | ';' | '|' | '&' | '<' | '>' | '(' | ')') => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
        .into_iter()
        .map(|w| w.trim_start_matches("./").replace('\\', "/"))
        .filter(|w| !w.is_empty() && !w.starts_with('-') && !w.starts_with('/') && !w.contains(".."))
        .collect()
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
    /// Repo-relative path a rollout may change (repeatable): the solution scope. Without one,
    /// runs still execute but mint no comparative evidence.
    #[arg(long = "editable")]
    pub editable: Vec<String>,
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
        editable: normalize_editable(&a.editable)?,
        // Redefining a task withdraws its replay authorization.
        auto_probe: None,
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

/// `--editable` paths: repo-relative, no traversal, `/`-separated, deduplicated, sorted.
pub fn normalize_editable(paths: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for p in paths {
        let n = p.trim().replace('\\', "/").trim_start_matches("./").to_string();
        if n.is_empty() || n.starts_with('/') || n.contains(':') || n.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
            bail!(Usage(format!("--editable {p:?}: a repo-relative file path without `..`")));
        }
        out.push(n);
    }
    out.sort();
    out.dedup();
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

    #[test]
    fn porcelain_z_reads_renames_and_spaces() {
        let out = b" M src/a b.rs\0R  new name.rs\0old name.rs\0?? data/expected.txt\0";
        assert_eq!(porcelain_z_paths(out), vec!["src/a b.rs", "new name.rs", "old name.rs", "data/expected.txt"]);
    }

    #[test]
    fn verifier_paths_and_protection() {
        assert_eq!(verifier_paths("script:python3 'tools/my check.py' --fast"), vec!["python3", "tools/my check.py"]);
        assert_eq!(verifier_paths("parity:out.csv=./data/expected.csv"), vec!["out.csv", "data/expected.csv"]);
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("data")).unwrap();
        std::fs::write(d.path().join("data/expected.csv"), "x").unwrap();
        let t = Task {
            id: "t".into(),
            repo: d.path().to_path_buf(),
            prompt: "p".into(),
            verifier: Some("parity:out.csv=data/expected.csv".into()),
            max_time_secs: 30,
            tools: None,
            seeds: vec![1],
            created_at: 0,
            editable: vec!["data/expected.csv".into(), "out.csv".into(), "src/tests.rs".into(), "src/lib.rs".into()],
            auto_probe: None,
        };
        assert!(t.is_protected("data/expected.csv"), "the expected file in the repo");
        assert!(t.is_protected("src/tests.rs") && t.is_protected("crates/x/src/test.rs"));
        assert!(!t.is_protected("out.csv") && !t.is_protected("src/lib.rs"));
        assert!(t.may_change("out.csv") && t.may_change("src/lib.rs"));
        assert!(!t.may_change("data/expected.csv") && !t.may_change("src/tests.rs"), "protected even when listed");
    }
}
