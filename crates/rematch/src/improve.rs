//! Brownfield `improve <skill> [--split-only]` (SPEC §Starting states): register the
//! existing skill as a provisional entry (files left in place), audit the sessions that
//! read it, refuse below 5 sessions (offer the split), mechanically split a large
//! non-package skill, else propose a unit-level sibling from clustered redirects.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use clap::Args;
use rematch_core::features::{head, rank_stems, MAX_PREDICATE_STEMS};
use rematch_core::ledger::{EntryFilter, SessionFilter};
use rematch_core::library::{self, SHARED_BRANCH};
use rematch_core::lint::{self, LintConfig, LintContext};
use rematch_core::package::{self, SkillDoc, SPLIT_THRESHOLD};
use rematch_core::predicate::{self, MAX_EXTS};
use rematch_core::redirect;
use rematch_core::trace::{self, TraceRecord};
use rematch_core::types::{EditorScope, Entry, Family, Kind, Moment, Origin, RedirectClass, SessionRecord, Status};
use serde_json::{json, Value};

use crate::app::{new_entry, AppCtx, NewEntry};
use crate::editor::CallBudget;
use crate::import_cmd::{ranked_text_stems, stem_corpus, trigger_text};
use crate::lake_read::Tally;
use crate::reflect::{self, CandidateSpec, Proposed};

pub const MIN_AUDIT_SESSIONS: usize = 5;

#[derive(Args, Debug, Clone)]
pub struct ImproveArgs {
    pub skill: String,
    /// Only the mechanical split (no model, no session minimum).
    #[arg(long)]
    pub split_only: bool,
    #[arg(long)]
    pub branch: Option<String>,
    /// Scripted editor command (tests).
    #[arg(long)]
    pub editor: Option<String>,
}

fn plugins_node_modules() -> PathBuf {
    let home = rematch_core::paths::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    std::env::var_os("OMP_PLUGINS_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".omp").join("plugins")).join("node_modules")
}

/// Skill dir by name: agent skills, plugin skills (`node_modules/[@org/]pkg/skills/<name>`),
/// project and user skill roots omp scans (`.omp`, `.agents`, `.claude`, `.codex`, …), then a registered ledger entry.
pub fn locate_skill(ctx: &AppCtx, name: &str) -> Result<Option<PathBuf>> {
    let mut cands = vec![ctx.paths.skills_dir().join(name)];
    let nm = plugins_node_modules();
    for ent in std::fs::read_dir(&nm).into_iter().flatten().flatten() {
        let p = ent.path();
        if ent.file_name().to_string_lossy().starts_with('@') {
            for sub in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                cands.push(sub.path().join("skills").join(name));
            }
        } else {
            cands.push(p.join("skills").join(name));
        }
    }
    let base = rematch_core::import::repo_of(&ctx.cwd).unwrap_or_else(|| ctx.cwd.clone());
    // Project roots omp scans (`omp://skills.md` providers): native, agents, claude, codex,
    // opencode, github; then the user-level ones that load by default.
    for rel in [".omp/skills", ".agents/skills", ".agent/skills", ".claude/skills", ".codex/skills", ".opencode/skills", ".github/skills"] {
        cands.push(base.join(rel).join(name));
    }
    if let Some(home) = rematch_core::paths::home_dir() {
        for rel in [".agents/skills", ".agent/skills", ".claude/skills", ".codex/skills", ".config/opencode/skills"] {
            cands.push(home.join(rel).join(name));
        }
    }
    if let Some(found) = cands.into_iter().find(|d| d.join("SKILL.md").is_file()) {
        return Ok(Some(rematch_core::paths::canonicalize(&found).unwrap_or(found)));
    }
    Ok(ctx
        .ledger
        .list_entries(&EntryFilter { name: Some(name.into()), ..Default::default() })?
        .into_iter()
        .rev()
        .find(|e| e.kind == Kind::Skill && e.path.join("SKILL.md").is_file() && !matches!(e.status, Status::Retired))
        .map(|e| e.path))
}

