//! `eval`: the skill creator's workbench (Anthropic `skill-creator` evals + SkillOpt-style
//! gate, inside rematch's package layout).
//!
//! - `eval init <entry>`: create or extend the entry's suite, `<entry dir>/evals/evals.json`
//!   (`cases` with prompts and optional verifiers, `trigger_eval` queries that should or should
//!   not trigger the skill). Anthropic's `evals/evals.json` (`evals`, integer ids) reads as is.
//!   A foreign entry (a user's skill registered in place, never written to) keeps its suite in
//!   `REMATCH_ROOT/evals/<name>/evals.json`.
//! - `eval trigger <entry>`: deterministic, zero-cost trigger accuracy: the predicate against
//!   each `trigger_eval` query and against every recorded session, unit retrieval
//!   (`retrieve::rank`) per query, and the lint/catalog budgets; precision, recall, pass rate
//!   and the stems a missed positive query would need.
//! - `eval run <entry>`: per case one baseline rollout (the repo's harness without the entry)
//!   and one with-skill rollout (the entry's files on top, drafts included) in fresh worktrees
//!   (`batch::rollout`), judged by the case's verifier; a self-contained HTML side-by-side
//!   report and `results.json` under `REMATCH_ROOT/evals/<name>/`, a receipt in the ledger,
//!   and with `--activate` a draft that passes the gate (no regression, and a win or clean
//!   skill use with no failing verifier) becomes provisional with the winning trace as evidence.
//!
//! `evals/` is test material: it never ships into a rollout's skills dir and is not part of
//! an entry's content identity (`library::content_sha256`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use rematch_core::features::{exts_of, head, rank_stems, stems_of_text, strip_selector, MAX_PREDICATE_STEMS};
use rematch_core::ledger::{EntryFilter, SessionFilter};
use rematch_core::library::{self, EVALS_DIR};
use rematch_core::lint::{entry_cost, lint_entry, LintContext};
use rematch_core::package::{SkillDoc, TRIGGER_RE};
use rematch_core::predicate::{self, Place};
use rematch_core::trace::{self, TraceRecord};
use rematch_core::types::{
    Activation, Certificate, Entry, LintFinding, Moment, Predicate, Receipt, Severity, Status, Verdict,
    DEFAULT_RETRIEVE_BUDGET_BYTES, DEFAULT_RETRIEVE_MIN_OVERLAP,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Map, Value};

use crate::app::AppCtx;
use crate::batch::rollout::{run_rollout, Rollout};
use crate::batch::substrate::{read_entry_files, BaseLibrary, CandidateTree, OmpSubstrate, Substrate, TreeEntry};
use crate::batch::tasks::{is_git_repo, parse_duration_secs, task_runs_dir, validate_id, Task};
use crate::batch::verifier::{default_spec_for, detail_of, is_pass, parse_spec, score_of};
use crate::config::{resolve_role, ROLE_SOLVER};
use crate::envelope::Usage;

/// `<entry dir>/evals/evals.json`
pub const EVALS_JSON: &str = "evals.json";
/// Rollout cap per arm when `eval run` gets no `--max-time`.
pub const DEFAULT_EVAL_MAX_TIME_SECS: u64 = 180;
/// The off-topic query `eval init` seeds as the negative trigger example (edit it into a near miss).
const SEED_NEGATIVE_QUERY: &str = "Write a limerick about a cat who loves rainy afternoons.";
/// Tool steps kept per arm in the report.
const TRAJECTORY_STEPS: usize = 60;
const STEP_TARGET_CHARS: usize = 160;
/// Sample ids listed per session bucket.
const SESSION_SAMPLES: usize = 5;

// ---------------------------------------------------------------------------------------------
// CLI

#[derive(Args, Debug, Clone)]
pub struct EvalArgs {
    #[command(subcommand)]
    pub cmd: EvalCmd,
}

#[derive(Subcommand, Debug, Clone)]
pub enum EvalCmd {
    /// Create or extend <entry dir>/evals/evals.json: cases (prompt, expected_output, verifier) and trigger_eval queries (Anthropic skill-creator `evals` accepted)
    Init(InitArgs),
    /// Deterministic trigger accuracy: the predicate against trigger_eval queries and recorded sessions, unit retrieval per query, catalog and router budgets
    Trigger(TriggerArgs),
    /// Side-by-side rollouts per case (baseline without the entry vs with it) in fresh worktrees; HTML report under REMATCH_ROOT/evals/<name>/; --activate promotes a draft that passes the gate
    Run(RunArgs),
}

#[derive(Args, Debug, Clone)]
pub struct InitArgs {
    /// Entry id or name (draft, provisional, kept or pinned).
    pub entry: String,
    /// Case prompts to add (merged; an existing prompt is not added twice).
    #[arg(long, num_args = 1..)]
    pub prompt: Vec<String>,
    /// Verifier spec for the added cases (check | tests | parity:<produced>=<expected> | receipt:<path> | <cmd>).
    #[arg(long)]
    pub verifier: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct TriggerArgs {
    /// Entry id or name (draft, provisional, kept or pinned).
    pub entry: String,
    /// Apply suggested stems from missed positive trigger_eval queries and sync on-disk draft edits into the ledger.
    #[arg(long)]
    pub fix: bool,
}

#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// Entry id or name (draft, provisional, kept or pinned).
    pub entry: String,
    /// Case prompts (instead of the suite's cases).
    #[arg(long, num_args = 1..)]
    pub prompt: Vec<String>,
    /// Verifier for cases without their own (default: the repo's detected check, else its tests).
    #[arg(long)]
    pub verifier: Option<String>,
    /// Time cap per rollout: `180`, `90s`, `10m` (default 180 s).
    #[arg(long, value_name = "SECS")]
    pub max_time: Option<String>,
    /// Promote a draft to provisional when the comparison passes the gate.
    #[arg(long)]
    pub activate: bool,
}

pub fn cmd_eval(ctx: &AppCtx, a: EvalArgs) -> Result<Value> {
    match a.cmd {
        EvalCmd::Init(a) => cmd_init(ctx, a),
        EvalCmd::Trigger(a) => cmd_trigger(ctx, a),
        EvalCmd::Run(a) => cmd_run(ctx, a),
    }
}

// ---------------------------------------------------------------------------------------------
// The suite (`evals/evals.json`)

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EvalSuite {
    #[serde(default)]
    pub skill_name: String,
    /// `evals` in Anthropic's skill-creator files.
    #[serde(default, alias = "evals")]
    pub cases: Vec<EvalCase>,
    #[serde(default, alias = "trigger_evals", alias = "trigger_Eval")]
    pub trigger_eval: Vec<TriggerQuery>,
    /// Fields other tools wrote, kept on rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EvalCase {
    /// A string here; skill-creator's integer ids read as their decimal text.
    #[serde(default, deserialize_with = "id_text")]
    pub id: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_output: Option<String>,
    /// Verifier spec (`batch::verifier::parse_spec`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier: Option<String>,
    #[serde(default = "yes")]
    pub should_trigger: bool,
    /// `files`, `expectations`, … (kept on rewrite).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerQuery {
    pub query: String,
    #[serde(default = "yes")]
    pub should_trigger: bool,
    /// Paths the request is about (their extensions feed the predicate's extension gate).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn yes() -> bool {
    true
}

fn id_text<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => s,
        Value::Null => String::new(),
        v => v.to_string(),
    })
}

impl EvalSuite {
    /// Every case gets a distinct non-empty id (`case-<n>`); returns whether any changed.
    fn normalize_ids(&mut self) -> bool {
        let mut seen = BTreeSet::new();
        let mut changed = false;
        for i in 0..self.cases.len() {
            let id = self.cases[i].id.trim().to_string();
            if id.is_empty() || seen.contains(&id) {
                let mut n = i + 1;
                while seen.contains(&format!("case-{n}")) || self.cases.iter().any(|c| c.id == format!("case-{n}")) {
                    n += 1;
                }
                self.cases[i].id = format!("case-{n}");
                changed = true;
            }
            seen.insert(self.cases[i].id.clone());
        }
        changed
    }

    /// Adds the prompts not already present as cases; returns the new ids.
    fn merge_prompts(&mut self, prompts: &[String], verifier: Option<&str>) -> Vec<String> {
        let mut added = Vec::new();
        for p in prompts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
            if self.cases.iter().any(|c| c.prompt.trim() == p) {
                continue;
            }
            self.cases.push(EvalCase {
                id: String::new(),
                prompt: p.to_string(),
                verifier: verifier.map(str::to_string),
                should_trigger: true,
                ..Default::default()
            });
            self.normalize_ids();
            added.push(self.cases.last().map(|c| c.id.clone()).unwrap_or_default());
        }
        added
    }
}

/// `<dir>/evals/evals.json`
pub fn evals_file(dir: &Path) -> PathBuf {
    dir.join(EVALS_DIR).join(EVALS_JSON)
}

/// `REMATCH_ROOT/evals/<name>/`: reports, results and foreign entries' suites.
pub fn eval_dir(ctx: &AppCtx, name: &str) -> PathBuf {
    ctx.paths.root.join("evals").join(name)
}

