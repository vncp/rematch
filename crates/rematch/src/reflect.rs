//! `reflect --pending` (SPEC §loop 1–8): first `count` (fold finished traces → counters →
//! decide → EXPERIENCE.md), then one bounded reflection per pending file: triage → moment →
//! predicate → rate limits → editor → lint (one retry) → dedup → stage. `reflect --history`
//! first hands flagged imported sessions to that loop through synthesized traces. Also
//! `count`, `mark` and `record`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use rematch_core::counters;
use rematch_core::dedup;
use rematch_core::experience::{self, ExperienceQuery};
use rematch_core::features::{exts_of, head, rank_stems, stems_of_session, stems_of_text, MAX_PREDICATE_STEMS};
use rematch_core::import::apply_trace_signals;
use rematch_core::ledger::{enum_str, EntryFilter, SessionFilter};
use rematch_core::library::{self, REMATCH_JSON};
use rematch_core::lint::{self, LintConfig, LintContext};
use rematch_core::predicate;
use rematch_core::rate::{self, RateLimit};
use rematch_core::redirect;
use rematch_core::trace::{self, TraceRecord, TriageKind};
use rematch_core::types::{
    Counters, Decision, Edit, EditorScope, Entry, Family, Kind, LessonTag, LintReport, Manifest, Moment, Origin,
    Paired, Predicate, Proposal, Redirect, RedirectClass, SessionRecord, Severity, Status,
};
use serde_json::{json, Value};

use crate::app::{new_entry, AppCtx, NewEntry};
use crate::config::{ROLE_CLASSIFIER, ROLE_EDITOR};
use crate::editor::{self, CallBudget, EditorDecision, EditorReply};
use crate::scope::anchor::{self, AnchorOutcome, AnchorReceipt};

/// Hard rule: at most three model calls per reflection (classifier batch, editor, one lint retry).
pub const MAX_MODEL_CALLS: u32 = 3;
/// Records on each side of the moment in the editor excerpt.
pub const EXCERPT_RADIUS: usize = 15;
/// A pending file whose editor call failed this many times is dropped.
pub const MAX_ATTEMPTS: u64 = 3;
/// Default `reflect --history --limit`: pending files created per run.
pub const HISTORY_LIMIT: usize = 10;

#[derive(Args, Debug, Clone, Default)]
pub struct ReflectArgs {
    /// Process every pending reflection (the only mode; kept for the documented argv).
    #[arg(long)]
    pub pending: bool,
    /// Only this session (count + reflect).
    #[arg(long)]
    pub session: Option<String>,
    /// Scripted editor command replacing omp for every model call (tests).
    #[arg(long)]
    pub editor: Option<String>,
    /// Run detached and return a job id.
    #[arg(long)]
    pub detach: bool,
    /// First hand flagged imported sessions (`import --history`) without a trace to reflection.
    #[arg(long)]
    pub history: bool,
    /// With --history: only sessions started on or after this UTC date.
    #[arg(long, value_name = "YYYY-MM-DD", requires = "history")]
    pub since: Option<String>,
    /// With --history: at most this many sessions handed to reflection per run (default 10).
    #[arg(long, value_name = "N", requires = "history")]
    pub limit: Option<usize>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct CountArgs {
    #[arg(long)]
    pub session: Option<String>,
    /// Scripted command for the classifier/reflector calls (tests).
    #[arg(long)]
    pub editor: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct MarkArgs {
    #[arg(long)]
    pub session: String,
    #[arg(long, value_parser = ["correction", "preference", "scope"])]
    pub class: String,
}

#[derive(Args, Debug, Clone)]
pub struct RecordArgs {
    #[arg(long)]
    pub session: String,
    /// Event JSON (`{"ev":"mark","class":"correction"}`).
    #[arg(long)]
    pub event: String,
}

fn refuse_when_off(ctx: &AppCtx) -> Result<()> {
    if ctx.paths.is_off() {
        bail!("rematch is off (REMATCH=0 or {}); `rematch on` to enable", ctx.paths.off_marker.display());
    }
    Ok(())
}

pub fn cmd_reflect(ctx: &AppCtx, a: ReflectArgs) -> Result<Value> {
    refuse_when_off(ctx)?;
    if a.detach {
        let mut argv = vec!["reflect".to_string(), "--pending".to_string()];
        if let Some(s) = &a.session {
            argv.extend(["--session".into(), s.clone()]);
        }
        if let Some(e) = &a.editor {
            argv.extend(["--editor".into(), e.clone()]);
        }
        if a.history {
            argv.push("--history".into());
        }
        if let Some(s) = &a.since {
            argv.extend(["--since".into(), s.clone()]);
        }
        if let Some(n) = a.limit {
            argv.extend(["--limit".into(), n.to_string()]);
        }
        let id = crate::jobs::spawn_detached(ctx, "reflect", &argv, &json!({}))?;
        return Ok(json!({"job_id": id, "lines": [format!("reflect running detached: job {id}")]}));
    }
    let since = a.since.as_deref().map(parse_since).transpose()?;
    let Some(_lock) = crate::jobs::LockFile::acquire(ctx.paths.pending.join(".lock"))? else {
        return Ok(json!({"skipped": "another reflect --pending is running", "lines": ["reflect: already running"]}));
    };
    let history = if a.history {
        Some(stage_history(ctx, a.session.as_deref(), since, a.limit.unwrap_or(HISTORY_LIMIT))?)
    } else {
        None
    };
    let count = count_finished(ctx, a.session.as_deref(), a.editor.as_deref())?;
    let classified: BTreeSet<String> =
        count.classified_sessions.iter().cloned().collect();
    let mut reflections = Vec::new();
    for sid in pending_sessions(ctx, a.session.as_deref())? {
        let classifier_used = classified.contains(&sid);
        let r = match reflect_one(ctx, &sid, a.editor.as_deref(), classifier_used) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("rematch reflect {sid}: {e:#}");
                json!({"session": sid, "outcome": "error", "error": format!("{e:#}")})
            }
        };
        reflections.push(r);
    }
    let history = history.map(|h| h.report(&reflections));
    let mut lines: Vec<String> = history.iter().map(|(_, line)| line.clone()).collect();
    lines.extend(count.lines.iter().cloned());
    for r in &reflections {
        lines.push(reflection_line(r));
    }
    let mut out = json!({"count": count.to_json(), "reflections": reflections, "lines": lines});
    if let Some((report, _)) = history {
        out["history"] = report;
    }
    Ok(out)
}

fn reflection_line(r: &Value) -> String {
    let sid = r.get("session").and_then(Value::as_str).unwrap_or("?");
    let short: String = sid.chars().take(8).collect();
    let outcome = r.get("outcome").and_then(Value::as_str).unwrap_or("?");
    match outcome {
        "staged" => format!(
            "staged {} ({}) from session {short}",
            r.get("name").and_then(Value::as_str).unwrap_or("?"),
            r.get("kind").and_then(Value::as_str).unwrap_or("entry")
        ),
        _ => format!(
            "session {short}: {outcome}{}",
            r.get("reason").and_then(Value::as_str).map(|x| format!(" ({x})")).unwrap_or_default()
        ),
    }
}

pub fn cmd_count(ctx: &AppCtx, a: CountArgs) -> Result<Value> {
    refuse_when_off(ctx)?;
    let c = count_finished(ctx, a.session.as_deref(), a.editor.as_deref())?;
    let mut v = c.to_json();
    v["lines"] = json!(c.lines);
    Ok(v)
}

// ------------------------------------------------------------------------------------
// count