fn common_repo(sessions: &[SessionRecord]) -> Option<String> {
    let mut repos = sessions.iter().map(|s| s.repo.clone());
    let first = repos.next()??;
    repos.all(|r| r.as_deref() == Some(first.as_str())).then_some(first)
}

/// Register (or reuse) the provisional brownfield entry for the skill at `dir` (also used by
/// `profile adopt`, which then pins it).
pub(crate) fn register(ctx: &AppCtx, name: &str, dir: &Path, sessions: &[SessionRecord], branch: &str) -> Result<(Entry, bool)> {
    for e in ctx.ledger.list_entries(&EntryFilter { name: Some(name.into()), ..Default::default() })? {
        let same_dir = rematch_core::paths::canonicalize(&e.path).map(|p| p == dir).unwrap_or(e.path == dir);
        if same_dir && !matches!(e.status, Status::Retired) {
            return Ok((e, false));
        }
    }
    let md = std::fs::read_to_string(dir.join("SKILL.md"))?;
    let doc = SkillDoc::parse(&md).unwrap_or(SkillDoc { body: md.clone(), ..Default::default() });
    if let Some(b) = doc.rematch() {
        if let Some(e) = ctx.ledger.get_entry(&b.id)? {
            return Ok((e, false));
        }
    }
    let (text, trigger_line) = trigger_text(&doc);
    let corpus = stem_corpus(ctx)?;
    let mut terms = rematch_core::features::stems_of_text(&format!("{} {text}", name.replace('-', " ")));
    terms.extend(terms.clone());
    for s in sessions {
        terms.extend(s.stems.iter().cloned());
    }
    let mut stems = rank_stems(&terms, &corpus);
    stems.truncate(MAX_PREDICATE_STEMS);
    if stems.is_empty() {
        stems = ranked_text_stems(&text, &corpus);
    }
    let mut freq: BTreeMap<&str, usize> = BTreeMap::new();
    for s in sessions {
        for x in &s.exts {
            *freq.entry(x.as_str()).or_default() += 1;
        }
    }
    let mut exts: Vec<(&str, usize)> = freq.into_iter().collect();
    exts.sort_by_key(|x| std::cmp::Reverse(x.1));
    let exts: Vec<String> = exts.into_iter().take(MAX_EXTS).map(|(e, _)| e.to_string()).collect();
    let domain = rematch_core::domain::majority_domain(sessions.iter().map(|s| s.domain.as_deref()));
    let pred = predicate::derive(common_repo(sessions).as_deref(), &exts, &stems, domain.as_deref());
    let mut e = new_entry(
        NewEntry {
            kind: Kind::Skill,
            name: name.to_string(),
            branch: branch.to_string(),
            status: Status::Provisional,
            family: Family(rematch_core::counters::UNKNOWN_FAMILY.into()),
            origin: Origin::Import { source: "brownfield".into() },
            predicate: pred,
            trigger_line,
            path: dir.to_path_buf(),
        },
        ctx.now,
    )?;
    e.status_since = Some(ctx.now);
    e.content_sha256 = Some(rematch_core::library::content_sha256(dir)?);
    ctx.ledger.upsert_entry(&e)?;
    Ok((e, true))
}

/// Cluster groups: (class, key) → (count, examples, `<session>#<seq>` moments).
type Groups = BTreeMap<(String, String), (usize, Vec<String>, Vec<String>)>;

