//! Bounded automatic probes (DESIGN §7 "Comparative support", §18): one engine-run reference /
//! candidate comparison per provisional entry version, in detached `reflect --pending` after a
//! session ends.
//!
//! Evidence semantics. A probe runs a task twice through the existing batch substrate (omp, a
//! fresh git worktree of the repo's HEAD, skills overlay, memory off): the baseline loads the
//! kept/pinned library only, the candidate the same library plus the entry. Task, prompt,
//! verifier, tools, model, seed and base commit are identical; the verifier is fixed by the
//! engine or user, so a candidate can neither choose nor edit it. The outcome
//! ([`ComparativeOutcome`]) is engine-classified from the two verdicts: `supports` (candidate
//! passed where baseline failed, or scored higher) immediately keeps the entry when no
//! recurrence or harm was recorded (`counters::keep_reason`), `regresses` immediately retires
//! it, `tie`/`cost_worse` decide nothing (observational acceptance may still keep it),
//! `incomplete` (a failed, killed or model-drifted rollout, or a moved base) never promotes.
//!
//! Eligibility. Skills, rules and facts whose files the omp substrate materializes (no
//! `AGENTS.fragment.md`, no `scripts/`); hooks, agents, scripts and instruction fragments are
//! host- or execution-specific and stay on the observed-use path. Probes run **automatically**,
//! without a `probe:` approval, when the task replays a recorded session of that repo in an
//! isolated worktree (`suggest_for` trace grounding), the verifier is engine-chosen and sealed
//! (the repo's detected fast check, detected test runner, or an observed plain test-runner
//! command), and the run fits the budget (`policy.json` `probes.daily_runs`, default 3;
//! `probes.max_parallel`, default 1; per-rollout time cap ≤ [`PROBE_MAX_TIME_SECS`]). The
//! `probe:<entry id>` review action is kept only for probes that use a user-supplied task/verifier
//! or exceed rollout caps (one-tap hash-bound review).
//!
//! Caps: [`PROBE_ROLLOUTS`] rollouts per probe, each ≤ [`PROBE_MAX_TIME_SECS`] (+ the
//! rollout kill grace), [`PROBES_PER_PASS`] per reflect pass (or `probes.max_parallel`),
//! `probes.daily_runs` (default [`PROBES_PER_DAY`]) per 24 h (`0` = off), no editor or other
//! model calls, one probe per (entry, version, content); a global lock and the task's run lock;
//! SIGINT/SIGTERM cancel the running rollout (recorded `incomplete`).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use rematch_core::features::head;
use rematch_core::ledger::EntryFilter;
use rematch_core::predicate::{matches_place, Place};
use rematch_core::trace::{self, TraceRecord};
use rematch_core::types::{
    ComparativeEvidence, ComparativeOutcome, Entry, EntryId, Kind, ModelRef, Origin, Sha256Hex, Status, UnixMs,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::actions::{surface_of, Action, ActionEntry, Commands, PROBE};
use crate::app::AppCtx;
use crate::batch::rollout::{run_rollout, Rollout, RolloutSummary};
use crate::batch::substrate::{read_entry_files, BaseLibrary, CandidateTree, OmpSubstrate, Substrate, TreeEntry};
use crate::batch::tasks::{is_git_repo, list_tasks, save_task, task_runs_dir, validate_id, ReplayAuthorization, Task};
use crate::batch::verifier::{default_spec_for, is_pass, parse_spec, resolve_spec_for_task, score_of, Verifier};
use crate::config::{resolve_role, ROLE_SOLVER};
use crate::envelope::Cancelled;
use crate::jobs::LockFile;
use crate::omp;

/// Rollouts per probe: one baseline, one candidate.
pub const PROBE_ROLLOUTS: u32 = 2;
/// Host `--max-time` per rollout (the task's own when lower). The engine's outer deadline
/// adds `batch::rollout::KILL_GRACE` before tearing the process tree down.
pub const PROBE_MAX_TIME_SECS: u64 = 300;
/// Default probes one reflect pass runs (overridden by `policy.json` `probes.max_parallel`).
pub const PROBES_PER_PASS: usize = 1;
/// Default probes started in any 24 hours (`policy.json` `probes.daily_runs`).
pub const PROBES_PER_DAY: usize = rematch_core::domain::DEFAULT_PROBE_DAILY_RUNS;
/// The execution harness every probe runs in.
pub const HARNESS: &str = "omp";
/// Tools of a suggested task with an editable scope: no shell, no network.
pub const SUGGESTED_TOOLS: &str = "read,grep,glob,edit,write";
/// Tools of a suggested read-only (review/analysis) task: every repo path sealed.
pub const READ_ONLY_TOOLS: &str = "read,grep,glob";
/// Test-runner invocations an observed command may be adopted as a verifier from.
const TEST_RUNNERS: &[&str] = &[
    "cargo test", "cargo nextest run", "pytest", "python -m pytest", "python3 -m pytest", "bun test", "npm test",
    "npm run test", "pnpm test", "yarn test", "go test", "make test",
];
const DAY_MS: UnixMs = 24 * 60 * 60 * 1000;

/// Probe status of one entry version (persisted, latest per entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    /// Replayable kind, no authorized task covers its repo: a `probe:` action waits.
    AwaitingAuthorization,
    /// Not run, with the reason (unsupported kind, dirty repo, cap, declined, …).
    Skipped,
    Supports,
    Regresses,
    Tie,
    CostWorse,
    Incomplete,
}

impl ProbeStatus {
    fn outcome(self) -> Option<ComparativeOutcome> {
        Some(match self {
            ProbeStatus::Supports => ComparativeOutcome::Supports,
            ProbeStatus::Regresses => ComparativeOutcome::Regresses,
            ProbeStatus::Tie => ComparativeOutcome::Tie,
            ProbeStatus::CostWorse => ComparativeOutcome::CostWorse,
            ProbeStatus::Incomplete => ComparativeOutcome::Incomplete,
            _ => return None,
        })
    }

    fn of(o: ComparativeOutcome) -> ProbeStatus {
        match o {
            ComparativeOutcome::Supports => ProbeStatus::Supports,
            ComparativeOutcome::Regresses => ProbeStatus::Regresses,
            ComparativeOutcome::Tie => ProbeStatus::Tie,
            ComparativeOutcome::CostWorse => ProbeStatus::CostWorse,
            ComparativeOutcome::Incomplete => ProbeStatus::Incomplete,
        }
    }
}

/// `probes/records/<entry id>.json`: the latest probe state of an entry. Engine-written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeRecord {
    pub id: String,
    pub entry: EntryId,
    pub name: String,
    pub version: u32,
    #[serde(default)]
    pub content_sha256: Option<Sha256Hex>,
    pub status: ProbeStatus,
    #[serde(default)]
    pub reason: String,
    /// `probe` (this module) or `batch` (a `run` winner over its reference rollout).
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub verifier: Option<String>,
    #[serde(default)]
    pub harness: String,
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub seed: Option<u64>,
    /// The base commit both rollouts ran.
    #[serde(default)]
    pub base_head: Option<String>,
    /// Library entry ids both harnesses shared (the baseline).
    #[serde(default)]
    pub base_entries: Vec<EntryId>,
    #[serde(default)]
    pub baseline: Option<RolloutSummary>,
    #[serde(default)]
    pub candidate: Option<RolloutSummary>,
    /// Full record in the ledger blob store.
    #[serde(default)]
    pub blob: Option<Sha256Hex>,
    pub at: UnixMs,
}

impl ProbeRecord {
    fn new(e: &Entry, status: ProbeStatus, reason: impl Into<String>, at: UnixMs) -> ProbeRecord {
        ProbeRecord {
            id: probe_id(e),
            entry: e.id.clone(),
            name: e.name.clone(),
            version: e.version,
            content_sha256: e.content_sha256.clone(),
            status,
            reason: reason.into(),
            source: "probe".into(),
            task: None,
            verifier: None,
            harness: HARNESS.into(),
            model: None,
            seed: None,
            base_head: None,
            base_entries: Vec::new(),
            baseline: None,
            candidate: None,
            blob: None,
            at,
        }
    }

    /// A comparison ran (or was cut short): the entry version's one evaluation is spent.
    pub fn spent(&self) -> bool {
        self.status.outcome().is_some()
    }

    fn binds(&self, e: &Entry) -> bool {
        self.version == e.version && self.content_sha256 == e.content_sha256
    }
}

/// `<entry short id>-v<version>-<content sha12>`.
fn probe_id(e: &Entry) -> String {
    let sha = e.content_sha256.as_deref().map(|s| &s[..s.len().min(12)]).unwrap_or("nosha");
    format!("{}-v{}-{sha}", crate::scope::short_id(&e.id), e.version)
}

fn dir(ctx: &AppCtx) -> PathBuf {
    ctx.paths.root.join("probes")
}

fn record_path(ctx: &AppCtx, entry: &str) -> PathBuf {
    dir(ctx).join("records").join(format!("{}.json", rematch_core::paths::id_file_stem(entry)))
}

fn suggestion_path(ctx: &AppCtx, entry: &str) -> PathBuf {
    dir(ctx).join("suggestions").join(format!("{}.json", rematch_core::paths::id_file_stem(entry)))
}