#[derive(Debug, Default)]
pub struct CountSummary {
    pub folded: Vec<String>,
    pub classified_sessions: Vec<String>,
    pub classifier: Value,
    pub rows: usize,
    pub decisions: Vec<Value>,
    pub divergence: Vec<Value>,
    pub skipped_unfinished: usize,
    /// Phase-3 passes run after the fold: per-family split, shared promotion, stale sweep.
    pub scope: Value,
    pub lines: Vec<String>,
}

impl CountSummary {
    fn to_json(&self) -> Value {
        json!({
            "folded": self.folded,
            "classifier": self.classifier,
            "counter_rows": self.rows,
            "decisions": self.decisions,
            "divergence": self.divergence,
            "skipped_unfinished": self.skipped_unfinished,
            "scope": self.scope,
        })
    }
}

fn trace_files(ctx: &AppCtx) -> Result<Vec<(String, PathBuf)>> {
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(&ctx.paths.traces)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| Some((p.file_stem()?.to_str()?.to_string(), p)))
        .collect();
    v.sort();
    Ok(v)
}

fn finished(records: &[TraceRecord]) -> bool {
    records.iter().any(|r| matches!(r, TraceRecord::SessionShutdown { .. }))
}

/// Ledger row for a trace. A trace synthesized for an imported session (`reflect --history`)
/// refreshes the import row's signals (seqs then address the trace) and keeps the importer's
/// counts instead of replacing the row with a trace-sourced one; `count` then marks it counted
/// once like any other session.
fn fold_for_ledger(records: &[TraceRecord], sid: &str, existing: Option<SessionRecord>) -> SessionRecord {
    let folded = trace::fold_trace(records, sid);
    match existing {
        Some(mut imported) if folded.source == "import" && imported.source == "import" => {
            apply_trace_signals(&mut imported, &folded);
            imported
        }
        _ => folded,
    }
}

/// Entries the user disabled while the session ran (harm).
fn disabled_during(ctx: &AppCtx, s: &SessionRecord) -> Result<Vec<String>> {
    let end = s.ended_at.unwrap_or(i64::MAX);
    Ok(ctx
        .ledger
        .list_entries(&EntryFilter { status: Some(Status::Retired), ..Default::default() })?
        .into_iter()
        .filter(|e| s.loaded_entries.contains(&e.id))
        .filter(|e| e.status_note.as_deref().is_some_and(|n| n.starts_with("disabled:")))
        .filter(|e| e.status_since.is_some_and(|t| t >= s.started_at && t <= end))
        .map(|e| e.id)
        .collect())
}

/// Fold every finished, uncounted trace (DESIGN §2 `count`).
pub fn count_finished(ctx: &AppCtx, only: Option<&str>, editor_cmd: Option<&str>) -> Result<CountSummary> {
    let mut sum = CountSummary::default();
    let mut sessions: Vec<SessionRecord> = Vec::new();
    for (sid, path) in trace_files(ctx)? {
        if only.is_some_and(|o| o != sid) {
            continue;
        }
        let existing = ctx.ledger.get_session(&sid)?;
        if existing.as_ref().is_some_and(|s| s.counted) {
            continue;
        }
        let records = trace::read_trace(&path)?;
        if !finished(&records) {
            sum.skipped_unfinished += 1;
            continue;
        }
        let s = fold_for_ledger(&records, &sid, existing);
        ctx.ledger.upsert_session(&s)?;
        sessions.push(s);
    }

    // One batched classifier call over every unclassified, non-mark redirect.
    let batch: Vec<Redirect> = sessions
        .iter()
        .flat_map(|s| s.redirects.iter())
        .filter(|r| r.class.is_none() && redirect::mark_class(&r.prelabel).is_none())
        .cloned()
        .collect();
    if batch.is_empty() {
        sum.classifier = json!({"called": false, "reason": "no redirects to classify"});
    } else {
        let prompt = redirect::build_classifier_prompt(&batch);
        match editor::call(ctx, editor_cmd, ROLE_CLASSIFIER, &ctx.improver_file("classifier.md"), &prompt) {
            Ok(reply) => {
                let parsed = redirect::parse_classifier_reply(&reply);
                for s in sessions.iter_mut() {
                    redirect::apply_classifications(&mut s.redirects, &parsed);
                    for r in &s.redirects {
                        if let Some(c) = r.class {
                            if redirect::mark_class(&r.prelabel).is_none() {
                                ctx.ledger.set_redirect_class(&s.id, r.seq, c, r.confidence)?;
                            }
                        }
                    }
                }
                let ids: BTreeSet<&str> = batch.iter().map(|r| r.session_id.as_str()).collect();
                sum.classified_sessions = ids.iter().map(|s| s.to_string()).collect();
                sum.classifier = json!({"called": true, "items": batch.len(), "labelled": parsed.len()});
            }
            Err(e) => {
                eprintln!("rematch count: classifier failed, using prelabels: {e:#}");
                let ids: BTreeSet<&str> = batch.iter().map(|r| r.session_id.as_str()).collect();
                // The call was spent even though it failed.
                sum.classified_sessions = ids.iter().map(|s| s.to_string()).collect();
                sum.classifier = json!({"called": true, "error": format!("{e:#}")});
            }
        }
    }

    let mut affected: BTreeSet<String> = BTreeSet::new();
    for s in &sessions {
        // Re-read so the classifier labels written above are part of the record.
        let s = ctx.ledger.get_session(&s.id)?.unwrap_or_else(|| s.clone());
        let loaded: Vec<Entry> =
            s.loaded_entries.iter().filter_map(|id| ctx.ledger.get_entry(id).ok().flatten()).collect();
        let disabled = disabled_during(ctx, &s)?;
        let rows = counters::fold_session(&ctx.ledger, &s, &loaded, &disabled)?;
        sum.rows += rows.len();
        affected.extend(rows.into_iter().map(|r| r.entry_id));
        sum.folded.push(s.id.clone());
    }

    for id in affected {
        let Some(e) = ctx.ledger.get_entry(&id)? else { continue };
        let c = ctx.ledger.counters(&id)?;
        if let Some((harmed, clean)) = counters::family_divergence(&c) {
            sum.divergence.push(json!({"entry": id, "name": e.name, "harmed": harmed, "clean": clean}));
        }
        let d = counters::decide(&e, &c, ctx.now);
        if d == Decision::Continue {
            continue;
        }
        match apply_decision(ctx, &e, d, &c, editor_cmd) {
            Ok(v) => {
                sum.lines.push(format!("{} {}: {}", enum_str(&d), e.name, experience::counters_line(&c).lines().next().unwrap_or("")));
                sum.decisions.push(v);
            }
            Err(err) => {
                eprintln!("rematch count: decision {d:?} for {} failed: {err:#}", e.id);
                sum.decisions.push(json!({"entry": id, "decision": d, "error": format!("{err:#}")}));
            }
        }
    }
    sum.scope = scope_passes(ctx, &mut sum.lines);
    if !sum.folded.is_empty() {
        sum.lines.insert(0, format!("counted {} session(s), {} counter row(s)", sum.folded.len(), sum.rows));
    }
    Ok(sum)
}

/// Scope slice passes (idempotent). A failing pass is reported, never fatal to counting.
fn scope_passes(ctx: &AppCtx, lines: &mut Vec<String>) -> Value {
    let mut out = serde_json::Map::new();
    match crate::scope::split::check(ctx) {
        Ok(v) => {
            for s in &v {
                if let Ok(j) = serde_json::to_value(s) {
                    if let Some(sib) = j.get("sibling_name").and_then(Value::as_str) {
                        lines.push(format!("split: staged per-family sibling {sib}"));
                    }
                }
            }
            out.insert("split".into(), serde_json::to_value(&v).unwrap_or(Value::Null));
        }
        Err(e) => {
            out.insert("split_error".into(), json!(format!("{e:#}")));
        }
    }
    match crate::scope::transfer::promote_shared(ctx) {
        Ok(v) => {
            out.insert("promotions".into(), serde_json::to_value(&v).unwrap_or(Value::Null));
        }
        Err(e) => {
            out.insert("promotions_error".into(), json!(format!("{e:#}")));
        }
    }
    match crate::scope::stale::sweep(ctx) {
        Ok(v) => {
            if !v.is_empty() {
                lines.push(format!("stale: {} entr(ies) marked stale", v.len()));
            }
            out.insert("stale".into(), serde_json::to_value(&v).unwrap_or(Value::Null));
        }
        Err(e) => {
            out.insert("stale_error".into(), json!(format!("{e:#}")));
        }
    }
    Value::Object(out)
}

