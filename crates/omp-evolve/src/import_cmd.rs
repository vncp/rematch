//! `import --history [--sessions <dir>]` (omp session JSONL → ledger) and
//! `import --forge <path>` (compound-forge skills → provisional brownfield entries,
//! learnings → provisional lessons). Both are idempotent.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Args;
use evolve_core::features::{rank_stems, stems_of_text, MAX_PREDICATE_STEMS};
use evolve_core::import::{extract_outcomes, read_session, scan_sessions_dir, session_record};
use evolve_core::ledger::{EntryFilter, SessionFilter};
use evolve_core::library::{validate_name, EVOLVE_JSON};
use evolve_core::outcome::{minhash, outcome_tokens, MINHASH_K};
use evolve_core::package::{slug, SkillDoc};
use evolve_core::predicate;
use evolve_core::types::{Entry, Family, Kind, Origin, Status};
use serde_json::{json, Value};

use crate::app::{new_entry, AppCtx, NewEntry};

pub const FORGE_BRANCH: &str = "forge";

#[derive(Args, Debug, Clone)]
pub struct ImportArgs {
    /// Import omp session history into the ledger.
    #[arg(long)]
    pub history: bool,
    /// Sessions directory (default: `<agent dir>/sessions`).
    #[arg(long, value_name = "DIR")]
    pub sessions: Option<PathBuf>,
    /// compound-forge checkout: skills/* → provisional brownfield entries, learnings/*.md → lessons.
    #[arg(long, value_name = "PATH")]
    pub forge: Option<PathBuf>,
}

pub fn cmd_import(ctx: &AppCtx, a: ImportArgs) -> Result<Value> {
    if !a.history && a.forge.is_none() {
        return Err(crate::envelope::Usage("import needs --history and/or --forge <path>".into()).into());
    }
    let mut out = json!({});
    let mut lines = Vec::new();
    if a.history {
        let dir = a.sessions.clone().unwrap_or_else(|| ctx.paths.agent_dir.join("sessions"));
        let h = import_history(ctx, &dir)?;
        lines.push(format!(
            "history: {} files, {} new, {} updated, {} kept from traces, {} failed; {} sessions with skill reads, {} redirects, {} outcomes",
            h["files"], h["new"], h["updated"], h["skipped_traced"], h["failed"], h["with_skill_reads"], h["redirects"], h["outcomes"]
        ));
        out["history"] = h;
    }
    if let Some(forge) = &a.forge {
        let f = import_forge(ctx, forge)?;
        lines.push(format!(
            "forge: {} skills registered ({} existing), {} lessons registered ({} existing)",
            f["skills_registered"], f["skills_existing"], f["lessons_registered"], f["lessons_existing"]
        ));
        out["forge"] = f;
    }
    out["lines"] = json!(lines);
    Ok(out)
}

pub fn import_history(ctx: &AppCtx, dir: &Path) -> Result<Value> {
    if !dir.is_dir() {
        bail!("sessions dir {} does not exist", dir.display());
    }
    let files = scan_sessions_dir(dir)?;
    let (mut new, mut updated, mut skipped, mut failed) = (0u32, 0u32, 0u32, 0u32);
    let (mut skill_sessions, mut redirects, mut outcomes, mut subagents) = (0u32, 0u32, 0u32, 0u32);
    let mut errors = Vec::new();
    let mut skills: std::collections::BTreeMap<String, u32> = Default::default();
    for f in &files {
        let session = match read_session(&f.path) {
            Ok(s) => s,
            Err(e) => {
                failed += 1;
                if errors.len() < 10 {
                    errors.push(format!("{}: {e:#}", f.path.display()));
                }
                continue;
            }
        };
        let rec = session_record(&session);
        if rec.id.is_empty() {
            failed += 1;
            continue;
        }
        if f.parent_id.is_some() {
            subagents += 1;
        }
        match ctx.ledger.get_session(&rec.id)? {
            // Our own trace of the session is richer (loaded entries, prelabels): keep it.
            Some(existing) if existing.source == "trace" => {
                skipped += 1;
                continue;
            }
            Some(_) => updated += 1,
            None => new += 1,
        }
        if !rec.skill_reads.is_empty() {
            skill_sessions += 1;
            for s in &rec.skill_reads {
                *skills.entry(s.clone()).or_default() += 1;
            }
        }
        redirects += rec.redirects.len() as u32;
        ctx.ledger.upsert_session(&rec)?;
        for o in extract_outcomes(&session) {
            ctx.ledger.insert_outcome(&o)?;
            ctx.ledger.put_minhash(&o.id, &minhash(&outcome_tokens(&o), MINHASH_K))?;
            outcomes += 1;
        }
    }
    eprintln!("omp-evolve import: {} session files from {}", files.len(), dir.display());
    Ok(json!({
        "dir": dir,
        "files": files.len(),
        "new": new,
        "updated": updated,
        "skipped_traced": skipped,
        "failed": failed,
        "subagent_files": subagents,
        "with_skill_reads": skill_sessions,
        "skills_read": skills,
        "redirects": redirects,
        "outcomes": outcomes,
        "errors": errors,
    }))
}

/// Session stem corpus for tf-idf ranking.
pub fn stem_corpus(ctx: &AppCtx) -> Result<Vec<Vec<String>>> {
    Ok(ctx.ledger.list_sessions(&SessionFilter::default())?.into_iter().map(|s| s.stems).collect())
}

