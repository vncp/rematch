//! Evidence drift (SPEC "Statuses": `stale` ← a fact's source snapshot, the model
//! family in use, or the fork commit changed). Stale entries are excluded (moved to
//! `archive/stale/` by `library::set_status`); `recanary` brings one back as a new
//! provisional version (fresh id ⇒ counters reset), or the user retires it
//! (`library disable`).
//!
//! Fact snapshots: `facts/*.md` frontmatter `source: <path>` (absolute, `~/…`, or
//! relative to the entry's repo) gets `source_sha256: <hex>` written by
//! [`snapshot_sources`] when the entry is staged; a different (or vanished) source
//! file later makes the entry stale.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use clap::Args;
use rematch_core::ledger::{sha256_hex, EntryFilter, SessionFilter};
use rematch_core::library::{load_entry, stage, SHARED_BRANCH};
use rematch_core::package::SkillDoc;
use rematch_core::predicate::ANY_REPO;
use rematch_core::{Entry, Status};
use serde::Serialize;
use serde_json::{json, Value};

use super::{change_status, git, package_edits, unescape_glob};
use crate::app::{new_id, AppCtx};

/// Frontmatter key holding the source snapshot hash of a fact.
pub const SOURCE_SHA_KEY: &str = "source_sha256";
/// Counted sessions in a row under another family before an entry goes stale.
pub const FAMILY_WINDOW: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    FactSource,
    ModelFamily,
    ForkCommit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StaleEvent {
    pub entry_id: String,
    pub name: String,
    pub reason: StaleReason,
    pub detail: String,
}

#[derive(Debug, Clone, Args)]
pub struct StaleArgs {
    /// Report drift without moving entries to `stale`.
    #[arg(long)]
    pub dry_run: bool,
    /// Bring a stale entry back as a new provisional version (new canary).
    #[arg(long, value_name = "ENTRY")]
    pub recanary: Option<String>,
}

/// Directory a relative `source:` resolves against: the entry's exact repo glob.
fn source_base(entry_dir: &Path) -> Option<PathBuf> {
    let e = load_entry(entry_dir).ok()?;
    let g = e.predicate.repo_glob.trim();
    (!g.is_empty() && g != ANY_REPO).then(|| PathBuf::from(unescape_glob(g)))
}

fn resolve_source(src: &str, base: Option<&Path>) -> Option<PathBuf> {
    let src = src.trim();
    if src.is_empty() {
        return None;
    }
    if let Some(rest) = src.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest));
    }
    let p = PathBuf::from(src);
    if p.is_absolute() {
        Some(p)
    } else {
        base.map(|b| b.join(p))
    }
}

fn fact_files(entry_dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(entry_dir.join("facts")) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "md"))
        .collect();
    v.sort();
    v
}

/// Record the sha256 of every fact's `source:` file into its frontmatter
/// (`source_sha256`). Call after `library::stage` writes the entry. Returns the number
/// of facts snapshotted; a source that does not resolve to a file is left unrecorded.
pub fn snapshot_sources(entry_dir: &Path) -> Result<usize> {
    let base = source_base(entry_dir);
    let mut n = 0;
    for f in fact_files(entry_dir) {
        let mut doc = SkillDoc::parse(&std::fs::read_to_string(&f)?)?;
        let Some(src) = doc.get_str("source").and_then(|s| resolve_source(s, base.as_deref())) else { continue };
        let Ok(bytes) = std::fs::read(&src) else { continue };
        doc.set(SOURCE_SHA_KEY, sha256_hex(&bytes));
        std::fs::write(&f, doc.render())?;
        n += 1;
    }
    Ok(n)
}

fn fact_drift(e: &Entry) -> Result<Vec<String>> {
    let base = source_base(&e.path);
    let mut out = Vec::new();
    for f in fact_files(&e.path) {
        let doc = SkillDoc::parse(&std::fs::read_to_string(&f)?)?;
        let (Some(src), Some(want)) = (doc.get_str("source"), doc.get_str(SOURCE_SHA_KEY)) else { continue };
        let name = f.file_name().unwrap_or_default().to_string_lossy();
        match resolve_source(src, base.as_deref()).map(std::fs::read) {
            Some(Ok(bytes)) if sha256_hex(&bytes) == want => {}
            Some(Ok(_)) => out.push(format!("facts/{name}: source {src} changed since as_of snapshot")),
            _ => out.push(format!("facts/{name}: source {src} no longer readable")),
        }
    }
    Ok(out)
}