/// Whether rematch owns the entry's directory: it lives under REMATCH_ROOT, or it is a
/// kept copy whose SKILL.md carries this entry's `rematch:` block. Hand-written skills
/// registered by `improve`/`import --forge` (even under `~/.omp/agent/skills`) are not.
pub fn owns_dir(ctx: &AppCtx, e: &Entry) -> bool {
    if e.path.starts_with(&ctx.paths.root) {
        return true;
    }
    std::fs::read_to_string(e.path.join("SKILL.md"))
        .ok()
        .and_then(|t| rematch_core::package::SkillDoc::parse(&t).ok())
        .and_then(|d| d.rematch())
        .is_some_and(|b| b.id == e.id)
}

/// Entries whose files are user/forge-owned (or lessons, which are library records):
/// status changes are recorded in the ledger only and never move files.
pub fn is_foreign(ctx: &AppCtx, e: &Entry) -> bool {
    e.kind == Kind::Lesson || !owns_dir(ctx, e)
}

/// Ledger(+rematch.json)-only status change for foreign entries and lessons.
pub fn record_status(ctx: &AppCtx, e: &Entry, to: Status, note: &str) -> Result<Entry> {
    if e.pinned {
        bail!("entry {} is pinned (human-owned)", e.id);
    }
    if !library::can_transition(e.status, to) {
        bail!("illegal transition {:?} → {to:?} for {}", e.status, e.id);
    }
    let mut n = e.clone();
    n.status = to;
    n.status_since = Some(ctx.now);
    n.status_note = Some(note.to_string());
    n.updated_at = ctx.now;
    ctx.ledger.upsert_entry(&n)?;
    if n.path.starts_with(&ctx.paths.root) && n.path.join(REMATCH_JSON).is_file() {
        // Keep extra keys (evidence_sha256, source_path) of imported lessons.
        let mut v: Value = serde_json::from_str(&std::fs::read_to_string(n.path.join(REMATCH_JSON))?)?;
        if let (Value::Object(m), Value::Object(fresh)) = (&mut v, serde_json::to_value(&n)?) {
            m.extend(fresh);
        }
        std::fs::write(n.path.join(REMATCH_JSON), serde_json::to_string_pretty(&v)?)?;
    }
    Ok(n)
}

fn experience_dir(ctx: &AppCtx, e: &Entry) -> PathBuf {
    if owns_dir(ctx, e) {
        e.path.clone()
    } else {
        ctx.paths.root.join("brownfield").join(&e.id)
    }
}

/// Apply a Keep/Retire/Dormant decision and write EXPERIENCE.md for keep/retire.
/// A Keep that replaces a kept entry first runs its anchor replay (`scope::anchor`): a
/// failing replay turns it into a Retire with an `anchor replay failed: …` note.
pub fn apply_decision(ctx: &AppCtx, e: &Entry, d: Decision, c: &Counters, editor_cmd: Option<&str>) -> Result<Value> {
    if d == Decision::Continue {
        return Ok(json!({"entry": e.id, "decision": d}));
    }
    let mut d = d;
    let mut note = format!("counters: {}", experience::counters_line(c).lines().next().unwrap_or(""));
    let mut anchor = None;
    if d == Decision::Keep {
        if let Some(kept) = anchor::replaced_kept(ctx, e)? {
            let outcome = anchor::replay_before_replace(ctx, e, &kept)?;
            if let AnchorOutcome::Replayed { pass: false, receipt } = &outcome {
                d = Decision::Retire;
                note = anchor::failure_note(receipt);
            }
            anchor = Some((kept, outcome));
        }
    }
    let to = match d {
        Decision::Keep => Status::Kept,
        Decision::Retire => Status::Retired,
        _ => Status::Dormant,
    };
    let moved = if is_foreign(ctx, e) {
        record_status(ctx, e, to, &format!("{note} (files left in place)"))?
    } else {
        library::set_status(&ctx.ledger, &ctx.paths, &e.id, to, Some(&note))?
    };
    let mut out = json!({"entry": e.id, "name": e.name, "decision": d, "status": moved.status, "path": moved.path});
    if let Some((kept, outcome)) = &anchor {
        out["anchor"] = json!({"kept": kept.id, "replay": outcome});
        if let AnchorOutcome::Replayed { pass: true, receipt } = outcome {
            let canary = Paired { matched: c.matched, recurrence: c.recurrence, harm: c.harm, family: e.family.clone() };
            out["anchor"]["certificate"] = json!(anchor::certify(ctx, &moved, receipt, Some(canary))?);
        }
    }
    if matches!(d, Decision::Keep | Decision::Retire) {
        let replay = match anchor.as_ref().map(|(_, o)| o) {
            Some(AnchorOutcome::Replayed { receipt, .. }) => Some(&**receipt),
            _ => None,
        };
        let (situation, lesson, tag, source) = reflector_lesson(ctx, &moved, d, c, replay, editor_cmd);
        let dir = experience_dir(ctx, &moved);
        if dir.starts_with(ctx.paths.root.join("brownfield")) {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join(REMATCH_JSON), serde_json::to_string_pretty(&moved)?)?;
        }
        experience::write_experience(&dir, &situation, &lesson, tag, &moved.evidence, c)?;
        out["experience"] = json!({"path": dir.join(experience::EXPERIENCE_MD), "source": source, "tag": tag});
        // SPEC: the reflector tags each kept/retired lesson invariant or domain.
        if moved.lesson_tag.is_none() || (source == "reflector" && moved.lesson_tag != Some(tag)) {
            let mut t = moved.clone();
            t.lesson_tag = Some(tag);
            ctx.ledger.upsert_entry(&t)?;
            if !is_foreign(ctx, &t) && t.path.join(REMATCH_JSON).is_file() {
                std::fs::write(t.path.join(REMATCH_JSON), serde_json::to_string_pretty(&t)?)?;
            }
        }
    }
    Ok(out)
}

fn entry_files(dir: &Path, cap: usize) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let rel = p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            if rel == REMATCH_JSON || rel == experience::EXPERIENCE_MD {
                continue;
            }
            if let Ok(t) = std::fs::read_to_string(&p) {
                out.insert(rel, rematch_core::features::head(&t, cap));
            }
        }
    }
    out
}