/// Trigger text of a skill: description plus every `Use when …` line of the body.
pub fn trigger_text(doc: &SkillDoc) -> (String, String) {
    let desc = doc.get_str("description").unwrap_or("").trim().to_string();
    let use_when: Vec<&str> = doc
        .body
        .lines()
        .map(str::trim)
        .filter(|l| l.to_ascii_lowercase().trim_start_matches(['*', '-', ' ', '_']).starts_with("use when"))
        .collect();
    let trigger_line = use_when.first().map(|s| s.to_string()).unwrap_or_else(|| desc.clone());
    (format!("{desc}\n{}", use_when.join("\n")), trigger_line)
}

/// Ranked predicate stems of a text against the ledger corpus (≤ 10).
pub fn ranked_text_stems(text: &str, corpus: &[Vec<String>]) -> Vec<String> {
    let mut r = rank_stems(&stems_of_text(text), corpus);
    r.truncate(MAX_PREDICATE_STEMS);
    r
}

fn existing_import(ctx: &AppCtx, name: &str, source: &str) -> Result<Option<Entry>> {
    Ok(ctx
        .ledger
        .list_entries(&EntryFilter { name: Some(name.into()), ..Default::default() })?
        .into_iter()
        .find(|e| matches!(&e.origin, Origin::Import { source: s } if s == source)))
}

pub fn import_forge(ctx: &AppCtx, forge: &Path) -> Result<Value> {
    let forge = crate::app::absolute(forge)?;
    let skills_dir = forge.join("skills");
    if !skills_dir.is_dir() {
        bail!("{} has no skills/ directory", forge.display());
    }
    let corpus = stem_corpus(ctx)?;
    let family = Family("unknown/unknown".into());
    let mut registered = Vec::new();
    let mut existing = Vec::new();
    let mut skipped = Vec::new();
    let mut dirs: Vec<PathBuf> =
        std::fs::read_dir(&skills_dir)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.join("SKILL.md").is_file()).collect();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        if validate_name(&name).is_err() {
            skipped.push(json!({"name": name, "reason": "invalid name"}));
            continue;
        }
        if let Some(e) = existing_import(ctx, &name, "forge")? {
            existing.push(json!({"id": e.id, "name": name}));
            continue;
        }
        let md = std::fs::read_to_string(dir.join("SKILL.md"))?;
        let doc = SkillDoc::parse(&md).unwrap_or(SkillDoc { body: md.clone(), ..Default::default() });
        let (text, trigger_line) = trigger_text(&doc);
        let stems = ranked_text_stems(&format!("{name} {text}"), &corpus);
        let pred = predicate::derive(None, &[], &stems);
        let e = new_entry(
            NewEntry {
                kind: Kind::Skill,
                name: name.clone(),
                branch: FORGE_BRANCH.into(),
                status: Status::Provisional,
                family: family.clone(),
                origin: Origin::Import { source: "forge".into() },
                predicate: pred,
                trigger_line,
                path: dir.clone(),
            },
            ctx.now,
        )?;
        // The skill already loads natively through the forge plugin: register only,
        // never copy or write into the forge checkout.
        let mut e = e;
        e.status_since = Some(ctx.now);
        ctx.ledger.upsert_entry(&e)?;
        registered.push(json!({"id": e.id, "name": name, "stems": e.predicate.stems, "path": dir}));
    }

    let mut lessons = Vec::new();
    let mut lessons_existing = Vec::new();
    let learn = forge.join("learnings");
    if learn.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&learn)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "md") && p.file_name().is_some_and(|n| n != "README.md"))
            .collect();
        files.sort();
        for f in files {
            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let name = format!("lesson-{}", slug(stem));
            if let Some(e) = existing_import(ctx, &name, "forge")? {
                lessons_existing.push(json!({"id": e.id, "name": name}));
                continue;
            }
            let bytes = std::fs::read(&f).with_context(|| format!("read {}", f.display()))?;
            let sha = ctx.ledger.put_blob(&bytes)?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let doc = SkillDoc::parse(&text).unwrap_or(SkillDoc { body: text.clone(), ..Default::default() });
            let title = doc
                .get_str("title")
                .map(str::to_string)
                .or_else(|| doc.body.lines().find(|l| l.starts_with("# ")).map(|l| l.trim_start_matches("# ").trim().to_string()))
                .unwrap_or_else(|| stem.replace('-', " "));
            let stems = ranked_text_stems(&format!("{title}\n{}", doc.body), &corpus);
            let dir = ctx.paths.root.join("lessons").join(FORGE_BRANCH).join(&name);
            std::fs::create_dir_all(&dir)?;
            let mut e = new_entry(
                NewEntry {
                    kind: Kind::Lesson,
                    name: name.clone(),
                    branch: FORGE_BRANCH.into(),
                    status: Status::Provisional,
                    family: family.clone(),
                    origin: Origin::Import { source: "forge".into() },
                    predicate: predicate::derive(None, &[], &stems),
                    trigger_line: title.clone(),
                    path: dir.clone(),
                },
                ctx.now,
            )?;
            e.status_since = Some(ctx.now);
            std::fs::write(dir.join("LESSON.md"), &bytes)?;
            let mut ev = serde_json::to_value(&e)?;
            ev["evidence_sha256"] = json!(sha);
            ev["source_path"] = json!(f);
            std::fs::write(dir.join(EVOLVE_JSON), serde_json::to_string_pretty(&ev)?)?;
            ctx.ledger.upsert_entry(&e)?;
            lessons.push(json!({"id": e.id, "name": name, "evidence_sha256": sha, "source": f}));
        }
    }
    Ok(json!({
        "forge": forge,
        "skills_registered": registered.len(),
        "skills_existing": existing.len(),
        "lessons_registered": lessons.len(),
        "lessons_existing": lessons_existing.len(),
        "skills": registered,
        "lessons": lessons,
        "skipped": skipped,
        "eval_suites": "ignored (kept as files for reference)",
    }))
}
