//! Greenfield `create <name> --from sessions|task|description` and `activate`.
//! sessions: ≥ 3 ledger sessions matching the request → distill (provisional);
//! task: one reference rollout (batch) → propose from its trace (provisional);
//! description: editor from the description + files → `draft` (never loaded).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, ValueEnum};
use rematch_core::features::{head, jaccard, rank_stems, stems_of_session, stems_of_text, MAX_PREDICATE_STEMS};
use rematch_core::ledger::SessionFilter;
use rematch_core::library;
use rematch_core::predicate::{self, MAX_EXTS};
use rematch_core::rate::{self, RateLimit};
use rematch_core::trace::{self, TraceRecord};
use rematch_core::types::{EditorScope, Family, Moment, Origin, Predicate, SessionRecord, Status};
use serde_json::{json, Value};

use crate::app::AppCtx;
use crate::editor::CallBudget;
use crate::lake_read::{self, Lookup, Source};
use crate::reflect::{self, CandidateSpec, Proposed};

pub const MIN_REFERENCE_SESSIONS: usize = 3;
pub const MAX_REFERENCE_SESSIONS: usize = 8;
const FILE_BYTES: usize = 16 * 1024;

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum From {
    Sessions,
    Task,
    Description,
}

#[derive(Args, Debug, Clone)]
pub struct CreateArgs {
    pub name: String,
    #[arg(long, value_enum)]
    pub from: From,
    #[arg(long)]
    pub task: Option<String>,
    #[arg(long)]
    pub branch: Option<String>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long, num_args = 1..)]
    pub files: Vec<PathBuf>,
    /// Scripted editor command (tests).
    #[arg(long)]
    pub editor: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ActivateArgs {
    pub name: String,
    /// Run the task once (reference rollout) and attach its trace as evidence.
    #[arg(long, conflicts_with = "unverified")]
    pub task: Option<String>,
    /// Accept the risk: activate without a trace.
    #[arg(long)]
    pub unverified: bool,
}

fn request_text(a: &CreateArgs) -> String {
    format!("{} {}", a.name.replace(['-', '_'], " "), a.description.clone().unwrap_or_default())
}

fn branch_of(ctx: &AppCtx, a: &CreateArgs) -> Result<String> {
    let b = match &a.branch {
        Some(b) => b.clone(),
        None => library::branch_for(rematch_core::import::repo_of(&ctx.cwd).as_deref(), &ctx.cwd),
    };
    library::validate_name(&b)?;
    Ok(b)
}

fn read_files(files: &[PathBuf]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for f in files {
        let t = std::fs::read_to_string(f).with_context(|| format!("read {}", f.display()))?;
        let key = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| f.display().to_string());
        out.insert(key, head(&t, FILE_BYTES));
    }
    Ok(out)
}

fn check_rate(ctx: &AppCtx, pred: &Predicate, branch: &str) -> Result<()> {
    match rate::can_admit(&ctx.ledger, &predicate::predicate_hash(pred), branch, ctx.now) {
        Ok(()) | Err(RateLimit::NeedsMerge { .. }) => Ok(()),
        Err(e) => bail!("refused by rate limits: {e}"),
    }
}

pub fn cmd_create(ctx: &AppCtx, a: CreateArgs) -> Result<Value> {
    if ctx.paths.is_off() {
        bail!("rematch is off; `rematch on` to enable");
    }
    library::validate_name(&a.name)?;
    if ctx.ledger.list_entries(&rematch_core::ledger::EntryFilter { name: Some(a.name.clone()), ..Default::default() })?
        .iter()
        .any(|e| matches!(e.status, Status::Draft | Status::Provisional | Status::Kept | Status::Pinned))
    {
        bail!("an active entry named {} already exists (`rematch improve {}` revises it)", a.name, a.name);
    }
    match a.from {
        From::Sessions => from_sessions(ctx, &a),
        From::Task => from_task(ctx, &a),
        From::Description => from_description(ctx, &a),
    }
}