/// Reflector call (`improver/reflector.md`); deterministic record when it fails. `replay`
/// is the anchor replay that preceded a keep (or turned it into a retire).
fn reflector_lesson(
    ctx: &AppCtx,
    e: &Entry,
    d: Decision,
    c: &Counters,
    replay: Option<&AnchorReceipt>,
    editor_cmd: Option<&str>,
) -> (String, String, LessonTag, &'static str) {
    let mut input = json!({
        "entry": {"id": e.id, "name": e.name, "kind": e.kind, "trigger_line": e.trigger_line,
                  "redirect_class": e.redirect_class, "family": e.family, "branch": e.branch, "origin": e.origin},
        "manifest": e.manifest,
        "files": entry_files(&e.path, 4096),
        "counters": c,
        "decision": d,
    });
    if let Some(r) = replay {
        input["anchor_replay"] = json!({
            "task": r.task,
            "verifier": r.verifier,
            "pass": crate::batch::verifier::is_pass(&r.verdict),
            "detail": head(crate::batch::verifier::detail_of(&r.verdict), 1024),
        });
    }
    let reply = editor::call(ctx, editor_cmd, ROLE_EDITOR, &ctx.improver_file("reflector.md"), &editor::wrap_input("reflector", &input));
    if let Ok(text) = reply {
        if let Some(v) = redirect::extract_json(&text) {
            let s = v.get("situation").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let tag = match v.get("tag").and_then(Value::as_str) {
                Some("invariant") => LessonTag::Invariant,
                _ => LessonTag::Domain,
            };
            // Prompt 6 / D25: when the record does not support a mechanism, the reflector adds
            // no lesson. The EXPERIENCE.md record still exists (situation + counters), with the
            // reason, so the next editor sees that this decision carried no transferable lesson.
            let no_lesson = v.get("lesson").is_some_and(Value::is_null);
            let l = if no_lesson {
                let why = v.get("no_lesson_reason").and_then(Value::as_str).unwrap_or("evidence insufficient").trim();
                format!("No lesson retained: {why}")
            } else {
                v.get("lesson").and_then(Value::as_str).unwrap_or("").trim().to_string()
            };
            if !s.is_empty() && !l.is_empty() {
                return (s, l, if no_lesson { LessonTag::Domain } else { tag }, "reflector");
            }
        }
        eprintln!("rematch: reflector reply unusable for {}; writing a deterministic record", e.id);
    } else if let Err(err) = reply {
        eprintln!("rematch: reflector failed for {}: {err:#}; writing a deterministic record", e.id);
    }
    let class = e.redirect_class.map(redirect::class_name).unwrap_or("error-cluster/takeover");
    let situation = format!("{} ({:?}) was staged for a {class} redirect: {}", e.name, e.kind, e.trigger_line);
    let lesson = match d {
        Decision::Keep => format!(
            "Kept: {} matched sessions with no recurrence of the {class} redirect and no harm event.",
            c.matched
        ),
        _ => match replay {
            Some(r) => format!(
                "Retired before replacing a kept entry: {} matched clean sessions, but one replay of task {} with it in place failed the verifier.",
                c.matched, r.task
            ),
            None => format!(
                "Retired: recurrence {} and harm {} over {} matched sessions; the entry did not prevent the {class} redirect.",
                c.recurrence, c.harm, c.matched
            ),
        },
    };
    let tag = e.lesson_tag.unwrap_or(match e.manifest.as_ref().map(|m| m.scope) {
        Some(EditorScope::Invariant) => LessonTag::Invariant,
        _ => LessonTag::Domain,
    });
    (situation, lesson, tag, "deterministic")
}

// ------------------------------------------------------------------------------------
// reflect

fn pending_sessions(ctx: &AppCtx, only: Option<&str>) -> Result<Vec<String>> {
    let mut v: Vec<String> = std::fs::read_dir(&ctx.paths.pending)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| p.file_stem()?.to_str().map(str::to_string))
        .filter(|s| !s.starts_with('.'))
        .filter(|s| only.is_none_or(|o| o == s))
        .collect();
    v.sort();
    Ok(v)
}

fn pending_path(ctx: &AppCtx, sid: &str) -> PathBuf {
    ctx.paths.pending.join(format!("{sid}.json"))
}

fn done(ctx: &AppCtx, sid: &str, mut out: Value) -> Result<Value> {
    let p = pending_path(ctx, sid);
    if p.exists() {
        std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
    }
    out["session"] = json!(sid);
    Ok(out)
}

/// Lint rule names + summaries the editor sees (DESIGN §12).
pub fn lint_rules() -> Value {
    json!([
        {"rule": "identifiers", "summary": "no session ids, task-specific absolute paths, or user names"},
        {"rule": "string_match_user", "summary": "no branching on the user's literal words"},
        {"rule": "executable_risk", "summary": "no URLs, network calls, curl|sh, or destructive git unless the manifest explains it and approval is requested"},
        {"rule": "trigger", "summary": "non-empty repo glob, ≤10 stems, ≤8 extensions, a trigger line in SKILL.md, non-empty description; kind=agent: AGENT.md frontmatter name (= entry name, not main/sub) and description"},
        {"rule": "scripts", "summary": "every referenced scripts/<path> is shipped; scripts are non-executable until approved"},
        {"rule": "always_on", "summary": "SKILL.md bodies, agent descriptions and AGENTS.fragment.md within 8192 bytes per branch; pay for new text by deletion"},
        {"rule": "fragment", "summary": "AGENTS.fragment.md is non-empty and uses ### or deeper headings (it is appended under `## rematch: <name>`)"},
        {"rule": "fork_path", "summary": "entry.fork_path is a repo-relative file path without `..`"},
        {"rule": "form_order", "summary": "trigger line, then laws, procedure, pitfalls, facts"},
        {"rule": "facts_as_of", "summary": "facts/*.md (and kind=fact SKILL.md) carry as_of"},
        {"rule": "hook", "summary": "hook rule with a message and pass condition; regex compiles and does not match everything"},
        {"rule": "transfer", "summary": "domain lessons and facts never transfer"}
    ])
}

pub fn parse_kind(s: &str) -> Result<Kind> {
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "skill" | "" => Kind::Skill,
        "rule" => Kind::Rule,
        "script" => Kind::Script,
        "agent" => Kind::Agent,
        "hook" => Kind::Hook,
        "fact" => Kind::Fact,
        "lesson" => Kind::Lesson,
        other => bail!("unknown entry kind {other:?}"),
    })
}

fn kebab_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Lint inputs of the local user (login names).
pub fn user_names() -> Vec<String> {
    let mut v: Vec<String> = ["USER", "LOGNAME"].iter().filter_map(|k| std::env::var(k).ok()).filter(|s| s.len() >= 3).collect();
    v.sort();
    v.dedup();
    v
}

/// Everything the editor reply must be turned into before lint/stage.
pub struct Candidate {
    pub entry: Entry,
    pub edits: Vec<Edit>,
    pub dir: tempfile::TempDir,
    pub report: LintReport,
}

pub struct CandidateSpec<'a> {
    pub branch: &'a str,
    pub family: Family,
    pub origin: Origin,
    pub predicate: Predicate,
    pub redirect_class: Option<RedirectClass>,
    pub scope: EditorScope,
    pub fallback_moment: Moment,
    pub status: Status,
    pub sibling_of: Option<String>,
    /// Forced entry name (create/improve: the user named it).
    pub name_override: Option<String>,
}

