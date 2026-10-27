//! `run-worker --job <id>`: one Harness² cycle for a task (SPEC "Batch mode").
//!
//! 1. Reference rollout with the current harness (base library for the task repo).
//!    Entries a previous run staged for this task (`pending_next_run`) are loaded in it
//!    and decided by its verdict: pass ⇒ `kept`, fail ⇒ `retired` (the next-run rule).
//!    A keep that replaces a kept entry first replays the kept entry's originating task
//!    once with the pending entry in its place (`scope::anchor`); a failing replay retires
//!    the pending entry. Anchor replays are outside the cycle's rollout budget.
//! 2. Invariant and domain candidates: one editor call each (`improver/editor.md`, input =
//!    the reference trajectory around the longest tool-error stretch, else its tail where
//!    the verifier failed), lint, then one probe rollout each on the same task and seed.
//! 3. Composition of every (invariant, domain) pair: the best-ranked pair gets one editor
//!    call with scope `compose` (both candidates, their verdicts and the behavioral diff)
//!    while the model-call budget lasts; the others, and any composer failure, use the
//!    deterministic merge (union of the candidates' files; the domain side's laws that are
//!    simhash near-duplicates of an invariant law are dropped). Each composed harness runs
//!    once; with several composed variants the best one runs once more (confirmation).
//! 4. The verifier picks: highest (pass, score); ties go to the reference (KEEP_BASE).
//!    The winner's lessons are staged `provisional`, origin `Task`, `pending_next_run`.
//!
//! Budget at width k: rollouts = (k+1)² + [k>1] — 1 reference + 2k candidates + k²
//! composed + 1 confirmation when k > 1 (4 at k=1, 10 at k=2); model calls = 2k editors +
//! 1 composer. A reference that passes a score-less verifier ends the cycle after one
//! rollout: nothing can rank above it. Workspaces `runs/<task>/<seed>/` are removed after
//! `runs/<task>/receipt.json` is written (and on failure); every rollout's event stream
//! stays in the ledger's blob store.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use rematch_core::dedup::{entry_content, near_duplicate, route_duplicate};
use rematch_core::features::{head, rank_stems};
use rematch_core::ledger::{parse_enum, EntryFilter};
use rematch_core::library::{branch_for, dir_for, primary_doc, set_status, stage, validate_name};
use rematch_core::lint::{branch_always_on_bytes, lint_entry, LintConfig, LintContext};
use rematch_core::package::SkillDoc;
use rematch_core::predicate::{derive, predicate_hash};
use rematch_core::trace::TraceRecord;
use rematch_core::types::{
    Activation, Certificate, Edit, EditorScope, Entry, Family, Kind, LessonTag, LintReport, Manifest, Moment,
    Origin, Predicate, Receipt, Scope, Status, Verdict,
};
use rematch_core::family::family_of_ref;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::rollout::{run_rollout, tracing_line, Rollout};
use super::substrate::{
    pending_for_task, write_entry_files, BaseLibrary, CandidateTree, OmpSubstrate, Substrate, TreeEntry,
};
use super::tasks::{is_git_repo, load_task, task_runs_dir, Task};
use super::verifier::{detail_of, is_pass, parse_spec, score_of, Verifier};
use crate::app::{new_id, AppCtx};
use crate::config::{resolve_role, ROLE_SOLVER};
use crate::editor::{call_editor, parse_editor_reply, CallBudget, EditorDecision, EditorEntry, EditorReply};
use crate::envelope::Cancelled;
use crate::scope::anchor::{certify, failure_note, replaced_kept, replay_in_run, AnchorOutcome};
use crate::jobs;
use crate::omp;

pub const MAX_WIDTH: u32 = 4;

/// Lint rule ids shown to the editor (DESIGN §12).
const LINT_RULES: &[&str] = &[
    "identifiers",
    "string_match_user",
    "executable_risk",
    "trigger",
    "scripts",
    "always_on",
    "form_order",
    "facts_as_of",
    "hook",
];

/// Records before / after the failure moment in the editor excerpt.
const EXCERPT_BEFORE: usize = 8;
const EXCERPT_AFTER: usize = 6;
/// Tail kept when the verifier failed without a tool-error stretch.
const EXCERPT_TAIL: usize = 14;

/// Rollouts at width k: 1 reference + 2k candidates + k² composed + 1 confirmation (k > 1).
pub fn rollout_budget(width: u32) -> u32 {
    (width + 1) * (width + 1) + u32::from(width > 1)
}

/// Model calls at width k: 2k editor calls + 1 composer call.
pub fn model_call_budget(width: u32) -> u32 {
    2 * width + 1
}

/// The job payload written by `run` (merged into `jobs/<id>.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    pub task: String,
    pub verifier: String,
    pub width: u32,
    pub seed: u64,
    /// Scripted editor (tests); `None` ⇒ omp with the editor role.
    #[serde(default)]
    pub editor: Option<String>,
    /// omp binary resolved when the job was submitted.
    #[serde(default)]
    pub omp_bin: Option<PathBuf>,
}

#[derive(Debug, Clone, clap::Args)]
pub struct RunWorkerArgs {
    #[arg(long)]
    pub job: String,
}

pub fn cmd_run_worker(ctx: &AppCtx, args: RunWorkerArgs) -> Result<Value> {
    let rec = jobs::read(ctx, &args.job)?;
    let spec: JobSpec = serde_json::from_value(rec).context("job payload")?;
    let mut progress = Progress::new(Some(&args.job), rollout_budget(spec.width));
    match run_cycle(ctx, &spec, &mut progress) {
        Ok(v) => {
            jobs::update(ctx, &args.job, "done", &json!({"phase": "done", "result": v.clone()}))?;
            Ok(v)
        }
        Err(e) => {
            let status = if e.downcast_ref::<Cancelled>().is_some() { "cancelled" } else { "failed" };
            let _ = jobs::update(ctx, &args.job, status, &json!({"phase": status, "error": format!("{e:#}")}));
            Err(e)
        }
    }
}

/// `create --from task`: one reference rollout with the current harness, no candidates.
/// Returns the trace (rematch schema, `trace::read_trace`) at
/// `runs/<task>/traces/<session_id>.jsonl`; the workspace is removed.
pub fn cmd_reference_rollout(ctx: &AppCtx, task_id: &str) -> Result<PathBuf> {
    reference_rollout(ctx, task_id, &omp::omp_bin())
}