/// Matching sessions with their overlap and stem jaccard, best first.
pub type Scored = Vec<(SessionRecord, usize, f64)>;

/// Sessions whose stems/exts overlap the request, best first, and where they came from:
/// the lake's `sessions` (`list:stems` / `list:exts` contains_any, every machine's history)
/// when the lake holds this ledger's whole journal, else the ledger.
pub fn matching_sessions(ctx: &AppCtx, req_stems: &[String], req_exts: &[String]) -> Result<(Scored, Source)> {
    let (candidates, src) = candidate_sessions(ctx, req_stems, req_exts)?;
    let mut scored = Vec::new();
    let need = req_stems.len().clamp(1, 2);
    for s in candidates {
        let overlap = req_stems.iter().filter(|x| s.stems.contains(x)).count()
            + req_exts.iter().filter(|x| s.exts.contains(x)).count();
        if overlap < need {
            continue;
        }
        let j = jaccard(req_stems, &s.stems);
        scored.push((s, overlap, j));
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal)));
    Ok((scored, src))
}

/// Every session that shares a stem or an extension with the request, in `started_at` order.
fn candidate_sessions(ctx: &AppCtx, stems: &[String], exts: &[String]) -> Result<(Vec<SessionRecord>, Source)> {
    if lake_read::current_lake(ctx).is_some() {
        match lake_sessions(ctx, stems, exts) {
            Ok(v) => return Ok((v, Source::Lake)),
            Err(e) => eprintln!("rematch create: lake sessions unreadable, reading the ledger: {e:#}"),
        }
    }
    Ok((ctx.ledger.list_sessions(&SessionFilter::default())?, Source::Ledger))
}

fn lake_sessions(ctx: &AppCtx, stems: &[String], exts: &[String]) -> Result<Vec<SessionRecord>> {
    let mut rows = Vec::new();
    for (key, values) in [("list:stems", stems), ("list:exts", exts)] {
        if !values.is_empty() {
            rows.extend(lake_read::lake_rows(ctx, "sessions", Some(&Lookup { key, values: values.to_vec(), window: None }))?);
        }
    }
    let mut out: Vec<(String, SessionRecord)> = lake_read::as_of(rows, i64::MAX)
        .into_iter()
        .filter_map(|r| serde_json::from_value(r.json).ok().map(|s| (r.machine, s)))
        .collect();
    out.sort_by(|(ma, a), (mb, b)| (a.started_at, ma, &a.id).cmp(&(b.started_at, mb, &b.id)));
    Ok(out.into_iter().map(|(_, s)| s).collect())
}

fn reference_of(ctx: &AppCtx, s: &SessionRecord) -> Value {
    let trace = ctx.paths.traces.join(format!("{}.jsonl", s.id));
    let records = lake_read::lake_events(ctx, s).or_else(|| trace.is_file().then(|| trace::read_trace(&trace).unwrap_or_default()));
    let excerpt = records
        .and_then(|recs| trace::find_moment(s).map(|m| trace::excerpt_around(&recs, m.seq, 5, 5)))
        .unwrap_or_default();
    json!({
        "session_id": s.id,
        "source": s.source,
        "repo_name": s.repo.as_deref().map(|r| Path::new(r).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()),
        "redirects": s.redirects.iter().map(|r| json!({"seq": r.seq, "prelabel": r.prelabel, "class": r.class, "text": head(&r.text_head, 600), "deliverable": head(&r.deliverable_head, 600)})).collect::<Vec<_>>(),
        "skill_reads": s.skill_reads,
        "files": s.files_touched.iter().filter_map(|f| Path::new(f).file_name().map(|n| n.to_string_lossy().to_string())).take(20).collect::<Vec<_>>(),
        "exts": s.exts,
        "stems": s.stems.iter().take(20).collect::<Vec<_>>(),
        "tool_errors": s.tool_errors,
        "turns": s.turns,
        "trace_excerpt": excerpt,
    })
}