/// The last `FAMILY_WINDOW` counted sessions in the entry's branch (any branch for
/// `shared`) all ran a family the entry was neither created nor verified under.
fn family_drift(ctx: &AppCtx, e: &Entry) -> Result<Option<String>> {
    let filter = SessionFilter {
        branch: (e.branch != SHARED_BRANCH).then(|| e.branch.clone()),
        counted: Some(true),
        ..Default::default()
    };
    // Imported/brownfield entries carry no creating family: no family to drift from.
    if e.family.0 == rematch_core::counters::UNKNOWN_FAMILY {
        return Ok(None);
    }
    let sessions = ctx.ledger.list_sessions(&filter)?;
    if sessions.len() < FAMILY_WINDOW {
        return Ok(None);
    }
    let last = &sessions[sessions.len() - FAMILY_WINDOW..];
    let foreign = |s: &rematch_core::SessionRecord| {
        s.family.as_ref().is_some_and(|f| *f != e.family && !e.verified_on.contains(f))
    };
    if !last.iter().all(foreign) {
        return Ok(None);
    }
    let fams: std::collections::BTreeSet<&str> = last.iter().filter_map(|s| s.family.as_ref()).map(|f| f.0.as_str()).collect();
    Ok(Some(format!(
        "last {FAMILY_WINDOW} counted sessions ran {} (entry: {}{})",
        fams.into_iter().collect::<Vec<_>>().join(", "),
        e.family.0,
        if e.verified_on.is_empty() {
            String::new()
        } else {
            format!("; verified on {}", e.verified_on.iter().map(|f| f.0.as_str()).collect::<Vec<_>>().join(", "))
        }
    )))
}

fn fork_drift(e: &Entry) -> Option<String> {
    let (_, Some(repo), Some(want)) = (e.fork_path.as_ref()?, e.fork_repo.as_ref(), e.fork_commit.as_ref()) else {
        return None;
    };
    match git(repo, &["rev-parse", "HEAD"]) {
        Ok(head) if head == *want => None,
        Ok(head) => Some(format!("fork {} moved from {} to {}", repo.display(), &want[..want.len().min(12)], &head[..head.len().min(12)])),
        Err(_) => None, // fork checkout not available on this machine: cannot judge drift
    }
}

/// Detect drift on kept and provisional entries (read-only; pinned entries are
/// human-owned and skipped). One event per entry: fact source, then family, then fork.
pub fn check(ctx: &AppCtx) -> Result<Vec<StaleEvent>> {
    let mut out = Vec::new();
    for e in ctx.ledger.list_entries(&EntryFilter::default())? {
        if e.pinned || !matches!(e.status, Status::Kept | Status::Provisional) {
            continue;
        }
        let ev = |reason, detail: String| StaleEvent { entry_id: e.id.clone(), name: e.name.clone(), reason, detail };
        let facts = fact_drift(&e)?;
        if !facts.is_empty() {
            out.push(ev(StaleReason::FactSource, facts.join("; ")));
        } else if let Some(d) = family_drift(ctx, &e)? {
            out.push(ev(StaleReason::ModelFamily, d));
        } else if let Some(d) = fork_drift(&e) {
            out.push(ev(StaleReason::ForkCommit, d));
        }
    }
    Ok(out)
}

/// `check` + move every drifted entry to `stale` (excluded from loading). Entries whose
/// files are the user's (brownfield, forge imports) get the status in the ledger only.
pub fn sweep(ctx: &AppCtx) -> Result<Vec<StaleEvent>> {
    let events = check(ctx)?;
    for ev in &events {
        let e = ctx.ledger.get_entry(&ev.entry_id)?.ok_or_else(|| anyhow!("entry {} vanished", ev.entry_id))?;
        change_status(ctx, &e, Status::Stale, &format!("stale: {}", ev.detail))?;
    }
    Ok(events)
}

pub fn cmd_stale(ctx: &AppCtx, args: StaleArgs) -> Result<Value> {
    if let Some(r) = args.recanary {
        return cmd_recanary(ctx, &r);
    }
    let events = if args.dry_run { check(ctx)? } else { sweep(ctx)? };
    Ok(json!({ "applied": !args.dry_run, "stale": events }))
}