fn load_suite(path: &Path) -> Result<Option<EvalSuite>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(serde_json::from_str(&t).with_context(|| format!("parse {}", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn save_suite(path: &Path, suite: &EvalSuite) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(suite)? + "\n").with_context(|| format!("write {}", path.display()))
}

/// Where an entry's suite is read and written: the entry dir when its files are rematch's;
/// a foreign entry's own `evals/evals.json` is read until a copy under
/// `REMATCH_ROOT/evals/<name>/` exists, and writes go to that copy.
fn suite_location(ctx: &AppCtx, e: &Entry) -> (PathBuf, PathBuf) {
    let own = evals_file(&e.path);
    if crate::scope::owns_files(ctx, e) {
        return (own.clone(), own);
    }
    let mirror = eval_dir(ctx, &e.name).join(EVALS_JSON);
    (if mirror.is_file() { mirror.clone() } else { own }, mirror)
}

/// What the skill is for: the trigger line without its `Use when …` lead, else the SKILL.md
/// description, else the name.
fn trigger_clause(name: &str, trigger_line: &str) -> String {
    let line = trigger_line.trim();
    let clause = match TRIGGER_RE.find(line) {
        Some(m) => line[m.end()..].trim(),
        None => line,
    };
    let clause = clause.trim_end_matches(['.', '!', ' ']).trim();
    if clause.is_empty() {
        name.replace(['-', '_'], " ")
    } else {
        clause.to_string()
    }
}

fn trigger_line_of(e: &Entry) -> String {
    if !e.trigger_line.trim().is_empty() {
        return e.trigger_line.clone();
    }
    std::fs::read_to_string(e.path.join(library::primary_doc(e.kind)))
        .ok()
        .and_then(|t| SkillDoc::parse(&t).ok())
        .and_then(|d| d.get_str("description").map(str::to_string))
        .unwrap_or_default()
}

/// A new suite: the prompts as cases (else two starter cases from the trigger clause) and one
/// positive (the first prompt, else a request for the clause) and one negative trigger query.
pub fn seed_suite(skill_name: &str, trigger_line: &str, prompts: &[String], verifier: Option<&str>) -> EvalSuite {
    let clause = trigger_clause(skill_name, trigger_line);
    let mut suite = EvalSuite { skill_name: skill_name.to_string(), ..Default::default() };
    let given: Vec<&str> = prompts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    if given.is_empty() {
        let starters = [
            (
                format!("In this repository: {clause}. Apply your usual process, then summarize what you did and why."),
                "Describe what a good deliverable contains (edit me).",
            ),
            (
                format!("{clause}: walk through it step by step and list each concrete finding with its file path."),
                "A concrete, file-anchored list; no generic advice (edit me).",
            ),
        ];
        for (i, (p, want)) in starters.into_iter().enumerate() {
            suite.cases.push(EvalCase {
                id: format!("case-{}", i + 1),
                prompt: p,
                expected_output: Some(want.to_string()),
                verifier: verifier.map(str::to_string),
                should_trigger: true,
                ..Default::default()
            });
        }
    } else {
        suite.merge_prompts(&given.iter().map(|s| s.to_string()).collect::<Vec<_>>(), verifier);
    }
    let positive = given.first().map(|s| s.to_string()).unwrap_or_else(|| format!("Can you help me with this: {clause}?"));
    suite.trigger_eval = vec![
        TriggerQuery { query: positive, should_trigger: true, ..Default::default() },
        TriggerQuery { query: SEED_NEGATIVE_QUERY.to_string(), should_trigger: false, ..Default::default() },
    ];
    suite
}

/// `create --eval-prompt`: seed (or extend) `<dir>/evals/evals.json` with the prompts.
pub fn write_default_evals(dir: &Path, skill_name: &str, trigger_line: &str, prompts: &[String]) -> Result<PathBuf> {
    let path = evals_file(dir);
    let suite = match load_suite(&path)? {
        Some(mut s) => {
            s.normalize_ids();
            s.merge_prompts(prompts, None);
            s
        }
        None => seed_suite(skill_name, trigger_line, prompts, None),
    };
    save_suite(&path, &suite)?;
    Ok(path)
}

/// Exact id, else the newest live (kept, pinned, provisional) entry of that name, else its draft.
fn resolve(ctx: &AppCtx, id_or_name: &str) -> Result<Entry> {
    if let Some(e) = ctx.ledger.get_entry(id_or_name)? {
        return Ok(e);
    }
    let rank = |s: Status| match s {
        Status::Kept | Status::Pinned | Status::Provisional => 2,
        Status::Draft => 1,
        _ => 0,
    };
    ctx.ledger
        .list_entries(&EntryFilter { name: Some(id_or_name.to_string()), ..Default::default() })?
        .into_iter()
        .filter(|e| rank(e.status) > 0)
        .max_by_key(|e| (rank(e.status), e.updated_at))
        .ok_or_else(|| anyhow!("no draft, provisional, kept or pinned entry {id_or_name:?} (`rematch library ls`)"))
}

// ---------------------------------------------------------------------------------------------
// eval init

pub fn cmd_init(ctx: &AppCtx, a: InitArgs) -> Result<Value> {
    let e = resolve(ctx, &a.entry)?;
    let (read, write) = suite_location(ctx, &e);
    let verifier = a.verifier.as_deref().map(str::trim).filter(|v| !v.is_empty());
    if let Some(v) = verifier {
        parse_spec(v).map_err(|err| Usage(format!("--verifier: {err:#}")))?;
    }
    let (suite, created, added, changed) = match load_suite(&read)? {
        Some(mut s) => {
            let mut changed = s.normalize_ids();
            if s.skill_name.trim().is_empty() {
                s.skill_name = e.name.clone();
                changed = true;
            }
            let added = s.merge_prompts(&a.prompt, verifier);
            let changed = changed || !added.is_empty() || read != write;
            (s, false, added, changed)
        }
        None => {
            let s = seed_suite(&e.name, &trigger_line_of(&e), &a.prompt, verifier);
            let added = s.cases.iter().map(|c| c.id.clone()).collect();
            (s, true, added, true)
        }
    };
    if changed {
        save_suite(&write, &suite)?;
    }
    let mut lines = vec![format!(
        "{} {} · {} case(s) · {} trigger quer{}",
        if created { "created" } else if changed { "updated" } else { "unchanged" },
        write.display(),
        suite.cases.len(),
        suite.trigger_eval.len(),
        if suite.trigger_eval.len() == 1 { "y" } else { "ies" },
    )];
    for c in &suite.cases {
        let mark = if added.contains(&c.id) { "+" } else { " " };
        lines.push(format!("  {mark} {}: {}{}", c.id, head(&c.prompt, 100), c.verifier.as_deref().map(|v| format!(" [verifier {v}]")).unwrap_or_default()));
    }
    for q in &suite.trigger_eval {
        lines.push(format!("  trigger {} {:?}", if q.should_trigger { "yes" } else { "no " }, head(&q.query, 100)));
    }
    if created && a.prompt.is_empty() {
        lines.push("  starter cases and queries were derived from the trigger line: edit them, then `rematch eval trigger` / `rematch eval run`".into());
    }
    Ok(json!({
        "entry": e, "path": write, "created": created, "added": added,
        "cases": suite.cases, "trigger_eval": suite.trigger_eval, "lines": lines,
    }))
}

// ---------------------------------------------------------------------------------------------
// eval trigger

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UnitHit {
    pub path: String,
    pub overlap: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryResult {
    pub query: String,
    pub should_trigger: bool,
    /// The predicate's content test (`predicate::count_hit`) on the query.
    pub triggered: bool,
    pub pass: bool,
    /// Distinct query stems.
    pub stems: Vec<String>,
    /// Query stems among the predicate's.
    pub overlap: Vec<String>,
    pub exts: Vec<String>,
    /// `pass` | `fail` | `none` (the predicate has no extensions) | `untested` (the query names no file).
    pub ext_gate: &'static str,
    /// Units `retrieve::rank` returns for the query.
    pub units: Vec<UnitHit>,
    /// Stems that would make a missed positive query trigger.
    pub suggest_stems: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SessionCoverage {
    pub total: usize,
    /// Sessions the predicate matches (place and content).
    pub matched: usize,
    /// Sessions that read the skill.
    pub read: usize,
    pub read_and_matched: usize,
    /// Read the skill but the predicate missed them (stems too narrow).
    pub read_missed: usize,
    /// Matched but never read it (stems too wide, or the skill was not needed).
    pub matched_unread: usize,
    pub read_missed_sample: Vec<String>,
    pub matched_unread_sample: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RetrievalSummary {
    pub units: Vec<String>,
    pub positive: usize,
    pub positive_retrieved: usize,
    pub negative: usize,
    pub negative_retrieved: usize,
    /// `references/*.md`: the retrieve hook serves `units/` and `facts/` only.
    pub not_retrievable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LintSummary {
    pub ok: bool,
    pub findings: Vec<LintFinding>,
    pub catalog_bytes: usize,
    pub catalog_other_bytes: usize,
    pub always_on_limit: usize,
    pub description_bytes: usize,
    pub description_limit: usize,
    pub router_bytes: usize,
    pub router_limit: usize,
    pub laws: usize,
    /// Laws a check can hold the agent to: MUST/NEVER/ALWAYS or a backticked command, path or name.
    pub checkable_laws: usize,
    /// The description or the router carries a `Use when …` applicability clause.
    pub applicability_clause: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TriggerReport {
    pub entry: String,
    pub id: String,
    pub status: Status,
    pub predicate: Predicate,
    /// `--cwd` and whether the predicate's place test holds there.
    pub cwd: PathBuf,
    pub loads_here: bool,
    pub queries: Vec<QueryResult>,
    pub true_positives: usize,
    pub false_positives: usize,
    pub true_negatives: usize,
    pub false_negatives: usize,
    pub precision: Option<f64>,
    pub recall: Option<f64>,
    pub pass_rate: Option<f64>,
    pub retrieval: RetrievalSummary,
    pub sessions: SessionCoverage,
    pub lint: LintSummary,
    pub suggestions: Vec<String>,
    #[serde(skip)]
    pub lint_report: rematch_core::types::LintReport,
}

pub fn cmd_trigger(ctx: &AppCtx, a: TriggerArgs) -> Result<Value> {
    let mut e = resolve(ctx, &a.entry)?;
    let (read, _) = suite_location(ctx, &e);
    let suite = load_suite(&read)?.unwrap_or_default();
    if a.fix && crate::scope::owns_files(ctx, &e) {
        sync_on_disk_draft_edits(ctx, &mut e)?;
    }
    let mut r = trigger_check(ctx, &e, &suite)?;
    let mut fixed_stems = Vec::new();
    if a.fix {
        for q in &r.queries {
            for s in &q.suggest_stems {
                if !e.predicate.stems.iter().any(|x| norm(x) == norm(s)) && e.predicate.stems.len() < MAX_PREDICATE_STEMS {
                    e.predicate.stems.push(s.clone());
                    fixed_stems.push(s.clone());
                }
            }
        }
        if !fixed_stems.is_empty() || crate::scope::owns_files(ctx, &e) {
            if e.path.is_dir() {
                if let Ok(sha) = library::content_sha256(&e.path) {
                    e.content_sha256 = Some(sha);
                }
            }
            library::persist(&ctx.ledger, &e)?;
            r = trigger_check(ctx, &e, &suite)?;
        }
    }
    let mut lines = trigger_lines(&r, &read, suite.trigger_eval.is_empty() && !suite.cases.is_empty());
    if !fixed_stems.is_empty() {
        lines.insert(0, format!("applied --fix to {}: added stem(s) {} (predicate stems now {:?})", e.name, fixed_stems.join(", "), e.predicate.stems));
    }
    let mut v = serde_json::to_value(&r)?;
    v["suite"] = json!(read);
    v["fixed_stems"] = json!(fixed_stems);
    v["lines"] = json!(lines);
    Ok(v)
}

/// Sync on-disk edits made to a rematch-owned entry (`rematch.json` predicate/trigger_line and
/// `SKILL.md` description) into the ledger before running `--fix`.
fn sync_on_disk_draft_edits(ctx: &AppCtx, e: &mut Entry) -> Result<()> {
    let json_path = e.path.join(library::REMATCH_JSON);
    if let Ok(raw) = std::fs::read_to_string(&json_path) {
        if let Ok(disk) = serde_json::from_str::<Entry>(&raw) {
            if disk.id == e.id {
                e.predicate = disk.predicate;
                if !disk.trigger_line.trim().is_empty() {
                    e.trigger_line = disk.trigger_line;
                }
            }
        }
    }
    if let Ok(raw) = std::fs::read_to_string(e.path.join(library::primary_doc(e.kind))) {
        if let Ok(doc) = SkillDoc::parse(&raw) {
            if let Some(d) = doc.get_str("description").map(str::trim).filter(|d| !d.is_empty()) {
                e.trigger_line = d.to_string();
            }
        }
    }
    if e.path.is_dir() {
        if let Ok(sha) = library::content_sha256(&e.path) {
            e.content_sha256 = Some(sha);
        }
    }
    library::persist(&ctx.ledger, e)
}

fn ratio(n: usize, d: usize) -> Option<f64> {
    (d > 0).then(|| n as f64 / d as f64)
}

fn fmt_ratio(r: Option<f64>) -> String {
    r.map(|x| format!("{x:.2}")).unwrap_or_else(|| "n/a".into())
}

fn distinct(v: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    v.into_iter().filter(|s| seen.insert(s.clone())).collect()
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// `(laws, checkable)` of the `## Laws` section of a SKILL.md body.
fn law_counts(body: &str) -> (usize, usize) {
    let mut in_laws = false;
    let (mut laws, mut checkable) = (0, 0);
    for line in body.lines() {
        let t = line.trim();
        if let Some(h) = t.strip_prefix('#') {
            in_laws = h.trim_start_matches('#').trim().to_ascii_lowercase().starts_with("laws");
            continue;
        }
        if in_laws && (t.starts_with("- ") || t.starts_with("* ") || t.split_once(". ").is_some_and(|(n, _)| n.chars().all(|c| c.is_ascii_digit()))) {
            laws += 1;
            let upper = ["MUST", "NEVER", "ALWAYS", "SHALL"].iter().any(|w| t.split(|c: char| !c.is_ascii_alphabetic()).any(|x| x == *w));
            if upper || t.contains('`') {
                checkable += 1;
            }
        }
    }
    (laws, checkable)
}

fn md_rel_files(dir: &Path, sub: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.join(sub)];
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "md") {
                out.push(p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out
}

pub fn trigger_check(ctx: &AppCtx, e: &Entry, suite: &EvalSuite) -> Result<TriggerReport> {
    let pred = &e.predicate;
    let pred_stems: Vec<String> = pred.stems.iter().map(|s| norm(s)).collect();
    let repo = rematch_core::import::repo_of(&ctx.cwd);
    let loads_here = predicate::matches_place(pred, &Place::locate(repo.as_deref(), &ctx.cwd));
    let corpus = crate::import_cmd::stem_corpus(ctx)?;

    // a) predicate vs the queries (the cases' prompts when the suite lists no trigger queries)
    let queries: Vec<TriggerQuery> = if suite.trigger_eval.is_empty() {
        suite.cases.iter().map(|c| TriggerQuery { query: c.prompt.clone(), should_trigger: c.should_trigger, ..Default::default() }).collect()
    } else {
        suite.trigger_eval.clone()
    };
    let mut units_all = md_rel_files(&e.path, "units");
    units_all.extend(md_rel_files(&e.path, "facts"));
    let candidates = [(e.id.clone(), e.name.clone(), e.path.clone())];
    let mut results = Vec::new();
    let mut retrieval = RetrievalSummary { units: units_all.clone(), not_retrievable: md_rel_files(&e.path, "references"), ..Default::default() };
    let mut suggestions = Vec::new();
    for q in &queries {
        let stems = distinct(stems_of_text(&q.query));
        // Path-like words of the query (`src/app.tsx`, `main.rs.` at a sentence end) plus its `files`.
        let mut paths: Vec<String> = q
            .query
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| "\"'`(),;:".contains(c)).trim_end_matches(['.', '!', '?']).to_string())
            .filter(|w| w.contains('.'))
            .collect();
        paths.extend(q.files.iter().cloned());
        let exts = exts_of(&paths);
        // A query that names no file leaves the extension gate untested.
        let ext_gate = if pred.file_exts.is_empty() {
            "none"
        } else if exts.is_empty() {
            "untested"
        } else if predicate::count_hit(&Predicate { stems: Vec::new(), ..pred.clone() }, &exts, &[]) {
            "pass"
        } else {
            "fail"
        };
        let content = Predicate { file_exts: if exts.is_empty() { Vec::new() } else { pred.file_exts.clone() }, ..pred.clone() };
        let triggered = predicate::count_hit(&content, &exts, &stems);
        let overlap: Vec<String> = stems.iter().filter(|s| pred_stems.contains(&norm(s))).cloned().collect();
        let units = if units_all.is_empty() {
            Vec::new()
        } else {
            crate::retrieve::rank(&candidates, &q.query, DEFAULT_RETRIEVE_BUDGET_BYTES as usize, DEFAULT_RETRIEVE_MIN_OVERLAP)
                .units
                .into_iter()
                .map(|u| UnitHit { path: u.path, overlap: u.overlap })
                .collect()
        };
        let mut suggest_stems = Vec::new();
        if q.should_trigger && !triggered && ext_gate != "fail" {
            let need = pred_stems.len().min(2).saturating_sub(overlap.len()).max(1);
            let fresh: Vec<String> = stems.iter().filter(|s| !pred_stems.contains(&norm(s))).cloned().collect();
            suggest_stems = rank_stems(&fresh, &corpus).into_iter().take(need.max(2)).collect();
            if !suggest_stems.is_empty() {
                let room = MAX_PREDICATE_STEMS.saturating_sub(pred.stems.len());
                suggestions.push(format!(
                    "add stem(s) {} to trigger on {:?}{}",
                    suggest_stems.join(", "),
                    head(&q.query, 60),
                    if room < suggest_stems.len() { format!(" (the predicate holds {}/{MAX_PREDICATE_STEMS}: replace a weak stem)", pred.stems.len()) } else { String::new() }
                ));
            }
        } else if q.should_trigger && !triggered {
            suggestions.push(format!("{:?} names files outside the predicate's extensions {:?}", head(&q.query, 60), pred.file_exts));
        } else if !q.should_trigger && triggered {
            suggestions.push(format!(
                "{:?} should not trigger but shares stem(s) {}: drop or replace one",
                head(&q.query, 60),
                if overlap.is_empty() { "(none; the predicate has no stems)".to_string() } else { overlap.join(", ") }
            ));
        }
        if q.should_trigger {
            retrieval.positive += 1;
            retrieval.positive_retrieved += usize::from(!units.is_empty());
        } else {
            retrieval.negative += 1;
            retrieval.negative_retrieved += usize::from(!units.is_empty());
        }
        results.push(QueryResult {
            query: q.query.clone(),
            should_trigger: q.should_trigger,
            triggered,
            pass: triggered == q.should_trigger,
            stems,
            overlap,
            exts,
            ext_gate,
            units,
            suggest_stems,
        });
    }
    let count = |f: &dyn Fn(&QueryResult) -> bool| results.iter().filter(|r| f(r)).count();
    let tp = count(&|r| r.should_trigger && r.triggered);
    let fp = count(&|r| !r.should_trigger && r.triggered);
    let tn = count(&|r| !r.should_trigger && !r.triggered);
    let fn_ = count(&|r| r.should_trigger && !r.triggered);
    if !retrieval.not_retrievable.is_empty() {
        suggestions.push(format!(
            "{} reference file(s) are never retrieved by the context hook (it serves units/ and facts/): move them under units/",
            retrieval.not_retrievable.len()
        ));
    }
    if !units_all.is_empty() && retrieval.positive > 0 && retrieval.positive_retrieved < retrieval.positive {
        suggestions.push(format!(
            "{} of {} positive quer(ies) retrieve no unit: the description must share a stem with the request and a unit at least {DEFAULT_RETRIEVE_MIN_OVERLAP}",
            retrieval.positive - retrieval.positive_retrieved,
            retrieval.positive
        ));
    }

    // b) predicate vs recorded sessions
    let mut sessions = SessionCoverage::default();
    for s in ctx.ledger.list_sessions(&SessionFilter::default())? {
        let matched = predicate::count_match(pred, &Place::of_session(&s), &s.exts, &s.stems);
        let read = s.skill_reads.iter().any(|n| n == &e.name) || s.skill_entries.contains(&e.id);
        sessions.total += 1;
        sessions.matched += usize::from(matched);
        sessions.read += usize::from(read);
        sessions.read_and_matched += usize::from(read && matched);
        if read && !matched {
            sessions.read_missed += 1;
            if sessions.read_missed_sample.len() < SESSION_SAMPLES {
                sessions.read_missed_sample.push(s.id.clone());
            }
        }
        if matched && !read {
            sessions.matched_unread += 1;
            if sessions.matched_unread_sample.len() < SESSION_SAMPLES {
                sessions.matched_unread_sample.push(s.id.clone());
            }
        }
    }
    if sessions.read_missed > 0 {
        suggestions.push(format!("{} recorded session(s) read the skill but the predicate missed them: `rematch tune {}` widens its stems", sessions.read_missed, e.name));
    }

    // c) lint and budgets
    let cfg = crate::scope::lint_config(ctx)?;
    let catalog_other_bytes = library::catalog_other_bytes(&ctx.ledger, &ctx.paths, e, &e.path)?;
    let lint_report = lint_entry(
        &e.path,
        e,
        &cfg,
        &LintContext { catalog_other_bytes, user_names: crate::reflect::user_names(), transfer: false },
    )?;
    let cost = entry_cost(&e.path, e.kind);
    let doc = std::fs::read_to_string(e.path.join(library::primary_doc(e.kind))).ok().and_then(|t| SkillDoc::parse(&t).ok()).unwrap_or_default();
    let (laws, checkable_laws) = law_counts(&doc.body);
    let description = doc.get_str("description").unwrap_or("");
    let applicability_clause = TRIGGER_RE.is_match(description) || doc.body.lines().any(|l| TRIGGER_RE.is_match(l));
    if !applicability_clause {
        suggestions.push("no `Use when …` applicability clause in the description or router: say when the skill applies".into());
    }
    if laws > checkable_laws {
        suggestions.push(format!("{} of {laws} law(s) are not checkable: state them as MUST/NEVER with the exact command, path or name", laws - checkable_laws));
    }
    let lint = LintSummary {
        ok: lint_report.ok,
        findings: lint_report.findings.clone(),
        catalog_bytes: cost.catalog,
        catalog_other_bytes,
        always_on_limit: cfg.limits.always_on_bytes,
        description_bytes: cost.description,
        description_limit: cfg.limits.description_bytes,
        router_bytes: cost.body,
        router_limit: cfg.limits.router_bytes,
        laws,
        checkable_laws,
        applicability_clause,
    };
    Ok(TriggerReport {
        entry: e.name.clone(),
        id: e.id.clone(),
        status: e.status,
        predicate: pred.clone(),
        cwd: ctx.cwd.clone(),
        loads_here,
        true_positives: tp,
        false_positives: fp,
        true_negatives: tn,
        false_negatives: fn_,
        precision: ratio(tp, tp + fp),
        recall: ratio(tp, tp + fn_),
        pass_rate: ratio(tp + tn, results.len()),
        queries: results,
        retrieval,
        sessions,
        lint,
        suggestions,
        lint_report,
    })
}

/// `draft`, `provisional`, … (the ledger's spelling).
fn status_str(s: Status) -> String {
    serde_json::to_value(s).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn trigger_lines(r: &TriggerReport, suite: &Path, from_cases: bool) -> Vec<String> {
    let mut lines = vec![format!(
        "trigger eval {} ({}): pass {}/{} · precision {} · recall {}",
        r.entry,
        status_str(r.status),
        r.true_positives + r.true_negatives,
        r.queries.len(),
        fmt_ratio(r.precision),
        fmt_ratio(r.recall)
    )];
    if r.queries.is_empty() {
        lines.push(format!("  no queries in {}: `rematch eval init {}` seeds them", suite.display(), r.entry));
    } else if from_cases {
        lines.push("  no trigger_eval queries: the cases' prompts stand in".into());
    }
    for q in &r.queries {
        let units = if q.units.is_empty() { String::new() } else { format!(" · units {}", q.units.iter().map(|u| format!("{}({})", u.path, u.overlap)).collect::<Vec<_>>().join(", ")) };
        let suggest = if q.suggest_stems.is_empty() { String::new() } else { format!(" → add stems {}", q.suggest_stems.join(", ")) };
        lines.push(format!(
            "{} {} {:?}: {}{}{}",
            if q.pass { "pass" } else { "fail" },
            if q.should_trigger { "should trigger," } else { "should not trigger," },
            head(&q.query, 80),
            if q.triggered { "triggered" } else { "missed" },
            units,
            suggest
        ));
    }
    lines.push(format!(
        "  predicate stems {:?} exts {:?} · loads in {}: {}",
        r.predicate.stems,
        r.predicate.file_exts,
        r.cwd.display(),
        if r.loads_here { "yes" } else { "no" }
    ));
    if !r.retrieval.units.is_empty() {
        lines.push(format!(
            "  retrieval: {} unit(s) · positives retrieving {}/{} · negatives retrieving {}/{}",
            r.retrieval.units.len(),
            r.retrieval.positive_retrieved,
            r.retrieval.positive,
            r.retrieval.negative_retrieved,
            r.retrieval.negative
        ));
    }
    let s = &r.sessions;
    lines.push(format!(
        "  sessions: {} recorded · matched {} · read {} (read but missed {}, matched but unread {})",
        s.total, s.matched, s.read, s.read_missed, s.matched_unread
    ));
    let l = &r.lint;
    let errors = l.findings.iter().filter(|f| f.severity == Severity::Error).count();
    lines.push(format!(
        "  lint {} · catalog line {} B (+{} B others, limit {}) · description {}/{} B · router {}/{} B · laws {} ({} checkable) · applicability clause {}",
        if l.ok { "ok".to_string() } else { format!("{errors} error(s)") },
        l.catalog_bytes,
        l.catalog_other_bytes,
        l.always_on_limit,
        l.description_bytes,
        l.description_limit,
        l.router_bytes,
        l.router_limit,
        l.laws,
        l.checkable_laws,
        if l.applicability_clause { "yes" } else { "no" }
    ));
    for f in &l.findings {
        lines.push(format!("    [{}] {}", f.rule, f.message));
    }
    for s in &r.suggestions {
        lines.push(format!("  suggest: {s}"));
    }
    lines
}

// ---------------------------------------------------------------------------------------------
// eval run

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Step {
    pub tool: String,
    pub target: String,
    pub error: bool,
}

/// One arm of a case: the rollout's verdict, behavior and deliverable.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Arm {
    pub label: String,
    pub session_id: String,
    pub verdict: Option<Verdict>,
    /// `None` without a verifier.
    pub pass: Option<bool>,
    pub score: Option<f64>,
    /// Ran to the end: no stream/process error, not killed.
    pub clean: bool,
    pub error: Option<String>,
    pub killed: bool,
    pub tool_calls: u32,
    pub tool_errors: u32,
    pub turns: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub elapsed_ms: u64,
    /// The rollout read the entry (its SKILL.md or any of its files).
    pub skill_read: bool,
    /// Entry files other than SKILL.md the rollout read (`units/…`, `references/…`).
    pub units_read: Vec<String>,
    /// Repo paths the rollout changed (`git status`, before the verifier ran).
    pub changed_paths: Vec<String>,
    pub trajectory: Vec<Step>,
    /// The final assistant text (`turn_end.text_head`).
    pub deliverable: String,
    pub trace: PathBuf,
    pub events_blob: String,
    /// Evidence seq: the first read of the entry, else the last record.
    #[serde(skip)]
    pub evidence_seq: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Win,
    Tie,
    Regression,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Delta {
    pub tool_calls: i64,
    pub tool_errors: i64,
    pub turns: i64,
    pub tokens: i64,
    pub elapsed_ms: i64,
    pub deliverable_chars: i64,
    pub same_deliverable: bool,
    /// `line_diff` baseline → with-skill deliverable.
    pub deliverable_diff: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaseResult {
    pub id: String,
    pub prompt: String,
    pub expected_output: Option<String>,
    pub verifier: Option<String>,
    pub outcome: Outcome,
    pub reason: String,
    pub baseline: Arm,
    pub with_skill: Arm,
    pub delta: Delta,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    pub cases: usize,
    pub wins: usize,
    pub ties: usize,
    pub regressions: usize,
    pub pass_with: usize,
    pub pass_baseline: usize,
    /// Cases with a verifier verdict.
    pub verified: usize,
    /// Cases whose with-skill rollout read the entry.
    pub skill_used: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Gate {
    pub passed: bool,
    pub reason: String,
}

pub fn cmd_run(ctx: &AppCtx, a: RunArgs) -> Result<Value> {
    run_with(ctx, a, &crate::omp::omp_bin())
}

/// The repo the rollouts branch from: `--cwd`'s git toplevel, else the entry's origin repo.
fn eval_repo(ctx: &AppCtx, e: &Entry) -> Result<PathBuf> {
    if let Some(r) = rematch_core::import::repo_of(&ctx.cwd).filter(|r| is_git_repo(r)) {
        return Ok(r);
    }
    if let Some(r) = crate::scope::origin_repo(ctx, e)?.filter(|r| is_git_repo(r)) {
        return Ok(r);
    }
    Err(Usage(format!(
        "eval run needs a git repo to branch worktrees from: {} is not inside one and {} has no origin repo here (pass --cwd <repo>)",
        ctx.cwd.display(),
        e.name
    ))
    .into())
}

fn safe_id(s: &str) -> String {
    let id: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
    id.trim_start_matches('.').to_string()
}

/// `eval run` with an explicit omp binary (tests pass a fake).
pub fn run_with(ctx: &AppCtx, a: RunArgs, omp_bin: &Path) -> Result<Value> {
    if ctx.paths.is_off() {
        return Err(anyhow!("rematch is off; `rematch on` to run evals"));
    }
    let e = resolve(ctx, &a.entry)?;
    let (read, _) = suite_location(ctx, &e);
    let suite = load_suite(&read)?.unwrap_or_default();
    let cases: Vec<EvalCase> = if !a.prompt.is_empty() {
        a.prompt
            .iter()
            .filter(|p| !p.trim().is_empty())
            .enumerate()
            .map(|(i, p)| EvalCase { id: format!("prompt-{}", i + 1), prompt: p.trim().to_string(), should_trigger: true, ..Default::default() })
            .collect()
    } else {
        let mut s = suite.clone();
        s.normalize_ids();
        s.cases
    };
    if cases.is_empty() {
        return Err(Usage(format!(
            "no eval cases for {}: pass --prompt <TEXT> or run `rematch eval init {}` ({} has none)",
            e.name,
            e.name,
            read.display()
        ))
        .into());
    }
    let max_time_secs = a.max_time.as_deref().map(parse_duration_secs).transpose()?.unwrap_or(DEFAULT_EVAL_MAX_TIME_SECS);
    let repo = eval_repo(ctx, &e)?;
    let default_spec = default_spec_for(&repo).map(|(s, _)| s);
    // Verifier specs are checked before any rollout runs.
    let specs: Vec<Option<String>> = cases
        .iter()
        .map(|c| c.verifier.clone().or_else(|| a.verifier.clone()).or_else(|| default_spec.clone()).filter(|s| !s.trim().is_empty()))
        .collect();
    for (c, s) in cases.iter().zip(&specs) {
        if let Some(s) = s {
            parse_spec(s).map_err(|err| Usage(format!("case {}: verifier {s:?}: {err:#}", c.id)))?;
        }
    }

    let solver = resolve_role(&ctx.paths, &repo, ROLE_SOLVER)?.model;
    let sub = OmpSubstrate { repo: repo.clone(), solver: solver.clone() };
    let loaded = BaseLibrary::load(ctx, &repo)?;
    let base = BaseLibrary { entries: loaded.entries.into_iter().filter(|t| t.name != e.name && t.id.as_deref() != Some(e.id.as_str())).collect() };
    let mut files = read_entry_files(&e.path)?;
    files.retain(|rel, _| !rel.starts_with(&format!("{EVALS_DIR}/")));
    let baseline_tree = CandidateTree::reference(&base);
    let with_tree = CandidateTree { entries: vec![TreeEntry { name: e.name.clone(), id: Some(e.id.clone()), files }], base: base.clone() };
    let trigger = trigger_check(ctx, &e, &suite)?;

    let out_dir = eval_dir(ctx, &e.name);
    let traces_dir = out_dir.join("traces");
    std::fs::create_dir_all(&traces_dir)?;
    let mut results: Vec<CaseResult> = Vec::new();
    let mut with_records: Vec<Vec<TraceRecord>> = Vec::new();
    for (case, spec) in cases.iter().zip(&specs) {
        let mut task_id = safe_id(&format!("eval-{}-{}", e.name, case.id));
        task_id.truncate(128);
        validate_id(&task_id)?;
        let task = Task {
            id: task_id,
            repo: repo.clone(),
            prompt: case.prompt.clone(),
            verifier: spec.clone(),
            max_time_secs,
            tools: None,
            seeds: Vec::new(),
            created_at: ctx.now,
            editable: Vec::new(),
            auto_probe: None,
        };
        let verifier = spec.as_deref().map(parse_spec).transpose()?;
        let runs = task_runs_dir(ctx, &task.id);
        std::fs::create_dir_all(&runs)?;
        let lock = crate::jobs::LockFile::acquire(runs.join(".lock"))?.ok_or_else(|| anyhow!("another eval of {} is running case {}", e.name, case.id))?;
        let seed = task.first_seed();
        let baseline = run_rollout(ctx, &sub, omp_bin, &baseline_tree, &task, seed, "baseline", verifier.as_deref());
        let with_skill = run_rollout(ctx, &sub, omp_bin, &with_tree, &task, seed, "with_skill", verifier.as_deref());
        for r in [&baseline, &with_skill].into_iter().flatten() {
            let _ = sub.cleanup(&r.install);
        }
        drop(lock);
        let _ = std::fs::remove_dir_all(&runs);
        let (baseline, with_skill) = (baseline?, with_skill?);
        let b = arm_of(&baseline, &e.name, &traces_dir, &case.id)?;
        let w = arm_of(&with_skill, &e.name, &traces_dir, &case.id)?;
        let (outcome, reason) = compare(&b, &w);
        let delta = delta_of(&b, &w);
        with_records.push(with_skill.records);
        results.push(CaseResult {
            id: case.id.clone(),
            prompt: case.prompt.clone(),
            expected_output: case.expected_output.clone(),
            verifier: spec.clone(),
            outcome,
            reason,
            baseline: b,
            with_skill: w,
            delta,
        });
    }

    let summary = Summary {
        cases: results.len(),
        wins: results.iter().filter(|c| c.outcome == Outcome::Win).count(),
        ties: results.iter().filter(|c| c.outcome == Outcome::Tie).count(),
        regressions: results.iter().filter(|c| c.outcome == Outcome::Regression).count(),
        pass_with: results.iter().filter(|c| c.with_skill.pass == Some(true)).count(),
        pass_baseline: results.iter().filter(|c| c.baseline.pass == Some(true)).count(),
        verified: results.iter().filter(|c| c.with_skill.pass.is_some()).count(),
        skill_used: results.iter().filter(|c| c.with_skill.skill_read).count(),
    };
    let gate = gate_of(&summary, &results);

    // Strict validation gate: only a draft, only on a passing comparison.
    let mut entry = e.clone();
    let mut activated = false;
    let mut evidence: Option<Moment> = None;
    let mut gate_lines = Vec::new();
    if a.activate {
        if e.status != Status::Draft {
            gate_lines.push(format!("--activate: {} is {}, not a draft; nothing to promote", e.name, status_str(e.status)));
        } else if !gate.passed {
            gate_lines.push(format!("refused --activate: {}", gate.reason));
        } else {
            let winner = results
                .iter()
                .position(|c| c.outcome == Outcome::Win)
                .or_else(|| results.iter().position(|c| c.with_skill.skill_read))
                .unwrap_or(0);
            entry = library::activate_draft(&ctx.ledger, &ctx.paths, &e.id)?;
            let m = record_evidence(ctx, &with_records[winner], &results[winner].with_skill)?;
            entry = crate::reflect::append_evidence(ctx, &entry.id, &m)?;
            evidence = Some(m);
            activated = true;
            gate_lines.push(format!(
                "activated {} as provisional: {} (evidence {} from case {})",
                entry.name,
                gate.reason,
                results[winner].with_skill.session_id,
                results[winner].id
            ));
        }
    }

    // Receipt: the lint of the entry and the comparison as one verdict.
    let detail = format!(
        "eval {} case(s): {} win, {} tie, {} regression; with-skill pass {}/{}, baseline pass {}/{}; gate {}: {}",
        summary.cases, summary.wins, summary.ties, summary.regressions, summary.pass_with, summary.verified, summary.pass_baseline, summary.verified,
        if gate.passed { "passed" } else { "failed" }, gate.reason
    );
    let score = ratio(summary.pass_with, summary.verified);
    let verdict = if gate.passed { Verdict::Pass { score, detail } } else { Verdict::Fail { score, detail } };
    let cert = Certificate { lint: trigger.lint_report.clone(), canary: None, verifier: Some(verdict) };
    let certificate = ctx.ledger.put_blob(serde_json::to_string(&cert)?.as_bytes())?;
    let receipt = ctx.ledger.insert_receipt(&Receipt { certificate: certificate.clone(), entry: entry.id.clone(), activation: Activation::Personal })?;

    let report_html = out_dir.join("report.html");
    let results_json = out_dir.join("results.json");
    let report = json!({
        "entry": entry,
        "generated_at": ctx.now,
        "repo": repo,
        "solver": crate::omp::selector(&solver),
        "max_time_secs": max_time_secs,
        "suite": read,
        "trigger": trigger,
        "cases": results,
        "summary": summary,
        "gate": gate,
        "activate_requested": a.activate,
        "activated": activated,
        "evidence": evidence,
        "receipt": receipt.id,
        "certificate": certificate,
    });
    std::fs::write(&results_json, serde_json::to_string_pretty(&report)? + "\n").with_context(|| format!("write {}", results_json.display()))?;
    let html = render_html(&entry, &repo, &crate::omp::selector(&solver), &trigger, &results, &summary, &gate, activated, ctx.now);
    std::fs::write(&report_html, html).with_context(|| format!("write {}", report_html.display()))?;

    let mut lines = vec![format!(
        "eval run {}: {} case(s) · wins {} · ties {} · regressions {} · with-skill pass {}/{} · baseline pass {}/{}",
        e.name, summary.cases, summary.wins, summary.ties, summary.regressions, summary.pass_with, summary.verified, summary.pass_baseline, summary.verified
    )];
    for c in &results {
        let used = if c.with_skill.skill_read {
            if c.with_skill.units_read.is_empty() { " · skill read".to_string() } else { format!(" · skill read, units {}", c.with_skill.units_read.join(", ")) }
        } else {
            " · skill not read".to_string()
        };
        let word = match c.outcome {
            Outcome::Win => "win",
            Outcome::Tie => "tie",
            Outcome::Regression => "regression",
        };
        lines.push(format!("{word} {}: {}{used}", c.id, c.reason));
    }
    lines.push(format!("  gate {}: {}", if gate.passed { "passed" } else { "failed" }, gate.reason));
    lines.extend(gate_lines);
    lines.push(format!("  report {}", report_html.display()));
    Ok(json!({
        "entry": entry,
        "cases": results,
        "summary": summary,
        "gate": gate,
        "trigger": trigger,
        "report_html": report_html,
        "results_json": results_json,
        "receipt": receipt.id,
        "activated": activated,
        "evidence": evidence,
        "lines": lines,
    }))
}

/// Folds the winning with-skill trace into a ledger session (as `activate --task` does) and
/// returns its evidence moment.
fn record_evidence(ctx: &AppCtx, records: &[TraceRecord], arm: &Arm) -> Result<Moment> {
    let mut s = trace::fold_trace(records, &arm.session_id);
    s.domain = rematch_core::domain::session_domain(&s, &ctx.domain_policy()?);
    ctx.ledger.upsert_session(&s)?;
    Ok(Moment { session_id: arm.session_id.clone(), seq: arm.evidence_seq })
}

/// Whether and how a rollout used the entry `name`: `(read, files other than SKILL.md, first seq)`.
fn skill_use(records: &[TraceRecord], name: &str) -> (bool, Vec<String>, Option<u32>) {
    let marker = format!("skills/{name}/");
    let uri = format!("skill://{name}");
    let mut read = false;
    let mut first = None;
    let mut units = BTreeSet::new();
    for r in records {
        let TraceRecord::ToolResult { seq, skill_read, path, input, .. } = r else { continue };
        let mut hit = skill_read.as_deref() == Some(name);
        let mut paths: Vec<String> = path.iter().cloned().collect();
        paths.extend(trace::paths_in_tool_input(input));
        for p in paths {
            let p = p.replace('\\', "/");
            let rel = match (p.find(&marker), p.strip_prefix(&uri)) {
                (Some(i), _) => Some(p[i + marker.len()..].to_string()),
                (None, Some(rest)) if rest.is_empty() || rest.starts_with('/') => Some(rest.trim_start_matches('/').to_string()),
                _ => None,
            };
            if let Some(rel) = rel {
                hit = true;
                let rel = strip_selector(&rel).to_string();
                if !rel.is_empty() && rel != "SKILL.md" {
                    units.insert(rel);
                }
            }
        }
        if hit {
            read = true;
            first.get_or_insert(*seq);
        }
    }
    (read, units.into_iter().collect(), first)
}

fn step_target(input: &Value) -> String {
    let s = ["command", "path", "file_path", "pattern", "query", "url"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| if input.is_null() { String::new() } else { input.to_string() });
    head(&s.replace('\n', " "), STEP_TARGET_CHARS)
}

fn arm_of(r: &Rollout, name: &str, traces_dir: &Path, case_id: &str) -> Result<Arm> {
    let (read_files, units_read, first) = skill_use(&r.records, name);
    let skill_read = read_files || r.session.skill_reads.iter().any(|n| n == name);
    let deliverable = r
        .records
        .iter()
        .rev()
        .find_map(|x| match x {
            TraceRecord::TurnEnd { text_head, .. } if !text_head.trim().is_empty() => Some(text_head.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let trajectory = r
        .records
        .iter()
        .filter_map(|x| match x {
            TraceRecord::ToolResult { tool, input, is_error, .. } => Some(Step { tool: tool.clone(), target: step_target(input), error: *is_error }),
            _ => None,
        })
        .take(TRAJECTORY_STEPS)
        .collect();
    let trace_path = traces_dir.join(format!("{}-{}.jsonl", safe_id(case_id), r.label));
    let mut text = String::new();
    for rec in &r.records {
        text.push_str(&serde_json::to_string(rec)?);
        text.push('\n');
    }
    std::fs::write(&trace_path, text).with_context(|| format!("write {}", trace_path.display()))?;
    let last_seq = r.records.last().map(TraceRecord::seq).unwrap_or(0);
    Ok(Arm {
        label: r.label.clone(),
        session_id: r.session.id.clone(),
        pass: r.verdict.as_ref().map(is_pass),
        score: r.verdict.as_ref().and_then(score_of),
        verdict: r.verdict.clone(),
        clean: r.error.is_none() && !r.killed,
        error: r.error.clone(),
        killed: r.killed,
        tool_calls: r.session.tool_calls,
        tool_errors: r.session.tool_errors,
        turns: r.session.turns,
        tokens_in: r.session.tokens_in,
        tokens_out: r.session.tokens_out,
        elapsed_ms: r.elapsed_ms,
        skill_read,
        units_read,
        changed_paths: r.seal_violations.clone(),
        trajectory,
        deliverable,
        trace: trace_path,
        events_blob: r.events_blob.clone(),
        evidence_seq: first.unwrap_or(last_seq),
    })
}

/// Baseline → with-skill: a failed run, then the verifier (pass, then score) decides.
fn compare(b: &Arm, w: &Arm) -> (Outcome, String) {
    if b.clean && !w.clean {
        return (Outcome::Regression, format!("with-skill run failed: {}", w.error.as_deref().unwrap_or("killed")));
    }
    if !b.clean && w.clean {
        return (Outcome::Win, format!("baseline run failed ({}); with-skill completed", b.error.as_deref().unwrap_or("killed")));
    }
    match (b.pass, w.pass) {
        (Some(false), Some(true)) => (Outcome::Win, "verifier: baseline fail → with-skill pass".into()),
        (Some(true), Some(false)) => (Outcome::Regression, "verifier: baseline pass → with-skill fail".into()),
        (Some(p), Some(_)) => match (b.score, w.score) {
            (Some(x), Some(y)) if y > x + 1e-9 => (Outcome::Win, format!("verifier score {x} → {y}")),
            (Some(x), Some(y)) if y < x - 1e-9 => (Outcome::Regression, format!("verifier score {x} → {y}")),
            _ => (Outcome::Tie, format!("verifier: both {}", if p { "pass" } else { "fail" })),
        },
        _ => (Outcome::Tie, "no verifier verdict: compare the deliverables".into()),
    }
}

fn delta_of(b: &Arm, w: &Arm) -> Delta {
    let d = |x: u64, y: u64| y as i64 - x as i64;
    Delta {
        tool_calls: d(b.tool_calls.into(), w.tool_calls.into()),
        tool_errors: d(b.tool_errors.into(), w.tool_errors.into()),
        turns: d(b.turns.into(), w.turns.into()),
        tokens: d(b.tokens_in + b.tokens_out, w.tokens_in + w.tokens_out),
        elapsed_ms: d(b.elapsed_ms, w.elapsed_ms),
        deliverable_chars: w.deliverable.chars().count() as i64 - b.deliverable.chars().count() as i64,
        same_deliverable: b.deliverable == w.deliverable,
        deliverable_diff: crate::actions::line_diff(&b.deliverable, &w.deliverable),
    }
}

/// No regression, and either a strict improvement (a win) or, when no with-skill verifier
/// failed, every with-skill run completed cleanly and actually used the skill.
fn gate_of(s: &Summary, cases: &[CaseResult]) -> Gate {
    if s.regressions > 0 {
        return Gate { passed: false, reason: format!("{} regression(s) vs baseline", s.regressions) };
    }
    if s.wins > 0 {
        return Gate { passed: true, reason: format!("{} win(s), no regression", s.wins) };
    }
    if cases.iter().any(|c| c.with_skill.pass == Some(false)) {
        return Gate { passed: false, reason: "no win and a with-skill verifier failed".into() };
    }
    if cases.iter().any(|c| !c.with_skill.clean) {
        return Gate { passed: false, reason: "no win and a with-skill run did not complete cleanly".into() };
    }
    if s.skill_used == 0 {
        return Gate { passed: false, reason: "no win and no with-skill run read the skill".into() };
    }
    Gate { passed: true, reason: format!("no regression; every with-skill run completed cleanly and {} used the skill", s.skill_used) }
}

// ---------------------------------------------------------------------------------------------
// HTML report (self-contained: inline CSS, no scripts, no external resources)

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

const CSS: &str = r#"
:root{color-scheme:light dark;--bg:#ffffff;--fg:#1f2328;--muted:#59636e;--card:#f6f8fa;--border:#d1d9e0;--accent:#0969da;--win:#1a7f37;--loss:#cf222e;--tie:#6e7781;--add:#dafbe1;--del:#ffebe9;--code:#eff2f5}
@media (prefers-color-scheme: dark){:root{--bg:#0d1117;--fg:#e6edf3;--muted:#9198a1;--card:#151b23;--border:#3d444d;--accent:#4493f8;--win:#3fb950;--loss:#f85149;--tie:#9198a1;--add:#12261e;--del:#2d1215;--code:#1f2630}}
*{box-sizing:border-box}
body{margin:0;padding:24px;background:var(--bg);color:var(--fg);font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif}
main{max-width:1280px;margin:0 auto}
h1{font-size:22px;margin:0 0 4px}h2{font-size:18px;margin:28px 0 10px;border-bottom:1px solid var(--border);padding-bottom:6px}h3{font-size:16px;margin:0}
.muted{color:var(--muted)}code,pre{font:12px/1.45 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
code{background:var(--code);padding:1px 4px;border-radius:4px}
pre{background:var(--code);padding:10px;border-radius:6px;white-space:pre-wrap;word-break:break-word;margin:0;max-height:480px;overflow:auto}
.cards{display:flex;flex-wrap:wrap;gap:12px;margin:16px 0}
.card{background:var(--card);border:1px solid var(--border);border-radius:8px;padding:10px 14px;min-width:120px}
.card b{display:block;font-size:22px}
.badge{display:inline-block;border-radius:999px;padding:1px 9px;font-size:12px;font-weight:600;color:#fff;background:var(--tie);vertical-align:middle}
.win,.pass{background:var(--win)}.regression,.fail{background:var(--loss)}.tie,.none{background:var(--tie)}
table{border-collapse:collapse;width:100%;margin:8px 0}th,td{border:1px solid var(--border);padding:5px 8px;text-align:left;vertical-align:top}
th{background:var(--card);font-weight:600}
article{border:1px solid var(--border);border-radius:10px;padding:16px;margin:16px 0;background:var(--bg)}
.cols{display:grid;grid-template-columns:1fr 1fr;gap:12px}@media (max-width:860px){.cols{grid-template-columns:1fr}}
.col h4{margin:8px 0 6px;font-size:13px;text-transform:uppercase;letter-spacing:.04em;color:var(--muted)}
ol.steps{margin:0;padding-left:22px;max-height:320px;overflow:auto}ol.steps li.err{color:var(--loss)}
.diff span{display:block}.diff .a{background:var(--add)}.diff .d{background:var(--del)}
details summary{cursor:pointer;color:var(--accent);margin:6px 0}
ul.findings{margin:4px 0;padding-left:20px}
"#;

fn badge(class: &str, text: &str) -> String {
    format!("<span class=\"badge {class}\">{}</span>", esc(text))
}

/// `<code>a, b</code>`, or `—` for nothing.
fn code_list(items: &[String]) -> String {
    if items.is_empty() { "—".into() } else { format!("<code>{}</code>", esc(&items.join(", "))) }
}

fn verdict_badge(a: &Arm) -> String {
    match a.pass {
        Some(true) => badge("pass", &a.score.map(|s| format!("pass {s}")).unwrap_or_else(|| "pass".into())),
        Some(false) => badge("fail", &a.score.map(|s| format!("fail {s}")).unwrap_or_else(|| "fail".into())),
        None => badge("none", "no verifier"),
    }
}

fn signed(n: i64) -> String {
    if n > 0 { format!("+{n}") } else { n.to_string() }
}

#[allow(clippy::too_many_arguments)]
fn render_html(e: &Entry, repo: &Path, solver: &str, t: &TriggerReport, cases: &[CaseResult], s: &Summary, gate: &Gate, activated: bool, now: i64) -> String {
    let mut h = String::new();
    let when = chrono::DateTime::from_timestamp_millis(now).map(|d| d.to_rfc3339()).unwrap_or_default();
    h.push_str(&format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>rematch eval · {}</title><style>{CSS}</style></head><body><main>\n",
        esc(&e.name)
    ));
    h.push_str(&format!(
        "<header><h1>rematch eval · <code>{}</code></h1><div class=\"muted\">status <b>{}</b>{} · repo <code>{}</code> · solver <code>{}</code> · {}</div><p>{}</p></header>\n",
        esc(&e.name),
        status_str(e.status),
        if activated { " (activated by this run)" } else { "" },
        esc(&repo.display().to_string()),
        esc(solver),
        esc(&when),
        esc(&e.trigger_line)
    ));
    h.push_str("<section class=\"cards\">");
    for (label, n, class) in [("wins", s.wins, "win"), ("ties", s.ties, "tie"), ("regressions", s.regressions, "regression")] {
        h.push_str(&format!("<div class=\"card\"><b>{n}</b>{}</div>", badge(class, label)));
    }
    h.push_str(&format!("<div class=\"card\"><b>{}/{}</b>with-skill pass</div>", s.pass_with, s.verified));
    h.push_str(&format!("<div class=\"card\"><b>{}/{}</b>baseline pass</div>", s.pass_baseline, s.verified));
    h.push_str(&format!("<div class=\"card\"><b>{}/{}</b>cases used the skill</div>", s.skill_used, s.cases));
    h.push_str(&format!(
        "<div class=\"card\"><b>{}</b>gate · {}</div>",
        if gate.passed { badge("pass", "passed") } else { badge("fail", "failed") },
        esc(&gate.reason)
    ));
    h.push_str("</section>\n");

    // Trigger and retrieval
    h.push_str(&format!(
        "<section><h2>Trigger &amp; retrieval</h2><p>pass rate <b>{}</b> · precision <b>{}</b> · recall <b>{}</b> · predicate stems {} exts {} · loads in <code>{}</code>: <b>{}</b></p>",
        fmt_ratio(t.pass_rate),
        fmt_ratio(t.precision),
        fmt_ratio(t.recall),
        code_list(&t.predicate.stems),
        code_list(&t.predicate.file_exts),
        esc(&t.cwd.display().to_string()),
        if t.loads_here { "yes" } else { "no" }
    ));
    if t.queries.is_empty() {
        h.push_str("<p class=\"muted\">No trigger queries: <code>rematch eval init</code> seeds them.</p>");
    } else {
        h.push_str("<table><tr><th>query</th><th>expected</th><th>predicate</th><th>stem overlap</th><th>ext gate</th><th>units retrieved</th><th>result</th></tr>");
        for q in &t.queries {
            let units = if q.units.is_empty() { "—".to_string() } else { q.units.iter().map(|u| format!("<code>{}</code> ({})", esc(&u.path), u.overlap)).collect::<Vec<_>>().join("<br>") };
            let suggest = if q.suggest_stems.is_empty() { String::new() } else { format!("<br><span class=\"muted\">add stems: <code>{}</code></span>", esc(&q.suggest_stems.join(", "))) };
            h.push_str(&format!(
                "<tr><td>{}{suggest}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{units}</td><td>{}</td></tr>",
                esc(&q.query),
                if q.should_trigger { "trigger" } else { "no trigger" },
                if q.triggered { "triggered" } else { "missed" },
                code_list(&q.overlap),
                q.ext_gate,
                if q.pass { badge("pass", "pass") } else { badge("fail", "fail") }
            ));
        }
        h.push_str("</table>");
    }
    let ss = &t.sessions;
    h.push_str(&format!(
        "<p>Recorded sessions: <b>{}</b> · predicate matched <b>{}</b> · read the skill <b>{}</b> · read but missed <b>{}</b> · matched but unread <b>{}</b></p>",
        ss.total, ss.matched, ss.read, ss.read_missed, ss.matched_unread
    ));
    let l = &t.lint;
    h.push_str(&format!(
        "<p>Lint {} · catalog line <b>{} B</b> (+{} B others, always-on limit {} B) · description <b>{}/{} B</b> · router <b>{}/{} B</b> · laws <b>{}</b> ({} checkable) · applicability clause <b>{}</b></p>",
        if l.ok { badge("pass", "ok") } else { badge("fail", "errors") },
        l.catalog_bytes, l.catalog_other_bytes, l.always_on_limit, l.description_bytes, l.description_limit, l.router_bytes, l.router_limit, l.laws, l.checkable_laws,
        if l.applicability_clause { "yes" } else { "no" }
    ));
    if !l.findings.is_empty() || !t.suggestions.is_empty() {
        h.push_str("<ul class=\"findings\">");
        for f in &l.findings {
            h.push_str(&format!("<li><code>{}</code> {:?}: {}</li>", esc(&f.rule), f.severity, esc(&f.message)));
        }
        for sg in &t.suggestions {
            h.push_str(&format!("<li>suggest: {}</li>", esc(sg)));
        }
        h.push_str("</ul>");
    }
    h.push_str("</section>\n<section><h2>Cases</h2>\n");

    for c in cases {
        let (class, word) = match c.outcome {
            Outcome::Win => ("win", "win"),
            Outcome::Tie => ("tie", "tie"),
            Outcome::Regression => ("regression", "regression"),
        };
        h.push_str(&format!("<article><h3><code>{}</code> {}</h3><p class=\"muted\">{}</p>", esc(&c.id), badge(class, word), esc(&c.reason)));
        h.push_str(&format!("<details open><summary>Prompt</summary><pre>{}</pre></details>", esc(&c.prompt)));
        if let Some(x) = &c.expected_output {
            h.push_str(&format!("<details><summary>Expected output</summary><pre>{}</pre></details>", esc(x)));
        }
        h.push_str(&format!("<p>verifier <code>{}</code></p>", esc(c.verifier.as_deref().unwrap_or("none"))));
        let (b, w) = (&c.baseline, &c.with_skill);
        let row = |name: &str, x: String, y: String, d: String| format!("<tr><th>{name}</th><td>{x}</td><td>{y}</td><td>{d}</td></tr>");
        h.push_str("<table><tr><th></th><th>baseline</th><th>with skill</th><th>Δ</th></tr>");
        h.push_str(&row("verdict", verdict_badge(b), verdict_badge(w), String::new()));
        h.push_str(&row("tool calls", b.tool_calls.to_string(), w.tool_calls.to_string(), signed(c.delta.tool_calls)));
        h.push_str(&row("tool errors", b.tool_errors.to_string(), w.tool_errors.to_string(), signed(c.delta.tool_errors)));
        h.push_str(&row("turns", b.turns.to_string(), w.turns.to_string(), signed(c.delta.turns)));
        h.push_str(&row("tokens in / out", format!("{} / {}", b.tokens_in, b.tokens_out), format!("{} / {}", w.tokens_in, w.tokens_out), signed(c.delta.tokens)));
        h.push_str(&row("elapsed", format!("{:.1} s", b.elapsed_ms as f64 / 1000.0), format!("{:.1} s", w.elapsed_ms as f64 / 1000.0), format!("{:+.1} s", c.delta.elapsed_ms as f64 / 1000.0)));
        let used = |a: &Arm| {
            if !a.skill_read {
                "no".to_string()
            } else if a.units_read.is_empty() {
                "yes".to_string()
            } else {
                format!("yes · {}", a.units_read.iter().map(|u| format!("<code>{}</code>", esc(u))).collect::<Vec<_>>().join(", "))
            }
        };
        h.push_str(&row("skill read", used(b), used(w), String::new()));
        let changed = |a: &Arm| if a.changed_paths.is_empty() { "—".to_string() } else { a.changed_paths.iter().map(|p| format!("<code>{}</code>", esc(p))).collect::<Vec<_>>().join("<br>") };
        h.push_str(&row("changed paths", changed(b), changed(w), String::new()));
        if b.error.is_some() || w.error.is_some() {
            h.push_str(&row("error", esc(b.error.as_deref().unwrap_or("—")), esc(w.error.as_deref().unwrap_or("—")), String::new()));
        }
        let detail = |a: &Arm| a.verdict.as_ref().map(|v| esc(detail_of(v))).unwrap_or_default();
        if b.verdict.is_some() || w.verdict.is_some() {
            h.push_str(&row("verifier detail", format!("<pre>{}</pre>", detail(b)), format!("<pre>{}</pre>", detail(w)), String::new()));
        }
        h.push_str("</table>");
        let steps = |a: &Arm| {
            if a.trajectory.is_empty() {
                return "<p class=\"muted\">no tool calls</p>".to_string();
            }
            let items: String = a
                .trajectory
                .iter()
                .map(|s| format!("<li{}><code>{}</code> {}</li>", if s.error { " class=\"err\"" } else { "" }, esc(&s.tool), esc(&s.target)))
                .collect();
            format!("<ol class=\"steps\">{items}</ol>")
        };
        h.push_str(&format!(
            "<div class=\"cols\"><div class=\"col\"><h4>baseline trajectory</h4>{}</div><div class=\"col\"><h4>with-skill trajectory</h4>{}</div></div>",
            steps(b),
            steps(w)
        ));
        h.push_str(&format!(
            "<div class=\"cols\"><div class=\"col\"><h4>baseline deliverable</h4><pre>{}</pre></div><div class=\"col\"><h4>with-skill deliverable</h4><pre>{}</pre></div></div>",
            esc(&b.deliverable),
            esc(&w.deliverable)
        ));
        let summary = if c.delta.same_deliverable {
            "identical deliverables".to_string()
        } else {
            let added = c.delta.deliverable_diff.lines().filter(|l| l.starts_with('+')).count();
            let removed = c.delta.deliverable_diff.lines().filter(|l| l.starts_with('-')).count();
            format!("deliverable {} chars · {added} line(s) added, {removed} removed", signed(c.delta.deliverable_chars))
        };
        h.push_str(&format!("<details{}><summary>Deliverable delta: {}</summary><pre class=\"diff\">", if c.delta.same_deliverable { "" } else { " open" }, esc(&summary)));
        for line in c.delta.deliverable_diff.lines() {
            let cls = if line.starts_with('+') { "a" } else if line.starts_with('-') { "d" } else { "c" };
            h.push_str(&format!("<span class=\"{cls}\">{}</span>", if line.is_empty() { " ".to_string() } else { esc(line) }));
        }
        h.push_str("</pre></details></article>\n");
    }
    h.push_str("</section></main></body></html>\n");
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{new_entry, NewEntry};
    use rematch_core::ledger::Ledger;
    use rematch_core::paths::RematchPaths;
    use rematch_core::types::{Edit, Family, Kind, Origin};

    struct Env {
        _tmp: tempfile::TempDir,
        ctx: AppCtx,
    }

    /// Isolated roots, a scripted solver model and a git repo (the `--cwd`).
    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let paths = RematchPaths::with_roots(tmp.path().join("rematch"), tmp.path().join("agent"));
        paths.ensure_dirs().unwrap();
        for (name, text) in crate::app::IMPROVER_FILES {
            std::fs::write(paths.improver.join(name), text).unwrap();
        }
        std::fs::create_dir_all(&paths.agent_dir).unwrap();
        std::fs::write(paths.agent_dir.join("config.yml"), "modelRoles:\n  default: fake/solver-1\n").unwrap();
        let ledger = Ledger::open(&paths.ledger_file).unwrap();
        let repo = tmp.path().join("demo-repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("build.sh"), "echo build\n").unwrap();
        for args in [&["init", "-q", "."][..], &["config", "user.email", "t@e.st"], &["config", "user.name", "tester"], &["add", "."], &["commit", "-qm", "init"]] {
            assert!(std::process::Command::new("git").args(args).current_dir(&repo).status().unwrap().success(), "git {args:?}");
        }
        let ctx = AppCtx { paths, ledger, now: rematch_core::ledger::now_ms(), cwd: repo, text: false };
        Env { _tmp: tmp, ctx }
    }

    fn draft(e: &Env, name: &str, stems: &[&str], files: &[(&str, &str)]) -> Entry {
        let entry = new_entry(
            NewEntry {
                kind: Kind::Skill,
                name: name.into(),
                branch: "demo-repo".into(),
                status: Status::Draft,
                family: Family("unknown".into()),
                origin: Origin::Description,
                predicate: Predicate { repo_glob: "**".into(), stems: stems.iter().map(|s| s.to_string()).collect(), ..Default::default() },
                trigger_line: "Use when reviewing checkout accessibility.".into(),
                path: PathBuf::new(),
            },
            e.ctx.now,
        )
        .unwrap();
        let edits: Vec<Edit> = files.iter().map(|(p, c)| Edit::Write { path: p.to_string(), content: c.to_string() }).collect();
        library::stage(&e.ctx.ledger, &e.ctx.paths, &entry, &edits).unwrap()
    }

    #[test]
    fn eval_init_and_trigger_checks_predicate_and_units() {
        let e = env();
        let stems: Vec<String> = distinct(stems_of_text("checkout accessibility review"));
        let stems: Vec<&str> = stems.iter().map(String::as_str).collect();
        // Anthropic skill-creator layout: `evals` with integer ids, extra fields kept.
        let anthropic = r#"{
          "skill_name": "checkout-ux",
          "evals": [{"id": 1, "prompt": "Review the checkout form for accessibility issues", "expected_output": "labelled fields", "files": []}],
          "trigger_evals": [
            {"query": "Review the checkout page for accessibility problems", "should_trigger": true},
            {"query": "Write a limerick about a cat", "should_trigger": false},
            {"query": "Audit the payment form contrast", "should_trigger": true}
          ]
        }"#;
        let d = draft(
            &e,
            "checkout-ux",
            &stems,
            &[
                ("SKILL.md", "---\nname: checkout-ux\ndescription: Use when reviewing checkout accessibility.\n---\n\nUse when reviewing checkout accessibility.\n\n## Laws\n- MUST run `scripts/axe.sh` before reporting.\n- Be thorough.\n"),
                ("units/checklist.md", "# Checkout accessibility checklist\n\n- Review every checkout form field for a visible label.\n"),
                ("references/guide.md", "# Guide\n"),
                ("evals/evals.json", anthropic),
            ],
        );
        let sha = d.content_sha256.clone().unwrap();

        // init merges a prompt into the alias-read suite and keeps the extra fields.
        let out = cmd_init(&e.ctx, InitArgs { entry: "checkout-ux".into(), prompt: vec!["Check the checkout labels".into()], verifier: None }).unwrap();
        assert_eq!(out["cases"].as_array().unwrap().len(), 2, "{out:#}");
        assert_eq!(out["cases"][0]["id"], "1");
        assert_eq!(out["cases"][0]["files"], json!([]));
        assert_eq!(out["added"], json!(["case-2"]));
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(evals_file(&d.path)).unwrap()).unwrap();
        assert_eq!(saved["cases"].as_array().unwrap().len(), 2);
        assert_eq!(saved["trigger_eval"].as_array().unwrap().len(), 3);
        assert_eq!(library::content_sha256(&d.path).unwrap(), sha, "evals/ is not package content");

        let out = cmd_trigger(&e.ctx, TriggerArgs { entry: "checkout-ux".into(), fix: false }).unwrap();
        let q = out["queries"].as_array().unwrap();
        assert_eq!(q.len(), 3, "{out:#}");
        // Positive query: predicate hit and the checklist unit retrieved.
        assert_eq!((q[0]["triggered"].as_bool(), q[0]["pass"].as_bool()), (Some(true), Some(true)), "{out:#}");
        assert_eq!(q[0]["units"][0]["path"], "units/checklist.md", "{out:#}");
        // Negative query: no hit, nothing retrieved.
        assert_eq!((q[1]["triggered"].as_bool(), q[1]["pass"].as_bool()), (Some(false), Some(true)));
        assert!(q[1]["units"].as_array().unwrap().is_empty());
        // Missed positive: missing stems suggested.
        assert_eq!((q[2]["triggered"].as_bool(), q[2]["pass"].as_bool()), (Some(false), Some(false)));
        let suggested: Vec<&str> = q[2]["suggest_stems"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(!suggested.is_empty() && suggested.iter().all(|s| stems_of_text("Audit the payment form contrast").contains(&s.to_string())), "{suggested:?}");
        assert!(out["suggestions"].as_array().unwrap().iter().any(|s| s.as_str().unwrap().starts_with("add stem(s)")), "{out:#}");
        assert_eq!((out["true_positives"].as_u64(), out["false_negatives"].as_u64(), out["true_negatives"].as_u64()), (Some(1), Some(1), Some(1)));
        assert_eq!(out["precision"].as_f64(), Some(1.0));
        assert_eq!(out["recall"].as_f64(), Some(0.5));
        assert!(out["loads_here"].as_bool().unwrap());
        assert_eq!(out["retrieval"]["not_retrievable"], json!(["references/guide.md"]));
        assert_eq!((out["lint"]["laws"].as_u64(), out["lint"]["checkable_laws"].as_u64()), (Some(2), Some(1)));
        assert!(out["lint"]["applicability_clause"].as_bool().unwrap());
        assert!(out["lint"]["catalog_bytes"].as_u64().unwrap() > 0);
        assert!(out["lines"][0].as_str().unwrap().starts_with("trigger eval checkout-ux"), "{out:#}");

        // A fresh entry without a suite: init seeds two cases and two trigger queries.
        draft(&e, "fresh-skill", &stems, &[("SKILL.md", "---\nname: fresh-skill\ndescription: Use when reviewing checkout accessibility.\n---\n\nUse when reviewing checkout accessibility.\n")]);
        let out = cmd_init(&e.ctx, InitArgs { entry: "fresh-skill".into(), prompt: vec![], verifier: Some("tests".into()) }).unwrap();
        assert_eq!(out["created"], true);
        assert_eq!(out["cases"].as_array().unwrap().len(), 2);
        assert!(out["cases"][0]["prompt"].as_str().unwrap().contains("reviewing checkout accessibility"), "{out:#}");
        assert_eq!(out["cases"][0]["verifier"], "tests");
        let tq = out["trigger_eval"].as_array().unwrap();
        assert_eq!((tq[0]["should_trigger"].as_bool(), tq[1]["should_trigger"].as_bool()), (Some(true), Some(false)));
        // The seeded positive triggers, the seeded negative does not.
        let out = cmd_trigger(&e.ctx, TriggerArgs { entry: "fresh-skill".into(), fix: false }).unwrap();
        assert_eq!(out["pass_rate"].as_f64(), Some(1.0), "{out:#}");
    }

    #[cfg(unix)]
    #[test]
    fn eval_run_compares_baseline_and_with_skill_writes_html_and_activates_draft() {
        let e = env();
        let fake_omp = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/fake-omp-batch.sh");
        let d = draft(
            &e,
            "marker-skill",
            &["build", "marker"],
            &[
                ("SKILL.md", "---\nname: marker-skill\ndescription: Use when finishing a build script task.\n---\n\nUse when finishing a build script task.\n\n## Laws\n- MUST create the rematch-test-marker file before yielding.\n"),
                ("units/marker.md", "# Marker\n\nCreate the marker file.\n"),
            ],
        );
        // No suite and no prompt: a usage error naming both ways out.
        let err = run_with(&e.ctx, RunArgs { entry: "marker-skill".into(), prompt: vec![], verifier: None, max_time: None, activate: true }, &fake_omp).unwrap_err();
        assert!(err.downcast_ref::<Usage>().is_some() && format!("{err:#}").contains("rematch eval init marker-skill"), "{err:#}");

        let suite = json!({
            "skill_name": "marker-skill",
            "cases": [{"id": "case-1", "prompt": "Make the check pass in the build script", "verifier": "test -f .rematch-marker"}],
            "trigger_eval": [{"query": "finish the build script marker", "should_trigger": true}]
        });
        std::fs::create_dir_all(d.path.join("evals")).unwrap();
        std::fs::write(evals_file(&d.path), suite.to_string()).unwrap();

        let out = run_with(&e.ctx, RunArgs { entry: "marker-skill".into(), prompt: vec![], verifier: None, max_time: Some("30s".into()), activate: true }, &fake_omp).unwrap();
        assert_eq!(out["summary"]["wins"], 1, "{out:#}");
        assert_eq!((out["summary"]["ties"].as_u64(), out["summary"]["regressions"].as_u64()), (Some(0), Some(0)));
        assert_eq!((out["summary"]["pass_with"].as_u64(), out["summary"]["pass_baseline"].as_u64()), (Some(1), Some(0)));
        let c = &out["cases"][0];
        assert_eq!(c["outcome"], "win");
        assert_eq!(c["baseline"]["pass"], false);
        assert_eq!(c["with_skill"]["pass"], true);
        assert_eq!(c["with_skill"]["skill_read"], true, "{c:#}");
        assert_eq!(c["baseline"]["skill_read"], false);
        assert_eq!(c["with_skill"]["deliverable"], "done");
        assert_eq!(c["with_skill"]["changed_paths"], json!([".rematch-marker"]));
        assert_eq!(c["delta"]["tool_calls"], 1, "the with-skill run read the skill once more");
        assert!(c["with_skill"]["trajectory"].as_array().unwrap().iter().any(|s| s["tool"] == "read"));

        let html_path = PathBuf::from(out["report_html"].as_str().unwrap());
        let results_path = PathBuf::from(out["results_json"].as_str().unwrap());
        assert_eq!(html_path, e.ctx.paths.root.join("evals/marker-skill/report.html"));
        let html = std::fs::read_to_string(&html_path).unwrap();
        assert!(html.starts_with("<!doctype html>") && html.contains("prefers-color-scheme: dark"));
        assert!(!html.contains("<script") && !html.contains("http://") && !html.contains("https://"), "self-contained");
        assert!(html.contains("Make the check pass in the build script") && html.contains("with-skill deliverable") && html.contains("Trigger &amp; retrieval"));
        let results: Value = serde_json::from_str(&std::fs::read_to_string(&results_path).unwrap()).unwrap();
        assert_eq!(results["summary"]["wins"], 1);
        assert_eq!(results["activated"], true);

        // Promoted with the winning trace as evidence, and a receipt.
        assert_eq!(out["activated"], true, "{out:#}");
        let now = e.ctx.ledger.get_entry(&d.id).unwrap().unwrap();
        assert_eq!(now.status, Status::Provisional);
        let sid = c["with_skill"]["session_id"].as_str().unwrap();
        assert!(now.evidence.iter().any(|m| m.session_id == sid), "{:?}", now.evidence);
        assert!(e.ctx.ledger.get_session(sid).unwrap().is_some());
        assert_eq!(e.ctx.ledger.list_receipts(Some(&d.id)).unwrap().len(), 1);
        // Worktrees and run dirs are gone; the report's traces stay.
        let wt = std::process::Command::new("git").arg("-C").arg(&e.ctx.cwd).args(["worktree", "list"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&wt.stdout).lines().count(), 1);
        assert!(PathBuf::from(c["with_skill"]["trace"].as_str().unwrap()).is_file());

        // A provisional entry is not promoted again; a regression refuses the gate.
        let out = run_with(&e.ctx, RunArgs { entry: "marker-skill".into(), prompt: vec!["Make the check pass".into()], verifier: Some("test ! -f .rematch-marker".into()), max_time: Some("30s".into()), activate: true }, &fake_omp).unwrap();
        assert_eq!(out["summary"]["regressions"], 1, "{out:#}");
        assert_eq!(out["gate"]["passed"], false);
        assert_eq!(out["activated"], false);
    }
}