fn moment_of(s: &SessionRecord) -> Moment {
    trace::find_moment(s).unwrap_or(Moment { session_id: s.id.clone(), seq: 0 })
}

fn stage_result(ctx: &AppCtx, p: Proposed, session: Option<&str>, hash_pred: &Predicate, admit: bool) -> Result<Value> {
    match p {
        Proposed::KeepBase(r) => {
            let row = ctx.ledger.insert_proposal(None, session, &json!({"decision": r.decision, "manifest": r.manifest}), true)?;
            Ok(json!({"outcome": "keep_base", "proposal": row.id, "lines": ["editor returned KEEP_BASE; nothing staged"]}))
        }
        Proposed::LintFailed(c, r) => {
            let row = ctx.ledger.insert_proposal(None, session, &json!({"decision": r.decision, "entry": c.entry, "lint": c.report}), false)?;
            Ok(json!({"outcome": "lint_failed", "proposal": row.id, "findings": c.report.findings,
                      "lines": [format!("lint failed after retry: {} finding(s)", c.report.findings.len())]}))
        }
        Proposed::Candidate(c, r, attempts) => {
            let staged = library::stage(&ctx.ledger, &ctx.paths, &c.entry, &c.edits)?;
            crate::scope::stale::snapshot_sources(&staged.path)?;
            if admit {
                rate::record_admission(&ctx.ledger, &predicate::predicate_hash(hash_pred), &staged.id, ctx.now)?;
            }
            let row = ctx.ledger.insert_proposal(
                Some(&staged.id),
                session,
                &json!({"decision": r.decision, "entry": staged, "edits": c.edits, "lint": c.report}),
                true,
            )?;
            Ok(json!({
                "outcome": "staged", "id": staged.id, "name": staged.name, "status": staged.status, "path": staged.path,
                "branch": staged.branch, "predicate": staged.predicate, "proposal": row.id, "editor_attempts": attempts,
                "lines": [format!("{} {} at {} · predicate {} exts {:?} stems {:?}", if staged.status == Status::Draft { "drafted" } else { "staged" }, staged.name,
                                  staged.path.display(), staged.predicate.repo_glob, staged.predicate.file_exts, staged.predicate.stems)],
            }))
        }
    }
}