/// [`cmd_reference_rollout`] with an explicit omp binary.
pub fn reference_rollout(ctx: &AppCtx, task_id: &str, omp_bin: &Path) -> Result<PathBuf> {
    let task = load_task(ctx, task_id)?;
    let runs = task_runs_dir(ctx, &task.id);
    std::fs::create_dir_all(&runs)?;
    let _lock = jobs::LockFile::acquire(runs.join(".lock"))?
        .ok_or_else(|| anyhow!("another run of task {} is in progress", task.id))?;
    let solver = resolve_role(&ctx.paths, &task.repo, ROLE_SOLVER)?.model;
    let sub = OmpSubstrate { repo: task.repo.clone(), solver };
    let base = BaseLibrary::load(ctx, &task.repo)?;
    let seed = task.first_seed();
    let r = run_rollout(ctx, &sub, omp_bin, &CandidateTree::reference(&base), &task, seed, "reference", None);
    let out = r.and_then(|r| {
        if let Some(e) = &r.error {
            bail!("reference rollout failed: {e}");
        }
        let dir = runs.join("traces");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.jsonl", r.session.id));
        let mut text = String::new();
        for rec in &r.records {
            text.push_str(&serde_json::to_string(rec)?);
            text.push('\n');
        }
        std::fs::write(&path, text)?;
        Ok((path, r))
    });
    let cleanup = clean_workspaces(&sub, &runs, seed, out.as_ref().ok().map(|(_, r)| r));
    let (path, _) = out?;
    cleanup?;
    Ok(path)
}

/// Job progress mirrored into `jobs/<id>.json` (`phase`, `rollouts_done`, `verdicts`).
pub struct Progress<'a> {
    job: Option<&'a str>,
    pub phase: String,
    pub rollouts_done: u32,
    pub rollouts_budget: u32,
    pub model_calls: u32,
    pub verdicts: Vec<Value>,
}

impl<'a> Progress<'a> {
    pub fn new(job: Option<&'a str>, rollouts_budget: u32) -> Progress<'a> {
        Progress { job, phase: String::new(), rollouts_done: 0, rollouts_budget, model_calls: 0, verdicts: Vec::new() }
    }

    fn flush(&self, ctx: &AppCtx) -> Result<()> {
        if let Some(job) = self.job {
            jobs::update(
                ctx,
                job,
                "running",
                &json!({
                    "phase": self.phase,
                    "rollouts_done": self.rollouts_done,
                    "rollouts_budget": self.rollouts_budget,
                    "model_calls": self.model_calls,
                    "verdicts": self.verdicts,
                }),
            )?;
        }
        Ok(())
    }

    fn set_phase(&mut self, ctx: &AppCtx, phase: impl Into<String>) -> Result<()> {
        self.phase = phase.into();
        tracing_line(&self.phase);
        self.flush(ctx)
    }

    fn rollout_done(&mut self, ctx: &AppCtx, r: &Rollout) -> Result<()> {
        self.rollouts_done += 1;
        let v = r.verdict.as_ref();
        self.verdicts.push(json!({
            "label": r.label,
            "pass": v.is_some_and(is_pass),
            "score": v.and_then(score_of),
            "detail": v.map(|v| head(detail_of(v), 300)),
            "tool_errors": r.session.tool_errors,
            "turns": r.session.turns,
            "error": r.error,
        }));
        self.flush(ctx)
    }
}

/// A lesson proposed by the editor (or merged), linted, as files.
#[derive(Debug, Clone)]
struct Lesson {
    name: String,
    files: BTreeMap<String, String>,
    editor: EditorEntry,
    manifest: Manifest,
    lint: LintReport,
}

impl Lesson {
    fn tree_entry(&self) -> TreeEntry {
        TreeEntry { name: self.name.clone(), id: None, files: self.files.clone() }
    }
}

/// A harness that was rolled out (candidate or composed).
struct Variant {
    label: String,
    kind: &'static str,
    scope: Option<EditorScope>,
    composer: Option<String>,
    lessons: Vec<Lesson>,
    rollout: Rollout,
    confirm: Option<Rollout>,
}

impl Variant {
    /// (pass, score) after the confirmation rollout, when there was one.
    fn key(&self) -> (bool, Option<f64>) {
        let mut k = verdict_key(self.rollout.verdict.as_ref());
        if let Some(c) = &self.confirm {
            let ck = verdict_key(c.verdict.as_ref());
            k = (k.0 && ck.0, match (k.1, ck.1) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            });
        }
        k
    }
}

fn verdict_key(v: Option<&Verdict>) -> (bool, Option<f64>) {
    (v.is_some_and(is_pass), v.and_then(score_of))
}

fn cmp_key(a: (bool, Option<f64>), b: (bool, Option<f64>)) -> Ordering {
    a.0.cmp(&b.0).then(a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal))
}

/// Ranking among non-reference harnesses: (pass, score) desc, then fewer tool errors,
/// fewer turns, fewer lessons, earlier label.
fn rank(a: &Variant, b: &Variant) -> Ordering {
    cmp_key(b.key(), a.key())
        .then(a.rollout.session.tool_errors.cmp(&b.rollout.session.tool_errors))
        .then(a.rollout.session.turns.cmp(&b.rollout.session.turns))
        .then(a.lessons.len().cmp(&b.lessons.len()))
}

/// What one editor/composer call produced.
#[derive(Debug, Clone, Serialize)]
struct ProposalRecord {
    label: String,
    scope: String,
    decision: String,
    error: Option<String>,
    lint: Option<LintReport>,
    names: Vec<String>,
    proposal_id: Option<String>,
}

struct Cycle<'a> {
    ctx: &'a AppCtx,
    task: &'a Task,
    spec: &'a JobSpec,
    sub: &'a OmpSubstrate,
    omp_bin: PathBuf,
    verifier: &'a dyn Verifier,
    lint_cfg: LintConfig,
    branch: String,
    family: Family,
    budget: CallBudget,
    rollouts: Vec<Rollout>,
    proposals: Vec<ProposalRecord>,
    candidates_dir: PathBuf,
}

