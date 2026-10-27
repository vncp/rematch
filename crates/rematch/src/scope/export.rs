//! Work ↔ home export (SPEC §Scope "Work → home export", §Models "Home and work are
//! separate libraries"): invariant entries only, each with per-entry approval and the
//! diff shown, off by default at work.
//!
//! Policy: `REMATCH_ROOT/policy.json` `{"environment":"work"|"home","export_enabled":bool}`
//! (other keys belong to `rematch policy` and `domain`). Missing file ⇒ home.
//! `export_enabled` defaults to false at work and true at home; export refuses when work &&
//! !export_enabled.
//!
//! Layout of the export dir (default `REMATCH_ROOT/export/`, a git repo created on first
//! export): `<entry-id>/` holding the package files and a `rematch.json` stripped to a
//! portable provisional entry (no counters, evidence, approvals or absolute path).
//! Workspace `facts/` never travel.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Args;
use rematch_core::dedup::{entry_content, route_duplicate, EntryContent};
use rematch_core::domain::POLICY_FILE;
use rematch_core::ledger::{sha256_hex, EntryFilter, SessionFilter};
use rematch_core::library::{load_entry, stage, REMATCH_JSON, SHARED_BRANCH};
use rematch_core::lint::{branch_always_on_bytes, LintContext};
use rematch_core::predicate::derive;
use rematch_core::{Activation, Edit, EditorScope, Entry, LessonTag, LintReport, Origin, Receipt, Status};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{branch_anchor, git, git_commit, lint_edits, lint_errors, package_edits, resolve_entry};
use crate::app::{absolute, AppCtx};
use crate::envelope::Usage;

/// Local record of declined exports: `{entry_id: content sha256}`.
pub const DECLINED_FILE: &str = "export-declined.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    Work,
    #[default]
    Home,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub environment: Environment,
    #[serde(default)]
    pub export_enabled: Option<bool>,
    #[serde(default)]
    pub skip_profiles: Vec<String>,
}