fn from_sessions(ctx: &AppCtx, a: &CreateArgs) -> Result<Value> {
    let req = stems_of_text(&request_text(a));
    let files = read_files(&a.files)?;
    let req_exts = rematch_core::features::exts_of(&a.files.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>());
    let (matches, source) = matching_sessions(ctx, &req, &req_exts)?;
    if matches.len() < MIN_REFERENCE_SESSIONS {
        bail!(
            "found {} matching session(s) in the {}; {MIN_REFERENCE_SESSIONS} are needed to distill from sessions (try --from task --task <id>, or --from description for a draft)",
            matches.len(),
            source.name()
        );
    }
    let refs: Vec<&SessionRecord> = matches.iter().take(MAX_REFERENCE_SESSIONS).map(|(s, _, _)| s).collect();
    let best = refs[0];
    let repos: std::collections::BTreeSet<Option<&str>> = refs.iter().map(|s| s.repo.as_deref()).collect();
    let repo = if repos.len() == 1 { repos.into_iter().next().flatten() } else { None };
    let mut ext_freq: BTreeMap<&str, usize> = BTreeMap::new();
    for s in &refs {
        for e in &s.exts {
            *ext_freq.entry(e.as_str()).or_default() += 1;
        }
    }
    let mut exts: Vec<(&str, usize)> = ext_freq.into_iter().collect();
    exts.sort_by_key(|x| std::cmp::Reverse(x.1));
    let exts: Vec<String> = exts.into_iter().take(MAX_EXTS).map(|(e, _)| e.to_string()).collect();
    let mut doc_terms = req.clone();
    doc_terms.extend(req.iter().cloned());
    for s in &refs {
        doc_terms.extend(s.stems.iter().cloned());
    }
    let corpus = crate::import_cmd::stem_corpus(ctx)?;
    let mut ranked = rank_stems(&doc_terms, &corpus);
    ranked.truncate(MAX_PREDICATE_STEMS);
    let pred = predicate::derive(repo, &exts, &ranked);
    let branch = branch_of(ctx, a)?;
    check_rate(ctx, &pred, &branch)?;
    let input = json!({
        "mode": "distill_from_references",
        "scope": "domain",
        "request": {"name": a.name, "description": a.description, "files": files},
        "references": refs.iter().map(|s| reference_of(ctx, s)).collect::<Vec<_>>(),
        "trace_excerpt": [],
        "deliverable": "",
        "redirect": Value::Null,
        "current_entry": Value::Null,
        "library": reflect::library_for_editor(ctx, repo.unwrap_or(&ctx.cwd.to_string_lossy()), &ranked)?,
        "lint": {"rules": reflect::lint_rules()},
        "retry_findings": Value::Null,
    });
    let spec = CandidateSpec {
        branch: &branch,
        family: best.family.clone().unwrap_or_else(|| Family(rematch_core::counters::UNKNOWN_FAMILY.into())),
        origin: Origin::Session { session_id: best.id.clone() },
        predicate: pred.clone(),
        redirect_class: None,
        scope: EditorScope::Domain,
        fallback_moment: moment_of(best),
        status: Status::Provisional,
        sibling_of: None,
        name_override: Some(a.name.clone()),
    };
    let mut budget = CallBudget::new(2);
    let p = reflect::propose(ctx, a.editor.as_deref(), &input, &spec, &mut budget)?;
    let p = match p {
        Proposed::Candidate(mut c, r, n) => {
            // Evidence moments from every reference session.
            for s in &refs {
                let m = moment_of(s);
                if !c.entry.evidence.contains(&m) {
                    c.entry.evidence.push(m);
                }
            }
            Proposed::Candidate(c, r, n)
        }
        other => other,
    };
    let mut out = stage_result(ctx, p, Some(&best.id), &pred, true)?;
    out["references"] = json!(refs.iter().map(|s| &s.id).collect::<Vec<_>>());
    out["source"] = json!(source);
    out["model_calls"] = json!(budget.used);
    Ok(out)
}