/// Editor reply → Entry + files in a temp dir → lint.
pub fn build_candidate(ctx: &AppCtx, reply: &EditorReply, spec: &CandidateSpec) -> Result<Candidate> {
    let er = reply.entry.as_ref().ok_or_else(|| anyhow!("EDIT reply without entry"))?;
    let m = reply.manifest.as_ref().ok_or_else(|| anyhow!("EDIT reply without manifest"))?;
    let kind = parse_kind(&er.kind)?;
    let name = spec.name_override.clone().unwrap_or_else(|| er.name.trim().to_string());
    let mut extra: Vec<(String, String)> = Vec::new();
    if library::validate_name(&name).is_err() || !kebab_ok(&name) {
        extra.push(("trigger".into(), format!("entry name {name:?} must be kebab-case [a-z0-9-], ≤ 64 chars")));
    }
    let safe_name = if extra.is_empty() { name.clone() } else { rematch_core::package::slug(&name) };
    let safe_name = if safe_name.is_empty() { "unnamed-entry".to_string() } else { safe_name };
    let mut predicate = spec.predicate.clone();
    if predicate.stems.is_empty() {
        let mut s: Vec<String> = er.stems.iter().flat_map(|x| stems_of_text(x)).collect();
        s.dedup();
        s.truncate(MAX_PREDICATE_STEMS);
        predicate.stems = s;
    }
    let trigger_line = if er.trigger_line.trim().is_empty() { er.description.trim().to_string() } else { er.trigger_line.trim().to_string() };
    let status_dir = match spec.status {
        Status::Draft => ctx.paths.drafts.join(&safe_name),
        _ => ctx.paths.staging.join(spec.branch).join(&safe_name),
    };
    let mut entry = new_entry(
        NewEntry {
            kind,
            name: safe_name,
            branch: spec.branch.to_string(),
            status: spec.status,
            family: spec.family.clone(),
            origin: spec.origin.clone(),
            predicate,
            trigger_line,
            path: status_dir,
        },
        ctx.now,
    )?;
    let evidence = if m.evidence.session_id.is_empty() { spec.fallback_moment.clone() } else { m.evidence.clone() };
    entry.evidence = vec![evidence.clone()];
    entry.redirect_class = spec.redirect_class;
    entry.hook = er.hook.clone();
    entry.fork_path = er.fork_path.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    entry.sibling_of = spec.sibling_of.clone();
    entry.lesson_tag = Some(match spec.scope {
        EditorScope::Invariant => LessonTag::Invariant,
        EditorScope::Domain => LessonTag::Domain,
    });
    entry.manifest = Some(Manifest {
        evidence,
        predicted_fix: m.predicted_fix.clone(),
        predicted_regression: m.predicted_regression.clone(),
        rationale: m.rationale.clone(),
        scope: spec.scope,
    });

    // The name omp loads is the entry name; keep a description (stage keeps both). An
    // agent's document is AGENT.md; every other kind is a skill with SKILL.md.
    let primary = library::primary_doc(kind);
    let mut edits = Vec::new();
    for (path, content) in &er.files {
        let mut content = content.clone();
        if path == primary {
            if let Ok(mut doc) = rematch_core::package::SkillDoc::parse(&content) {
                if doc.get_str("name") != Some(entry.name.as_str()) {
                    doc.set("name", entry.name.as_str());
                }
                if doc.get_str("description").is_none_or(|d| d.trim().is_empty()) {
                    let d = if er.description.trim().is_empty() { entry.trigger_line.as_str() } else { er.description.trim() };
                    doc.set("description", d);
                }
                content = doc.render();
            }
        }
        edits.push(Edit::Write { path: path.clone(), content });
    }
    // A missing SKILL.md is synthesized from the trigger line; a missing AGENT.md (the
    // subagent's system prompt) cannot be, and lint reports it.
    if kind != Kind::Agent && !er.files.contains_key(primary) {
        let desc = if er.description.trim().is_empty() { entry.trigger_line.clone() } else { er.description.trim().to_string() };
        let body = format!(
            "---\nname: {}\ndescription: {}\n---\n\n# {}\n\n{}\n",
            entry.name,
            serde_json::to_string(&desc)?,
            entry.name,
            entry.trigger_line
        );
        edits.push(Edit::Write { path: "SKILL.md".into(), content: body });
    }

    let dir = tempfile::tempdir()?;
    for ed in &edits {
        if let Edit::Write { path, content } = ed {
            let rel = Path::new(path);
            if rel.is_absolute() || rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
                extra.push(("identifiers".into(), format!("file path {path:?} must be relative without traversal")));
                continue;
            }
            let p = dir.path().join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&p, content)?;
        }
    }
    if entry.kind == Kind::Hook {
        match &entry.hook {
            Some(h) => std::fs::write(dir.path().join("hook.json"), serde_json::to_string_pretty(h)?)?,
            None => extra.push(("hook".into(), "kind=hook needs a hook rule".into())),
        }
    }
    let cfg = LintConfig::load(&ctx.improver_file("lint.toml"))?;
    let lctx = LintContext {
        branch_other_bytes: lint::branch_always_on_bytes(&ctx.paths.staging.join(spec.branch), &entry.path),
        user_names: user_names(),
        transfer: false,
    };
    let mut report = lint::lint_entry(dir.path(), &entry, &cfg, &lctx)?;
    for (rule, message) in extra {
        report.findings.push(rematch_core::types::LintFinding { rule, severity: Severity::Error, message, path: None });
    }
    report.ok = !report.findings.iter().any(|f| f.severity == Severity::Error);
    Ok(Candidate { entry, edits, dir, report })
}

/// Editor call → parse → candidate, with one lint retry (each call charged to `budget`).
pub enum Proposed {
    KeepBase(EditorReply),
    Candidate(Candidate, EditorReply, u32 /* attempts */),
    LintFailed(Candidate, EditorReply),
}

pub fn propose(
    ctx: &AppCtx,
    editor_cmd: Option<&str>,
    input: &Value,
    spec: &CandidateSpec,
    budget: &mut CallBudget,
) -> Result<Proposed> {
    budget.take("editor")?;
    let reply = editor::parse_editor_reply(&editor::call_editor(ctx, editor_cmd, input)?)?;
    if reply.decision == EditorDecision::KeepBase {
        return Ok(Proposed::KeepBase(reply));
    }
    let cand = build_candidate(ctx, &reply, spec)?;
    if cand.report.ok {
        return Ok(Proposed::Candidate(cand, reply, 1));
    }
    if budget.used >= budget.max {
        return Ok(Proposed::LintFailed(cand, reply));
    }
    let errors: Vec<_> = cand.report.findings.iter().filter(|f| f.severity == Severity::Error).cloned().collect();
    let mut retry_input = input.clone();
    retry_input["retry_findings"] = serde_json::to_value(&errors)?;
    retry_input["previous_reply"] = serde_json::to_value(&reply)?;
    budget.take("lint retry")?;
    let reply2 = match editor::call_editor(ctx, editor_cmd, &retry_input).and_then(|t| editor::parse_editor_reply(&t)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rematch: lint retry failed: {e:#}");
            return Ok(Proposed::LintFailed(cand, reply));
        }
    };
    if reply2.decision == EditorDecision::KeepBase {
        return Ok(Proposed::KeepBase(reply2));
    }
    let cand2 = build_candidate(ctx, &reply2, spec)?;
    if cand2.report.ok {
        Ok(Proposed::Candidate(cand2, reply2, 2))
    } else {
        Ok(Proposed::LintFailed(cand2, reply2))
    }
}

fn proposal_json(c: &Candidate, reply: &EditorReply) -> Result<Value> {
    let p = Proposal {
        edits: c.edits.clone(),
        manifest: c.entry.manifest.clone().ok_or_else(|| anyhow!("candidate without manifest"))?,
        cites: Vec::new(),
    };
    Ok(json!({"decision": reply.decision, "proposal": p, "entry": c.entry, "lint": c.report}))
}

/// Existing entries of `branch` (provisional + kept) with their content, for dedup.
pub fn branch_contents(ctx: &AppCtx, branch: &str) -> Result<Vec<(String, dedup::EntryContent)>> {
    let mut v = Vec::new();
    for st in [Status::Provisional, Status::Kept, Status::Pinned] {
        for e in ctx.ledger.list_entries(&EntryFilter { status: Some(st), branch: Some(branch.into()), ..Default::default() })? {
            if let Ok(c) = dedup::entry_content(&e.path) {
                v.push((e.id, c));
            }
        }
    }
    Ok(v)
}