pub fn run_cycle(ctx: &AppCtx, spec: &JobSpec, progress: &mut Progress) -> Result<Value> {
    if ctx.paths.is_off() {
        bail!("rematch is off; batch runs stage entries and are refused");
    }
    let task = load_task(ctx, &spec.task)?;
    let verifier = parse_spec(&spec.verifier)?;
    let width = spec.width.clamp(1, MAX_WIDTH);
    let runs = task_runs_dir(ctx, &task.id);
    std::fs::create_dir_all(&runs)?;
    let _lock = jobs::LockFile::acquire(runs.join(".lock"))?
        .ok_or_else(|| anyhow!("another run of task {} is in progress", task.id))?;
    let solver = resolve_role(&ctx.paths, &task.repo, ROLE_SOLVER)?.model;
    let sub = OmpSubstrate { repo: task.repo.clone(), solver: solver.clone() };
    let git_repo = is_git_repo(&task.repo).then_some(task.repo.as_path());
    let mut cycle = Cycle {
        ctx,
        task: &task,
        spec,
        sub: &sub,
        omp_bin: spec.omp_bin.clone().unwrap_or_else(omp::omp_bin),
        verifier: verifier.as_ref(),
        lint_cfg: LintConfig::load(&ctx.improver_file("lint.toml"))?,
        branch: branch_for(git_repo, &task.repo),
        family: family_of_ref(&solver),
        budget: CallBudget::new(model_call_budget(width)),
        rollouts: Vec::new(),
        proposals: Vec::new(),
        candidates_dir: runs.join(spec.seed.to_string()).join("candidates"),
    };
    let result = cycle.execute(width, progress);
    let cleanup = cycle.cleanup(&runs);
    let v = result?;
    cleanup?;
    Ok(v)
}