fn from_task(ctx: &AppCtx, a: &CreateArgs) -> Result<Value> {
    let task = a.task.as_deref().ok_or_else(|| crate::envelope::Usage("--from task needs --task <id>".into()))?;
    let trace_path = crate::batch::worker::cmd_reference_rollout(ctx, task)?;
    let sid = trace_path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| anyhow!("bad trace path"))?.to_string();
    let records = trace::read_trace(&trace_path)?;
    let s = trace::fold_trace(&records, &sid);
    ctx.ledger.upsert_session(&s)?;
    let moment = trace::find_moment(&s).unwrap_or(Moment { session_id: sid.clone(), seq: records.last().map(TraceRecord::seq).unwrap_or(0) });
    let inputs: Vec<String> = records.iter().filter_map(|r| match r { TraceRecord::Input { text, .. } => Some(text.clone()), _ => None }).collect();
    let mut doc_terms = stems_of_text(&request_text(a));
    doc_terms.extend(stems_of_session(&inputs, &s.files_touched));
    let corpus = crate::import_cmd::stem_corpus(ctx)?;
    let mut ranked = rank_stems(&doc_terms, &corpus);
    ranked.truncate(MAX_PREDICATE_STEMS);
    let pred = predicate::derive(s.repo.as_deref(), &s.exts, &ranked);
    let branch = branch_of(ctx, a)?;
    check_rate(ctx, &pred, &branch)?;
    let last_text = records.iter().rev().find_map(|r| match r { TraceRecord::TurnEnd { text_head, .. } if !text_head.is_empty() => Some(text_head.clone()), _ => None }).unwrap_or_default();
    let input = json!({
        "mode": "from_reference_rollout",
        "scope": "domain",
        "request": {"name": a.name, "description": a.description, "task": task, "files": read_files(&a.files)?},
        "trace_excerpt": trace::excerpt_around(&records, moment.seq, reflect::EXCERPT_RADIUS, reflect::EXCERPT_RADIUS),
        "deliverable": last_text,
        "redirect": Value::Null,
        "moment": moment,
        "current_entry": Value::Null,
        "library": reflect::library_for_editor(ctx, &s.repo.clone().unwrap_or_else(|| s.cwd.clone()), &ranked)?,
        "lint": {"rules": reflect::lint_rules()},
        "retry_findings": Value::Null,
    });
    let spec = CandidateSpec {
        branch: &branch,
        family: s.family.clone().unwrap_or_else(|| Family(rematch_core::counters::UNKNOWN_FAMILY.into())),
        origin: Origin::Task { task_id: task.to_string() },
        predicate: pred.clone(),
        redirect_class: None,
        scope: EditorScope::Domain,
        fallback_moment: moment,
        status: Status::Provisional,
        sibling_of: None,
        name_override: Some(a.name.clone()),
    };
    let mut budget = CallBudget::new(2);
    let p = reflect::propose(ctx, a.editor.as_deref(), &input, &spec, &mut budget)?;
    let mut out = stage_result(ctx, p, Some(&sid), &pred, true)?;
    out["trace"] = json!(trace_path);
    out["model_calls"] = json!(budget.used);
    Ok(out)
}

fn from_description(ctx: &AppCtx, a: &CreateArgs) -> Result<Value> {
    let desc = a.description.clone().unwrap_or_default();
    let files = read_files(&a.files)?;
    if desc.trim().is_empty() && files.is_empty() {
        return Err(crate::envelope::Usage("--from description needs --description <text> and/or --files".into()).into());
    }
    let repo = rematch_core::import::repo_of(&ctx.cwd);
    let paths: Vec<String> = a.files.iter().map(|p| p.to_string_lossy().to_string()).collect();
    let corpus = crate::import_cmd::stem_corpus(ctx)?;
    let mut ranked = rank_stems(&stems_of_session(&[request_text(a)], &paths), &corpus);
    ranked.truncate(MAX_PREDICATE_STEMS);
    let pred = predicate::derive(repo.as_deref().map(|r| r.to_str().unwrap_or("")), &rematch_core::features::exts_of(&paths), &ranked);
    let branch = branch_of(ctx, a)?;
    let input = json!({
        "mode": "from_description",
        "scope": "domain",
        "request": {"name": a.name, "description": desc, "files": files},
        "trace_excerpt": [],
        "deliverable": "",
        "redirect": Value::Null,
        "current_entry": Value::Null,
        "library": reflect::library_for_editor(ctx, &repo.as_deref().unwrap_or(&ctx.cwd).to_string_lossy(), &ranked)?,
        "lint": {"rules": reflect::lint_rules()},
        "retry_findings": Value::Null,
    });
    let spec = CandidateSpec {
        branch: &branch,
        family: Family(rematch_core::counters::UNKNOWN_FAMILY.into()),
        origin: Origin::Description,
        predicate: pred.clone(),
        redirect_class: None,
        scope: EditorScope::Domain,
        fallback_moment: Moment { session_id: String::new(), seq: 0 },
        status: Status::Draft,
        sibling_of: None,
        name_override: Some(a.name.clone()),
    };
    let mut budget = CallBudget::new(2);
    let p = reflect::propose(ctx, a.editor.as_deref(), &input, &spec, &mut budget)?;
    let p = match p {
        // No trace behind a description: no evidence moment.
        Proposed::Candidate(mut c, r, n) => {
            c.entry.evidence.retain(|m| !m.session_id.is_empty());
            Proposed::Candidate(c, r, n)
        }
        other => other,
    };
    let mut out = stage_result(ctx, p, None, &pred, false)?;
    out["model_calls"] = json!(budget.used);
    Ok(out)
}