/// Redirect clusters: (class, top stems) → count + examples.
fn clusters(sessions: &[SessionRecord]) -> Vec<Value> {
    let mut groups = Groups::new();
    for s in sessions {
        for r in &s.redirects {
            let class = redirect::effective_class(r).map(redirect::class_name).unwrap_or("unclassified").to_string();
            let stems = rematch_core::features::stems_of_text(&r.text_head);
            let key_stems: Vec<String> = stems.iter().take(2).cloned().collect();
            let g = groups.entry((class, key_stems.join(" "))).or_default();
            g.0 += 1;
            if g.1.len() < 5 {
                g.1.push(head(&r.text_head, 300));
            }
            if g.2.len() < 5 {
                g.2.push(format!("{}#{}", s.id, r.seq));
            }
        }
    }
    sorted_clusters(groups)
}

/// Tool-error clusters (class `error_cluster`) grouped by the failing tools of each cluster's
/// window: count, error heads as examples, `<session>#<seq>` of the result that tipped it.
fn error_clusters(traced: &[(SessionRecord, Vec<TraceRecord>)]) -> Vec<Value> {
    let mut groups = Groups::new();
    for (s, records) in traced {
        let results: Vec<(u32, &str, bool, &str)> = records
            .iter()
            .filter_map(|r| match r {
                TraceRecord::ToolResult { seq, tool, is_error, content_head, .. } => Some((*seq, tool.as_str(), *is_error, content_head.as_str())),
                _ => None,
            })
            .collect();
        for &seq in &s.error_cluster_seqs {
            let Some(end) = results.iter().position(|r| r.0 == seq) else { continue };
            let window = &results[(end + 1).saturating_sub(trace::ERROR_CLUSTER_WINDOW)..=end];
            let errors: Vec<_> = window.iter().filter(|r| r.2).collect();
            let tools: std::collections::BTreeSet<&str> = errors.iter().map(|r| r.1).collect();
            let g = groups.entry(("error_cluster".into(), tools.into_iter().collect::<Vec<_>>().join(" "))).or_default();
            g.0 += 1;
            for e in errors {
                if g.1.len() < 5 {
                    g.1.push(format!("{}: {}", e.1, head(e.3, 300)));
                }
            }
            if g.2.len() < 5 {
                g.2.push(format!("{}#{}", s.id, seq));
            }
        }
    }
    sorted_clusters(groups)
}

fn sorted_clusters(groups: Groups) -> Vec<Value> {
    let mut v: Vec<Value> = groups
        .into_iter()
        .map(|((class, stems), (count, examples, moments))| json!({"class": class, "stems": stems, "count": count, "examples": examples, "moments": moments}))
        .collect();
    v.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
    v
}

/// Sessions each with its trace records.
type Traced = Vec<(SessionRecord, Vec<TraceRecord>)>;

/// Sessions with a signal (redirect or error cluster), each with its trace — the lake's
/// events file, the live trace, or synthesized from the harness transcript for imported
/// sessions — and a row whose seqs address that trace, plus where the traces came from.
/// Sessions whose trace cannot be found are left out.
fn traced_signals(ctx: &AppCtx, sessions: &[SessionRecord]) -> Result<(Traced, Tally)> {
    let mut out = Vec::new();
    let mut tally = Tally::default();
    for s in sessions.iter().filter(|s| !s.redirects.is_empty() || s.error_clusters > 0) {
        if let Some((row, records, src)) = crate::import_cmd::session_with_records(ctx, s)? {
            tally.add(src);
            out.push((row, records));
        }
    }
    Ok((out, tally))
}

/// The editor's moment: the most recent session's earliest redirect the classifier did not
/// dismiss; else the first cluster of the session with the most error clusters (then tool
/// errors, then recency).
fn pick_moment(traced: &[(SessionRecord, Vec<TraceRecord>)]) -> Option<(Moment, &[TraceRecord])> {
    let redirect = traced.iter().rev().find_map(|(s, records)| {
        s.redirects
            .iter()
            .filter(|r| redirect::mark_class(&r.prelabel).is_some() || r.class != Some(RedirectClass::None))
            .map(|r| r.seq)
            .min()
            .map(|seq| (Moment { session_id: s.id.clone(), seq }, records.as_slice()))
    });
    redirect.or_else(|| {
        traced
            .iter()
            .enumerate()
            .filter(|(_, (s, _))| !s.error_cluster_seqs.is_empty())
            .max_by_key(|(i, (s, _))| (s.error_clusters, s.tool_errors, *i))
            .map(|(_, (s, records))| (Moment { session_id: s.id.clone(), seq: s.error_cluster_seqs[0] }, records.as_slice()))
    })
}