impl Cycle<'_> {
    fn rollout(&mut self, tree: &CandidateTree, label: &str, progress: &mut Progress) -> Result<Rollout> {
        progress.set_phase(self.ctx, format!("rollout {label}"))?;
        let r = run_rollout(
            self.ctx,
            self.sub,
            &self.omp_bin,
            tree,
            self.task,
            self.spec.seed,
            label,
            Some(self.verifier),
        )?;
        progress.rollout_done(self.ctx, &r)?;
        self.rollouts.push(r.clone());
        Ok(r)
    }

    fn cleanup(&self, runs: &Path) -> Result<()> {
        let mut first_err = None;
        for r in &self.rollouts {
            if let Err(e) = self.sub.cleanup(&r.install) {
                first_err.get_or_insert(e);
            }
        }
        let seed_dir = runs.join(self.spec.seed.to_string());
        if seed_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&seed_dir) {
                first_err.get_or_insert(e.into());
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    fn execute(&mut self, width: u32, progress: &mut Progress) -> Result<Value> {
        let ctx = self.ctx;
        // 1. Reference rollout with the current harness (pending entries included).
        let pending = pending_for_task(ctx, &self.task.id)?;
        let mut base0 = BaseLibrary::load(ctx, &self.task.repo)?;
        for e in &pending {
            if !base0.entries.iter().any(|b| b.id.as_deref() == Some(e.id.as_str())) && e.path.is_dir() {
                base0.entries.push(TreeEntry {
                    name: e.name.clone(),
                    id: Some(e.id.clone()),
                    files: super::substrate::read_entry_files(&e.path)?,
                });
            }
        }
        let reference = self.rollout(&CandidateTree::reference(&base0), "reference", progress)?;

        // Next-run rule for entries a previous run of this task staged.
        progress.set_phase(ctx, "next-run decisions")?;
        let (decisions, retired) = self.resolve_pending(&pending, &reference)?;
        let base = base0.without(&retired);

        let ref_key = verdict_key(reference.verdict.as_ref());
        let mut variants: Vec<Variant> = Vec::new();
        let mut early_stop = None;
        if ref_key.0 && ref_key.1.is_none() {
            early_stop = Some("reference passed a score-less verifier; no harness can rank above it".to_string());
        } else {
            self.candidates(width, &reference, &base, &mut variants, progress)?;
            self.compose(&reference, &base, &mut variants, progress)?;
        }

        // 5. The verifier picks.
        progress.set_phase(ctx, "select")?;
        let mut order: Vec<usize> = (0..variants.len()).collect();
        order.sort_by(|&a, &b| rank(&variants[a], &variants[b]).then(a.cmp(&b)));
        let winner = order
            .first()
            .map(|&i| &variants[i])
            .filter(|v| v.key().0 && cmp_key(v.key(), ref_key) == Ordering::Greater);

        let mut staged = Vec::new();
        let mut admission = Value::Null;
        if let Some(w) = winner {
            progress.set_phase(ctx, format!("stage {}", w.label))?;
            let (s, a) = self.stage_winner(w, &reference)?;
            staged = s;
            admission = a;
        }

        let receipt = json!({
            "task": self.task.id,
            "job_spec": self.spec,
            "seed": self.spec.seed,
            "width": width,
            "budget": {
                "formula": "rollouts = (k+1)^2 + [k>1]: 1 reference + 2k candidates + k^2 composed + 1 confirmation when k>1; model calls = 2k editors + 1 composer",
                "rollouts_planned": rollout_budget(width),
                "rollouts_done": self.rollouts.len(),
                "model_calls_budget": self.budget.max,
                "model_calls_used": self.budget.used,
            },
            "verifier": self.verifier.describe(),
            "solver": self.sub.solver,
            "branch": self.branch,
            "early_stop": early_stop,
            "next_run": decisions,
            "rollouts": self.rollouts.iter().map(Rollout::summary).collect::<Vec<_>>(),
            "proposals": self.proposals,
            "variants": variants.iter().map(|v| json!({
                "label": v.label,
                "kind": v.kind,
                "scope": v.scope,
                "composer": v.composer,
                "lessons": v.lessons.iter().map(|l| json!({"name": l.name, "lint": l.lint, "scope": l.manifest.scope})).collect::<Vec<_>>(),
                "pass": v.key().0,
                "score": v.key().1,
                "rollout": v.rollout.label,
                "confirm": v.confirm.as_ref().map(|c| c.label.clone()),
            })).collect::<Vec<_>>(),
            "winner": winner.map_or("reference".to_string(), |w| w.label.clone()),
            "decision": if winner.is_some() { "EDIT" } else { "KEEP_BASE" },
            "staged": staged,
            "admission": admission,
            "at": ctx.now,
        });
        let path = task_runs_dir(ctx, &self.task.id).join("receipt.json");
        let text = serde_json::to_string_pretty(&receipt)?;
        std::fs::write(&path, &text).with_context(|| format!("write {}", path.display()))?;
        let blob = ctx.ledger.put_blob(text.as_bytes())?;
        Ok(json!({
            "task": self.task.id,
            "winner": receipt["winner"],
            "decision": receipt["decision"],
            "rollouts": self.rollouts.len(),
            "model_calls": self.budget.used,
            "staged": receipt["staged"],
            "next_run": receipt["next_run"],
            "receipt": path,
            "receipt_blob": blob,
        }))
    }

    /// Pending entries of this task: reference pass ⇒ kept, fail ⇒ retired. A keep that
    /// replaces a kept entry first runs its anchor replay (`scope::anchor`); a failing
    /// replay retires the pending entry and the kept one stays.
    fn resolve_pending(&self, pending: &[Entry], reference: &Rollout) -> Result<(Vec<Value>, Vec<String>)> {
        let ctx = self.ctx;
        let pass = reference.verdict.as_ref().is_some_and(is_pass);
        let mut out = Vec::new();
        let mut retired = Vec::new();
        for e in pending {
            let mut anchor_json = Value::Null;
            let mut anchor_pass = None;
            let (to, note) = if pass {
                let note = format!("batch next run of task {} passed ({})", self.task.id, reference.session.id);
                match replaced_kept(ctx, e)? {
                    None => (Status::Kept, note),
                    Some(kept) => match replay_in_run(ctx, e, &kept, &self.omp_bin, &self.task.id) {
                        Ok(outcome) => {
                            anchor_json = json!({"kept": kept.id, "replay": outcome});
                            match outcome {
                                AnchorOutcome::Replayed { pass: false, receipt } => (Status::Retired, failure_note(&receipt)),
                                AnchorOutcome::Replayed { pass: true, receipt } => {
                                    anchor_pass = Some(receipt);
                                    (Status::Kept, note)
                                }
                                AnchorOutcome::NoVerifier => (Status::Kept, note),
                            }
                        }
                        Err(err) => {
                            // Keep the flag so the next run decides again.
                            out.push(json!({"id": e.id, "name": e.name, "status": e.status, "error": format!("anchor replay: {err:#}")}));
                            continue;
                        }
                    },
                }
            } else {
                (Status::Retired, format!("batch next run of task {} failed ({})", self.task.id, reference.session.id))
            };
            let mut e2 = e.clone();
            e2.pending_next_run = false;
            if to == Status::Kept && !e2.verified_on.contains(&self.family) {
                e2.verified_on.push(self.family.clone());
            }
            ctx.ledger.upsert_entry(&e2)?;
            match set_status(&ctx.ledger, &ctx.paths, &e.id, to, Some(&note)) {
                Ok(moved) => {
                    let mut row = json!({"id": e.id, "name": e.name, "status": to});
                    if to == Status::Kept {
                        let lint = lint_entry(&moved.path, &moved, &self.lint_cfg, &LintContext::default())?;
                        self.insert_receipt(&moved.id, lint, reference.verdict.clone())?;
                        if let Some(r) = &anchor_pass {
                            anchor_json["certificate"] = json!(certify(ctx, &moved, r, None)?);
                        }
                    } else {
                        retired.push(e.id.clone());
                    }
                    if !anchor_json.is_null() {
                        row["anchor"] = anchor_json;
                    }
                    out.push(row);
                }
                Err(err) => {
                    // Keep the flag so the next run decides again.
                    ctx.ledger.upsert_entry(e)?;
                    out.push(json!({"id": e.id, "name": e.name, "status": e.status, "error": format!("{err:#}")}));
                }
            }
        }
        Ok((out, retired))
    }

    fn insert_receipt(&self, entry_id: &str, lint: LintReport, verifier: Option<Verdict>) -> Result<String> {
        let cert = Certificate { lint, canary: None, verifier };
        let sha = self.ctx.ledger.put_blob(serde_json::to_string(&cert)?.as_bytes())?;
        self.ctx.ledger.insert_receipt(&Receipt {
            certificate: sha.clone(),
            entry: entry_id.to_string(),
            activation: Activation::Personal,
        })?;
        Ok(sha)
    }

    /// Editor input common to every call of this cycle.
    fn editor_input(&self, reference: &Rollout, base: &BaseLibrary) -> Result<Value> {
        let records = &reference.records;
        let excerpt: Vec<TraceRecord> = match longest_error_stretch(records) {
            Some(seq) => rematch_core::trace::excerpt_around(records, seq, EXCERPT_BEFORE, EXCERPT_AFTER),
            None => records[records.len().saturating_sub(EXCERPT_TAIL)..].to_vec(),
        };
        let deliverable = records
            .iter()
            .rev()
            .find_map(|r| match r {
                TraceRecord::TurnEnd { text_head, .. } if !text_head.is_empty() => Some(text_head.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let detail = reference.verdict.as_ref().map(|v| head(detail_of(v), 1024)).unwrap_or_default();
        let mut library = Vec::new();
        for b in &base.entries {
            let trigger = match &b.id {
                Some(id) => self.ctx.ledger.get_entry(id)?.map(|e| e.trigger_line).unwrap_or_default(),
                None => String::new(),
            };
            library.push(json!({
                "name": b.name,
                "trigger_line": trigger,
                "experience": b.files.get("EXPERIENCE.md").map(|t| head(t, 1024)).unwrap_or_default(),
            }));
        }
        Ok(json!({
            "scope": "domain",
            "trace_excerpt": excerpt,
            "deliverable": deliverable,
            "redirect": {"class": "correction", "text": format!("Verifier {} judged the run: {detail}", self.verifier.describe())},
            "current_entry": null,
            "library": library,
            "lint": {"rules": LINT_RULES},
            "retry_findings": null,
            "task": {"prompt": head(&self.task.prompt, 4096)},
            "verifier": {"spec": self.verifier.describe(), "verdict": reference.verdict},
        }))
    }

    /// 2. k invariant + k domain candidates, each probed once.
    fn candidates(
        &mut self,
        width: u32,
        reference: &Rollout,
        base: &BaseLibrary,
        variants: &mut Vec<Variant>,
        progress: &mut Progress,
    ) -> Result<()> {
        let common = self.editor_input(reference, base)?;
        for scope in [EditorScope::Invariant, EditorScope::Domain] {
            let scope_s = scope_str(scope);
            let mut prior: Vec<Value> = Vec::new();
            for i in 1..=width {
                let label = format!("{scope_s}-{i}");
                progress.set_phase(self.ctx, format!("editor {label}"))?;
                let mut input = common.clone();
                input["scope"] = json!(scope_s);
                if !prior.is_empty() {
                    input["prior_candidates"] = json!(prior);
                }
                let lesson = self.propose(&label, scope_s, &input, scope, reference, progress)?;
                let Some(lesson) = lesson else { continue };
                prior.push(json!({"name": lesson.name, "trigger_line": lesson.editor.trigger_line}));
                let tree = CandidateTree { entries: vec![lesson.tree_entry()], base: base.clone() };
                let rollout = self.rollout(&tree, &label, progress)?;
                variants.push(Variant {
                    label,
                    kind: "candidate",
                    scope: Some(scope),
                    composer: None,
                    lessons: vec![lesson],
                    rollout,
                    confirm: None,
                });
            }
        }
        Ok(())
    }

    /// One editor call → a linted lesson (`None` when KEEP_BASE, failed, or lint-rejected).
    fn propose(
        &mut self,
        label: &str,
        scope_s: &str,
        input: &Value,
        scope: EditorScope,
        reference: &Rollout,
        progress: &mut Progress,
    ) -> Result<Option<Lesson>> {
        let mut rec = ProposalRecord {
            label: label.to_string(),
            scope: scope_s.to_string(),
            decision: "error".into(),
            error: None,
            lint: None,
            names: Vec::new(),
            proposal_id: None,
        };
        let outcome = self.propose_inner(label, input, scope, reference, progress, &mut rec);
        let lesson = match outcome {
            Ok(l) => l,
            Err(e) if e.downcast_ref::<Cancelled>().is_some() => return Err(e),
            Err(e) => {
                rec.error = Some(format!("{e:#}"));
                None
            }
        };
        self.proposals.push(rec);
        Ok(lesson)
    }

    fn propose_inner(
        &mut self,
        label: &str,
        input: &Value,
        scope: EditorScope,
        reference: &Rollout,
        progress: &mut Progress,
        rec: &mut ProposalRecord,
    ) -> Result<Option<Lesson>> {
        self.budget.take(label)?;
        progress.model_calls = self.budget.used;
        let text = call_editor(self.ctx, self.spec.editor.as_deref(), input)?;
        let reply = parse_editor_reply(&text)?;
        let row = self.ctx.ledger.insert_proposal(
            None,
            Some(&reference.session.id),
            &json!({"task": self.task.id, "label": label, "reply": reply}),
            reply.decision == EditorDecision::Edit,
        )?;
        rec.proposal_id = Some(row.id);
        if reply.decision == EditorDecision::KeepBase {
            rec.decision = "KEEP_BASE".into();
            return Ok(None);
        }
        rec.decision = "EDIT".into();
        let lesson = self.prepare_lesson(label, &reply, scope, reference)?;
        rec.names.push(lesson.name.clone());
        rec.lint = Some(lesson.lint.clone());
        Ok(lesson.lint.ok.then_some(lesson))
    }

    /// Normalize the reply's files (SKILL.md — AGENT.md for agents — frontmatter
    /// name/description, hook.json), write them under the candidates dir and lint them as
    /// a provisional entry.
    fn prepare_lesson(&self, label: &str, reply: &EditorReply, scope: EditorScope, reference: &Rollout) -> Result<Lesson> {
        let e = reply.entry.clone().ok_or_else(|| anyhow!("EDIT reply without entry"))?;
        let m = reply.manifest.clone().ok_or_else(|| anyhow!("EDIT reply without manifest"))?;
        validate_name(&e.name)?;
        let kind: Kind = parse_enum(&e.kind).unwrap_or(Kind::Skill);
        let primary = primary_doc(kind);
        let mut files = e.files.clone();
        // A missing AGENT.md (the subagent's system prompt) is not synthesized; lint reports it.
        if kind != Kind::Agent || files.contains_key(primary) {
            let text = files
                .get(primary)
                .cloned()
                .unwrap_or_else(|| format!("\n# {}\n\n{}\n", e.name, e.trigger_line));
            let mut doc = SkillDoc::parse(&text)?;
            doc.set("name", e.name.as_str());
            if doc.get_str("description").is_none_or(|d| d.trim().is_empty()) {
                let d = if e.description.trim().is_empty() { &e.trigger_line } else { &e.description };
                doc.set("description", d.as_str());
            }
            files.insert(primary.into(), doc.render());
        }
        if let Some(h) = &e.hook {
            files.insert("hook.json".into(), serde_json::to_string_pretty(h)?);
        }
        // Evidence must point into the reference trajectory.
        let evidence = if m.evidence.session_id == reference.session.id
            && reference.records.iter().any(|r| r.seq() == m.evidence.seq)
        {
            m.evidence.clone()
        } else {
            Moment { session_id: reference.session.id.clone(), seq: failure_seq(&reference.records) }
        };
        let manifest = Manifest {
            evidence,
            predicted_fix: m.predicted_fix.clone(),
            predicted_regression: m.predicted_regression.clone(),
            rationale: m.rationale.clone(),
            scope,
        };
        let mut lesson = Lesson { name: e.name.clone(), files, editor: e, manifest, lint: LintReport::default() };
        lesson.lint = self.lint_lesson(label, &lesson, reference)?;
        Ok(lesson)
    }

    fn lint_lesson(&self, label: &str, lesson: &Lesson, reference: &Rollout) -> Result<LintReport> {
        let dir = self.candidates_dir.join(label).join(&lesson.name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        write_entry_files(&dir, &lesson.files)?;
        let mut entry = self.entry_for(lesson, reference)?;
        entry.path = dir.clone();
        let lctx = LintContext {
            branch_other_bytes: branch_always_on_bytes(&self.ctx.paths.staging_branch(&self.branch), &dir),
            user_names: std::env::var("USER").ok().filter(|u| !u.is_empty()).into_iter().collect(),
            transfer: false,
        };
        lint_entry(&dir, &entry, &self.lint_cfg, &lctx)
    }

    /// Provisional entry for a lesson: origin Task, branch and predicate of the task repo.
    fn entry_for(&self, lesson: &Lesson, reference: &Rollout) -> Result<Entry> {
        let now = self.ctx.now;
        let kind: Kind = parse_enum(&lesson.editor.kind).unwrap_or(Kind::Skill);
        Ok(Entry {
            id: new_id(),
            kind,
            name: lesson.name.clone(),
            branch: self.branch.clone(),
            scope: Scope::Personal,
            status: Status::Provisional,
            family: self.family.clone(),
            verified_on: Vec::new(),
            helpful: 0,
            harmful: 0,
            evidence: vec![lesson.manifest.evidence.clone()],
            origin: Origin::Task { task_id: self.task.id.clone() },
            predicate: self.predicate(reference)?,
            trigger_line: lesson.editor.trigger_line.clone(),
            redirect_class: None,
            predecessor: None,
            version: 1,
            path: PathBuf::new(),
            created_at: now,
            updated_at: now,
            manifest: Some(lesson.manifest.clone()),
            lesson_tag: Some(match lesson.manifest.scope {
                EditorScope::Invariant => LessonTag::Invariant,
                EditorScope::Domain => LessonTag::Domain,
            }),
            pinned: false,
            hook: lesson.editor.hook.clone(),
            approved_scripts: Vec::new(),
            approved_fragment: None,
            approved_agent: None,
            sibling_of: None,
            status_since: None,
            status_note: None,
            pending_next_run: true,
            fork_path: lesson.editor.fork_path.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            fork_repo: None,
            fork_commit: None,
            families_excluded: Vec::new(),
            links: Vec::new(),
            content_sha256: None,
            last_tune_at: None,
            tune: None,
        })
    }

    /// DESIGN §6: the task repo, the reference rollout's file extensions, and its stems
    /// (task prompt + touched basenames) ranked by tf-idf against the session corpus; the
    /// domain is the reference rollout's (`domain::session_domain`).
    fn predicate(&self, reference: &Rollout) -> Result<Predicate> {
        let corpus: Vec<Vec<String>> =
            self.ctx.ledger.list_sessions(&Default::default())?.into_iter().map(|s| s.stems).collect();
        let stems = rank_stems(&reference.session.stems, &corpus);
        let domain = rematch_core::domain::session_domain(&reference.session, &self.ctx.domain_policy()?);
        Ok(derive(Some(&self.task.repo.to_string_lossy()), &reference.session.exts, &stems, domain.as_deref()))
    }

    /// 3–4. Composed variants for every (invariant, domain) pair; confirmation when > 1.
    fn compose(
        &mut self,
        reference: &Rollout,
        base: &BaseLibrary,
        variants: &mut Vec<Variant>,
        progress: &mut Progress,
    ) -> Result<()> {
        let inv: Vec<usize> = (0..variants.len()).filter(|&i| variants[i].scope == Some(EditorScope::Invariant)).collect();
        let dom: Vec<usize> = (0..variants.len()).filter(|&i| variants[i].scope == Some(EditorScope::Domain)).collect();
        let mut pairs: Vec<(usize, usize)> = inv.iter().flat_map(|&a| dom.iter().map(move |&b| (a, b))).collect();
        // Best-ranked pair first: it gets the composer call.
        pairs.sort_by(|&(a1, b1), &(a2, b2)| {
            let s = |a: usize, b: usize| {
                (
                    u8::from(variants[a].key().0) + u8::from(variants[b].key().0),
                    std::cmp::Reverse(variants[a].rollout.session.tool_errors + variants[b].rollout.session.tool_errors),
                )
            };
            s(a2, b2).cmp(&s(a1, b1))
        });
        let mut composed: Vec<Variant> = Vec::new();
        for (n, &(a, b)) in pairs.iter().enumerate() {
            let label = format!("composed-{}-{}", label_index(&variants[a].label), label_index(&variants[b].label));
            let mut composer = None;
            let mut lessons = None;
            if n == 0 && self.budget.used < self.budget.max {
                progress.set_phase(self.ctx, format!("composer {label}"))?;
                let input = self.composer_input(reference, base, &variants[a], &variants[b])?;
                match self.propose(&label, "compose", &input, EditorScope::Domain, reference, progress)? {
                    Some(l) => {
                        composer = Some("editor".to_string());
                        lessons = Some(vec![l]);
                    }
                    None => composer = Some("editor declined or failed; deterministic merge".to_string()),
                }
            }
            let lessons = match lessons {
                Some(l) => l,
                None => {
                    composer.get_or_insert_with(|| "deterministic merge".to_string());
                    match self.merge(&label, &variants[a], &variants[b], reference)? {
                        Some(l) => l,
                        None => continue,
                    }
                }
            };
            let tree = CandidateTree { entries: lessons.iter().map(Lesson::tree_entry).collect(), base: base.clone() };
            let rollout = self.rollout(&tree, &label, progress)?;
            composed.push(Variant { label, kind: "composed", scope: None, composer, lessons, rollout, confirm: None });
        }
        if composed.len() > 1 {
            let best = (0..composed.len())
                .min_by(|&x, &y| rank(&composed[x], &composed[y]).then(x.cmp(&y)))
                .expect("non-empty");
            let tree = CandidateTree { entries: composed[best].lessons.iter().map(Lesson::tree_entry).collect(), base: base.clone() };
            let label = format!("confirm-{}", composed[best].label);
            composed[best].confirm = Some(self.rollout(&tree, &label, progress)?);
        }
        variants.extend(composed);
        Ok(())
    }

    fn composer_input(&self, reference: &Rollout, base: &BaseLibrary, a: &Variant, b: &Variant) -> Result<Value> {
        let mut input = self.editor_input(reference, base)?;
        let side = |v: &Variant| {
            json!({
                "label": v.label,
                "scope": v.scope,
                "entries": v.lessons.iter().map(|l| json!({"name": l.name, "trigger_line": l.editor.trigger_line, "files": l.files})).collect::<Vec<_>>(),
                "verdict": v.rollout.verdict,
            })
        };
        let diff = |r: &Rollout| {
            json!({
                "label": r.label,
                "pass": r.verdict.as_ref().is_some_and(is_pass),
                "tool_errors": r.session.tool_errors,
                "error_clusters": r.session.error_clusters,
                "turns": r.session.turns,
                "tool_calls": r.session.tool_calls,
            })
        };
        input["scope"] = json!("compose");
        input["candidates"] = json!([side(a), side(b)]);
        input["behavioral_diff"] = json!([diff(reference), diff(&a.rollout), diff(&b.rollout)]);
        input["compose_instructions"] = json!(
            "Merge the two candidate entries into one entry that keeps what the passing or less error-prone candidate did differently; drop laws that did not change behavior."
        );
        Ok(input)
    }

    /// Deterministic merge: invariant lessons as-is; each domain lesson either merges into
    /// a same-named invariant lesson (files union, non-duplicate laws appended) or stays
    /// separate with near-duplicate laws removed. `None` if a merged lesson fails lint.
    fn merge(&mut self, label: &str, inv: &Variant, dom: &Variant, reference: &Rollout) -> Result<Option<Vec<Lesson>>> {
        let law_headings = self.lint_cfg.form.headings.get("laws").cloned().unwrap_or_default();
        let mut out: Vec<Lesson> = inv.lessons.clone();
        let inv_laws: Vec<String> =
            out.iter().filter_map(|l| l.files.get("SKILL.md")).flat_map(|md| law_lines(md, &law_headings)).collect();
        for d in &dom.lessons {
            let md = d.files.get("SKILL.md").cloned().unwrap_or_default();
            if let Some(same) = out.iter_mut().find(|l| l.name == d.name) {
                let fresh: Vec<String> =
                    law_lines(&md, &law_headings).into_iter().filter(|l| !inv_laws.iter().any(|x| near_duplicate(x, l))).collect();
                for (p, c) in &d.files {
                    same.files.entry(p.clone()).or_insert_with(|| c.clone());
                }
                if let Some(s) = same.files.get_mut("SKILL.md") {
                    *s = append_laws(s, &fresh, &law_headings);
                }
            } else {
                let mut d2 = d.clone();
                d2.files.insert("SKILL.md".into(), drop_dup_laws(&md, &inv_laws, &law_headings));
                out.push(d2);
            }
        }
        let mut rec = ProposalRecord {
            label: label.to_string(),
            scope: "compose".into(),
            decision: "MERGE".into(),
            error: None,
            lint: None,
            names: out.iter().map(|l| l.name.clone()).collect(),
            proposal_id: None,
        };
        let mut ok = true;
        let mut findings = Vec::new();
        for l in &mut out {
            l.lint = self.lint_lesson(label, l, reference)?;
            ok &= l.lint.ok;
            findings.extend(l.lint.findings.clone());
        }
        rec.lint = Some(LintReport { ok, findings });
        self.proposals.push(rec);
        Ok(ok.then_some(out))
    }

    /// Stage the winner's lessons provisional with `pending_next_run`. Near-duplicates of
    /// an existing entry append evidence to it instead; one admission per winner.
    fn stage_winner(&self, w: &Variant, reference: &Rollout) -> Result<(Vec<Value>, Value)> {
        let ctx = self.ctx;
        let predicate = self.predicate(reference)?;
        let hash = predicate_hash(&predicate);
        let mut existing = Vec::new();
        for st in [Status::Provisional, Status::Kept, Status::Pinned] {
            for e in ctx.ledger.list_entries(&EntryFilter { status: Some(st), branch: Some(self.branch.clone()), ..Default::default() })? {
                if e.path.is_dir() {
                    existing.push((e.id.clone(), entry_content(&e.path)?));
                }
            }
        }
        let mut fresh = Vec::new();
        let mut out = Vec::new();
        for l in &w.lessons {
            let dir = self.candidates_dir.join(&w.label).join(&l.name);
            if !dir.is_dir() {
                write_entry_files(&dir, &l.files)?;
            }
            match route_duplicate(&entry_content(&dir)?, &existing) {
                Some(id) => {
                    let mut e = ctx.ledger.get_entry(id)?.ok_or_else(|| anyhow!("entry {id} vanished"))?;
                    if !e.evidence.contains(&l.manifest.evidence) {
                        e.evidence.push(l.manifest.evidence.clone());
                        e.updated_at = ctx.now;
                        ctx.ledger.upsert_entry(&e)?;
                        std::fs::write(e.path.join(rematch_core::library::REMATCH_JSON), serde_json::to_string_pretty(&e)?)?;
                    }
                    out.push(json!({"name": l.name, "merged_into": id}));
                }
                None => fresh.push(l),
            }
        }
        if fresh.is_empty() {
            return Ok((out, json!({"skipped": "every lesson duplicated an existing entry"})));
        }
        let kept_per_domain = ctx.domain_policy()?.kept_per_domain();
        if let Err(r) = rematch_core::rate::can_admit(&ctx.ledger, &hash, &self.branch, predicate.domain.as_deref(), kept_per_domain, ctx.now) {
            let note = format!("{r}");
            for l in fresh {
                out.push(json!({"name": l.name, "refused": note}));
            }
            return Ok((out, json!({"refused": note, "predicate_hash": hash})));
        }
        for l in fresh {
            let mut entry = self.entry_for(l, reference)?;
            entry.name = self.free_name(&entry)?;
            let mut files = l.files.clone();
            let primary = primary_doc(entry.kind);
            if entry.name != l.name && files.contains_key(primary) {
                let mut doc = SkillDoc::parse(&files[primary])?;
                doc.set("name", entry.name.as_str());
                files.insert(primary.into(), doc.render());
            }
            let edits: Vec<Edit> = files.into_iter().map(|(path, content)| Edit::Write { path, content }).collect();
            let staged = stage(&ctx.ledger, &ctx.paths, &entry, &edits)?;
            rematch_core::rate::record_admission(&ctx.ledger, &hash, &staged.id, ctx.now)?;
            let cert = self.insert_receipt(&staged.id, l.lint.clone(), w.rollout.verdict.clone())?;
            out.push(json!({
                "id": staged.id,
                "name": staged.name,
                "status": staged.status,
                "pending_next_run": staged.pending_next_run,
                "path": staged.path,
                "lesson_tag": staged.lesson_tag,
                "certificate": cert,
            }));
        }
        Ok((out, json!({"admitted": true, "predicate_hash": hash})))
    }

    /// The lesson's name, or `<name>-N` when another entry holds its staging dir.
    fn free_name(&self, entry: &Entry) -> Result<String> {
        let mut e = entry.clone();
        for n in 1..=20 {
            e.name = if n == 1 { entry.name.clone() } else { format!("{}-{n}", entry.name) };
            if !dir_for(&self.ctx.paths, &e, Status::Provisional).exists() {
                return Ok(e.name);
            }
        }
        bail!("no free staging name for {}", entry.name)
    }
}

fn scope_str(s: EditorScope) -> &'static str {
    match s {
        EditorScope::Invariant => "invariant",
        EditorScope::Domain => "domain",
    }
}

/// `invariant-2` → `2`.
fn label_index(label: &str) -> &str {
    label.rsplit('-').next().unwrap_or(label)
}

/// Seq of the first tool result of the longest run of consecutive failing tool results.
fn longest_error_stretch(records: &[TraceRecord]) -> Option<u32> {
    let mut best: Option<(usize, u32)> = None;
    let mut cur: Option<(usize, u32)> = None;
    for r in records {
        if let TraceRecord::ToolResult { seq, is_error, .. } = r {
            if *is_error {
                cur = Some(cur.map_or((1, *seq), |(n, s)| (n + 1, s)));
                if best.is_none_or(|(bn, _)| cur.is_some_and(|(n, _)| n > bn)) {
                    best = cur;
                }
            } else {
                cur = None;
            }
        }
    }
    best.map(|(_, s)| s)
}

/// The moment cited when the editor's evidence is not in the trajectory.
fn failure_seq(records: &[TraceRecord]) -> u32 {
    longest_error_stretch(records).unwrap_or_else(|| records.last().map(TraceRecord::seq).unwrap_or(0))
}

fn is_bullet(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("+ ")
        || t.split_once(['.', ')']).is_some_and(|(n, rest)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && rest.starts_with(' '))
}

fn heading_is(line: &str, keywords: &[String]) -> Option<bool> {
    let t = line.trim_start();
    if !t.starts_with('#') {
        return None;
    }
    let h = t.trim_start_matches('#').trim().to_ascii_lowercase();
    Some(keywords.iter().any(|k| h.contains(k.as_str())))
}

/// Bullet lines inside sections whose heading classifies as laws.
fn law_lines(md: &str, keywords: &[String]) -> Vec<String> {
    let mut in_laws = false;
    let mut out = Vec::new();
    for l in md.lines() {
        if let Some(is_laws) = heading_is(l, keywords) {
            in_laws = is_laws;
        } else if in_laws && is_bullet(l) {
            out.push(l.trim().to_string());
        }
    }
    out
}

fn drop_dup_laws(md: &str, existing: &[String], keywords: &[String]) -> String {
    let mut in_laws = false;
    let mut out = Vec::new();
    for l in md.lines() {
        if let Some(is_laws) = heading_is(l, keywords) {
            in_laws = is_laws;
        } else if in_laws && is_bullet(l) && existing.iter().any(|x| near_duplicate(x, l.trim())) {
            continue;
        }
        out.push(l);
    }
    let mut s = out.join("\n");
    if md.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Insert `laws` at the end of the first laws section (or a new `## Laws` section at the
/// end of the document).
fn append_laws(md: &str, laws: &[String], keywords: &[String]) -> String {
    if laws.is_empty() {
        return md.to_string();
    }
    let lines: Vec<&str> = md.lines().collect();
    let start = lines.iter().position(|l| heading_is(l, keywords) == Some(true));
    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    match start {
        Some(i) => {
            let end = (i + 1..lines.len()).find(|&j| heading_is(lines[j], keywords).is_some()).unwrap_or(lines.len());
            let mut at = end;
            while at > i + 1 && lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            for (n, l) in laws.iter().enumerate() {
                out.insert(at + n, l.clone());
            }
        }
        None => {
            out.push(String::new());
            out.push("## Laws".into());
            out.extend(laws.iter().cloned());
        }
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Workspace cleanup for a single rollout (reference-only runs).
fn clean_workspaces(sub: &OmpSubstrate, runs: &Path, seed: u64, r: Option<&Rollout>) -> Result<()> {
    if let Some(r) = r {
        sub.cleanup(&r.install)?;
    }
    let d = runs.join(seed.to_string());
    if d.exists() {
        std::fs::remove_dir_all(&d)?;
    }
    Ok(())
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn batch_width_formula() {
        assert_eq!(rollout_budget(1), 4);
        assert_eq!(rollout_budget(2), 10);
        assert_eq!(rollout_budget(3), 17);
        assert_eq!(model_call_budget(1), 3);
        assert_eq!(model_call_budget(2), 5);
    }

    #[test]
    fn batch_law_merge_helpers() {
        let kw: Vec<String> = ["laws", "rules"].map(String::from).to_vec();
        let a = "Use when x.\n\n## Laws\n- MUST run the check script before yielding the result.\n\n## Procedure\n1. go\n";
        let b = "Use when y.\n\n## Laws\n- MUST run the check script before yielding the result.\n- NEVER edit generated files under the build dir.\n";
        let la = law_lines(a, &kw);
        assert_eq!(la, vec!["- MUST run the check script before yielding the result."]);
        let dropped = drop_dup_laws(b, &la, &kw);
        assert!(!dropped.contains("check script"), "{dropped}");
        assert!(dropped.contains("NEVER edit generated"));
        let merged = append_laws(a, &["- NEVER edit generated files under the build dir.".into()], &kw);
        let laws_at = merged.find("NEVER edit").unwrap();
        assert!(laws_at < merged.find("## Procedure").unwrap(), "{merged}");
        assert!(append_laws("# t\n", &["- MUST x".into()], &kw).ends_with("## Laws\n- MUST x\n"));
    }

    #[test]
    fn batch_error_stretch() {
        let tr = |seq: u32, err: bool| TraceRecord::ToolResult {
            seq,
            t: 0,
            tool: "bash".into(),
            call_id: String::new(),
            input: Value::Null,
            args_hash: String::new(),
            result_bytes: 0,
            is_error: err,
            error_class: None,
            content_head: String::new(),
            skill_read: None,
            path: None,
        };
        let recs = vec![tr(1, true), tr(2, false), tr(3, true), tr(4, true), tr(5, false)];
        assert_eq!(longest_error_stretch(&recs), Some(3));
        assert_eq!(longest_error_stretch(&[tr(1, false)]), None);
    }
}