fn write_json<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(v)?)?;
    std::fs::rename(&tmp, p).with_context(|| format!("write {}", p.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(p: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

pub fn record_of(ctx: &AppCtx, entry: &str) -> Option<ProbeRecord> {
    read_json(&record_path(ctx, entry))
}

fn save_record(ctx: &AppCtx, r: &ProbeRecord) -> Result<()> {
    write_json(&record_path(ctx, &r.entry), r)
}

/// The comparison `counters::decide` may use for `e`: a spent record of its current version
/// and content.
pub fn evidence_for(ctx: &AppCtx, e: &Entry) -> Option<ComparativeEvidence> {
    let r = record_of(ctx, &e.id).filter(|r| r.binds(e))?;
    Some(ComparativeEvidence {
        probe: r.id.clone(),
        outcome: r.status.outcome()?,
        version: r.version,
        content_sha256: r.content_sha256.clone(),
        at: r.at,
    })
}

/// Every persisted probe record, newest first (status/digest/library surfaces).
pub fn statuses(ctx: &AppCtx) -> Result<Vec<Value>> {
    let mut out: Vec<ProbeRecord> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir(ctx).join("records")) {
        for ent in rd.flatten() {
            if let Some(r) = read_json::<ProbeRecord>(&ent.path()) {
                out.push(r);
            }
        }
    }
    out.sort_by(|a, b| b.at.cmp(&a.at).then(a.entry.cmp(&b.entry)));
    Ok(out.iter().map(status_json).collect())
}

fn status_json(r: &ProbeRecord) -> Value {
    let side = |s: &Option<RolloutSummary>| {
        s.as_ref().map(|s| {
            json!({"pass": s.pass, "score": s.verdict.as_ref().and_then(score_of), "tool_errors": s.tool_errors,
                   "turns": s.turns, "tokens": s.tokens_in + s.tokens_out, "elapsed_ms": s.elapsed_ms, "killed": s.killed,
                   "error": s.error})
        })
    };
    json!({
        "id": r.id, "entry": r.entry, "name": r.name, "version": r.version, "content_sha256": r.content_sha256,
        "status": r.status, "reason": r.reason, "source": r.source, "task": r.task, "verifier": r.verifier,
        "harness": r.harness, "model": r.model, "seed": r.seed, "base_head": r.base_head,
        "baseline": side(&r.baseline), "candidate": side(&r.candidate), "blob": r.blob, "at": r.at,
        "line": line_of(r),
    })
}

fn line_of(r: &ProbeRecord) -> String {
    let what = match r.status {
        ProbeStatus::AwaitingAuthorization => "awaiting replay authorization",
        ProbeStatus::Skipped => "probe skipped",
        ProbeStatus::Supports => "comparative support",
        ProbeStatus::Regresses => "comparison regressed",
        ProbeStatus::Tie => "comparison tied",
        ProbeStatus::CostWorse => "comparison tied at higher cost",
        ProbeStatus::Incomplete => "comparison incomplete",
    };
    let on = r.task.as_deref().map(|t| format!(" on task {t} ({})", r.harness)).unwrap_or_default();
    let undo = match r.status {
        ProbeStatus::Supports => format!(" (`rematch library disable {}` or `rematch library rollback {}`)", r.entry, r.name),
        ProbeStatus::Regresses => format!(" (`rematch library why` / `rematch library disable {}`)", r.entry),
        _ => String::new(),
    };
    format!("probe {} v{}: {what}{on}: {}{undo}", r.name, r.version, r.reason)
}

// ------------------------------------------------------------------------------------
// eligibility

/// Why the omp substrate cannot compare `e`, if it cannot. Rollouts mount a skills dir only
/// (`--no-extensions --no-rules`, memory off): any other surface would be an unchanged
/// candidate, so it is reported unsupported rather than compared.
fn unsupported(ctx: &AppCtx, e: &Entry) -> Option<String> {
    let pending = rematch_core::library::pending_parts(&ctx.paths, e);
    if !pending.is_empty() {
        let parts: Vec<String> = pending.iter().map(rematch_core::ledger::enum_str).collect();
        return Some(format!(
            "unapproved runtime parts ({}): never materialized for a probe; observed-use path",
            parts.join(", ")
        ));
    }
    if !matches!(e.kind, Kind::Skill | Kind::Rule | Kind::Fact) || e.hook.is_some() {
        return Some(format!(
            "unsupported surface: {} entries are host- or runtime-specific and the omp rollout substrate mounts skills only; observed-use path",
            rematch_core::ledger::enum_str(&e.kind)
        ));
    }
    if e.path.join(rematch_core::library::AGENTS_FRAGMENT).is_file() {
        return Some("unsupported surface: AGENTS.fragment.md is not mounted by rollouts (`--no-rules`); observed-use path".into());
    }
    match read_entry_files(&e.path) {
        Ok(files) => {
            if let Some(f) = files.keys().find(|f| !f.ends_with(".md")) {
                return Some(format!("ships {f}: probes compare skill text only, never executable parts; observed-use path"));
            }
        }
        Err(err) => return Some(format!("entry files unreadable: {err:#}")),
    }
    if e.content_sha256.is_none() {
        return Some("no content hash to bind a comparison to".into());
    }
    None
}

/// Authorized tasks whose repo the entry's predicate place-matches (its own task first).
fn covering_tasks(ctx: &AppCtx, e: &Entry, authorized: bool) -> Result<Vec<Task>> {
    let mut out: Vec<Task> = list_tasks(ctx)?
        .into_iter()
        .filter(|t| t.authorized() == authorized && t.repo.is_dir())
        .filter(|t| {
            let repo = is_git_repo(&t.repo).then_some(t.repo.as_path());
            matches_place(&e.predicate, &Place::locate(repo, &t.repo))
                || matches!(&e.origin, Origin::Task { task_id } if *task_id == t.id)
        })
        .collect();
    out.sort_by_key(|t| !matches!(&e.origin, Origin::Task { task_id } if *task_id == t.id));
    Ok(out)
}

/// A clean git work tree at a resolvable HEAD, else why not.
fn replayable_repo(repo: &Path) -> std::result::Result<String, String> {
    if !is_git_repo(repo) {
        return Err(format!("{} is not a git work tree: no fixed base snapshot to replay", repo.display()));
    }
    let head = crate::scope::git(repo, &["rev-parse", "HEAD"]).map_err(|e| format!("no HEAD commit: {e:#}"))?;
    match crate::scope::git(repo, &["status", "--porcelain", "--untracked-files=no"]) {
        Ok(s) if s.is_empty() => Ok(head),
        Ok(_) => Err(format!(
            "{} has uncommitted changes: a HEAD worktree would not reproduce the traced task",
            repo.display()
        )),
        Err(e) => Err(format!("git status: {e:#}")),
    }
}

fn started_last_day(ctx: &AppCtx) -> usize {
    let Ok(rd) = std::fs::read_dir(dir(ctx).join("records")) else { return 0 };
    rd.flatten()
        .filter_map(|e| read_json::<ProbeRecord>(&e.path()))
        .filter(|r| r.source == "probe" && r.spent() && ctx.now - r.at < DAY_MS)
        .count()
}
fn probe_limits(ctx: &AppCtx) -> (usize, usize) {
    match ctx.domain_policy() {
        Ok(p) => (p.probe_daily_runs(), p.probe_max_parallel()),
        Err(_) => (PROBES_PER_DAY, PROBES_PER_PASS),
    }
}

/// Active-session staleness window: an unshut trace segment with no record within 30 minutes
/// (by its latest record timestamp, else file mtime) is treated as crashed/abandoned so it
/// cannot block automatic probes forever.
pub const ACTIVE_SESSION_TTL_MS: i64 = 30 * 60 * 1000;

/// Whether any traced session currently has an unfinished segment (no `session_shutdown` after
/// its last `session_start`) with activity within [`ACTIVE_SESSION_TTL_MS`]. Probes run only
/// after a session ends, never while a user's session is active.
pub fn has_active_session(ctx: &AppCtx) -> bool {
    let Ok(rd) = std::fs::read_dir(&ctx.paths.traces) else { return false };
    for ent in rd.flatten() {
        let p = ent.path();
        if p.extension().is_some_and(|x| x == "jsonl") {
            if let Ok(records) = trace::read_trace(&p) {
                let last_start = records.iter().rposition(|r| matches!(r, TraceRecord::SessionStart { .. }));
                if let Some(idx) = last_start {
                    if !records[idx..].iter().any(|r| matches!(r, TraceRecord::SessionShutdown { .. })) {
                        let last_t = records[idx..]
                            .iter()
                            .map(TraceRecord::t)
                            .max()
                            .filter(|t| *t > 0)
                            .or_else(|| {
                                std::fs::metadata(&p)
                                    .ok()?
                                    .modified()
                                    .ok()?
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .ok()
                                    .map(|d| d.as_millis() as i64)
                            })
                            .unwrap_or(0);
                        if last_t > 0 && ctx.now.saturating_sub(last_t) <= ACTIVE_SESSION_TTL_MS {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}


// ------------------------------------------------------------------------------------
// the pass (`reflect --pending`, detached)

/// One bounded probe pass: refresh the status of every provisional entry, run at most
/// [`PROBES_PER_PASS`] authorized comparisons. Returns the ids of entries whose comparison
/// completed this pass (the caller decides them) and a summary.
pub fn pass(ctx: &AppCtx, lines: &mut Vec<String>) -> Result<(Vec<EntryId>, Value)> {
    pass_with(ctx, lines, &omp::omp_bin())
}

/// [`pass`] with an explicit omp binary.
pub fn pass_with(ctx: &AppCtx, lines: &mut Vec<String>, omp_bin: &Path) -> Result<(Vec<EntryId>, Value)> {
    if ctx.paths.is_off() {
        return Ok((Vec::new(), json!({"skipped": "rematch is off", "ran": [], "entries": []})));
    }
    if has_active_session(ctx) {
        return Ok((Vec::new(), json!({"skipped": "an active session is running; probes run only after a session ends", "ran": [], "entries": []})));
    }
    let (daily_cap, pass_cap) = probe_limits(ctx);
    let Some(_lock) = LockFile::acquire({
        std::fs::create_dir_all(dir(ctx))?;
        dir(ctx).join(".lock")
    })?
    else {
        return Ok((Vec::new(), json!({"skipped": "another probe pass is running"})));
    };
    let mut ran: Vec<EntryId> = Vec::new();
    let mut report = Vec::new();
    let entries = ctx.ledger.list_entries(&EntryFilter { status: Some(Status::Provisional), ..Default::default() })?;
    for e in entries.iter().filter(|e| !e.pinned && !e.pending_next_run) {
        if record_of(ctx, &e.id).is_some_and(|r| r.spent() && r.binds(e)) {
            continue; // this version's one evaluation is spent
        }
        let rec = match unsupported(ctx, e) {
            Some(why) => ProbeRecord::new(e, ProbeStatus::Skipped, why, ctx.now),
            None if daily_cap == 0 => {
                ProbeRecord::new(e, ProbeStatus::Skipped, "automatic probes off (`policy.json` `probes.daily_runs = 0`)", ctx.now)
            }
            None => {
                let auth_task = covering_tasks(ctx, e, true)?.into_iter().next();
                let suggestion = if auth_task.is_none() { suggest_for(ctx, e)? } else { None };
                let candidate_task: Option<Task> = match (&auth_task, &suggestion) {
                    (Some(t), _) => Some(t.clone()),
                    (None, Some(s)) if s.auto_eligible() => Some(s.task.clone()),
                    _ => None,
                };
                match (candidate_task, suggestion) {
                    (None, Some(s)) if s.declined_at.is_some() => {
                        ProbeRecord::new(e, ProbeStatus::Skipped, "replay declined for this repo; observed-use path", ctx.now)
                    }
                    (None, Some(s)) => ProbeRecord::new(
                        e,
                        ProbeStatus::AwaitingAuthorization,
                        match &s.blocked {
                            Some(b) => format!("no authorized task covers its repo; suggested replay blocked: {b}"),
                            None => format!("user-supplied task/verifier or rollout cap requires review: `rematch library inspect {PROBE}{}`", e.id),
                        },
                        ctx.now,
                    ),
                    (None, None) => {
                        ProbeRecord::new(e, ProbeStatus::Skipped, "no authorized task and no originating trace to ground a replay in", ctx.now)
                    }
                    (Some(task), _) if ran.len() >= pass_cap => ProbeRecord {
                        task: Some(task.id),
                        ..ProbeRecord::new(e, ProbeStatus::Skipped, format!("pass cap ({pass_cap}) reached; next pass"), ctx.now)
                    },
                    (Some(task), _) if started_last_day(ctx) >= daily_cap => ProbeRecord {
                        task: Some(task.id),
                        ..ProbeRecord::new(e, ProbeStatus::Skipped, format!("daily cap ({daily_cap} probes per 24 h) reached"), ctx.now)
                    },
                    (Some(task), _) => match replayable_repo(&task.repo) {
                        Err(why) => ProbeRecord { task: Some(task.id), ..ProbeRecord::new(e, ProbeStatus::Skipped, why, ctx.now) },
                        Ok(head) => {
                            let r = run_probe(ctx, e, &task, &head, omp_bin)?;
                            if r.spent() {
                                ran.push(e.id.clone());
                            }
                            r
                        }
                    },
                }
            }
        };
        let changed = record_of(ctx, &e.id).is_none_or(|old| (old.status, &old.reason, old.version) != (rec.status, &rec.reason, rec.version));
        if changed {
            save_record(ctx, &rec)?;
            if rec.spent() || rec.status == ProbeStatus::AwaitingAuthorization {
                lines.push(line_of(&rec));
            }
        }
        report.push(json!({"entry": rec.entry, "name": rec.name, "status": rec.status, "reason": rec.reason, "task": rec.task}));
    }
    let summary = json!({"ran": ran, "entries": report, "caps": caps_for(ctx)});
    Ok((ran, summary))
}

pub fn caps() -> Value {
    json!({
        "rollouts_per_probe": PROBE_ROLLOUTS,
        "host_max_time_secs_per_rollout": PROBE_MAX_TIME_SECS,
        "outer_deadline_margin_secs": crate::batch::rollout::KILL_GRACE.as_secs(),
        "per_pass": PROBES_PER_PASS, "per_day": PROBES_PER_DAY, "max_parallel": PROBES_PER_PASS,
        "daily_runs": PROBES_PER_DAY, "editor_calls": 0,
        "solver_calls": "only inside the two omp rollouts", "per_entry_version": 1, "harness": HARNESS,
    })
}

pub fn caps_for(ctx: &AppCtx) -> Value {
    let (daily, parallel) = probe_limits(ctx);
    json!({
        "rollouts_per_probe": PROBE_ROLLOUTS,
        "host_max_time_secs_per_rollout": PROBE_MAX_TIME_SECS,
        "outer_deadline_margin_secs": crate::batch::rollout::KILL_GRACE.as_secs(),
        "per_pass": parallel, "per_day": daily, "max_parallel": parallel,
        "daily_runs": daily, "editor_calls": 0,
        "solver_calls": "only inside the two omp rollouts", "per_entry_version": 1, "harness": HARNESS,
    })
}

/// Baseline (kept/pinned library) then candidate (+ the entry) on one task, seed, model and
/// base commit. Records the outcome; never decides status itself.
fn run_probe(ctx: &AppCtx, e: &Entry, task: &Task, base_head: &str, omp_bin: &Path) -> Result<ProbeRecord> {
    let mut rec = ProbeRecord::new(e, ProbeStatus::Incomplete, "", ctx.now);
    rec.task = Some(task.id.clone());
    rec.base_head = Some(base_head.to_string());
    let verifier_spec = resolve_spec_for_task(task).map(|(v, _)| v);
    let Some(spec) = verifier_spec else {
        rec.status = ProbeStatus::Skipped;
        rec.reason = format!("task {} has no verifier", task.id);
        return Ok(rec);
    };
    rec.verifier = Some(spec.clone());
    if !task.sealable() {
        rec.status = ProbeStatus::Skipped;
        rec.reason = format!("task {} declares no editable paths and its tools can write: the judge cannot be sealed", task.id);
        return Ok(rec);
    }
    let verifier = parse_spec(&spec)?;
    let runs = task_runs_dir(ctx, &task.id);
    std::fs::create_dir_all(&runs)?;
    let Some(_task_lock) = LockFile::acquire(runs.join(".lock"))? else {
        rec.status = ProbeStatus::Skipped;
        rec.reason = format!("task {} is running; next pass", task.id);
        return Ok(rec);
    };
    let solver = resolve_role(&ctx.paths, &task.repo, ROLE_SOLVER)?.model;
    rec.model = Some(solver.clone());
    let seed = task.first_seed();
    rec.seed = Some(seed);
    let mut capped = task.clone();
    capped.max_time_secs = task.max_time_secs.min(PROBE_MAX_TIME_SECS);
    // Common base: kept and pinned entries only (no other provisional trial leaks in).
    let mut base = BaseLibrary::load(ctx, &task.repo)?.without(std::slice::from_ref(&e.id));
    base.entries.retain(|b| {
        b.id.as_ref()
            .and_then(|id| ctx.ledger.get_entry(id).ok().flatten())
            .is_some_and(|x| matches!(x.status, Status::Kept | Status::Pinned))
    });
    rec.base_entries = base.ids();
    let candidate = TreeEntry { name: e.name.clone(), id: Some(e.id.clone()), files: read_entry_files(&e.path)? };
    let sub = OmpSubstrate { repo: task.repo.clone(), solver: solver.clone() };
    let mut rollouts: Vec<Rollout> = Vec::new();
    let mut failure: Option<anyhow::Error> = None;
    for (label, tree) in [
        ("probe-baseline", CandidateTree::reference(&base)),
        ("probe-candidate", CandidateTree { entries: vec![candidate], base: base.clone() }),
    ] {
        match run_rollout(ctx, &sub, omp_bin, &tree, &capped, seed, label, Some(verifier.as_ref() as &dyn Verifier)) {
            Ok(r) => rollouts.push(r),
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    // Seal the judge: a rollout that changed a verifier input voids the comparison.
    let tampered: Vec<(String, Vec<String>)> = rollouts
        .iter()
        .map(|r| (r.label.clone(), r.seal_violations.clone()))
        .filter(|(_, t)| !t.is_empty())
        .collect();
    for r in &rollouts {
        let _ = sub.cleanup(&r.install);
    }
    let seed_dir = runs.join(seed.to_string());
    let _ = std::fs::remove_dir_all(&seed_dir);
    rec.baseline = rollouts.first().map(Rollout::summary);
    rec.candidate = rollouts.get(1).map(Rollout::summary);
    let (outcome, reason) = match (&failure, rollouts.as_slice()) {
        (Some(err), _) if err.downcast_ref::<Cancelled>().is_some() => (ComparativeOutcome::Incomplete, "cancelled".to_string()),
        (Some(err), _) => (ComparativeOutcome::Incomplete, format!("rollout failed: {}", head(&format!("{err:#}"), 300))),
        (None, [b, c]) => match crate::scope::git(&task.repo, &["rev-parse", "HEAD"]) {
            _ if !tampered.is_empty() => (
                ComparativeOutcome::Incomplete,
                format!(
                    "refused: a rollout changed sealed paths outside the task's editable scope ({}); the judge was not sealed",
                    tampered.iter().map(|(l, t)| format!("{l}: {}", t.join(", "))).collect::<Vec<_>>().join("; ")
                ),
            ),
            Ok(h) if h == base_head => attributed(classify(b, c), &e.name, &e.id, c),
            _ => (ComparativeOutcome::Incomplete, "the repo's HEAD moved during the probe: no common base".into()),
        },
        _ => (ComparativeOutcome::Incomplete, "a rollout did not run".into()),
    };
    rec.status = ProbeStatus::of(outcome);
    rec.reason = reason;
    let blob = json!({
        "kind": "probe", "record": &rec, "caps": caps_for(ctx),
        "task": {"id": task.id, "repo": task.repo, "prompt_sha256": rematch_core::ledger::sha256_hex(task.prompt.as_bytes()),
                 "tools": task.tools, "fingerprint": task.fingerprint()},
    });
    rec.blob = Some(ctx.ledger.put_blob(serde_json::to_string(&blob)?.as_bytes())?);
    if let Some(err) = failure.filter(|e| e.downcast_ref::<Cancelled>().is_some()) {
        save_record(ctx, &rec)?;
        return Err(err);
    }
    Ok(rec)
}

/// Engine classification of a baseline/candidate pair (see the module docs).
pub fn classify(base: &Rollout, cand: &Rollout) -> (ComparativeOutcome, String) {
    for (side, r) in [("baseline", base), ("candidate", cand)] {
        if let Some(err) = &r.error {
            return (ComparativeOutcome::Incomplete, format!("{side} rollout invalid: {}", head(err, 300)));
        }
        if r.verdict.is_none() {
            return (ComparativeOutcome::Incomplete, format!("{side} rollout has no verdict"));
        }
    }
    if base.session.model != cand.session.model {
        return (ComparativeOutcome::Incomplete, "rollouts ran different models".into());
    }
    let key = |r: &Rollout| (r.verdict.as_ref().is_some_and(is_pass), r.verdict.as_ref().and_then(score_of));
    let ((bp, bs), (cp, cs)) = (key(base), key(cand));
    let vs = format!("baseline {}, candidate {}", if bp { "pass" } else { "fail" }, if cp { "pass" } else { "fail" });
    match (bp, cp) {
        (false, true) => return (ComparativeOutcome::Supports, vs),
        (true, false) => return (ComparativeOutcome::Regresses, vs),
        _ => {}
    }
    if let (Some(b), Some(c)) = (bs, cs) {
        if c > b {
            return (ComparativeOutcome::Supports, format!("{vs}; score {b} → {c}"));
        }
        if c < b {
            return (ComparativeOutcome::Regresses, format!("{vs}; score {b} → {c}"));
        }
    }
    let tokens = |r: &Rollout| r.session.tokens_in + r.session.tokens_out;
    if cand.session.tool_errors > base.session.tool_errors || tokens(cand) * 2 > tokens(base) * 3 {
        return (
            ComparativeOutcome::CostWorse,
            format!("{vs}; tool errors {} → {}, tokens {} → {}", base.session.tool_errors, cand.session.tool_errors, tokens(base), tokens(cand)),
        );
    }
    (ComparativeOutcome::Tie, format!("{vs}; no difference"))
}

/// A difference counts only when the candidate rollout actually read the entry: without a
/// read the two harnesses behaved alike by construction, so any verdict difference is noise
/// and the pair is a tie.
fn attributed(c: (ComparativeOutcome, String), name: &str, id: &str, cand: &Rollout) -> (ComparativeOutcome, String) {
    let read = cand.session.skill_reads.iter().any(|n| n == name) || cand.session.skill_entries.iter().any(|x| x == id);
    match c {
        (ComparativeOutcome::Supports | ComparativeOutcome::Regresses, why) if !read => {
            (ComparativeOutcome::Tie, format!("{why}; but the candidate never read {name}: no attributable effect"))
        }
        other => other,
    }
}

/// The comparison a batch `run` already made: its winner beat the reference rollout on the
/// same task, seed, model and base (`batch::worker`). Recorded on the staged entry so its
/// next-run keep is comparative support of exactly that content.
/// `read_name` is the name the winner's rollout loaded the lesson under.
#[allow(clippy::too_many_arguments)]
pub fn record_batch_comparison(
    ctx: &AppCtx,
    e: &Entry,
    read_name: &str,
    task: &Task,
    verifier: &str,
    solver: &ModelRef,
    seed: u64,
    reference: &Rollout,
    winner: &Rollout,
) -> Result<ProbeRecord> {
    let touched: Vec<String> = [reference, winner]
        .iter()
        .flat_map(|r| r.seal_violations.iter().map(move |p| format!("{}: {p}", r.label)))
        .collect();
    let (outcome, reason) = if !task.sealable() {
        (ComparativeOutcome::Incomplete, format!("unsealed judge: task {} declares no editable paths and its tools can write", task.id))
    } else if touched.is_empty() {
        attributed(classify(reference, winner), read_name, &e.id, winner)
    } else {
        (ComparativeOutcome::Incomplete, format!("refused: a rollout changed sealed paths ({})", touched.join(", ")))
    };
    let mut rec = ProbeRecord::new(e, ProbeStatus::of(outcome), format!("batch run winner over its reference: {reason}"), ctx.now);
    rec.source = "batch".into();
    rec.task = Some(task.id.clone());
    rec.verifier = Some(verifier.to_string());
    rec.model = Some(solver.clone());
    rec.seed = Some(seed);
    rec.base_head = crate::scope::git(&task.repo, &["rev-parse", "HEAD"]).ok();
    rec.base_entries = reference.install.loaded.clone();
    rec.baseline = Some(reference.summary());
    rec.candidate = Some(winner.summary());
    rec.blob = Some(ctx.ledger.put_blob(serde_json::to_string(&json!({"kind": "batch_comparison", "record": &rec}))?.as_bytes())?);
    save_record(ctx, &rec)?;
    Ok(rec)
}

// ------------------------------------------------------------------------------------
// replay authorization (actions `probe:<entry id>`)

/// `probes/suggestions/<entry id>.json`: the replay a user can authorize for an entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub entry: EntryId,
    /// The task to authorize: an existing one, or one defined on approval.
    pub task: Task,
    pub existing: bool,
    /// Where the verifier came from: `task`, `observed: <cmd>` or `detected: tests`.
    pub verifier_source: String,
    /// Where the prompt came from: `task` or `trace <session>#<seq>`.
    pub prompt_source: String,
    #[serde(default)]
    pub blocked: Option<String>,
    #[serde(default)]
    pub declined_at: Option<UnixMs>,
    pub at: UnixMs,
}

impl Suggestion {
    /// Whether this suggestion can run automatically without a `probe:` approval:
    /// it replays a recorded session of the repo (`!existing` and `prompt_source` from a trace)
    /// in an isolated worktree, its verifier is engine-chosen and sealed (`detected: …` or
    /// `observed: …` plain test runner), its workspace is sealable, and its rollout time cap fits
    /// [`PROBE_MAX_TIME_SECS`].
    pub fn auto_eligible(&self) -> bool {
        self.declined_at.is_none()
            && self.blocked.is_none()
            && !self.existing
            && self.prompt_source.starts_with("trace ")
            && (self.verifier_source.starts_with("detected:") || self.verifier_source.starts_with("observed:"))
            && self.task.verifier.as_ref().is_some_and(|v| !v.trim().is_empty())
            && self.task.sealable()
            && self.task.max_time_secs <= PROBE_MAX_TIME_SECS
    }
}

/// The suggestion for `e`, built once (an existing unauthorized task of its repo, else one
/// grounded in its originating trace); `None` when nothing grounds a replay.
pub fn suggest_for(ctx: &AppCtx, e: &Entry) -> Result<Option<Suggestion>> {
    let cached = read_json::<Suggestion>(&suggestion_path(ctx, &e.id));
    if cached.as_ref().is_some_and(|c| c.declined_at.is_some()) {
        return Ok(cached);
    }
    if unsupported(ctx, e).is_some() {
        return Ok(None);
    }
    let s = match covering_tasks(ctx, e, false)?.into_iter().next() {
        Some(mut task) => {
            let resolved = resolve_spec_for_task(&task);
            let verifier_source = resolved.as_ref().map(|(_, src)| src.clone()).unwrap_or_else(|| "none".into());
            if task.verifier.is_none() {
                task.verifier = resolved.map(|(v, _)| v);
            }
            Suggestion {
                entry: e.id.clone(),
                blocked: if task.verifier.is_none() {
                    Some(format!("task {} has no verifier: `rematch task add {} … --verifier <cmd>`", task.id, task.id))
                } else if !task.sealable() {
                    Some(format!("task {} declares no editable paths and its tools can write, so its judge cannot be sealed: `rematch task add {} … --editable <path>` (or `--tools read,grep,glob` for a read-only task)", task.id, task.id))
                } else {
                    None
                },
                verifier_source,
                prompt_source: "task".into(),
                task,
                existing: true,
                declined_at: None,
                at: ctx.now,
            }
        }
        None => match from_trace(ctx, e)? {
            Some(s) => s,
            None => return Ok(None),
        },
    };
    if declined_repo(ctx, &s.task.repo) {
        let s = Suggestion { declined_at: Some(ctx.now), ..s };
        write_json(&suggestion_path(ctx, &e.id), &s)?;
        return Ok(Some(s));
    }
    // Recomputed every time (a user may have fixed the task it names); an unchanged
    // suggestion keeps its first-seen time.
    let s = match cached {
        Some(c) if c.task.id == s.task.id && c.task.fingerprint() == s.task.fingerprint() && c.blocked == s.blocked => c,
        _ => s,
    };
    write_json(&suggestion_path(ctx, &e.id), &s)?;
    Ok(Some(s))
}

/// A task from the entry's originating session: the task request before the moment, the
/// session's repo, its last observed plain test-runner command as the verifier.
fn from_trace(ctx: &AppCtx, e: &Entry) -> Result<Option<Suggestion>> {
    let Origin::Session { session_id } = &e.origin else { return Ok(None) };
    let Some(session) = ctx.ledger.get_session(session_id)? else { return Ok(None) };
    let Some(repo) = session.repo.as_deref().map(PathBuf::from).filter(|r| r.is_dir()) else { return Ok(None) };
    let path = ctx.paths.trace_file(session_id);
    if !path.is_file() {
        return Ok(None);
    }
    let records = trace::read_trace(&path)?;
    let moment = e.manifest.as_ref().map(|m| m.evidence.seq).unwrap_or(u32::MAX);
    let Some((seq, prompt)) = records.iter().rev().find_map(|r| match r {
        TraceRecord::Input { seq, text, prelabel, .. }
            if *seq < moment && rematch_core::redirect::mark_class(prelabel).is_none() && !text.trim().is_empty() =>
        {
            Some((*seq, text.clone()))
        }
        _ => None,
    }) else {
        return Ok(None);
    };
    let observed = session.commands.iter().rev().find(|c| is_test_command(c)).cloned();
    let (verifier, verifier_source, blocked) = match observed {
        Some(c) => (Some(format!("script:{c}")), format!("observed: {c}"), None),
        None => match default_spec_for(&repo) {
            Some((spec, source)) => (Some(spec), source, None),
            None => (
                None,
                "none".to_string(),
                Some("no test command observed and no fast check or test runner detected; define a verifier with `rematch task add`".to_string()),
            ),
        },
    };
    // Solution scope: the files the originating session changed, minus verifier inputs.
    let repo_s = repo.to_string_lossy().replace('\\', "/");
    let changed: Vec<String> = session
        .files_changed
        .iter()
        .map(|(p, _)| {
            let p = p.replace('\\', "/");
            p.strip_prefix(&format!("{}/", repo_s.trim_end_matches('/'))).map(str::to_string).unwrap_or(p)
        })
        .collect();
    // Unique per entry version: names repeat across repos, branches and versions.
    let mut id = format!("probe-{}", e.name);
    id.truncate(100);
    let id = format!("{id}-{}-v{}", crate::scope::short_id(&e.id), e.version);
    validate_id(&id)?;
    let mut task = Task {
        id,
        repo,
        prompt,
        verifier,
        max_time_secs: PROBE_MAX_TIME_SECS,
        tools: Some(SUGGESTED_TOOLS.into()),
        seeds: vec![crate::batch::tasks::DEFAULT_SEED],
        created_at: ctx.now,
        editable: Vec::new(),
        auto_probe: None,
    };
    let candidates: Vec<String> = changed.into_iter().filter(|p| !task.is_protected(p)).collect();
    // Paths outside the repo or otherwise invalid drop out one by one.
    task.editable = candidates
        .iter()
        .filter_map(|p| crate::batch::tasks::normalize_editable(std::slice::from_ref(p)).ok())
        .flatten()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if task.editable.is_empty() {
        // No originating writes: a read-only review/analysis replay, everything sealed.
        task.tools = Some(READ_ONLY_TOOLS.into());
    }
    Ok(Some(Suggestion {
        entry: e.id.clone(),
        task,
        existing: false,
        verifier_source,
        prompt_source: format!("trace {session_id}#{seq}"),
        blocked,
        declined_at: None,
        at: ctx.now,
    }))
}

/// A plain test-runner invocation: a known runner prefix, no shell composition.
fn is_test_command(c: &str) -> bool {
    let c = c.trim();
    let plain = !c.contains(['|', ';', '&', '>', '<', '`', '$', '\n']);
    plain && TEST_RUNNERS.iter().any(|r| c == *r || c.starts_with(&format!("{r} ")))
}

fn declined_path(ctx: &AppCtx) -> PathBuf {
    dir(ctx).join("declined-repos.json")
}

fn declined_repo(ctx: &AppCtx, repo: &Path) -> bool {
    read_json::<Vec<PathBuf>>(&declined_path(ctx)).is_some_and(|v| v.iter().any(|r| r == repo))
}

/// The diff a user authorizes: the exact task, verifier, tools and caps.
fn spec_text(s: &Suggestion) -> String {
    let t = &s.task;
    format!(
        "task: {}{}\nrepo: {}\nprompt ({}):\n{}\nverifier ({}): {}\ntools: {}\nhost max time per rollout: {} s (probe cap {} s; the engine's outer deadline adds {} s)\nseed: {}\nper candidate version: {} rollouts (baseline without it, candidate with it), harness {}; the solver model runs only inside those rollouts, no editor calls\nisolation: a fresh git worktree of HEAD (not an OS sandbox); the active checkout is not touched\nseal: {}\n",
        t.id,
        if s.existing { " (existing)" } else { " (defined on approval)" },
        t.repo.display(),
        s.prompt_source,
        head(&t.prompt, 2000),
        s.verifier_source,
        t.verifier.as_deref().unwrap_or("(none)"),
        t.tools.as_deref().unwrap_or("omp defaults"),
        t.max_time_secs,
        PROBE_MAX_TIME_SECS,
        crate::batch::rollout::KILL_GRACE.as_secs(),
        t.first_seed(),
        PROBE_ROLLOUTS,
        HARNESS,
        t.seal_scope(),
    )
}

/// sha256 of the full reviewed snapshot the action shows: the action id, the entry version
/// and content, the whole task definition (repo, prompt, verifier, tools, time cap, seeds,
/// editable scope), the seal scope, the caps and the harness. Approval binds it; any change
/// makes the action stale.
fn spec_sha(s: &Suggestion, e: &Entry) -> String {
    let snapshot = json!({
        "action": format!("{PROBE}{}", e.id),
        "entry": {"id": e.id, "version": e.version, "content_sha256": e.content_sha256},
        "task": s.task.fingerprint(),
        "task_id": s.task.id,
        "existing": s.existing,
        "verifier_source": s.verifier_source,
        "prompt_source": s.prompt_source,
        "seal": s.task.seal_scope(),
        "caps": caps(),
        "harness": HARNESS,
    });
    rematch_core::ledger::sha256_hex(snapshot.to_string().as_bytes())
}

/// `probe:<entry id>` actions: entries awaiting replay authorization.
pub fn pending_actions(ctx: &AppCtx) -> Result<Vec<Action>> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir(ctx).join("suggestions")) else { return Ok(out) };
    let mut sugs: Vec<Suggestion> = rd.flatten().filter_map(|e| read_json(&e.path())).collect();
    sugs.sort_by(|a, b| a.at.cmp(&b.at).then(a.entry.cmp(&b.entry)));
    let (daily_cap, _) = probe_limits(ctx);
    for s in sugs.into_iter().filter(|s| s.declined_at.is_none()) {
        let Some(e) = ctx.ledger.get_entry(&s.entry)? else { continue };
        if e.status != Status::Provisional || record_of(ctx, &e.id).is_some_and(|r| r.spent() && r.binds(&e)) {
            continue;
        }
        if !covering_tasks(ctx, &e, true)?.is_empty() {
            continue; // an authorized task covers it; the next pass runs it
        }
        // Refresh: the user may have fixed the task the suggestion names since it was cached.
        let Some(s) = suggest_for(ctx, &e)?.filter(|s| s.declined_at.is_none()) else { continue };
        // Trace-grounded tasks with engine-chosen sealed verifiers run automatically inside the
        // daily budget; only user-supplied verifiers, blocked tasks, or tasks exceeding caps
        // surface a `probe:` review item.
        if s.auto_eligible() && daily_cap > 0 {
            continue;
        }
        if daily_cap == 0 && s.auto_eligible() {
            continue;
        }
        let id = format!("{PROBE}{}", e.id);
        let sha = spec_sha(&s, &e);
        let mut a = Action::new(&id, "probe", surface_of(e.kind), &e.name);
        a.entry = Some(ActionEntry::of(&e));
        a.sha256 = Some(sha.clone());
        a.target = format!("task {} in {}", s.task.id, s.task.repo.display());
        a.scope = "probe".into();
        a.effect = "replay".into();
        a.reason = format!(
            "provisional {} {}: a bounded comparison can support or reject it; without one it stays on the observed-use path",
            rematch_core::ledger::enum_str(&e.kind),
            e.name
        );
        a.summary = format!("authorize automatic replay of task {} to compare {} v{}", s.task.id, e.name, e.version);
        a.diff = spec_text(&s);
        a.commands = Commands::for_id(&id, Some(&sha));
        a.since = Some(s.at);
        if let Some(b) = &s.blocked {
            a.block(b.clone());
        }
        out.push(a);
    }
    Ok(out)
}

fn suggestion_of(ctx: &AppCtx, id: &str) -> Result<Suggestion> {
    let entry = id.strip_prefix(PROBE).unwrap_or(id);
    read_json(&suggestion_path(ctx, entry)).ok_or_else(|| anyhow!("no replay suggestion {id:?}"))
}

/// `library approve probe:<entry id> --sha <snapshot sha256>`: define (or keep) the task and
/// record the authorization bound to the reviewed snapshot. A missing or stale hash is refused.
pub fn approve_action(ctx: &AppCtx, id: &str, sha: Option<&str>) -> Result<Value> {
    let cached = suggestion_of(ctx, id)?;
    let e = ctx.ledger.get_entry(&cached.entry)?.ok_or_else(|| anyhow!("{id}: entry {} is gone", cached.entry))?;
    // The current suggestion (the task it names may have changed since it was cached).
    let s = suggest_for(ctx, &e)?.ok_or_else(|| anyhow!("{id}: no replay can be grounded for {} any more", e.name))?;
    if s.declined_at.is_some() {
        bail!("{id}: replay in {} was declined", s.task.repo.display());
    }
    if let Some(b) = &s.blocked {
        bail!("{id} is blocked: {b}");
    }
    let want = spec_sha(&s, &e);
    match sha {
        None => bail!("{id}: approval needs the reviewed snapshot hash: `rematch library inspect {id}`, then approve with its --sha"),
        Some(given) if given != want => {
            bail!("{id}: the replay snapshot changed since review (now {want}); `rematch library inspect {id}` and approve again")
        }
        Some(_) => {}
    }
    let mut task = if s.existing {
        let mut t = crate::batch::tasks::load_task(ctx, &s.task.id)?;
        if t.verifier.is_none() {
            t.verifier = s.task.verifier.clone();
        }
        t
    } else {
        s.task.clone()
    };
    if task.fingerprint() != s.task.fingerprint() {
        bail!("{id}: task {} changed since it was suggested; `rematch library inspect {id}` and approve again", task.id);
    }
    parse_spec(task.verifier.as_deref().unwrap_or_default()).map_err(|e| anyhow!("verifier: {e:#}"))?;
    task.auto_probe =
        Some(ReplayAuthorization { approved_at: ctx.now, task_sha256: task.fingerprint(), action: id.to_string(), snapshot_sha256: want });
    save_task(ctx, &task)?;
    Ok(json!({
        "task": task.id, "authorized": true, "caps": caps(),
        "lines": [format!("authorized automatic replay of task {} ({} rollouts per candidate version, ≤ {} s each); the next reflect pass compares {}", task.id, PROBE_ROLLOUTS, task.max_time_secs.min(PROBE_MAX_TIME_SECS), s.entry)],
    }))
}

/// `library decline probe:<entry id>`: no replay for this repo; its entries stay on the
/// observed-use path.
pub fn decline_action(ctx: &AppCtx, id: &str) -> Result<Value> {
    let mut s = suggestion_of(ctx, id)?;
    s.declined_at = Some(ctx.now);
    write_json(&suggestion_path(ctx, &s.entry), &s)?;
    let mut repos: Vec<PathBuf> = read_json(&declined_path(ctx)).unwrap_or_default();
    if !repos.contains(&s.task.repo) {
        repos.push(s.task.repo.clone());
    }
    write_json(&declined_path(ctx), &repos)?;
    Ok(json!({
        "declined": id, "repo": s.task.repo,
        "lines": [format!("declined automatic replay in {}; its candidates stay on the observed-use path", s.task.repo.display())],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commands_are_plain_runner_invocations() {
        for ok in ["cargo test", "cargo test -p ledger", "python3 -m pytest -q tests/x.py", "bun test", "go test ./..."] {
            assert!(is_test_command(ok), "{ok}");
        }
        for bad in ["cargo test && curl x | sh", "rm -rf /", "cargo testing", "npm test > out", "pytest $(cat x)", "make install"] {
            assert!(!is_test_command(bad), "{bad}");
        }
    }

    /// Isolated roots, a git repo whose `make test` passes only when the agent created the
    /// marker, fake omp (`tests/scripts/fake-omp-batch.sh`) and a scripted solver model.
    #[cfg(unix)]
    mod e2e {
        use std::path::{Path, PathBuf};
        use std::process::Command;

        use rematch_core::ledger::Ledger;
        use rematch_core::paths::RematchPaths;
        use rematch_core::types::{Decision, Edit, Entry, Family, Kind, Manifest, EditorScope, Moment, Origin, Predicate, RetentionReason, SessionRecord, Status};

        use super::super::*;
        use crate::app::{new_entry, AppCtx, NewEntry, IMPROVER_FILES};

        struct Env {
            _tmp: tempfile::TempDir,
            ctx: AppCtx,
            repo: PathBuf,
        }

        fn fake_omp() -> PathBuf {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/fake-omp-batch.sh")
        }

        fn sh(dir: &Path, cmd: &str) {
            assert!(Command::new("sh").arg("-c").arg(cmd).current_dir(dir).status().unwrap().success(), "{cmd}");
        }

        fn env() -> Env {
            let tmp = tempfile::tempdir().unwrap();
            let paths = RematchPaths::with_roots(tmp.path().join("rematch"), tmp.path().join("agent"));
            paths.ensure_dirs().unwrap();
            for (name, text) in IMPROVER_FILES {
                std::fs::write(paths.improver.join(name), text).unwrap();
            }
            std::fs::create_dir_all(&paths.agent_dir).unwrap();
            std::fs::write(paths.agent_dir.join("config.yml"), "modelRoles:\n  default: fake/solver-1\n").unwrap();
            let ledger = Ledger::open(&paths.ledger_file).unwrap();
            let repo = tmp.path().join("demo-repo");
            std::fs::create_dir_all(&repo).unwrap();
            sh(
                &repo,
                "git init -q . && git config user.email t@e.st && git config user.name tester && \
                 printf 'echo build\\n' > build.sh && printf 'test:\\n\\ttest -f .rematch-marker\\n' > Makefile && \
                 mkdir -p data && printf 'expected\\n' > data/expected.txt && \
                 git add . && git commit -qm init",
            );
            let ctx = AppCtx { paths, ledger, now: rematch_core::ledger::now_ms(), cwd: tmp.path().to_path_buf(), text: false };
            Env { _tmp: tmp, ctx, repo }
        }

        /// A flagged session that ran `make test`, its trace, and a candidate staged from it.
        fn staged(e: &Env, name: &str, extra: &[(&str, &str)]) -> Entry {
            let sid = format!("0199d000-0000-7000-8000-{:012}", name.len());
            let repo = e.repo.to_string_lossy().to_string();
            let trace = format!(
                "{{\"seq\":0,\"t\":1,\"ev\":\"session_start\",\"session_id\":\"{sid}\",\"cwd\":{r},\"repo\":{r}}}\n\
                 {{\"seq\":1,\"t\":2,\"ev\":\"input\",\"text\":\"Make the check pass in the build script\",\"prelabel\":\"none\"}}\n\
                 {{\"seq\":2,\"t\":3,\"ev\":\"tool_result\",\"tool\":\"bash\",\"call_id\":\"c\",\"input\":{{\"command\":\"make test\"}},\"is_error\":true}}\n\
                 {{\"seq\":3,\"t\":4,\"ev\":\"input\",\"text\":\"No, the check still fails\",\"prelabel\":\"negation\"}}\n\
                 {{\"seq\":4,\"t\":5,\"ev\":\"session_shutdown\",\"reasons\":[\"redirect:negation\"],\"flagged\":true}}\n",
                r = serde_json::to_string(&repo).unwrap()
            );
            std::fs::create_dir_all(&e.ctx.paths.traces).unwrap();
            std::fs::write(e.ctx.paths.trace_file(&sid), trace).unwrap();
            let s = SessionRecord {
                id: sid.clone(),
                cwd: repo.clone(),
                repo: Some(repo.clone()),
                branch: "demo-repo".into(),
                source: "trace".into(),
                commands: vec!["make test".into()],
                // The originating session's successful writes: the derived editable scope.
                files_changed: vec![(".rematch-marker".into(), "e3b0".into())],
                ..Default::default()
            };
            e.ctx.ledger.upsert_session(&s).unwrap();
            let mut entry = new_entry(
                NewEntry {
                    kind: Kind::Skill,
                    name: name.into(),
                    branch: "demo-repo".into(),
                    status: Status::Provisional,
                    family: Family("fake/solver".into()),
                    origin: Origin::Session { session_id: sid.clone() },
                    predicate: Predicate { repo_glob: rematch_core::predicate::escape_glob(&repo), ..Default::default() },
                    trigger_line: "Use when finishing a build script task in this repository.".into(),
                    path: PathBuf::new(),
                },
                e.ctx.now,
            )
            .unwrap();
            entry.manifest = Some(Manifest {
                evidence: Moment { session_id: sid, seq: 3 },
                predicted_fix: "the check passes".into(),
                predicted_regression: "none".into(),
                rationale: String::new(),
                scope: EditorScope::Domain,
            });
            let mut edits = vec![Edit::Write {
                path: "SKILL.md".into(),
                content: format!(
                    "---\nname: {name}\ndescription: Use when finishing a build script task in this repository.\n---\n\nUse when finishing a build script task in this repository.\n\n## Laws\n- MUST create the rematch-test-marker file before yielding.\n"
                ),
            }];
            edits.extend(extra.iter().map(|(p, c)| Edit::Write { path: p.to_string(), content: c.to_string() }));
            rematch_core::library::stage(&e.ctx.ledger, &e.ctx.paths, &entry, &edits).unwrap()
        }

        fn worktrees(repo: &Path) -> usize {
            let out = Command::new("git").arg("-C").arg(repo).args(["worktree", "list"]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).lines().count()
        }

        #[test]
        fn grounded_probe_runs_automatically_without_approval_and_supports_keep_or_retires_regression() {
            let e = env();
            let cand = staged(&e, "check-marker", &[]);
            let mut lines = Vec::new();

            // A trace-grounded task with an engine-chosen sealed verifier runs automatically
            // without any `probe:` approval action.
            let (ran, report) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert_eq!(ran, vec![cand.id.clone()], "{report:#}");
            assert!(pending_actions(&e.ctx).unwrap().is_empty(), "no approval action needed");
            let rec = record_of(&e.ctx, &cand.id).unwrap();
            assert_eq!(rec.status, ProbeStatus::Supports, "{rec:#?}");
            assert_eq!((rec.baseline.as_ref().unwrap().pass, rec.candidate.as_ref().unwrap().pass), (false, true));
            assert_eq!(rec.harness, "omp");
            assert_eq!(rec.model.as_ref().unwrap().id, "solver-1");
            assert!(rec.base_head.is_some() && rec.blob.is_some());
            assert!(rec.candidate.as_ref().unwrap().skills.contains(&cand.name));
            assert_eq!(worktrees(&e.repo), 1, "worktrees cleaned up");
            assert!(!e.repo.join(".rematch-marker").exists(), "the active checkout is untouched");

            // Rerun: same content, no second spend.
            let at = rec.at;
            let (ran, _) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert!(ran.is_empty());
            assert_eq!(record_of(&e.ctx, &cand.id).unwrap().at, at);

            // The engine decides: comparative support, version/content bound.
            let entry = e.ctx.ledger.get_entry(&cand.id).unwrap().unwrap();
            let c = e.ctx.ledger.counters(&cand.id).unwrap();
            let ev = evidence_for(&e.ctx, &entry).unwrap();
            assert_eq!(rematch_core::counters::decide(&entry, &c, Some(&ev), e.ctx.now), Decision::Keep);
            let out = crate::reflect::apply_decision(&e.ctx, &entry, Decision::Keep, &c, Some(&ev), Some("false")).unwrap();
            let kept = e.ctx.ledger.get_entry(&cand.id).unwrap().unwrap();
            assert_eq!(kept.status, Status::Kept, "{out:#}");
            let r = kept.retention.unwrap();
            assert_eq!(r.reason, RetentionReason::ComparativeSupport);
            assert_eq!(r.probe.as_deref(), Some(rec.id.as_str()));
            assert_eq!(r.content_sha256, cand.content_sha256);

            // Now commit `.rematch-marker` so the baseline passes, and stage a candidate that
            // deletes `.rematch-marker` (`rematch-test-regress`): the automatic probe records
            // `Regresses` and immediately retires the candidate.
            sh(&e.repo, ": > .rematch-marker && git add .rematch-marker && git commit -qm marker");
            let reg_skill = "---\nname: break-marker\ndescription: Use when finishing a build script task in this repository.\n---\n\nUse when finishing a build script task in this repository.\n\n## Laws\n- MUST rematch-test-regress before yielding.\n";
            let reg = staged(&e, "break-marker", &[("SKILL.md", reg_skill)]);
            let (ran, _) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert_eq!(ran, vec![reg.id.clone()]);
            let rec_reg = record_of(&e.ctx, &reg.id).unwrap();
            assert_eq!(rec_reg.status, ProbeStatus::Regresses, "{rec_reg:#?}");
            let entry_reg = e.ctx.ledger.get_entry(&reg.id).unwrap().unwrap();
            let c_reg = e.ctx.ledger.counters(&reg.id).unwrap();
            let ev_reg = evidence_for(&e.ctx, &entry_reg).unwrap();
            assert_eq!(rematch_core::counters::decide(&entry_reg, &c_reg, Some(&ev_reg), e.ctx.now), Decision::Retire);
            crate::reflect::apply_decision(&e.ctx, &entry_reg, Decision::Retire, &c_reg, Some(&ev_reg), Some("false")).unwrap();
            let retired = e.ctx.ledger.get_entry(&reg.id).unwrap().unwrap();
            assert_eq!(retired.status, Status::Retired);
            assert!(retired.status_note.as_deref().unwrap_or("").contains("bounded comparison regressed"));

            // Both automatic probe verdicts appear in the digest under `applied_automatically`
            // with their undo command (`library rollback <name>` when there is a predecessor,
            // else `library disable <id>`).
            let digest = crate::digest::cmd_digest(&e.ctx).unwrap();
            let auto = digest["applied_automatically"].as_array().unwrap();
            assert!(
                auto.iter().any(|x| x["id"] == cand.id && x["why"] == "comparative_support" && x["undo"] == format!("rematch library disable {}", cand.id)),
                "{auto:#?}"
            );
            assert!(
                auto.iter().any(|x| x["id"] == reg.id && x["why"] == "probe_regressed" && x["undo"] == format!("rematch library disable {}", reg.id)),
                "{auto:#?}"
            );
            // When a comparative-support keep replaced a predecessor, undo is `rematch library rollback <name>`.
            let mut kept_with_pred = e.ctx.ledger.get_entry(&cand.id).unwrap().unwrap();
            kept_with_pred.predecessor = Some("prev-entry-id".into());
            crate::reflect::save_entry(&e.ctx, &kept_with_pred).unwrap();
            let auto2 = crate::actions::applied_automatically(&e.ctx).unwrap();
            assert!(
                auto2.iter().any(|x| x.id == cand.id && x.undo == "rematch library rollback check-marker"),
                "{auto2:#?}"
            );
        }

        #[test]
        fn detected_fast_check_verifier_grounds_and_runs_automatically() {
            let e = env();
            sh(
                &e.repo,
                "mkdir -p src && printf '[package]\\nname = \"demo\"\\nversion = \"0.1.0\"\\nedition = \"2021\"\\n' > Cargo.toml && \
                 printf 'include!(\"../.rematch-marker\");\\n' > src/lib.rs && git add Cargo.toml src/lib.rs && git commit -qm cargo",
            );
            rematch_core::check::clear_detect_cache();
            let cand = staged(&e, "detected-check-skill", &[]);
            let sid = match &cand.origin {
                Origin::Session { session_id } => session_id.clone(),
                _ => unreachable!(),
            };
            let mut s = e.ctx.ledger.get_session(&sid).unwrap().unwrap();
            s.commands.clear(); // No observed test command: must fall back to detected fast check (`check`).
            e.ctx.ledger.upsert_session(&s).unwrap();

            let sug = suggest_for(&e.ctx, &cand).unwrap().unwrap();
            assert_eq!(sug.task.verifier.as_deref(), Some("check"), "{sug:?}");
            assert_eq!(sug.verifier_source, "detected: check (cargo)");
            assert!(sug.auto_eligible(), "{sug:?}");
            assert!(pending_actions(&e.ctx).unwrap().is_empty(), "detected fast check needs no approval");

            let mut lines = Vec::new();
            let (ran, _) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert_eq!(ran, vec![cand.id.clone()]);
            let rec = record_of(&e.ctx, &cand.id).unwrap();
            assert_eq!(rec.verifier.as_deref(), Some("check"));
            assert_eq!(rec.status, ProbeStatus::Supports, "baseline fails cargo check, candidate creates .rematch-marker and passes: {rec:#?}");
            let entry = e.ctx.ledger.get_entry(&cand.id).unwrap().unwrap();
            let c = e.ctx.ledger.counters(&cand.id).unwrap();
            let ev = evidence_for(&e.ctx, &entry).unwrap();
            crate::reflect::apply_decision(&e.ctx, &entry, Decision::Keep, &c, Some(&ev), Some("false")).unwrap();
            let kept = e.ctx.ledger.get_entry(&cand.id).unwrap().unwrap();
            assert_eq!(kept.status, Status::Kept);
            assert_eq!(kept.retention.unwrap().reason, RetentionReason::ComparativeSupport);

            // Commit `.rematch-marker` so baseline passes `cargo check`, then stage a regressing candidate.
            sh(&e.repo, ": > .rematch-marker && git add .rematch-marker && git commit -qm marker");
            let reg_skill = "---\nname: break-check\ndescription: Use when finishing a build script task in this repository.\n---\n\nUse when finishing a build script task in this repository.\n\n## Laws\n- MUST rematch-test-regress before yielding.\n";
            let reg = staged(&e, "break-check", &[("SKILL.md", reg_skill)]);
            let sid_reg = match &reg.origin {
                Origin::Session { session_id } => session_id.clone(),
                _ => unreachable!(),
            };
            let mut s_reg = e.ctx.ledger.get_session(&sid_reg).unwrap().unwrap();
            s_reg.commands.clear();
            e.ctx.ledger.upsert_session(&s_reg).unwrap();
            let (ran, _) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert_eq!(ran, vec![reg.id.clone()]);
            let rec_reg = record_of(&e.ctx, &reg.id).unwrap();
            assert_eq!(rec_reg.verifier.as_deref(), Some("check"));
            assert_eq!(rec_reg.status, ProbeStatus::Regresses, "{rec_reg:#?}");
            let entry_reg = e.ctx.ledger.get_entry(&reg.id).unwrap().unwrap();
            let c_reg = e.ctx.ledger.counters(&reg.id).unwrap();
            let ev_reg = evidence_for(&e.ctx, &entry_reg).unwrap();
            crate::reflect::apply_decision(&e.ctx, &entry_reg, Decision::Retire, &c_reg, Some(&ev_reg), Some("false")).unwrap();
            assert_eq!(e.ctx.ledger.get_entry(&reg.id).unwrap().unwrap().status, Status::Retired);
        }

        #[test]
        fn user_verifier_probe_requires_one_tap_hash_bound_approval() {
            let e = env();
            crate::batch::tasks::cmd_task_add(
                &e.ctx,
                crate::batch::tasks::TaskAddArgs {
                    id: "user-task".into(),
                    repo: e.repo.clone(),
                    prompt: "Make the check pass in the build script".into(),
                    verifier: Some("script:make test".into()),
                    max_time: Some("30s".into()),
                    tools: Some(SUGGESTED_TOOLS.into()),
                    seeds: vec![],
                    editable: vec![".rematch-marker".into()],
                },
            )
            .unwrap();
            let cand = staged(&e, "check-marker", &[]);
            let mut lines = Vec::new();

            // User-defined task/verifier is not auto-eligible: it produces a one-tap review item.
            let (ran, _) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert!(ran.is_empty());
            assert_eq!(record_of(&e.ctx, &cand.id).unwrap().status, ProbeStatus::AwaitingAuthorization);
            let actions = pending_actions(&e.ctx).unwrap();
            assert_eq!(actions.len(), 1, "{actions:?}");
            let a = &actions[0];
            assert_eq!(a.id, format!("probe:{}", cand.id));
            assert!(a.approvable(), "{a:?}");
            let sha = a.sha256.clone().unwrap();
            assert!(approve_action(&e.ctx, &a.id, None).is_err());
            assert!(approve_action(&e.ctx, &a.id, Some(&sha[..12])).is_err());
            approve_action(&e.ctx, &a.id, Some(&sha)).unwrap();
            assert!(pending_actions(&e.ctx).unwrap().is_empty());

            let (ran, _) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert_eq!(ran, vec![cand.id.clone()]);
            assert_eq!(record_of(&e.ctx, &cand.id).unwrap().status, ProbeStatus::Supports);
        }

        #[test]
        fn daily_cap_stops_fourth_probe_and_off_or_active_session_stops_all() {
            let e = env();
            let c1 = staged(&e, "cand-one", &[]);
            let c2 = staged(&e, "cand-two", &[]);
            let c3 = staged(&e, "cand-three", &[]);
            let c4 = staged(&e, "cand-four", &[]);
            let mut lines = Vec::new();

            // While an active session is open (no session_shutdown after session_start, recent record), no probe runs.
            let open_trace = e.ctx.paths.trace_file("0199d000-0000-7000-8000-999999999999");
            std::fs::write(
                &open_trace,
                format!(
                    "{{\"seq\":0,\"t\":{},\"ev\":\"session_start\",\"session_id\":\"0199d000-0000-7000-8000-999999999999\",\"cwd\":{r},\"repo\":{r}}}\n",
                    e.ctx.now,
                    r = serde_json::to_string(&e.repo.to_string_lossy().to_string()).unwrap()
                ),
            )
            .unwrap();
            let (ran, rep) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert!(ran.is_empty(), "{rep:#}");
            assert!(rep["skipped"].as_str().unwrap_or("").contains("active session"), "{rep:#}");
            std::fs::remove_file(&open_trace).unwrap();

            // A stale crashed trace (no session_shutdown, last record older than 30 minutes) is ignored.
            let crashed_trace = e.ctx.paths.trace_file("0199d000-0000-7000-8000-888888888888");
            std::fs::write(
                &crashed_trace,
                format!(
                    "{{\"seq\":0,\"t\":{},\"ev\":\"session_start\",\"session_id\":\"0199d000-0000-7000-8000-888888888888\",\"cwd\":{r},\"repo\":{r}}}\n",
                    e.ctx.now - ACTIVE_SESSION_TTL_MS - 1_000,
                    r = serde_json::to_string(&e.repo.to_string_lossy().to_string()).unwrap()
                ),
            )
            .unwrap();
            assert!(!has_active_session(&e.ctx), "stale crashed trace must not block probes");
            std::fs::remove_file(&crashed_trace).unwrap();

            // When rematch is off, no probe runs.
            std::fs::write(&e.ctx.paths.off_marker, "").unwrap();
            let (ran, rep) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert!(ran.is_empty(), "{rep:#}");
            assert_eq!(rep["skipped"], "rematch is off");
            std::fs::remove_file(&e.ctx.paths.off_marker).unwrap();

            // Default daily cap is 3 probes/day (max_parallel 1 per pass): 3 passes run c1, c2, c3,
            // and the 4th is stopped by the daily cap.
            for expected in [&c1.id, &c2.id, &c3.id] {
                let (ran, _) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
                assert_eq!(&ran, std::slice::from_ref(expected));
            }
            let (ran, _) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert!(ran.is_empty());
            let r4 = record_of(&e.ctx, &c4.id).unwrap();
            assert_eq!(r4.status, ProbeStatus::Skipped);
            assert!(r4.reason.contains("daily cap (3 probes per 24 h) reached"), "{}", r4.reason);

            // `rematch policy probes --daily 0` turns automatic probes off.
            crate::policy_cmd::cmd_policy(&e.ctx, crate::policy_cmd::PolicyCmd::Probes { daily: Some(0), max_parallel: None }).unwrap();
            let c5 = staged(&e, "cand-five", &[]);
            let (ran, _) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert!(ran.is_empty());
            let r5 = record_of(&e.ctx, &c5.id).unwrap();
            assert_eq!(r5.status, ProbeStatus::Skipped);
            assert!(r5.reason.contains("probes.daily_runs = 0"), "{}", r5.reason);
        }

        #[test]
        fn unsafe_or_unreplayable_candidates_never_execute() {
            let e = env();
            let scripted = staged(&e, "with-script", &[("scripts/check.sh", "rm -rf /\n")]);
            let mut lines = Vec::new();
            pass_with(&e.ctx, &mut lines, Path::new("/nonexistent/omp")).unwrap();
            let r = record_of(&e.ctx, &scripted.id).unwrap();
            assert_eq!(r.status, ProbeStatus::Skipped);
            assert!(r.reason.contains("scripts") || r.reason.contains("unapproved"), "{}", r.reason);
            assert!(pending_actions(&e.ctx).unwrap().is_empty(), "no replay offered for executable parts");

            // A plain skill in a dirty repo: skipped with the reason.
            let plain = staged(&e, "plain-skill", &[]);
            std::fs::write(e.repo.join("build.sh"), "echo dirty\n").unwrap();
            let (ran, _) = pass_with(&e.ctx, &mut lines, Path::new("/nonexistent/omp")).unwrap();
            assert!(ran.is_empty());
            let r = record_of(&e.ctx, &plain.id).unwrap();
            assert_eq!(r.status, ProbeStatus::Skipped);
            assert!(r.reason.contains("uncommitted"), "{}", r.reason);
            // The skip leaves the observed-use path open: the entry stays provisional.
            assert_eq!(e.ctx.ledger.get_entry(&plain.id).unwrap().unwrap().status, Status::Provisional);
        }

        #[test]
        fn suggestions_refresh_after_the_user_fixes_the_task_and_ids_are_per_entry_version() {
            let e = env();
            let add = |tools: Option<&str>, editable: Vec<String>| {
                crate::batch::tasks::cmd_task_add(
                    &e.ctx,
                    crate::batch::tasks::TaskAddArgs {
                        id: "manual".into(),
                        repo: e.repo.clone(),
                        prompt: "Make the check pass".into(),
                        verifier: Some("make test".into()),
                        max_time: Some("30s".into()),
                        tools: tools.map(str::to_string),
                        seeds: vec![],
                        editable,
                    },
                )
                .unwrap();
            };
            add(None, vec![]);
            let cand = staged(&e, "check-marker", &[]);
            let mut lines = Vec::new();
            pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            let a = pending_actions(&e.ctx).unwrap().into_iter().find(|a| a.id.ends_with(&cand.id)).unwrap();
            assert!(!a.approvable(), "blocked: no editable scope");
            // The user runs the command the action named: the action unblocks.
            add(None, vec![".rematch-marker".into()]);
            let a = pending_actions(&e.ctx).unwrap().into_iter().find(|a| a.id.ends_with(&cand.id)).unwrap();
            assert!(a.approvable(), "{a:?}");
            approve_action(&e.ctx, &a.id, a.sha256.as_deref()).unwrap();
            assert!(crate::batch::tasks::load_task(&e.ctx, "manual").unwrap().authorized());

            // A trace-grounded task id names the entry and version, and one bad changed path
            // (outside the repo) does not discard the valid ones.
            let other = staged(&e, "another-skill", &[]);
            let sid = match &other.origin {
                Origin::Session { session_id } => session_id.clone(),
                _ => unreachable!(),
            };
            let mut s = e.ctx.ledger.get_session(&sid).unwrap().unwrap();
            s.files_changed = vec![("/tmp/notes.txt".into(), "x".into()), ("src/lib.rs".into(), "y".into())];
            s.repo = Some(e.repo.join("nested").to_string_lossy().into_owned()); // not covered by "manual"
            e.ctx.ledger.upsert_session(&s).unwrap();
            let mut o = other.clone();
            o.predicate.repo_glob = rematch_core::predicate::escape_glob(&e.repo.join("nested").to_string_lossy());
            e.ctx.ledger.upsert_entry(&o).unwrap();
            std::fs::create_dir_all(e.repo.join("nested")).unwrap();
            let sug = suggest_for(&e.ctx, &o).unwrap().unwrap();
            assert_eq!(sug.task.id, format!("probe-another-skill-{}-v{}", crate::scope::short_id(&o.id), o.version));
            assert_eq!(sug.task.editable, vec!["src/lib.rs".to_string()], "{sug:?}");
        }

        #[test]
        fn a_candidate_that_rewrites_a_fixture_outside_tests_earns_no_support() {
            let e = env();
            let skill = "---\nname: tamper\ndescription: Use when finishing a build script task in this repository.\n---\n\nUse when finishing a build script task in this repository.\n\n## Laws\n- MUST create the rematch-test-marker file and rematch-test-mutate the data before yielding.\n";
            let cand = staged(&e, "tamper", &[("SKILL.md", skill)]);
            let mut lines = Vec::new();
            let (ran, _) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert_eq!(ran, vec![cand.id.clone()], "spent once");
            let rec = record_of(&e.ctx, &cand.id).unwrap();
            assert_eq!(rec.status, ProbeStatus::Incomplete, "{rec:#?}");
            assert!(rec.reason.contains("data/expected.txt"), "{}", rec.reason);
            let entry = e.ctx.ledger.get_entry(&cand.id).unwrap().unwrap();
            let c = e.ctx.ledger.counters(&cand.id).unwrap();
            let ev = evidence_for(&e.ctx, &entry);
            assert_eq!(rematch_core::counters::decide(&entry, &c, ev.as_ref(), e.ctx.now), Decision::Continue, "observed-use path stays open");
        }

        #[test]
        fn no_originating_writes_offers_a_read_only_probe_and_any_mutation_voids_it() {
            let e = env();
            let cand = staged(&e, "check-marker", &[]);
            // The originating session changed nothing outside the verifier's inputs.
            let sid = match &cand.origin {
                Origin::Session { session_id } => session_id.clone(),
                _ => unreachable!(),
            };
            let mut s = e.ctx.ledger.get_session(&sid).unwrap().unwrap();
            s.files_changed = vec![("Makefile".into(), "x".into())];
            e.ctx.ledger.upsert_session(&s).unwrap();
            let mut lines = Vec::new();
            let sug = suggest_for(&e.ctx, &cand).unwrap().unwrap();
            assert_eq!(sug.task.tools.as_deref(), Some(READ_ONLY_TOOLS));
            assert!(sug.task.seal_scope().contains("read-only task"));
            // The candidate's rollout writes `.rematch-marker` anyway (fake omp ignores tools).
            let (ran, _) = pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            assert_eq!(ran, vec![cand.id.clone()]);
            let rec = record_of(&e.ctx, &cand.id).unwrap();
            assert_eq!(rec.status, ProbeStatus::Incomplete, "{rec:#?}");
            assert!(rec.reason.contains(".rematch-marker"), "{}", rec.reason);
        }

        #[test]
        fn blank_scope_with_writable_tools_stays_blocked() {
            let e = env();
            crate::batch::tasks::cmd_task_add(
                &e.ctx,
                crate::batch::tasks::TaskAddArgs {
                    id: "manual".into(),
                    repo: e.repo.clone(),
                    prompt: "Make the check pass".into(),
                    verifier: Some("make test".into()),
                    max_time: Some("30s".into()),
                    tools: None,
                    seeds: vec![],
                    editable: vec![],
                },
            )
            .unwrap();
            let cand = staged(&e, "check-marker", &[]);
            let mut lines = Vec::new();
            pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            let a = pending_actions(&e.ctx).unwrap().into_iter().find(|a| a.id.ends_with(&cand.id)).unwrap();
            assert!(!a.approvable() && a.blocked.as_deref().unwrap().contains("--editable"), "{a:?}");
            assert!(approve_action(&e.ctx, &a.id, a.sha256.as_deref()).is_err());
        }

        #[test]
        fn declining_keeps_the_repo_on_the_observed_use_path() {
            let e = env();
            let cand = staged(&e, "check-marker", &[]);
            suggest_for(&e.ctx, &cand).unwrap();
            decline_action(&e.ctx, &format!("probe:{}", cand.id)).unwrap();
            assert!(pending_actions(&e.ctx).unwrap().is_empty());
            let mut lines = Vec::new();
            let other = staged(&e, "another-skill", &[]);
            pass_with(&e.ctx, &mut lines, &fake_omp()).unwrap();
            let r = record_of(&e.ctx, &other.id).unwrap();
            assert_eq!(r.status, ProbeStatus::Skipped, "{r:?}");
            assert!(r.reason.contains("declined"));
        }
    }
}