pub fn cmd_improve(ctx: &AppCtx, a: ImproveArgs) -> Result<Value> {
    if ctx.paths.is_off() {
        bail!("rematch is off; `rematch on` to enable");
    }
    library::validate_name(&a.skill)?;
    let dir = locate_skill(ctx, &a.skill)?.ok_or_else(|| anyhow!("skill {:?} not found (agent skills, plugin skills, .omp/skills, ledger)", a.skill))?;
    let sessions = ctx.ledger.list_sessions(&SessionFilter { skill_read: Some(a.skill.clone()), ..Default::default() })?;
    let traced = sessions.iter().filter(|s| s.source == "trace").count();
    let imported = sessions.len() - traced;
    let branch = match &a.branch {
        Some(b) => b.clone(),
        None => match common_repo(&sessions) {
            Some(r) => library::branch_for(Some(Path::new(&r)), Path::new(&r)),
            None => SHARED_BRANCH.to_string(),
        },
    };
    library::validate_name(&branch)?;
    let (base, registered) = register(ctx, &a.skill, &dir, &sessions, &branch)?;
    if base.pinned || base.status == Status::Pinned {
        bail!("{} is pinned (human-owned); not proposing a replacement", a.skill);
    }
    // Rate limit (SPEC): no new proposal on a trigger while its canary is pending. A sibling
    // of this skill already staged provisional (a split or an earlier unit-level version) is
    // that canary; the counters decide it before another version may be proposed.
    let sibling_dir = ctx.paths.staging.join(&branch).join(&a.skill);
    if sibling_dir.join(library::REMATCH_JSON).is_file() {
        if let Ok(sib) = library::load_entry(&sibling_dir) {
            if sib.id != base.id && sib.status == Status::Provisional {
                let c = ctx.ledger.counters(&sib.id)?;
                return Ok(json!({
                    "skill": a.skill, "entry": base.id, "branch": branch, "refused": true,
                    "pending_canary": {"id": sib.id, "version": sib.version, "note": sib.status_note, "path": sibling_dir,
                        "matched": c.matched, "recurrence": c.recurrence, "harm": c.harm},
                    "lines": [format!(
                        "{}: a provisional version (v{}, {}) is already pending its canary at {} — matched {} · recurrence {} · harm {}; the counters decide it before a new version is proposed (`rematch library disable <id>` to withdraw it)",
                        a.skill, sib.version, sib.status_note.as_deref().unwrap_or("sibling"), sibling_dir.display(), c.matched, c.recurrence, c.harm
                    )],
                }));
            }
        }
    }
    let audit = json!({
        "sessions": sessions.len(), "traced": traced, "imported": imported,
        "redirects": sessions.iter().map(|s| s.redirects.len()).sum::<usize>(),
        "tool_errors": sessions.iter().map(|s| s.tool_errors).sum::<u32>(),
        "error_clusters": sessions.iter().map(|s| s.error_clusters).sum::<u32>(),
        "session_ids": sessions.iter().map(|s| &s.id).collect::<Vec<_>>(),
    });
    let md = std::fs::read_to_string(dir.join("SKILL.md"))?;
    let large = md.len() > SPLIT_THRESHOLD && !package::is_package(&dir);
    let mut out = json!({"skill": a.skill, "path": dir, "entry": base.id, "registered": registered, "branch": branch, "audit": audit, "bytes": md.len()});

    if !a.split_only && sessions.len() < MIN_AUDIT_SESSIONS {
        out["refused"] = json!(true);
        out["traced_sessions"] = json!(sessions.len());
        out["offer"] = json!(if large { "split" } else { "none: SKILL.md is within 8 KB" });
        out["lines"] = json!([format!(
            "{}: {} session(s) used it ({} traced, {} imported); {MIN_AUDIT_SESSIONS} needed for a unit-level proposal{}",
            a.skill, sessions.len(), traced, imported,
            if large { "; `rematch improve --split-only` offers the mechanical split" } else { "" }
        )]);
        return Ok(out);
    }
    if a.split_only || large {
        return split(ctx, &a, &base, &dir, &md, &branch, out);
    }
    propose_unit_level(ctx, &a, &base, &dir, &sessions, &branch, out)
}