pub fn cmd_activate(ctx: &AppCtx, a: ActivateArgs) -> Result<Value> {
    if a.task.is_none() && !a.unverified {
        return Err(crate::envelope::Usage("activate needs --task <id> or --unverified".into()).into());
    }
    let mut evidence = None;
    if let Some(task) = &a.task {
        let trace_path = crate::batch::worker::cmd_reference_rollout(ctx, task)?;
        let sid = trace_path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        let records = trace::read_trace(&trace_path)?;
        let s = trace::fold_trace(&records, &sid);
        ctx.ledger.upsert_session(&s)?;
        evidence = Some(moment_of(&s));
    }
    let mut e = library::activate_draft(&ctx.ledger, &ctx.paths, &a.name)?;
    if let Some(m) = evidence {
        e = reflect::append_evidence(ctx, &e.id, &m)?;
    }
    let how = if a.unverified { "unverified (risk accepted)" } else { "after one reference run" };
    Ok(json!({"entry": e, "lines": [format!("activated {} as provisional {how}", e.name)]}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::testutil::{env, session};

    fn ids(m: &Scored) -> Vec<(String, usize)> {
        m.iter().map(|(s, n, _)| (s.id.clone(), *n)).collect()
    }

    /// The reference search over the lake's `sessions` finds the same sessions, in the same
    /// order, as over the ledger; a ledger write the lake has not seen sends it back.
    #[test]
    fn reference_search_agrees_on_the_ledger_and_the_lake() {
        let t = env();
        let ctx = &t.ctx;
        let mk = |id: &str, at: i64, exts: &[&str], stems: &[&str]| {
            let mut s = session(id, "payments", Some("/w/payments"), "anthropic/claude-opus", at);
            s.exts = exts.iter().map(|x| x.to_string()).collect();
            s.stems = stems.iter().map(|x| x.to_string()).collect();
            ctx.ledger.upsert_session(&s).unwrap();
        };
        mk("s-both", 1_000, &["rs"], &["ledger", "migrat"]);
        mk("s-stems", 2_000, &["py"], &["ledger", "migrat", "test"]);
        mk("s-one", 3_000, &["rs"], &["ledger", "round"]);
        mk("s-ext", 4_000, &["rs"], &["docs"]);
        mk("s-none", 5_000, &["md"], &["readme"]);
        let (stems, exts) = (vec!["ledger".to_string(), "migrat".to_string()], vec!["rs".to_string()]);
        let (ledger, src) = matching_sessions(ctx, &stems, &exts).unwrap();
        assert_eq!(src, Source::Ledger);
        let want = vec![("s-both".to_string(), 3), ("s-stems".to_string(), 2), ("s-one".to_string(), 2)];
        assert_eq!(ids(&ledger), want);

        #[cfg(feature = "lake")]
        {
            lake_read::export_all(ctx);
            let (lake, src) = matching_sessions(ctx, &stems, &exts).unwrap();
            assert_eq!((ids(&lake), src), (want, Source::Lake));
            let refs = |m: &Scored| m.iter().map(|(s, _, _)| reference_of(ctx, s)).collect::<Vec<_>>();
            assert_eq!(refs(&lake), refs(&ledger));
            mk("s-late", 6_000, &["rs"], &["ledger", "migrat"]);
            let (late, src) = matching_sessions(ctx, &stems, &exts).unwrap();
            assert_eq!((late.len(), src), (4, Source::Ledger), "the lake lags the ledger: read the ledger");
        }
    }
}