/// Append evidence to an existing entry (dedup route / merge).
pub fn append_evidence(ctx: &AppCtx, id: &str, m: &Moment) -> Result<Entry> {
    let mut e = ctx.ledger.get_entry(id)?.ok_or_else(|| anyhow!("no entry {id}"))?;
    if !e.evidence.contains(m) {
        e.evidence.push(m.clone());
    }
    e.updated_at = ctx.now;
    ctx.ledger.upsert_entry(&e)?;
    let ej = e.path.join(REMATCH_JSON);
    if ej.is_file() && owns_dir(ctx, &e) {
        let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&ej)?)?;
        v["evidence"] = serde_json::to_value(&e.evidence)?;
        v["updated_at"] = json!(e.updated_at);
        std::fs::write(&ej, serde_json::to_string_pretty(&v)?)?;
    }
    Ok(e)
}

fn deliverable_before(records: &[TraceRecord], seq: u32) -> String {
    records
        .iter()
        .rev()
        .filter(|r| r.seq() < seq)
        .find_map(|r| match r {
            TraceRecord::TurnEnd { text_head, .. } if !text_head.trim().is_empty() => Some(text_head.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Kept/provisional entry whose predicate matches the session (count-time rule).
pub fn current_entry_for(ctx: &AppCtx, s: &SessionRecord, branch: &str) -> Result<Option<Entry>> {
    let place = s.repo.clone().unwrap_or_else(|| s.cwd.clone());
    for st in [Status::Provisional, Status::Kept, Status::Pinned] {
        for e in ctx.ledger.list_entries(&EntryFilter { status: Some(st), ..Default::default() })? {
            if e.kind == Kind::Lesson {
                continue;
            }
            if st == Status::Provisional && e.branch != branch {
                continue;
            }
            if predicate::count_match(&e.predicate, &place, &s.exts, &s.stems) {
                return Ok(Some(e));
            }
        }
    }
    Ok(None)
}

pub fn library_for_editor(ctx: &AppCtx, place: &str, stems: &[String]) -> Result<Value> {
    let xs = experience::read_matching(&ctx.paths, &ExperienceQuery { repo_or_cwd: Some(place.to_string()), stems: stems.to_vec() })?;
    let mut out = Vec::new();
    for x in xs.into_iter().take(5) {
        let trigger = x.entry_id.as_deref().and_then(|id| ctx.ledger.get_entry(id).ok().flatten()).map(|e| e.trigger_line).unwrap_or_default();
        out.push(json!({"name": x.name, "trigger_line": trigger, "experience": format!("{}\n{}", x.situation, x.lesson), "tag": x.tag}));
    }
    Ok(Value::Array(out))
}

fn read_pending(ctx: &AppCtx, sid: &str) -> Result<Value> {
    let p = pending_path(ctx, sid);
    Ok(serde_json::from_str(&std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?).unwrap_or(json!({})))
}

fn bump_attempts(ctx: &AppCtx, sid: &str, mut pending: Value, err: &str) -> Result<u64> {
    let n = pending.get("attempts").and_then(Value::as_u64).unwrap_or(0) + 1;
    pending["attempts"] = json!(n);
    pending["last_error"] = json!(err);
    std::fs::write(pending_path(ctx, sid), serde_json::to_string(&pending)?)?;
    Ok(n)
}

/// One reflection for `pending/<sid>.json`.
pub fn reflect_one(ctx: &AppCtx, sid: &str, editor_cmd: Option<&str>, classifier_used: bool) -> Result<Value> {
    let pending = read_pending(ctx, sid)?;
    let trace_path = pending
        .get("trace")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .unwrap_or_else(|| ctx.paths.traces.join(format!("{sid}.jsonl")));
    if !trace_path.is_file() {
        return done(ctx, sid, json!({"outcome": "skipped", "reason": "no_trace"}));
    }
    let records = trace::read_trace(&trace_path)?;
    let session = match ctx.ledger.get_session(sid)? {
        // `count` already folded this trace into the row.
        Some(s) if s.source == "trace" || s.counted => s,
        existing => {
            let s = fold_for_ledger(&records, sid, existing);
            ctx.ledger.upsert_session(&s)?;
            s
        }
    };
    let mut budget = CallBudget::new(MAX_MODEL_CALLS);
    if classifier_used {
        budget.take("classifier")?;
    }

    let tri = trace::triage(&session, &records);
    let Some(moment) = tri.moment.clone() else {
        return done(ctx, sid, json!({"outcome": "skipped", "reason": "no_moment"}));
    };
    if tri.kind == TriageKind::Content {
        eprintln!("rematch reflect {sid}: content failure at seq {} ({}); logged, no proposal", moment.seq, tri.reason);
        return done(ctx, sid, json!({"outcome": "skipped", "reason": "content", "triage": tri}));
    }
    // SPEC §loop step 1: the signal is a redirect classified correction|preference, a tool-error
    // cluster, a takeover, or an explicit mark. A session whose only redirects the classifier
    // labelled scope|none carries no signal once classified; spend no editor call on it.
    let has_signal = session.error_clusters > 0
        || session.takeovers > 0
        || session.redirects.iter().any(|r| {
            r.prelabel.starts_with("mark:")
                || matches!(r.class, None | Some(RedirectClass::Correction) | Some(RedirectClass::Preference))
        });
    if !has_signal {
        eprintln!("rematch reflect {sid}: redirects classified scope/none and no other signal; no proposal");
        return done(ctx, sid, json!({"outcome": "skipped", "reason": "no_signal"}));
    }

    // Predicate (DESIGN §6).
    let excerpt = trace::excerpt_around(&records, moment.seq, EXCERPT_RADIUS, EXCERPT_RADIUS);
    let window_files: Vec<String> = excerpt
        .iter()
        .filter_map(|r| match r {
            TraceRecord::ToolResult { tool, input, path, .. } => {
                let mut f = trace::tool_input_files(tool, input);
                if let Some(p) = path {
                    f.push(p.clone());
                }
                Some(f)
            }
            _ => None,
        })
        .flatten()
        .collect();
    let mut exts = exts_of(&window_files);
    if exts.is_empty() {
        exts = session.exts.clone();
    }
    // SPEC §Trigger predicates: stems come from the originating turn and the files it touched.
    // The originating turn is the task request whose deliverable was redirected (the last
    // input before the moment); the redirect text contributes at lower weight so its wording
    // ("that's wrong", "every") does not crowd out the task's domain terms.
    let redirect_text: Option<String> = records.iter().find_map(|r| match r {
        TraceRecord::Input { seq, text, .. } if *seq == moment.seq => Some(text.clone()),
        _ => None,
    });
    let task_input: Option<String> = records.iter().rev().find_map(|r| match r {
        TraceRecord::Input { seq, text, prelabel, .. }
            if *seq < moment.seq && redirect::mark_class(prelabel).is_none() =>
        {
            Some(text.clone())
        }
        _ => None,
    });
    let files_for_stems: &[String] = if window_files.is_empty() { &session.files_touched } else { &window_files };
    let mut doc_terms: Vec<String> = Vec::new();
    if let Some(t) = &task_input {
        for _ in 0..2 {
            doc_terms.extend(stems_of_session(std::slice::from_ref(t), files_for_stems));
        }
    }
    if let Some(t) = &redirect_text {
        doc_terms.extend(stems_of_session(std::slice::from_ref(t), &[]));
    }
    if doc_terms.is_empty() {
        let inputs: Vec<String> = records
            .iter()
            .filter_map(|r| match r {
                TraceRecord::Input { text, prelabel, .. } if redirect::mark_class(prelabel).is_none() => Some(text.clone()),
                _ => None,
            })
            .collect();
        doc_terms = stems_of_session(&inputs, &session.files_touched);
    }
    let corpus = crate::import_cmd::stem_corpus(ctx)?;
    let mut ranked = rank_stems(&doc_terms, &corpus);
    ranked.truncate(MAX_PREDICATE_STEMS);
    let pred = predicate::derive(session.repo.as_deref(), &exts, &ranked);
    let hash = predicate::predicate_hash(&pred);
    let cwd = PathBuf::from(&session.cwd);
    let branch = library::branch_for(session.repo.as_deref().map(Path::new), &cwd);

    let mut needs_merge = false;
    if let Err(limit) = rate::can_admit(&ctx.ledger, &hash, &branch, ctx.now) {
        match limit {
            RateLimit::NeedsMerge { .. } => needs_merge = true,
            l @ (RateLimit::PendingCanary { .. } | RateLimit::DailyPredicate { .. } | RateLimit::BranchProvisionalCap { .. }) => {
                eprintln!("rematch reflect {sid}: refused: {l}");
                return done(ctx, sid, json!({"outcome": "rate_limited", "reason": l.to_string(), "predicate_hash": hash}));
            }
            other => return Err(anyhow!("rate check: {other}")),
        }
    }

    let place = session.repo.clone().unwrap_or_else(|| session.cwd.clone());
    let red = session.redirects.iter().find(|r| r.seq == moment.seq);
    let red_class = red.and_then(|r| counters::redirect_class_of(r.class, &r.prelabel));
    let current = current_entry_for(ctx, &session, &branch)?;
    let input = json!({
        "scope": tri.scope,
        "trace_excerpt": excerpt,
        "deliverable": red.map(|r| r.deliverable_head.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| deliverable_before(&records, moment.seq)),
        "redirect": {
            "class": red_class.map(redirect::class_name).unwrap_or(if session.error_cluster_seqs.contains(&moment.seq) { "error_cluster" } else { "none" }),
            "prelabel": red.map(|r| r.prelabel.clone()),
            "text": red.map(|r| r.text_head.clone()).unwrap_or_default(),
        },
        "moment": moment,
        "current_entry": current.as_ref().map(|e| json!({"name": e.name, "status": e.status, "files": entry_files(&e.path, 16 * 1024)})),
        "library": library_for_editor(ctx, &place, &ranked)?,
        "lint": {"rules": lint_rules()},
        "retry_findings": Value::Null,
    });
    let spec = CandidateSpec {
        branch: &branch,
        family: session.family.clone().unwrap_or_else(|| Family(counters::UNKNOWN_FAMILY.into())),
        origin: Origin::Session { session_id: sid.to_string() },
        predicate: pred.clone(),
        redirect_class: redirect::motivating_class(&session),
        scope: tri.scope,
        fallback_moment: moment.clone(),
        status: Status::Provisional,
        sibling_of: None,
        name_override: None,
    };

    let proposed = match propose(ctx, editor_cmd, &input, &spec, &mut budget) {
        Ok(p) => p,
        Err(e) => {
            let n = bump_attempts(ctx, sid, pending, &format!("{e:#}"))?;
            if n >= MAX_ATTEMPTS {
                let _ = ctx.ledger.insert_proposal(None, Some(sid), &json!({"decision": "ERROR", "error": format!("{e:#}")}), false);
                return done(ctx, sid, json!({"outcome": "editor_failed", "reason": format!("{e:#}"), "attempts": n, "model_calls": budget.used}));
            }
            return Ok(json!({"session": sid, "outcome": "editor_failed_retry_later", "reason": format!("{e:#}"), "attempts": n, "model_calls": budget.used}));
        }
    };
    assert!(budget.used <= MAX_MODEL_CALLS, "reflection exceeded the model-call budget");

    match proposed {
        Proposed::KeepBase(reply) => {
            let row = ctx.ledger.insert_proposal(
                current.as_ref().map(|e| e.id.as_str()),
                Some(sid),
                &json!({"decision": reply.decision, "manifest": reply.manifest, "moment": moment}),
                true,
            )?;
            done(ctx, sid, json!({"outcome": "keep_base", "proposal": row.id, "model_calls": budget.used}))
        }
        Proposed::LintFailed(c, reply) => {
            let row = ctx.ledger.insert_proposal(None, Some(sid), &proposal_json(&c, &reply)?, false)?;
            done(ctx, sid, json!({"outcome": "lint_failed", "proposal": row.id, "findings": c.report.findings, "model_calls": budget.used}))
        }
        Proposed::Candidate(c, reply, attempts) => {
            let content = dedup::entry_content(c.dir.path())?;
            let existing = branch_contents(ctx, &branch)?;
            let mut target = dedup::route_duplicate(&content, &existing).cloned();
            if target.is_none() && needs_merge {
                // Kept cap reached: merge into the closest existing entry of the branch.
                target = closest_entry(ctx, &branch, &pred)?;
            }
            let pj = proposal_json(&c, &reply)?;
            if let Some(id) = target {
                let e = append_evidence(ctx, &id, &c.entry.evidence[0])?;
                let row = ctx.ledger.insert_proposal(Some(&id), Some(sid), &pj, true)?;
                return done(
                    ctx,
                    sid,
                    json!({"outcome": "merged", "into": id, "name": e.name, "proposal": row.id, "reason": if needs_merge {"kept cap"} else {"near duplicate"}, "model_calls": budget.used}),
                );
            }
            let staged = library::stage(&ctx.ledger, &ctx.paths, &c.entry, &c.edits)?;
            crate::scope::stale::snapshot_sources(&staged.path)?;
            rate::record_admission(&ctx.ledger, &hash, &staged.id, ctx.now)?;
            let row = ctx.ledger.insert_proposal(Some(&staged.id), Some(sid), &pj, true)?;
            done(
                ctx,
                sid,
                json!({
                    "outcome": "staged", "id": staged.id, "name": staged.name, "kind": staged.kind, "branch": staged.branch,
                    "path": staged.path, "predicate": staged.predicate, "predicate_hash": hash, "proposal": row.id,
                    "editor_attempts": attempts, "model_calls": budget.used, "lint": c.report,
                }),
            )
        }
    }
}

fn closest_entry(ctx: &AppCtx, branch: &str, pred: &Predicate) -> Result<Option<String>> {
    let mut best: Option<(usize, String)> = None;
    for st in [Status::Kept, Status::Provisional] {
        for e in ctx.ledger.list_entries(&EntryFilter { status: Some(st), branch: Some(branch.into()), ..Default::default() })? {
            let overlap = e.predicate.stems.iter().filter(|s| pred.stems.contains(s)).count();
            if best.as_ref().is_none_or(|(b, _)| overlap > *b) {
                best = Some((overlap, e.id));
            }
        }
    }
    Ok(best.map(|(_, id)| id))
}

// ------------------------------------------------------------------------------------
// reflect --history

/// `--since YYYY-MM-DD` → UTC midnight in ms.
fn parse_since(s: &str) -> Result<i64> {
    let d = chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|e| crate::envelope::Usage(format!("--since {s:?}: expected YYYY-MM-DD ({e})")))?;
    Ok(d.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc().timestamp_millis())
}

/// What `reflect --history` did before the reflection loop.
#[derive(Debug, Default)]
struct HistoryStage {
    /// Sessions given a synthesized trace and a pending file this run.
    handed: Vec<String>,
    /// Sessions ruled out before reflection: `{session, outcome: "skipped", reason}`.
    skipped: Vec<Value>,
    /// Eligible sessions left for a later run by `--limit`.
    deferred: usize,
}

impl HistoryStage {
    /// Per-session report (handed sessions carry their reflection's outcome) and a summary line.
    fn report(self, reflections: &[Value]) -> (Value, String) {
        let mut sessions = Vec::new();
        for sid in &self.handed {
            let r = reflections.iter().find(|r| r["session"] == json!(sid));
            let mut row = json!({"session": sid, "outcome": r.and_then(|r| r.get("outcome")).cloned().unwrap_or(json!("pending"))});
            for key in ["reason", "name", "kind", "id"] {
                if let Some(v) = r.and_then(|r| r.get(key)) {
                    row[key] = v.clone();
                }
            }
            sessions.push(row);
        }
        let line = format!(
            "history: {} imported session(s) handed to reflection, {} skipped, {} deferred by --limit",
            self.handed.len(),
            self.skipped.len(),
            self.deferred
        );
        sessions.extend(self.skipped);
        (json!({"handed": self.handed, "deferred": self.deferred, "sessions": sessions}), line)
    }
}

/// `reflect --history`: every imported session that is flagged, has no trace file and no
/// `history_reflected_at` marker, newest first, gets `traces/<sid>.jsonl` synthesized from its
/// harness transcript (`import_cmd::read_transcript`: omp, Claude Code or Codex by id prefix,
/// `mode: "import"`) and `pending/<sid>.json`, so the
/// normal loop (count + reflect, rate limits, ≤ 3 model calls) handles it. At most `limit`
/// pending files per run. Sessions ruled out for good (subagent files, or not flagged by the
/// extension's rule) are marked too; a missing session file is left for a later run.
fn stage_history(ctx: &AppCtx, only: Option<&str>, since: Option<i64>, limit: usize) -> Result<HistoryStage> {
    let mut st = HistoryStage::default();
    let flagged = ctx.ledger.list_sessions(&SessionFilter { flagged: Some(true), since, ..Default::default() })?;
    for s in flagged.iter().rev() {
        if s.source != "import" || s.history_reflected_at.is_some() || only.is_some_and(|o| o != s.id) {
            continue;
        }
        let trace_path = ctx.paths.traces.join(format!("{}.jsonl", s.id));
        if trace_path.exists() {
            continue;
        }
        if st.handed.len() >= limit {
            st.deferred += 1;
            continue;
        }
        let mut skip = |reason: String| st.skipped.push(json!({"session": s.id, "outcome": "skipped", "reason": reason}));
        let transcript = match crate::import_cmd::read_transcript(ctx, s) {
            Ok(Some(t)) => t,
            Ok(None) => {
                skip("no_session_file".into());
                continue;
            }
            Err(e) => {
                skip(format!("unreadable: {e:#}"));
                continue;
            }
        };
        if transcript.subagent {
            // The adapters trace main sessions only.
            ctx.ledger.set_history_reflected(&s.id, ctx.now)?;
            skip("subagent".into());
            continue;
        }
        let records = transcript.records;
        let Some(TraceRecord::SessionShutdown { reasons, flagged: true, .. }) = records.last() else {
            ctx.ledger.set_history_reflected(&s.id, ctx.now)?;
            skip("not_flagged".into());
            continue;
        };
        let mut body = String::new();
        for r in &records {
            body.push_str(&serde_json::to_string(r)?);
            body.push('\n');
        }
        std::fs::create_dir_all(&ctx.paths.traces)?;
        let tmp = trace_path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &trace_path).with_context(|| format!("rename to {}", trace_path.display()))?;
        std::fs::create_dir_all(&ctx.paths.pending)?;
        let pending = json!({"session_id": s.id, "flagged_at": ctx.now, "reasons": reasons, "trace": trace_path, "source": "import"});
        std::fs::write(pending_path(ctx, &s.id), serde_json::to_string(&pending)?)?;
        ctx.ledger.set_history_reflected(&s.id, ctx.now)?;
        st.handed.push(s.id.clone());
    }
    Ok(st)
}

// ------------------------------------------------------------------------------------
// mark / record

pub fn cmd_mark(ctx: &AppCtx, a: MarkArgs) -> Result<Value> {
    refuse_when_off(ctx)?;
    let class = redirect::parse_class(&a.class).filter(|c| *c != RedirectClass::None).ok_or_else(|| anyhow!("bad class {}", a.class))?;
    let label = format!("mark:{}", redirect::class_name(class));
    let mut ledger_updated = false;
    let mut overrode = None;
    // A session marked after it ended may not be in the ledger yet: fold its trace first so
    // the mark lands on a real session record and `find_moment` can see it.
    let existing = match ctx.ledger.get_session(&a.session)? {
        Some(s) => Some(s),
        None => {
            let trace_path = ctx.paths.traces.join(format!("{}.jsonl", a.session));
            if trace_path.is_file() {
                let s = trace::fold_trace(&trace::read_trace(&trace_path)?, &a.session);
                ctx.ledger.upsert_session(&s)?;
                Some(s)
            } else {
                None
            }
        }
    };
    if let Some(mut s) = existing {
        // Override the classifier for the last turn …
        if let Some(last) = s.redirects.iter().filter(|r| redirect::mark_class(&r.prelabel).is_none()).max_by_key(|r| r.seq) {
            ctx.ledger.set_redirect_class(&s.id, last.seq, class, Some(1.0))?;
            overrode = Some(last.seq);
        }
        // … and record the mark itself, anchored to the last user turn in the trace (the
        // turn being overridden) so the moment and the redirect text are real.
        let trace_path = ctx.paths.traces.join(format!("{}.jsonl", a.session));
        let last_input = if trace_path.is_file() {
            trace::read_trace(&trace_path)?.into_iter().rev().find_map(|r| match r {
                TraceRecord::Input { seq, text, after_deliverable, t, .. } => {
                    Some((seq, text, after_deliverable.unwrap_or_default(), t))
                }
                _ => None,
            })
        } else {
            None
        };
        let (seq, text_head, deliverable_head, t) = match last_input {
            Some((seq, text, deliverable, t)) => (seq, head(&text, 512), head(&deliverable, 512), t),
            None => (s.redirects.iter().map(|r| r.seq).max().map_or(0, |m| m + 1), String::new(), String::new(), ctx.now),
        };
        if !s.redirects.iter().any(|r| r.prelabel == label && Some(r.seq) > overrode) {
            s.redirects.retain(|r| r.seq != seq || redirect::mark_class(&r.prelabel).is_some());
            s.redirects.push(Redirect {
                session_id: s.id.clone(),
                seq,
                prelabel: label.clone(),
                class: Some(class),
                confidence: Some(1.0),
                text_head,
                deliverable_head,
                t,
            });
        }
        s.flagged = true;
        ctx.ledger.upsert_session(&s)?;
        ledger_updated = true;
    }
    let p = pending_path(ctx, &a.session);
    let mut pending: Value = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| {
        json!({"session_id": a.session, "flagged_at": ctx.now, "reasons": [], "trace": ctx.paths.traces.join(format!("{}.jsonl", a.session))})
    });
    let reasons = pending["reasons"].as_array_mut();
    if let Some(r) = reasons {
        if !r.iter().any(|x| x.as_str() == Some(label.as_str())) {
            r.push(json!(label));
        }
    } else {
        pending["reasons"] = json!([label]);
    }
    std::fs::write(&p, serde_json::to_string(&pending)?)?;
    Ok(json!({
        "session": a.session, "class": redirect::class_name(class), "ledger_updated": ledger_updated,
        "overrode_seq": overrode, "pending": p,
        "lines": [format!("marked {} for session {}", redirect::class_name(class), a.session)],
    }))
}

pub fn cmd_record(ctx: &AppCtx, a: RecordArgs) -> Result<Value> {
    let ev: Value = serde_json::from_str(&a.event).context("--event must be JSON")?;
    match ev.get("ev").and_then(Value::as_str) {
        Some("mark") => {
            let class = ev.get("class").and_then(Value::as_str).ok_or_else(|| anyhow!("mark event needs class"))?;
            cmd_mark(ctx, MarkArgs { session: a.session, class: class.to_string() })
        }
        other => Err(crate::envelope::Usage(format!(
            "record accepts ev=mark only (got {:?}); every other event belongs in the trace file",
            other
        ))
        .into()),
    }
}