impl Policy {
    pub fn load(root: &Path) -> Result<Policy> {
        let p = root.join(POLICY_FILE);
        match std::fs::read_to_string(&p) {
            Ok(t) => serde_json::from_str(&t).with_context(|| format!("parse {}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Policy::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn export_allowed(&self) -> bool {
        self.export_enabled.unwrap_or(self.environment == Environment::Home)
    }
}

#[derive(Debug, Clone, Default, Args)]
pub struct ExportArgs {
    /// Export dir (git-tracked; default REMATCH_ROOT/export).
    #[arg(long)]
    pub to: Option<PathBuf>,
    /// Entry id or name (repeatable); default: every kept invariant entry.
    #[arg(long = "entry")]
    pub entries: Vec<String>,
    /// Approve the listed `--entry` exports (after seeing their diffs).
    #[arg(long)]
    pub approve: bool,
    /// Approve every candidate without per-entry review.
    #[arg(long)]
    pub approve_all: bool,
}

#[derive(Debug, Clone, Default, Args)]
pub struct ImportExportArgs {
    /// Export dir to read (default REMATCH_ROOT/export).
    #[arg(long)]
    pub from: Option<PathBuf>,
}

/// One export awaiting approval (shape of a digest approval item).
#[derive(Debug, Clone, Serialize)]
pub struct ExportCandidate {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub diff: String,
    pub reason: String,
    pub dropped_facts: Vec<String>,
}

fn is_invariant(e: &Entry) -> bool {
    e.lesson_tag == Some(LessonTag::Invariant) || e.manifest.as_ref().is_some_and(|m| m.scope == EditorScope::Invariant)
}

/// The portable form of an entry: provisional, counters/evidence/approvals and the
/// originating session stripped (work session ids stay at work).
fn portable(e: &Entry) -> Entry {
    Entry {
        status: Status::Provisional,
        origin: Origin::Import { source: format!("export:{}", e.id) },
        helpful: 0,
        harmful: 0,
        evidence: vec![],
        path: PathBuf::new(),
        pinned: false,
        approved_scripts: vec![],
        approved_fragment: None,
        approved_agent: None,
        predecessor: None,
        sibling_of: None,
        status_since: None,
        status_note: None,
        pending_next_run: false,
        fork_repo: None,
        fork_commit: None,
        families_excluded: vec![],
        last_tune_at: None,
        tune: None,
        manifest: e.manifest.clone().map(|mut m| {
            m.evidence.session_id.clear();
            m
        }),
        ..e.clone()
    }
}

struct Prepared {
    entry: Entry,
    files: Vec<Edit>,
    dropped: Vec<String>,
    lint: LintReport,
}

fn prepare(ctx: &AppCtx, e: &Entry) -> Result<Prepared> {
    let mut files = package_edits(&e.path)?;
    let mut dropped = Vec::new();
    files.retain(|f| match f {
        Edit::Write { path, .. } if path.starts_with("facts/") => {
            dropped.push(path.clone());
            false
        }
        _ => true,
    });
    let entry = portable(e);
    let lint = lint_edits(ctx, e, &files, &LintContext { transfer: true, ..Default::default() })?;
    files.push(Edit::Write { path: REMATCH_JSON.into(), content: serde_json::to_string_pretty(&entry)? + "\n" });
    Ok(Prepared { entry, files, dropped, lint })
}

fn write_tree(dir: &Path, files: &[Edit]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    for f in files {
        if let Edit::Write { path, content } = f {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().expect("file under dir"))?;
            std::fs::write(&p, content)?;
        }
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for ent in std::fs::read_dir(src)? {
        let ent = ent?;
        let (s, d) = (ent.path(), dst.join(ent.file_name()));
        if ent.file_type()?.is_dir() {
            copy_tree(&s, &d)?;
        } else {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

/// `git diff --no-index` of the current export (if any) against the prepared one;
/// empty when the export is up to date.
fn diff(existing: &Path, files: &[Edit]) -> Result<String> {
    let tmp = tempfile::tempdir()?;
    let (old, new) = (tmp.path().join("exported"), tmp.path().join("proposed"));
    std::fs::create_dir_all(&old)?;
    if existing.is_dir() {
        copy_tree(existing, &old)?;
    }
    write_tree(&new, files)?;
    let out = std::process::Command::new("git")
        .current_dir(tmp.path())
        .args(["diff", "--no-index", "--no-color", "--", "exported", "proposed"])
        .output()
        .context("spawn git diff")?;
    match out.status.code() {
        Some(0) | Some(1) => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
        _ => bail!("git diff --no-index failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

/// Make `dir` a git repo unless it already sits inside one.
fn ensure_git(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    if git(dir, &["rev-parse", "--is-inside-work-tree"]).is_err() {
        git(dir, &["init", "-q"])?;
    }
    Ok(())
}

fn export_dir(ctx: &AppCtx, to: Option<&Path>) -> Result<PathBuf> {
    Ok(match to {
        Some(p) => absolute(p)?,
        None => ctx.paths.export.clone(),
    })
}

pub fn cmd_export(ctx: &AppCtx, args: ExportArgs) -> Result<Value> {
    Ok(run(ctx, args)?.1)
}

/// Content identity of a prepared export (what a decline refers to).
fn content_hash(files: &[Edit]) -> String {
    let mut buf = Vec::new();
    for f in files {
        if let Edit::Write { path, content } = f {
            buf.extend_from_slice(path.as_bytes());
            buf.push(0);
            buf.extend_from_slice(content.as_bytes());
            buf.push(0);
        }
    }
    sha256_hex(&buf)
}

fn load_declined(ctx: &AppCtx) -> Result<BTreeMap<String, String>> {
    match std::fs::read_to_string(ctx.paths.root.join(DECLINED_FILE)) {
        Ok(t) => Ok(serde_json::from_str(&t)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

fn run(ctx: &AppCtx, args: ExportArgs) -> Result<(Vec<ExportCandidate>, Value)> {
    let policy = Policy::load(&ctx.paths.root)?;
    if !policy.export_allowed() {
        bail!(
            "export is off in the {:?} environment; set \"export_enabled\": true in {} to allow it",
            policy.environment,
            ctx.paths.root.join(POLICY_FILE).display()
        );
    }
    if args.approve && args.entries.is_empty() {
        return Err(Usage("--approve applies to the listed --entry ids; use --approve-all for every candidate".into()).into());
    }
    let to = export_dir(ctx, args.to.as_deref())?;
    let explicit = !args.entries.is_empty();
    let candidates: Vec<Entry> = if explicit {
        args.entries
            .iter()
            .map(|r| resolve_entry(ctx, r))
            .collect::<Result<_>>()?
    } else {
        ctx.ledger.list_entries(&EntryFilter { status: Some(Status::Kept), ..Default::default() })?
    };
    let declined = load_declined(ctx)?;
    let (mut pending, mut exported, mut blocked, mut skipped) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for e in candidates {
        if !matches!(e.status, Status::Kept | Status::Provisional | Status::Pinned) {
            skipped.push(json!({"id": e.id, "name": e.name, "reason": format!("status {:?}", e.status)}));
            continue;
        }
        if !is_invariant(&e) {
            if explicit {
                skipped.push(json!({"id": e.id, "name": e.name, "reason": "not invariant: domain entries stay where they were born"}));
            }
            continue;
        }
        let prep = prepare(ctx, &e)?;
        if !prep.lint.ok {
            blocked.push(json!({"id": e.id, "name": e.name, "findings": prep.lint.findings, "reason": lint_errors(&prep.lint)}));
            continue;
        }
        let dst = to.join(&e.id);
        let d = diff(&dst, &prep.files)?;
        if d.trim().is_empty() {
            skipped.push(json!({"id": e.id, "name": e.name, "reason": "export up to date"}));
            continue;
        }
        if !explicit && declined.get(&e.id) == Some(&content_hash(&prep.files)) {
            skipped.push(json!({"id": e.id, "name": e.name, "reason": "export declined for this content"}));
            continue;
        }
        let approved = args.approve_all || (args.approve && explicit);
        if !approved {
            pending.push(ExportCandidate {
                id: e.id.clone(),
                name: e.name.clone(),
                kind: "export",
                diff: d,
                reason: format!("export invariant {:?} {} to the other environment's library (arrives provisional)", e.kind, e.name),
                dropped_facts: prep.dropped,
            });
            continue;
        }
        ensure_git(&to)?;
        if dst.exists() {
            std::fs::remove_dir_all(&dst)?;
        }
        write_tree(&dst, &prep.files)?;
        git(&to, &["add", "-A", "--", &e.id])?;
        let commit = git_commit(&to, &format!("rematch export: {} ({})\n\nkind {:?}, version {}, family {}\n", e.name, e.id, e.kind, e.version, e.family.0), &[&e.id])?;
        let hashes: Vec<(String, String)> = prep
            .files
            .iter()
            .filter_map(|f| match f {
                Edit::Write { path, content } => Some((path.clone(), sha256_hex(content.as_bytes()))),
                _ => None,
            })
            .collect();
        let cert = json!({"entry": e.id, "what": "export", "to": to, "commit": commit, "files": hashes, "at": ctx.now});
        let certificate = ctx.ledger.put_blob(serde_json::to_string(&cert)?.as_bytes())?;
        ctx.ledger.insert_receipt(&Receipt { certificate, entry: e.id.clone(), activation: Activation::Prompted })?;
        exported.push(json!({"id": e.id, "name": e.name, "dir": dst, "commit": commit, "dropped_facts": prep.dropped, "entry": prep.entry.id}));
    }
    let report = json!({
        "to": to,
        "environment": policy.environment,
        "requires_approval": !pending.is_empty(),
        "pending": pending,
        "exported": exported,
        "blocked": blocked,
        "skipped": skipped,
    });
    Ok((pending, report))
}

/// Decline one export: it is not offered again until its content changes (the entry
/// itself is untouched; declining an export is not declining the entry).
pub fn decline(ctx: &AppCtx, id: &str) -> Result<Value> {
    let e = resolve_entry(ctx, id)?;
    let hash = content_hash(&prepare(ctx, &e)?.files);
    let mut declined = load_declined(ctx)?;
    declined.insert(e.id.clone(), hash);
    std::fs::write(ctx.paths.root.join(DECLINED_FILE), serde_json::to_string_pretty(&declined)?)?;
    Ok(json!({ "declined": e.id, "name": e.name }))
}

/// Exports awaiting approval (digest approval items, `kind: "export"`); empty when the
/// policy forbids export. Read-only.
pub fn pending(ctx: &AppCtx) -> Result<Vec<ExportCandidate>> {
    if !Policy::load(&ctx.paths.root)?.export_allowed() {
        return Ok(Vec::new());
    }
    Ok(run(ctx, ExportArgs::default())?.0)
}

/// Approve one pending export (the digest's confirm callback).
pub fn approve(ctx: &AppCtx, id: &str) -> Result<Value> {
    cmd_export(ctx, ExportArgs { to: None, entries: vec![id.to_string()], approve: true, approve_all: false })
}

/// A branch exists locally when this library has sessions, entries or a staging dir for it.
fn branch_exists(ctx: &AppCtx, branch: &str) -> Result<bool> {
    Ok(ctx.paths.staging_branch(branch).is_dir()
        || !ctx.ledger.list_entries(&EntryFilter { branch: Some(branch.into()), ..Default::default() })?.is_empty()
        || !ctx.ledger.list_sessions(&SessionFilter { branch: Some(branch.into()), ..Default::default() })?.is_empty())
}

/// `library import-export [--from <dir>]`: stage exported invariant entries as
/// provisional in their recorded branch when it exists here, else `shared`.
pub fn cmd_import_export(ctx: &AppCtx, args: ImportExportArgs) -> Result<Value> {
    let from = export_dir(ctx, args.from.as_deref())?;
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&from)
        .with_context(|| format!("read {}", from.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join(REMATCH_JSON).is_file())
        .collect();
    dirs.sort();
    let all = ctx.ledger.list_entries(&EntryFilter::default())?;
    let live: Vec<(String, EntryContent)> = all
        .iter()
        .filter(|e| matches!(e.status, Status::Kept | Status::Provisional | Status::Pinned))
        .filter_map(|e| entry_content(&e.path).ok().map(|c| (e.id.clone(), c)))
        .collect();
    let (mut staged_out, mut skipped, mut blocked) = (Vec::new(), Vec::new(), Vec::new());
    for dir in dirs {
        let mut src = load_entry(&dir)?;
        src.path = dir.clone();
        let known = all.iter().any(|x| {
            x.id == src.id || x.links.iter().any(|l| l.rel == rematch_core::LinkRel::TransfersFrom && l.id == src.id)
        });
        if known {
            skipped.push(json!({"id": src.id, "name": src.name, "reason": "already in this library"}));
            continue;
        }
        if !is_invariant(&src) {
            skipped.push(json!({"id": src.id, "name": src.name, "reason": "not invariant"}));
            continue;
        }
        if let Some(dup) = route_duplicate(&entry_content(&dir)?, &live) {
            skipped.push(json!({"id": src.id, "name": src.name, "reason": "near-duplicate", "duplicate_of": dup}));
            continue;
        }
        let branch = if branch_exists(ctx, &src.branch)? { src.branch.clone() } else { SHARED_BRANCH.to_string() };
        // The same entry on another machine: its domain travels with it.
        let place = derive(branch_anchor(ctx, &branch)?.as_deref(), &[], &[], src.predicate.domain.as_deref());
        let origin = Origin::Import { source: format!("export:{}", src.id) };
        let (e, files, dropped) = super::transfer::copy_for(&src, &branch, place, origin)?;
        let branch_dir = ctx.paths.staging_branch(&branch);
        let lctx = LintContext {
            branch_other_bytes: branch_always_on_bytes(&branch_dir, &branch_dir.join(&e.name)),
            transfer: true,
            ..Default::default()
        };
        let rep = lint_edits(ctx, &e, &files, &lctx)?;
        if !rep.ok {
            blocked.push(json!({"id": src.id, "name": src.name, "reason": lint_errors(&rep), "findings": rep.findings}));
            continue;
        }
        if all.iter().any(|x| x.name == e.name && x.branch == branch && x.status == Status::Provisional) {
            blocked.push(json!({"id": src.id, "name": src.name, "reason": format!("a provisional {} already canaries in {branch}", e.name)}));
            continue;
        }
        let s = stage(&ctx.ledger, &ctx.paths, &e, &files)?;
        staged_out.push(json!({"id": s.id, "name": s.name, "branch": s.branch, "from": src.id, "path": s.path, "dropped_facts": dropped}));
    }
    Ok(json!({ "from": from, "staged": staged_out, "skipped": skipped, "blocked": blocked }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::testutil::{entry, env, keep, sh, stage as stage_t};

    fn policy(ctx: &AppCtx, text: &str) {
        std::fs::write(ctx.paths.root.join(POLICY_FILE), text).unwrap();
    }

    #[test]
    fn export_refuses_at_work_by_default() {
        let t = env();
        policy(&t.ctx, r#"{"environment":"work"}"#);
        let err = cmd_export(&t.ctx, ExportArgs::default()).unwrap_err();
        assert!(format!("{err:#}").contains("export is off"), "{err:#}");
        assert!(pending(&t.ctx).unwrap().is_empty());
    }

    #[test]
    fn export_requires_approval_then_writes_and_imports_at_home() {
        let t = env();
        let ctx = &t.ctx;
        policy(ctx, r#"{"environment":"work","export_enabled":true}"#);
        let inv = stage_t(ctx, &entry("e-inv-x", "verify-before-claim", "quant"), &[("facts/rows.md", "---\nas_of: 2026-10-01\n---\n\nRows: 3\n")]);
        keep(ctx, &inv.id);
        let mut dom = entry("e-dom-x", "spx-quirks", "quant");
        dom.lesson_tag = Some(LessonTag::Domain);
        dom.manifest.as_mut().unwrap().scope = EditorScope::Domain;
        let dom = stage_t(ctx, &dom, &[]);
        keep(ctx, &dom.id);

        let out = cmd_export(ctx, ExportArgs::default()).unwrap();
        assert_eq!(out["requires_approval"], json!(true));
        let pend = out["pending"].as_array().unwrap();
        assert_eq!(pend.len(), 1, "domain entries never export: {out}");
        assert_eq!(pend[0]["id"], json!(inv.id));
        assert!(pend[0]["diff"].as_str().unwrap().contains("proposed/SKILL.md"));
        assert!(!ctx.paths.export.join(&inv.id).exists(), "nothing written without approval");

        let out = approve(ctx, &inv.id).unwrap();
        assert_eq!(out["exported"].as_array().unwrap().len(), 1, "{out}");
        let dir = ctx.paths.export.join(&inv.id);
        assert!(dir.join("SKILL.md").is_file());
        assert!(!dir.join("facts").exists(), "workspace facts never travel");
        let exported = load_entry(&dir).unwrap();
        assert_eq!(exported.status, Status::Provisional);
        assert_eq!((exported.helpful, exported.harmful), (0, 0));
        assert!(exported.evidence.is_empty());
        assert_eq!(exported.origin, Origin::Import { source: format!("export:{}", inv.id) });
        let json = std::fs::read_to_string(dir.join(REMATCH_JSON)).unwrap();
        assert!(!json.contains("\"s1\""), "no work session ids in the export: {json}");
        let log = sh(&ctx.paths.export, &["git", "log", "--format=%s"]);
        assert!(log.contains("rematch export: verify-before-claim"), "{log}");
        assert!(pending(ctx).unwrap().is_empty(), "up to date after export");

        // home library imports it as provisional in `shared` (branch quant unknown there)
        let home = env();
        let out = cmd_import_export(&home.ctx, ImportExportArgs { from: Some(ctx.paths.export.clone()) }).unwrap();
        let staged = out["staged"].as_array().unwrap();
        assert_eq!(staged.len(), 1, "{out}");
        assert_eq!(staged[0]["branch"], json!(SHARED_BRANCH));
        let e = home.ctx.ledger.get_entry(staged[0]["id"].as_str().unwrap()).unwrap().unwrap();
        assert_eq!(e.status, Status::Provisional);
        assert_eq!(e.origin, Origin::Import { source: format!("export:{}", inv.id) });
        let again = cmd_import_export(&home.ctx, ImportExportArgs { from: Some(ctx.paths.export.clone()) }).unwrap();
        assert!(again["staged"].as_array().unwrap().is_empty());
    }

    #[test]
    fn declined_export_is_not_offered_until_content_changes() {
        let t = env();
        let ctx = &t.ctx;
        let e = stage_t(ctx, &entry("e-inv-d", "verify-before-claim", "quant"), &[]);
        let e = keep(ctx, &e.id);
        assert_eq!(pending(ctx).unwrap().len(), 1);
        decline(ctx, &e.id).unwrap();
        assert!(pending(ctx).unwrap().is_empty());
        assert_eq!(ctx.ledger.get_entry(&e.id).unwrap().unwrap().status, Status::Kept, "entry untouched");
        std::fs::create_dir_all(e.path.join("units")).unwrap();
        std::fs::write(e.path.join("units/extra.md"), "## Procedure\n\n1. Re-run the check.\n").unwrap();
        assert_eq!(pending(ctx).unwrap().len(), 1, "changed content is offered again");
    }

    #[test]
    fn approve_flag_needs_entries() {
        let t = env();
        let err = cmd_export(&t.ctx, ExportArgs { approve: true, ..Default::default() }).unwrap_err();
        assert!(err.downcast_ref::<Usage>().is_some());
    }
}