fn sibling_entry(ctx: &AppCtx, base: &Entry, branch: &str, kind_note: &str) -> Result<Entry> {
    let mut e = new_entry(
        NewEntry {
            kind: Kind::Skill,
            name: base.name.clone(),
            branch: branch.to_string(),
            status: Status::Provisional,
            family: base.family.clone(),
            origin: Origin::Import { source: "brownfield".into() },
            predicate: base.predicate.clone(),
            trigger_line: base.trigger_line.clone(),
            path: ctx.paths.staging.join(branch).join(&base.name),
        },
        ctx.now,
    )?;
    e.sibling_of = Some(base.id.clone());
    e.version = base.version + 1;
    e.status_note = Some(kind_note.to_string());
    Ok(e)
}

/// Every text file of the original skill directory (the text that already loads today).
fn original_texts(dir: &Path) -> Result<Vec<String>> {
    fn walk(cur: &Path, out: &mut Vec<String>) -> Result<()> {
        for ent in std::fs::read_dir(cur)?.flatten() {
            let p = ent.path();
            if p.is_dir() {
                walk(&p, out)?;
            } else if let Ok(s) = std::fs::read_to_string(&p) {
                out.push(s);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, &mut out)?;
    Ok(out)
}

/// Downgrade text findings (`identifiers`, `string_match_user`, `executable_risk`) whose quoted
/// snippet already occurs in the original skill to warnings; recompute `ok`.
fn inherit_findings(mut report: rematch_core::types::LintReport, originals: &[String]) -> rematch_core::types::LintReport {
    use rematch_core::types::Severity;
    for f in &mut report.findings {
        if f.severity != Severity::Error || !matches!(f.rule.as_str(), "identifiers" | "string_match_user" | "executable_risk") {
            continue;
        }
        let Some(snippet) = f.message.split('`').nth(1).map(str::trim).filter(|s| s.len() >= 3) else {
            continue;
        };
        if originals.iter().any(|o| o.contains(snippet)) {
            f.severity = Severity::Warn;
            f.message = format!("{} (inherited from the original skill; not introduced by this change)", f.message);
        }
    }
    report.ok = report.findings.iter().all(|f| f.severity != Severity::Error);
    report
}

fn split(ctx: &AppCtx, a: &ImproveArgs, base: &Entry, dir: &Path, md: &str, branch: &str, mut out: Value) -> Result<Value> {
    let edits = package::mechanical_split(md, &a.skill)?;
    if edits.is_empty() {
        out["split"] = json!(false);
        out["lines"] = json!([format!("{} is {} bytes (≤ 8 KB) or already a package: nothing to split", a.skill, md.len())]);
        return Ok(out);
    }
    let mut entry = sibling_entry(ctx, base, branch, "mechanical split")?;
    // Carry the rest of the package (scripts, facts…) beside the split files.
    let mut all = edits.clone();
    copy_other_files(dir, &mut all)?;
    let tmp = tempfile::tempdir()?;
    for ed in &all {
        if let rematch_core::types::Edit::Write { path, content } = ed {
            let p = tmp.path().join(path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(p, content)?;
        }
    }
    let cfg = LintConfig::load(&ctx.improver_file("lint.toml"))?;
    let report = lint::lint_entry(
        tmp.path(),
        &entry,
        &cfg,
        &LintContext {
            branch_other_bytes: lint::branch_always_on_bytes(&ctx.paths.staging.join(branch), &entry.path),
            user_names: reflect::user_names(),
            transfer: false,
        },
    )?;
    // A mechanical split adds no text. Findings about text the skill already carried (and
    // which already loads today) are reported as warnings, not as a gate: the gate is for new
    // text. Everything the split itself introduces is still held to the full rules.
    let report = inherit_findings(report, &original_texts(dir)?);
    if !report.ok {
        let row = ctx.ledger.insert_proposal(Some(&base.id), None, &json!({"decision": "SPLIT", "edits": all, "lint": report}), false)?;
        out["split"] = json!(false);
        out["lint"] = json!(report);
        out["proposal"] = json!(row.id);
        out["lines"] = json!([format!("mechanical split of {} failed lint ({} finding(s)); not staged", a.skill, report.findings.len())]);
        return Ok(out);
    }
    entry.evidence = Vec::new();
    let staged = library::stage(&ctx.ledger, &ctx.paths, &entry, &all)?;
    crate::scope::stale::snapshot_sources(&staged.path)?;
    let row = ctx.ledger.insert_proposal(Some(&staged.id), None, &json!({"decision": "SPLIT", "edits": all, "lint": report}), true)?;
    out["split"] = json!(true);
    out["staged"] = json!({"id": staged.id, "path": staged.path, "sibling_of": base.id, "files": all.len()});
    out["proposal"] = json!(row.id);
    out["lines"] = json!([format!("split {} into a {}-file package staged beside it at {}", a.skill, all.len(), staged.path.display())]);
    Ok(out)
}

/// Files of the original package other than SKILL.md that the split did not produce.
fn copy_other_files(dir: &Path, edits: &mut Vec<rematch_core::types::Edit>) -> Result<()> {
    let produced: Vec<String> = edits
        .iter()
        .filter_map(|e| match e {
            rematch_core::types::Edit::Write { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d)?.flatten() {
            let p = ent.path();
            let rel = p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            if rel.starts_with('.') || rel == "rematch.json" || rel == "EXPERIENCE.md" || rel == "SKILL.md" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if !produced.contains(&rel) {
                if let Ok(content) = std::fs::read_to_string(&p) {
                    edits.push(rematch_core::types::Edit::Write { path: rel, content });
                }
            }
        }
    }
    Ok(())
}

fn propose_unit_level(
    ctx: &AppCtx,
    a: &ImproveArgs,
    base: &Entry,
    dir: &Path,
    sessions: &[SessionRecord],
    branch: &str,
    mut out: Value,
) -> Result<Value> {
    // The lake's events, live traces, or traces synthesized from harness transcripts for
    // imported sessions: the excerpt and clusters see imported history too, with seqs that
    // address those records.
    let (traced, tally) = traced_signals(ctx, sessions)?;
    out["source"] = json!(tally.source());
    out["sources"] = tally.sources();
    let rows: Vec<SessionRecord> = sessions
        .iter()
        .map(|s| traced.iter().find(|(t, _)| t.id == s.id).map_or_else(|| s.clone(), |(t, _)| t.clone()))
        .collect();
    let redirect_cl = clusters(&rows);
    let error_cl = error_clusters(&traced);
    let errors: u32 = rows.iter().map(|s| s.error_clusters).sum();
    let process = redirect_cl.iter().filter(|c| c["class"] == "correction").map(|c| c["count"].as_u64().unwrap_or(0)).sum::<u64>() + errors as u64
        >= redirect_cl.iter().filter(|c| c["class"] == "preference" || c["class"] == "scope").map(|c| c["count"].as_u64().unwrap_or(0)).sum::<u64>();
    let scope = if process { EditorScope::Invariant } else { EditorScope::Domain };
    let (excerpt, moment) = match pick_moment(&traced) {
        Some((m, records)) => (trace::excerpt_around(records, m.seq, reflect::EXCERPT_RADIUS, reflect::EXCERPT_RADIUS), m),
        None => (Vec::new(), Moment { session_id: sessions.last().map(|s| s.id.clone()).unwrap_or_default(), seq: 0 }),
    };
    let top = redirect_cl.first().or(error_cl.first()).cloned().unwrap_or(json!({"class": "none", "examples": []}));
    let mut cl = redirect_cl;
    cl.extend(error_cl);
    cl.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
    let input = json!({
        "mode": "improve_unit_level",
        "scope": scope,
        "trace_excerpt": excerpt,
        "deliverable": "",
        "redirect": {"class": top["class"], "text": top["examples"].get(0).cloned().unwrap_or(json!(""))},
        "clusters": cl,
        "audit": out["audit"],
        "current_entry": {"name": a.skill, "files": collect_files(dir)},
        "library": reflect::library_for_editor(ctx, base.predicate.repo_glob.as_str(), &base.predicate.stems)?,
        "lint": {"rules": reflect::lint_rules()},
        "retry_findings": Value::Null,
    });
    let spec = CandidateSpec {
        branch,
        family: base.family.clone(),
        origin: Origin::Import { source: "brownfield".into() },
        predicate: base.predicate.clone(),
        redirect_class: None,
        scope,
        fallback_moment: moment,
        status: Status::Provisional,
        sibling_of: Some(base.id.clone()),
        name_override: Some(a.skill.clone()),
    };
    let mut budget = CallBudget::new(2);
    match reflect::propose(ctx, a.editor.as_deref(), &input, &spec, &mut budget)? {
        Proposed::KeepBase(r) => {
            let row = ctx.ledger.insert_proposal(Some(&base.id), None, &json!({"decision": r.decision, "manifest": r.manifest}), true)?;
            out["outcome"] = json!("keep_base");
            out["proposal"] = json!(row.id);
            out["lines"] = json!([format!("editor kept {} as is (KEEP_BASE)", a.skill)]);
        }
        Proposed::LintFailed(c, r) => {
            let row = ctx.ledger.insert_proposal(Some(&base.id), None, &json!({"decision": r.decision, "entry": c.entry, "lint": c.report}), false)?;
            out["outcome"] = json!("lint_failed");
            out["proposal"] = json!(row.id);
            out["findings"] = json!(c.report.findings);
            out["lines"] = json!([format!("proposal for {} failed lint after one retry", a.skill)]);
        }
        Proposed::Candidate(mut c, r, n) => {
            c.entry.version = base.version + 1;
            let staged = library::stage(&ctx.ledger, &ctx.paths, &c.entry, &c.edits)?;
            crate::scope::stale::snapshot_sources(&staged.path)?;
            let row = ctx.ledger.insert_proposal(Some(&staged.id), None, &json!({"decision": r.decision, "entry": staged, "edits": c.edits, "lint": c.report}), true)?;
            out["outcome"] = json!("staged");
            out["staged"] = json!({"id": staged.id, "path": staged.path, "sibling_of": base.id});
            out["proposal"] = json!(row.id);
            out["editor_attempts"] = json!(n);
            out["lines"] = json!([format!("staged a sibling of {} at {} (the original stays until counters decide)", a.skill, staged.path.display())]);
        }
    }
    out["model_calls"] = json!(budget.used);
    Ok(out)
}

fn collect_files(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = ent.path();
            let rel = p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            if rel.starts_with('.') || rel == "rematch.json" || rel == "EXPERIENCE.md" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(t) = std::fs::read_to_string(&p) {
                out.insert(rel, head(&t, 24 * 1024));
            }
        }
    }
    out
}
