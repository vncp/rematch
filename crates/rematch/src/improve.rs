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
use rematch_core::trace;
use rematch_core::types::{EditorScope, Entry, Family, Kind, Moment, Origin, SessionRecord, Status};
use serde_json::{json, Value};

use crate::app::{new_entry, AppCtx, NewEntry};
use crate::editor::CallBudget;
use crate::import_cmd::{ranked_text_stems, stem_corpus, trigger_text};
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
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    std::env::var_os("OMP_PLUGINS_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".omp").join("plugins")).join("node_modules")
}

/// Skill dir by name: agent skills, plugin skills (`node_modules/[@org/]pkg/skills/<name>`),
/// project `.omp/skills`, then a registered ledger entry.
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
    cands.push(base.join(".omp").join("skills").join(name));
    if let Some(found) = cands.into_iter().find(|d| d.join("SKILL.md").is_file()) {
        return Ok(Some(std::fs::canonicalize(&found).unwrap_or(found)));
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

/// Register (or reuse) the provisional brownfield entry for the skill at `dir`.
fn register(ctx: &AppCtx, name: &str, dir: &Path, sessions: &[SessionRecord], branch: &str) -> Result<(Entry, bool)> {
    for e in ctx.ledger.list_entries(&EntryFilter { name: Some(name.into()), ..Default::default() })? {
        let same_dir = std::fs::canonicalize(&e.path).map(|p| p == dir).unwrap_or(e.path == dir);
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
    let pred = predicate::derive(common_repo(sessions).as_deref(), &exts, &stems);
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
    ctx.ledger.upsert_entry(&e)?;
    Ok((e, true))
}

/// Redirect clusters: (class, top stems) → count + examples.
fn clusters(sessions: &[SessionRecord]) -> Vec<Value> {
    type Group = (usize, Vec<String>, Vec<String>); // count, examples, moments
    let mut groups: BTreeMap<(String, String), Group> = BTreeMap::new();
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
    let mut v: Vec<Value> = groups
        .into_iter()
        .map(|((class, stems), (count, examples, moments))| json!({"class": class, "stems": stems, "count": count, "examples": examples, "moments": moments}))
        .collect();
    v.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
    v
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
    let cl = clusters(sessions);
    let errors: u32 = sessions.iter().map(|s| s.error_clusters).sum();
    let process = cl.iter().filter(|c| c["class"] == "correction").map(|c| c["count"].as_u64().unwrap_or(0)).sum::<u64>() + errors as u64
        >= cl.iter().filter(|c| c["class"] == "preference" || c["class"] == "scope").map(|c| c["count"].as_u64().unwrap_or(0)).sum::<u64>();
    let scope = if process { EditorScope::Invariant } else { EditorScope::Domain };
    // Most recent traced session with a moment gives the excerpt.
    let mut excerpt = Vec::new();
    let mut moment = Moment { session_id: sessions.last().map(|s| s.id.clone()).unwrap_or_default(), seq: 0 };
    for s in sessions.iter().rev() {
        let tp = ctx.paths.traces.join(format!("{}.jsonl", s.id));
        if let (true, Some(m)) = (tp.is_file(), trace::find_moment(s)) {
            excerpt = trace::excerpt_around(&trace::read_trace(&tp)?, m.seq, reflect::EXCERPT_RADIUS, reflect::EXCERPT_RADIUS);
            moment = m;
            break;
        }
    }
    let top = cl.first().cloned().unwrap_or(json!({"class": "none", "examples": []}));
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