/// Stale → provisional as a new version: new id (counters start at zero), version+1,
/// `predecessor` = the stale entry, sources re-snapshotted, fork commit cleared (the next
/// `ship` records it), family = the family now in use in the branch. The stale entry is
/// retired with a note pointing at its successor.
pub fn cmd_recanary(ctx: &AppCtx, id_or_name: &str) -> Result<Value> {
    let old = ctx
        .ledger
        .list_entries(&EntryFilter { status: Some(Status::Stale), ..Default::default() })?
        .into_iter()
        .rev()
        .find(|e| e.id == id_or_name || e.name == id_or_name)
        .ok_or_else(|| anyhow!("no stale entry {id_or_name:?}"))?;
    let live = ctx.ledger.list_entries(&EntryFilter { name: Some(old.name.clone()), status: Some(Status::Provisional), branch: Some(old.branch.clone()), ..Default::default() })?;
    if let Some(other) = live.first() {
        bail!("{} already has a provisional canary {} in branch {}", old.name, other.id, old.branch);
    }
    let files = package_edits(&old.path)?;
    let current_family = ctx
        .ledger
        .list_sessions(&SessionFilter {
            branch: (old.branch != SHARED_BRANCH).then(|| old.branch.clone()),
            counted: Some(true),
            ..Default::default()
        })?
        .into_iter()
        .rev()
        .find_map(|s| s.family);
    let mut e = old.clone();
    e.id = new_id();
    e.status = Status::Provisional;
    e.version = old.version + 1;
    e.predecessor = Some(old.id.clone());
    e.family = current_family.unwrap_or_else(|| old.family.clone());
    e.helpful = 0;
    e.harmful = 0;
    e.created_at = 0;
    e.updated_at = 0;
    e.status_since = None;
    e.status_note = Some(format!("re-canary of stale {}", old.id));
    e.fork_commit = None;
    e.pinned = false;
    e.pending_next_run = false;
    let staged = stage(&ctx.ledger, &ctx.paths, &e, &files)?;
    let snapshotted = snapshot_sources(&staged.path)?;
    let retired = change_status(ctx, &old, Status::Retired, &format!("re-canaried as {}", staged.id))?;
    Ok(json!({
        "entry": staged.id, "name": staged.name, "version": staged.version, "family": staged.family,
        "path": staged.path, "facts_snapshotted": snapshotted, "retired": retired.id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::testutil::{entry, env, keep, repo, session, sh, stage as stage_t};
    use rematch_core::Family;

    #[test]
    fn stale_detects_changed_fact_source_and_recanaries() {
        let t = env();
        let ctx = &t.ctx;
        let src = t.dir.join("data.csv");
        std::fs::write(&src, "a,b\n1,2\n").unwrap();
        let fact = format!("---\nas_of: 2026-10-01\nsource: {}\n---\n\nRows: 1\n", src.display());
        let e = stage_t(ctx, &entry("e-fact-1", "data-rows", "repo"), &[("facts/rows.md", &fact)]);
        assert_eq!(snapshot_sources(&e.path).unwrap(), 1);
        let recorded = std::fs::read_to_string(e.path.join("facts/rows.md")).unwrap();
        assert!(recorded.contains(&format!("source_sha256: {}", sha256_hex(b"a,b\n1,2\n"))), "{recorded}");
        assert!(check(ctx).unwrap().is_empty());

        std::fs::write(&src, "a,b\n1,2\n3,4\n").unwrap();
        let ev = check(ctx).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].reason, StaleReason::FactSource);
        let out = cmd_stale(ctx, StaleArgs { dry_run: false, recanary: None }).unwrap();
        assert_eq!(out["stale"].as_array().unwrap().len(), 1);
        let stale = ctx.ledger.get_entry(&e.id).unwrap().unwrap();
        assert_eq!(stale.status, Status::Stale);
        assert!(stale.path.starts_with(ctx.paths.archive.join("stale")));

        let out = cmd_stale(ctx, StaleArgs { dry_run: false, recanary: Some("data-rows".into()) }).unwrap();
        let new_id = out["entry"].as_str().unwrap();
        assert_ne!(new_id, e.id);
        let fresh = ctx.ledger.get_entry(new_id).unwrap().unwrap();
        assert_eq!(fresh.status, Status::Provisional);
        assert_eq!(fresh.version, 2);
        assert_eq!(fresh.predecessor.as_deref(), Some(e.id.as_str()));
        assert_eq!(ctx.ledger.counters(new_id).unwrap().matched, 0);
        assert_eq!(ctx.ledger.get_entry(&e.id).unwrap().unwrap().status, Status::Retired);
        assert!(check(ctx).unwrap().is_empty(), "fresh snapshot of the new source");
    }

    #[test]
    fn stale_on_family_change_after_three_sessions() {
        let t = env();
        let ctx = &t.ctx;
        let e = stage_t(ctx, &entry("e-fam-1", "fam-skill", "repo"), &[]);
        keep(ctx, &e.id);
        for i in 0..2 {
            ctx.ledger.upsert_session(&session(&format!("s{i}"), "repo", Some("/w/repo"), "openai/gpt-sol", 1_000 + i)).unwrap();
        }
        assert!(check(ctx).unwrap().is_empty(), "two sessions are not enough");
        ctx.ledger.upsert_session(&session("s9", "repo", Some("/w/repo"), "openai/gpt-sol", 2_000)).unwrap();
        let ev = check(ctx).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].reason, StaleReason::ModelFamily);
        // verified on the new family ⇒ not stale
        let mut v = ctx.ledger.get_entry(&e.id).unwrap().unwrap();
        v.verified_on.push(Family("openai/gpt-sol".into()));
        ctx.ledger.upsert_entry(&v).unwrap();
        assert!(check(ctx).unwrap().is_empty());
    }

    #[test]
    fn stale_never_moves_user_owned_skill_dirs() {
        let t = env();
        let ctx = &t.ctx;
        // a user's skill registered in place (brownfield): no rematch.json, files are theirs
        let dir = ctx.paths.skills_dir().join("user-skill");
        std::fs::create_dir_all(dir.join("facts")).unwrap();
        std::fs::write(dir.join("SKILL.md"), crate::scope::testutil::skill_md("user-skill")).unwrap();
        let src = t.dir.join("src.txt");
        std::fs::write(&src, "now").unwrap();
        std::fs::write(dir.join("facts/x.md"), format!("---\nas_of: 2026-10-01\nsource: {}\nsource_sha256: {}\n---\n\nx\n", src.display(), sha256_hex(b"then"))).unwrap();
        let mut e = entry("e-user-1", "user-skill", "shared");
        e.path = dir.clone();
        e.origin = rematch_core::Origin::Import { source: "brownfield".into() };
        ctx.ledger.upsert_entry(&e).unwrap();
        let ev = sweep(ctx).unwrap();
        assert_eq!(ev.len(), 1);
        let after = ctx.ledger.get_entry(&e.id).unwrap().unwrap();
        assert_eq!(after.status, Status::Stale);
        assert_eq!(after.path, dir, "ledger-only status change");
        assert!(dir.join("SKILL.md").is_file(), "user's skill stays in place");
        assert!(!dir.join("rematch.json").exists(), "nothing written into the user's dir");
    }

    #[test]
    fn unknown_family_entries_never_drift_by_family() {
        let t = env();
        let ctx = &t.ctx;
        let mut e = entry("e-imp-1", "imported", "repo");
        e.family = Family(rematch_core::counters::UNKNOWN_FAMILY.into());
        stage_t(ctx, &e, &[]);
        for i in 0..3 {
            ctx.ledger.upsert_session(&session(&format!("s{i}"), "repo", Some("/w/repo"), "anthropic/claude-opus", 1_000 + i)).unwrap();
        }
        assert!(check(ctx).unwrap().is_empty());
    }

    #[test]
    fn stale_on_fork_commit_change() {
        let t = env();
        let ctx = &t.ctx;
        let fork = repo(&t.dir.join("fork"));
        let mut e = entry("e-fork-1", "fork-prompt", "fork");
        e.fork_path = Some("prompts/x.md".into());
        e.fork_repo = Some(fork.clone());
        e.fork_commit = Some(sh(&fork, &["git", "rev-parse", "HEAD"]));
        stage_t(ctx, &e, &[]);
        assert!(check(ctx).unwrap().is_empty());
        std::fs::write(fork.join("y"), "y").unwrap();
        sh(&fork, &["git", "add", "y"]);
        sh(&fork, &["git", "commit", "-q", "-m", "y"]);
        let ev = check(ctx).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].reason, StaleReason::ForkCommit);
    }
}
