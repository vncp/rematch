//! Entry lifecycle on disk and in the ledger (DESIGN §1, §4, §6; SPEC "Library and
//! admission"). `rematch.json` is authoritative for files, the ledger for counters.
//!
//! Locations by status:
//! - draft → `drafts/<name>/` (never loaded)
//! - provisional → `staging/<branch>/<name>/` (loaded via customDirectories on match)
//! - kept → `<agent_dir>/skills/<name>/` (loaded natively); a replaced kept dir is
//!   saved to `predecessors/<name>/<version>/` (version 0 = a skill the plugin did not
//!   write)
//! - retired | dormant | stale → `archive/<status>/<id>/` (never loaded, never deleted)
//!
//! `pinned` is a flag (human-owned): automatic transitions refuse pinned entries; the
//! entry keeps its status and location. `Status::Pinned` is accepted as a status too.

use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::ledger::{now_ms, EntryFilter, Ledger};
use crate::package::{RematchBlock, SkillDoc};
use crate::paths::RematchPaths;
use crate::predicate::{matches_repo, predicate_hash};
use crate::types::{Activation, Edit, Entry, EntryId, Kind, Receipt, Status, UnixMs};

pub const SHARED_BRANCH: &str = "shared";
pub const REMATCH_JSON: &str = "rematch.json";
pub const HOOKS_README: &str = "README.md";

/// What a one-tap approval enables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    Scripts,
    Hook,
}

/// Legal status transitions (SPEC "Statuses"). Same-status is a no-op and legal.
pub fn can_transition(from: Status, to: Status) -> bool {
    use Status::*;
    from == to
        || matches!(
            (from, to),
            (Draft, Provisional)
                | (Draft, Retired)
                | (Provisional, Kept)
                | (Provisional, Retired)
                | (Provisional, Dormant)
                | (Provisional, Stale)
                | (Kept, Retired)
                | (Kept, Stale)
                | (Kept, Pinned)
                | (Pinned, Kept)
                | (Pinned, Retired)
                | (Stale, Provisional)
                | (Stale, Retired)
                | (Dormant, Provisional)
                | (Dormant, Retired)
        )
}

/// Sanitized branch name: basename of the repo (else cwd), `[a-z0-9._-]`, lowercase.
/// `shared` is reserved for cross-branch entries, so a repo named `shared` maps to
/// `repo-shared`.
pub fn branch_for(repo: Option<&Path>, cwd: &Path) -> String {
    let base = repo
        .unwrap_or(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut s: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c.to_ascii_lowercase() } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches(|c| c == '-' || c == '.').to_string();
    match s.as_str() {
        "" => "root".into(),
        SHARED_BRANCH => "repo-shared".into(),
        _ => s,
    }
}

/// Entry names become directory names and skill names: no separators, no traversal.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || name.contains(['/', '\\'])
        || name.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        bail!("invalid entry name {name:?}");
    }
    Ok(())
}

/// Canonical directory of an entry in a given status.
pub fn dir_for(paths: &RematchPaths, e: &Entry, status: Status) -> PathBuf {
    match status {
        Status::Draft => paths.drafts.join(&e.name),
        Status::Provisional => paths.staging.join(&e.branch).join(&e.name),
        Status::Kept | Status::Pinned => paths.skills_dir().join(&e.name),
        Status::Retired => paths.archive.join("retired").join(&e.id),
        Status::Dormant => paths.archive.join("dormant").join(&e.id),
        Status::Stale => paths.archive.join("stale").join(&e.id),
    }
}

pub fn load_entry(dir: &Path) -> Result<Entry> {
    let p = dir.join(REMATCH_JSON);
    let text = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", p.display()))
}

fn safe_rel(rel: &str) -> Result<PathBuf> {
    let p = PathBuf::from(rel);
    if rel.is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("edit path must be relative without traversal: {rel:?}");
    }
    Ok(p)
}

#[cfg(unix)]
fn set_mode(p: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_p: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

fn files_under(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d)? {
            let p = ent?.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for ent in std::fs::read_dir(src)? {
        let ent = ent?;
        let (s, d) = (ent.path(), dst.join(ent.file_name()));
        if ent.file_type()?.is_dir() {
            copy_dir(&s, &d)?;
        } else {
            std::fs::copy(&s, &d).with_context(|| format!("copy {}", s.display()))?;
        }
    }
    Ok(())
}

fn move_dir(src: &Path, dst: &Path) -> Result<()> {
    if src == dst {
        return Ok(());
    }
    if dst.exists() {
        bail!("destination exists: {}", dst.display());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(src, dst).is_err() {
        copy_dir(src, dst)?;
        std::fs::remove_dir_all(src)?;
    }
    prune_empty_parent(src);
    Ok(())
}

/// Remove an emptied `staging/<branch>/` (or similar) directory; ignore failures.
fn prune_empty_parent(p: &Path) {
    if let Some(parent) = p.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

/// Archive destinations are never overwritten: a second visit gets a suffix.
fn unique_archive_dir(dir: PathBuf, now: UnixMs) -> PathBuf {
    if dir.exists() {
        let mut s = dir.into_os_string();
        s.push(format!(".{now}"));
        PathBuf::from(s)
    } else {
        dir
    }
}

fn rematch_block(e: &Entry) -> RematchBlock {
    RematchBlock {
        id: e.id.clone(),
        status: e.status,
        branch: e.branch.clone(),
        family: e.family.clone(),
        version: e.version,
        predicate_hash: predicate_hash(&e.predicate),
    }
}

/// Rewrite `rematch.json` and the SKILL.md `rematch:` block from the entry.
fn sync_files(e: &Entry) -> Result<()> {
    std::fs::write(e.path.join(REMATCH_JSON), serde_json::to_string_pretty(e)?)?;
    let skill = e.path.join("SKILL.md");
    if skill.is_file() {
        let mut doc = SkillDoc::parse(&std::fs::read_to_string(&skill)?)?;
        doc.set_rematch(&rematch_block(e));
        std::fs::write(&skill, doc.render())?;
    }
    Ok(())
}

fn persist(ledger: &Ledger, e: &Entry) -> Result<()> {
    sync_files(e)?;
    ledger.upsert_entry(e)
}

/// Scripts are mode 0644 until approved; approved ones are 0755.
fn apply_script_modes(e: &Entry) -> Result<()> {
    let scripts = e.path.join("scripts");
    for p in files_under(&scripts)? {
        let rel = p.strip_prefix(&e.path).unwrap_or(&p).to_string_lossy().replace('\\', "/");
        set_mode(&p, if e.approved_scripts.contains(&rel) { 0o755 } else { 0o644 })?;
    }
    Ok(())
}

/// Stage a draft or provisional entry: write its files (edits relative to the entry
/// dir), the SKILL.md frontmatter with the `rematch:` block merged, `hook.json` for
/// hooks and `rematch.json`; scripts get mode 0644. Re-staging the same id overwrites;
/// a different entry already at the location is an error.
pub fn stage(ledger: &Ledger, paths: &RematchPaths, entry: &Entry, files: &[Edit]) -> Result<Entry> {
    validate_name(&entry.name)?;
    validate_name(&entry.branch)?;
    if !matches!(entry.status, Status::Draft | Status::Provisional) {
        bail!("stage accepts draft or provisional entries, got {:?}", entry.status);
    }
    let now = now_ms();
    let mut e = entry.clone();
    let dir = dir_for(paths, &e, e.status);
    if dir.join(REMATCH_JSON).is_file() {
        let existing = load_entry(&dir)?;
        if existing.id != e.id {
            bail!("{} already holds entry {}", dir.display(), existing.id);
        }
    }
    std::fs::create_dir_all(&dir)?;
    for edit in files {
        match edit {
            Edit::Write { path, content } => {
                let p = dir.join(safe_rel(path)?);
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&p, content)?;
            }
            Edit::Delete { path } => {
                let p = dir.join(safe_rel(path)?);
                if p.is_file() {
                    std::fs::remove_file(&p)?;
                }
            }
        }
    }
    let skill = dir.join("SKILL.md");
    let mut doc = if skill.is_file() {
        SkillDoc::parse(&std::fs::read_to_string(&skill)?)?
    } else {
        SkillDoc { body: format!("\n# {}\n\n{}\n", e.name, e.trigger_line), ..Default::default() }
    };
    if doc.get_str("name").is_none() {
        doc.set("name", e.name.as_str());
    }
    if doc.get_str("description").is_none_or(|d| d.trim().is_empty()) {
        doc.set("description", e.trigger_line.as_str());
    }
    std::fs::write(&skill, doc.render())?;
    if e.kind == Kind::Hook {
        let hook = e.hook.as_ref().ok_or_else(|| anyhow!("hook entry {} has no hook rule", e.id))?;
        std::fs::write(dir.join("hook.json"), serde_json::to_string_pretty(hook)?)?;
    }
    e.path = dir;
    e.updated_at = now;
    if e.created_at == 0 {
        e.created_at = now;
    }
    e.status_since.get_or_insert(now);
    persist(ledger, &e)?;
    apply_script_modes(&e)?;
    Ok(e)
}

fn get(ledger: &Ledger, id: &str) -> Result<Entry> {
    ledger.find_entry(id)?.ok_or_else(|| anyhow!("no entry {id:?}"))
}

/// Status change with the directory moves of the module doc. Automatic callers use
/// this; pinned entries are refused.
pub fn set_status(
    ledger: &Ledger,
    paths: &RematchPaths,
    id: &str,
    to: Status,
    note: Option<&str>,
) -> Result<Entry> {
    let e = get(ledger, id)?;
    if e.pinned {
        bail!("entry {} is pinned (human-owned); unpin it first", e.id);
    }
    transition(ledger, paths, e, to, note)
}

fn transition(ledger: &Ledger, paths: &RematchPaths, mut e: Entry, to: Status, note: Option<&str>) -> Result<Entry> {
    let from = e.status;
    if !can_transition(from, to) {
        bail!("illegal transition {from:?} → {to:?} for {}", e.id);
    }
    if from == to {
        return Ok(e);
    }
    let now = now_ms();
    let src = e.path.clone();
    let mut dst = dir_for(paths, &e, to);
    if matches!(to, Status::Retired | Status::Dormant | Status::Stale) {
        dst = unique_archive_dir(dst, now);
    }
    if matches!(to, Status::Kept | Status::Pinned) && !matches!(from, Status::Kept | Status::Pinned) {
        promote_into(ledger, paths, &mut e, &dst, now)?;
        copy_dir(&src, &dst)?;
        std::fs::remove_dir_all(&src)?;
        prune_empty_parent(&src);
    } else if src.is_dir() {
        move_dir(&src, &dst)?;
    } else {
        bail!("entry dir missing: {}", src.display());
    }
    if matches!(to, Status::Retired | Status::Dormant | Status::Stale) {
        remove_hook(paths, &e.id)?;
    }
    e.path = dst;
    e.status = to;
    e.status_since = Some(now);
    e.status_note = note.map(str::to_string);
    e.updated_at = now;
    persist(ledger, &e)?;
    apply_script_modes(&e)?;
    Ok(e)
}

/// Before a provisional entry takes `<skills>/<name>/`: save whatever is there to
/// `predecessors/<name>/<version>/`. A replaced rematch entry becomes dormant (its
/// files live in predecessors, ready for rollback); a pinned one blocks the keep.
fn promote_into(ledger: &Ledger, paths: &RematchPaths, e: &mut Entry, dst: &Path, now: UnixMs) -> Result<()> {
    if !dst.exists() {
        return Ok(());
    }
    let old = load_entry(dst).ok().and_then(|o| ledger.get_entry(&o.id).ok().flatten());
    if let Some(o) = &old {
        if o.pinned || o.status == Status::Pinned {
            bail!("kept entry {} ({}) is pinned; not replacing it", o.id, o.name);
        }
    }
    let version = old.as_ref().map_or(0, |o| o.version);
    let mut save = paths.predecessors.join(&e.name).join(version.to_string());
    if save.exists() {
        save = unique_archive_dir(save, now);
    }
    move_dir(dst, &save)?;
    if let Some(mut o) = old {
        remove_hook(paths, &o.id)?;
        o.status = Status::Dormant;
        o.status_since = Some(now);
        o.status_note = Some(format!("superseded by {}; predecessor for rollback", e.id));
        o.path = save;
        o.updated_at = now;
        persist(ledger, &o)?;
        e.predecessor = Some(o.id.clone());
        e.version = e.version.max(o.version + 1);
    }
    Ok(())
}

/// `activate <name>`: draft → provisional.
pub fn activate_draft(ledger: &Ledger, paths: &RematchPaths, id_or_name: &str) -> Result<Entry> {
    let e = ledger
        .list_entries(&EntryFilter { status: Some(Status::Draft), ..Default::default() })?
        .into_iter()
        .rev()
        .find(|e| e.id == id_or_name || e.name == id_or_name)
        .ok_or_else(|| anyhow!("no draft {id_or_name:?}"))?;
    transition(ledger, paths, e, Status::Provisional, Some("activated"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rollback {
    pub retired: Entry,
    /// The restored rematch entry, if the predecessor was one (else an external skill).
    pub restored: Option<Entry>,
    pub restored_version: u32,
}

/// `rollback <skill>`: restore the newest predecessor into `<skills>/<name>/` and
/// retire the current kept entry with a note.
pub fn rollback(ledger: &Ledger, paths: &RematchPaths, name: &str) -> Result<Rollback> {
    let current = ledger
        .list_entries(&EntryFilter { name: Some(name.into()), ..Default::default() })?
        .into_iter()
        .rev()
        .find(|e| matches!(e.status, Status::Kept | Status::Pinned))
        .ok_or_else(|| anyhow!("no kept entry named {name:?}"))?;
    let pred_root = paths.predecessors.join(name);
    let (version, pred_dir) = std::fs::read_dir(&pred_root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|d| d.ok())
        .filter_map(|d| Some((d.file_name().to_str()?.parse::<u32>().ok()?, d.path())))
        .max_by_key(|(v, _)| *v)
        .ok_or_else(|| anyhow!("no predecessor saved for {name:?}"))?;
    let skill_dir = paths.skills_dir().join(name);
    let note = format!("rolled back to predecessor v{version}");
    let retired = transition(ledger, paths, current, Status::Retired, Some(&note))?;
    move_dir(&pred_dir, &skill_dir)?;
    let _ = std::fs::remove_dir(&pred_root);
    let restored = match load_entry(&skill_dir).ok().and_then(|o| ledger.get_entry(&o.id).ok().flatten()) {
        Some(mut o) => {
            let now = now_ms();
            o.status = Status::Kept;
            o.status_since = Some(now);
            o.status_note = Some(format!("restored by rollback of {}", retired.id));
            o.path = skill_dir;
            o.updated_at = now;
            persist(ledger, &o)?;
            Some(o)
        }
        None => None,
    };
    Ok(Rollback { retired, restored, restored_version: version })
}

pub fn set_pinned(ledger: &Ledger, id: &str, pinned: bool) -> Result<Entry> {
    let mut e = get(ledger, id)?;
    e.pinned = pinned;
    e.updated_at = now_ms();
    persist(ledger, &e)?;
    Ok(e)
}

pub fn pin(ledger: &Ledger, id: &str) -> Result<Entry> {
    set_pinned(ledger, id, true)
}

pub fn unpin(ledger: &Ledger, id: &str) -> Result<Entry> {
    set_pinned(ledger, id, false)
}

/// `/rematch disable <id>`: user retirement (allowed on pinned entries; unpins).
pub fn disable(ledger: &Ledger, paths: &RematchPaths, id: &str, reason: &str) -> Result<Entry> {
    let mut e = get(ledger, id)?;
    e.pinned = false;
    transition(ledger, paths, e, Status::Retired, Some(&format!("disabled: {reason}")))
}

/// Declined approval retires the entry (SPEC "Statuses": retired ← approval declined).
pub fn decline(ledger: &Ledger, paths: &RematchPaths, id: &str, what: Approval) -> Result<Entry> {
    let e = get(ledger, id)?;
    let what = match what {
        Approval::Scripts => "scripts",
        Approval::Hook => "hook",
    };
    transition(ledger, paths, e, Status::Retired, Some(&format!("approval declined ({what})")))
}

#[derive(Debug, Clone, Serialize)]
struct ApprovalRecord<'a> {
    entry: &'a str,
    what: Approval,
    files: Vec<(String, String)>,
    at: UnixMs,
}

/// One-tap approval: scripts become 0755 / the hook is compiled into
/// `<agent_dir>/extensions/rematch-hooks/<id>.ts`. Records a receipt whose certificate
/// is the blob of the approval record (file hashes).
pub fn approve(ledger: &Ledger, paths: &RematchPaths, id: &str, what: Approval) -> Result<Receipt> {
    let mut e = get(ledger, id)?;
    if matches!(e.status, Status::Retired | Status::Dormant | Status::Stale | Status::Draft) {
        bail!("entry {} is {:?}; only loadable entries can be approved", e.id, e.status);
    }
    let mut hashed = Vec::new();
    match what {
        Approval::Scripts => {
            let scripts = files_under(&e.path.join("scripts"))?;
            if scripts.is_empty() {
                bail!("entry {} ships no scripts", e.id);
            }
            e.approved_scripts = scripts
                .iter()
                .map(|p| p.strip_prefix(&e.path).unwrap_or(p).to_string_lossy().replace('\\', "/"))
                .collect();
            for (p, rel) in scripts.iter().zip(&e.approved_scripts) {
                hashed.push((rel.clone(), crate::ledger::sha256_hex(&std::fs::read(p)?)));
            }
        }
        Approval::Hook => {
            let module = hook_module(&e)?;
            let dir = paths.hooks_dir();
            std::fs::create_dir_all(&dir)?;
            write_hooks_readme(&dir)?;
            let file = dir.join(format!("{}.ts", e.id));
            std::fs::write(&file, &module)?;
            hashed.push((file.to_string_lossy().to_string(), crate::ledger::sha256_hex(module.as_bytes())));
        }
    }
    let now = now_ms();
    e.updated_at = now;
    persist(ledger, &e)?;
    apply_script_modes(&e)?;
    let record = ApprovalRecord { entry: &e.id, what, files: hashed, at: now };
    let cert = ledger.put_blob(serde_json::to_string(&record)?.as_bytes())?;
    let receipt = Receipt { certificate: cert, entry: e.id.clone(), activation: Activation::Prompted };
    ledger.insert_receipt(&receipt)?;
    Ok(receipt)
}

/// The compiled verifier: `export default { id, tool, field, regex, message,
/// pass_condition, test(input) }`; `test` returns the block message or null. A leading
/// Rust-style `(?i)` becomes the JS `i` flag.
pub fn hook_module(e: &Entry) -> Result<String> {
    let h = e.hook.as_ref().ok_or_else(|| anyhow!("entry {} has no hook rule", e.id))?;
    regex::Regex::new(&h.regex).with_context(|| format!("hook regex {:?}", h.regex))?;
    if h.pass_condition.trim().is_empty() {
        bail!("hook {} has no pass condition", e.id);
    }
    let (source, flags) = match h.regex.strip_prefix("(?i)") {
        Some(rest) => (rest, "i"),
        None => (h.regex.as_str(), ""),
    };
    let j = |s: &str| serde_json::to_string(s).unwrap_or_default();
    Ok(format!(
        "// Generated by rematch for entry {name} ({id}). Approved verifier; do not edit.\n\
         // Remove with `rematch library disable {id}`.\n\
         const pattern = new RegExp({src}, {flags});\n\
         export default {{\n\
         \x20 id: {id_j},\n\
         \x20 tool: {tool},\n\
         \x20 field: {field},\n\
         \x20 regex: {regex},\n\
         \x20 message: {message},\n\
         \x20 pass_condition: {pass},\n\
         \x20 test(input) {{\n\
         \x20   const value = input == null ? undefined : input[{field}];\n\
         \x20   if (typeof value !== \"string\") return null;\n\
         \x20   return pattern.test(value) ? {message} : null;\n\
         \x20 }},\n\
         }};\n",
        name = e.name.replace('\n', " "),
        id = e.id.replace('\n', " "),
        src = j(source),
        flags = j(flags),
        id_j = j(&e.id),
        tool = j(&h.tool),
        field = j(&h.field),
        regex = j(&h.regex),
        message = j(&h.message),
        pass = j(&h.pass_condition),
    ))
}

fn write_hooks_readme(dir: &Path) -> Result<()> {
    let p = dir.join(HOOKS_README);
    if !p.exists() {
        std::fs::write(
            &p,
            "# rematch-hooks\n\nCompiled `tool_call` verifiers written by rematch after a one-tap approval.\n\
             omp does not load these files itself (non-index `.ts` in an extension subdirectory);\n\
             the rematch extension imports the ones whose entry matches the session.\n\n\
             Each module default-exports `{ id, tool, field, regex, message, pass_condition, test(input) }`;\n\
             `test` returns the block message or `null`.\n\n\
             Remove one with `rematch library disable <id>`; `rematch uninstall` removes this directory.\n",
        )?;
    }
    Ok(())
}

fn remove_hook(paths: &RematchPaths, id: &str) -> Result<()> {
    let f = paths.hooks_dir().join(format!("{id}.ts"));
    if f.exists() {
        std::fs::remove_file(f)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchedEntry {
    pub id: EntryId,
    pub name: String,
    pub status: Status,
    pub predicate_hit: bool,
}

/// Payload of `match --cwd` (DESIGN §2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MatchResult {
    pub branch: String,
    pub repo: Option<String>,
    pub skill_dirs: Vec<PathBuf>,
    pub entries: Vec<MatchedEntry>,
    pub hooks: Vec<PathBuf>,
    pub off: bool,
}

/// Load-time match (DESIGN §6): a branch's staging dir is contributed when ≥ 1 of its
/// provisional entries' repo globs matches the repo (else cwd). `entries` lists what
/// will load — provisional entries of contributed branches and every kept/pinned
/// entry — with their glob hit. `hooks` lists approved hook modules of loadable,
/// matching entries.
pub fn list_for_match(ledger: &Ledger, paths: &RematchPaths, repo: Option<&Path>, cwd: &Path) -> Result<MatchResult> {
    let mut out = MatchResult {
        branch: branch_for(repo, cwd),
        repo: repo.map(|r| r.to_string_lossy().to_string()),
        off: paths.is_off(),
        ..Default::default()
    };
    if out.off {
        return Ok(out);
    }
    let place = repo.unwrap_or(cwd).to_string_lossy().to_string();
    let provisional =
        ledger.list_entries(&EntryFilter { status: Some(Status::Provisional), ..Default::default() })?;
    let mut branches: Vec<&str> = provisional
        .iter()
        .filter(|e| matches_repo(&e.predicate, &place))
        .map(|e| e.branch.as_str())
        .collect();
    branches.sort();
    branches.dedup();
    for b in &branches {
        let d = paths.staging.join(b);
        if d.is_dir() {
            out.skill_dirs.push(d);
        }
    }
    let kept = [Status::Kept, Status::Pinned]
        .into_iter()
        .map(|s| ledger.list_entries(&EntryFilter { status: Some(s), ..Default::default() }))
        .collect::<Result<Vec<_>>>()?
        .concat();
    for e in provisional.iter().filter(|e| branches.contains(&e.branch.as_str())).chain(kept.iter()) {
        let hit = matches_repo(&e.predicate, &place);
        out.entries.push(MatchedEntry { id: e.id.clone(), name: e.name.clone(), status: e.status, predicate_hit: hit });
        let hook = paths.hooks_dir().join(format!("{}.ts", e.id));
        if hit && hook.is_file() {
            out.hooks.push(hook);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UninstallPlan {
    /// Directories to remove: kept entries the plugin wrote, `rematch-hooks/`, the root.
    pub remove: Vec<PathBuf>,
    /// `(from, to)` copies run before removal: original (non-rematch) skills that a
    /// kept entry replaced, restored from `predecessors/<name>/0/`.
    pub restore: Vec<(PathBuf, PathBuf)>,
}

/// Everything the plugin wrote and nothing else: kept skill dirs whose SKILL.md has a
/// `rematch:` block with an id known to the ledger, the rematch-hooks dir, and the root.
pub fn uninstall_plan(paths: &RematchPaths, ledger: &Ledger) -> Result<UninstallPlan> {
    let mut plan = UninstallPlan::default();
    let skills = paths.skills_dir();
    if skills.is_dir() {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&skills)?.filter_map(|d| d.ok().map(|d| d.path())).collect();
        dirs.sort();
        for d in dirs {
            let Ok(text) = std::fs::read_to_string(d.join("SKILL.md")) else { continue };
            let Some(block) = SkillDoc::parse(&text).ok().and_then(|doc| doc.rematch()) else { continue };
            if ledger.get_entry(&block.id)?.is_none() {
                continue;
            }
            let name = d.file_name().map(|n| n.to_os_string()).unwrap_or_default();
            let original = paths.predecessors.join(&name).join("0");
            if original.is_dir() {
                plan.restore.push((original, d.clone()));
            }
            plan.remove.push(d);
        }
    }
    let hooks = paths.hooks_dir();
    if hooks.exists() {
        plan.remove.push(hooks);
    }
    if paths.root.exists() {
        plan.remove.push(paths.root.clone());
    }
    Ok(plan)
}

/// Execute a plan: remove each kept dir, copy restores into place, then remove the
/// hooks dir and the root (which holds the predecessors).
pub fn apply_uninstall(plan: &UninstallPlan) -> Result<()> {
    let restored: Vec<&PathBuf> = plan.restore.iter().map(|(_, to)| to).collect();
    for d in plan.remove.iter().filter(|d| restored.contains(d)) {
        std::fs::remove_dir_all(d)?;
    }
    for (from, to) in &plan.restore {
        copy_dir(from, to)?;
    }
    for d in plan.remove.iter().filter(|d| !restored.contains(d)) {
        if d.is_dir() {
            std::fs::remove_dir_all(d)?;
        } else if d.exists() {
            std::fs::remove_file(d)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::tests::entry;
    use crate::types::HookRule;

    struct Env {
        _t: tempfile::TempDir,
        paths: RematchPaths,
        ledger: Ledger,
    }

    fn env() -> Env {
        let t = tempfile::tempdir().unwrap();
        let paths = RematchPaths::with_roots(t.path().join("rematch"), t.path().join("agent"));
        paths.ensure_dirs().unwrap();
        let ledger = Ledger::open(&paths.ledger_file).unwrap();
        Env { _t: t, paths, ledger }
    }

    fn w(path: &str, content: &str) -> Edit {
        Edit::Write { path: path.into(), content: content.into() }
    }

    fn skill(name: &str) -> String {
        format!("---\nname: {name}\ndescription: Use when testing.\ncustom: kept\n---\n# {name}\n\nUse when testing.\n")
    }

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn transition_table() {
        use Status::*;
        assert!(can_transition(Provisional, Kept));
        assert!(can_transition(Draft, Provisional));
        assert!(can_transition(Dormant, Provisional));
        assert!(can_transition(Kept, Kept));
        assert!(!can_transition(Retired, Provisional), "retired never leaves");
        assert!(!can_transition(Draft, Kept), "no keep without counters");
        assert!(!can_transition(Kept, Provisional));
        assert!(!can_transition(Kept, Dormant));
    }

    #[test]
    fn branch_names() {
        assert_eq!(branch_for(Some(Path::new("/w/My Repo!")), Path::new("/x")), "my-repo");
        assert_eq!(branch_for(None, Path::new("/w/rematch")), "rematch");
        assert_eq!(branch_for(Some(Path::new("/w/shared")), Path::new("/x")), "repo-shared");
        assert_eq!(branch_for(None, Path::new("/")), "root");
        assert_eq!(branch_for(None, Path::new("/w/.hidden")), "hidden");
    }

    #[test]
    fn stage_writes_package_and_refuses_collisions() {
        let x = env();
        let mut e = entry("e1", "ledger-skill", "repo", Status::Provisional);
        e.created_at = 0;
        let staged = stage(
            &x.ledger,
            &x.paths,
            &e,
            &[w("SKILL.md", &skill("ledger-skill")), w("scripts/check.sh", "cargo test\n"), w("units/a.md", "unit")],
        )
        .unwrap();
        let dir = x.paths.staging.join("repo/ledger-skill");
        assert_eq!(staged.path, dir);
        assert!(staged.created_at > 0 && staged.status_since.is_some());
        let doc = SkillDoc::parse(&std::fs::read_to_string(dir.join("SKILL.md")).unwrap()).unwrap();
        let b = doc.rematch().unwrap();
        assert_eq!((b.id.as_str(), b.status, b.branch.as_str()), ("e1", Status::Provisional, "repo"));
        assert_eq!(b.predicate_hash, predicate_hash(&e.predicate));
        assert_eq!(doc.get_str("custom"), Some("kept"), "unknown keys preserved");
        assert_eq!(load_entry(&dir).unwrap(), staged);
        assert_eq!(x.ledger.get_entry("e1").unwrap().unwrap(), staged);
        #[cfg(unix)]
        assert_eq!(mode(&dir.join("scripts/check.sh")), 0o644);

        let other = entry("e2", "ledger-skill", "repo", Status::Provisional);
        assert!(stage(&x.ledger, &x.paths, &other, &[]).is_err(), "different entry at same path");
        assert!(stage(&x.ledger, &x.paths, &staged, &[Edit::Delete { path: "units/a.md".into() }]).is_ok());
        assert!(!dir.join("units/a.md").exists());
        assert!(stage(&x.ledger, &x.paths, &staged, &[w("../escape", "x")]).is_err());
        assert!(stage(&x.ledger, &x.paths, &entry("e3", "k", "repo", Status::Kept), &[]).is_err());

        // missing SKILL.md is synthesized from the trigger line
        let d = stage(&x.ledger, &x.paths, &entry("e4", "bare", "repo", Status::Draft), &[]).unwrap();
        assert_eq!(d.path, x.paths.drafts.join("bare"));
        let doc = SkillDoc::parse(&std::fs::read_to_string(d.path.join("SKILL.md")).unwrap()).unwrap();
        assert_eq!(doc.get_str("description"), Some("Use when editing the ledger."));
    }

    #[test]
    fn keep_replaces_existing_skill_saving_predecessor_and_rollback_restores() {
        let x = env();
        // a hand-written skill already exists under the agent dir
        let user_dir = x.paths.skills_dir().join("ledger-skill");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(user_dir.join("SKILL.md"), "---\nname: ledger-skill\ndescription: mine\n---\nmine\n").unwrap();

        let e1 = entry("e1", "ledger-skill", "repo", Status::Provisional);
        stage(&x.ledger, &x.paths, &e1, &[w("SKILL.md", &skill("ledger-skill"))]).unwrap();
        let kept = set_status(&x.ledger, &x.paths, "e1", Status::Kept, None).unwrap();
        assert_eq!(kept.path, user_dir);
        assert!(!x.paths.staging.join("repo/ledger-skill").exists(), "staging removed");
        assert!(!x.paths.staging.join("repo").exists(), "empty branch dir pruned");
        assert_eq!(
            std::fs::read_to_string(x.paths.predecessors.join("ledger-skill/0/SKILL.md")).unwrap(),
            "---\nname: ledger-skill\ndescription: mine\n---\nmine\n"
        );
        let block = SkillDoc::parse(&std::fs::read_to_string(user_dir.join("SKILL.md")).unwrap()).unwrap().rematch().unwrap();
        assert_eq!(block.status, Status::Kept);

        // a sibling replaces the kept rematch entry
        let mut e2 = entry("e2", "ledger-skill", "repo", Status::Provisional);
        e2.sibling_of = Some("e1".into());
        stage(&x.ledger, &x.paths, &e2, &[w("SKILL.md", &skill("ledger-skill"))]).unwrap();
        let k2 = set_status(&x.ledger, &x.paths, "e2", Status::Kept, None).unwrap();
        assert_eq!(k2.predecessor.as_deref(), Some("e1"));
        assert_eq!(k2.version, 2);
        let old = x.ledger.get_entry("e1").unwrap().unwrap();
        assert_eq!(old.status, Status::Dormant);
        assert_eq!(old.path, x.paths.predecessors.join("ledger-skill/1"));

        // rollback: e2 retired with a note, e1 kept again
        let rb = rollback(&x.ledger, &x.paths, "ledger-skill").unwrap();
        assert_eq!(rb.restored_version, 1);
        assert_eq!(rb.retired.status, Status::Retired);
        assert!(rb.retired.status_note.as_deref().unwrap().contains("rolled back"));
        assert_eq!(rb.retired.path, x.paths.archive.join("retired/e2"));
        let r = rb.restored.unwrap();
        assert_eq!((r.id.as_str(), r.status, r.path.clone()), ("e1", Status::Kept, user_dir.clone()));
        assert_eq!(load_entry(&user_dir).unwrap().status, Status::Kept);

        // second rollback restores the user's original (not a rematch entry)
        let rb = rollback(&x.ledger, &x.paths, "ledger-skill").unwrap();
        assert_eq!(rb.restored_version, 0);
        assert!(rb.restored.is_none());
        assert!(std::fs::read_to_string(user_dir.join("SKILL.md")).unwrap().ends_with("mine\n"));
        assert!(rollback(&x.ledger, &x.paths, "ledger-skill").is_err(), "nothing kept left");
    }

    #[test]
    fn archive_moves_pin_disable_and_activation() {
        let x = env();
        stage(&x.ledger, &x.paths, &entry("d1", "draft-one", "repo", Status::Draft), &[]).unwrap();
        let p = activate_draft(&x.ledger, &x.paths, "draft-one").unwrap();
        assert_eq!(p.path, x.paths.staging.join("repo/draft-one"));
        assert!(!x.paths.drafts.join("draft-one").exists());

        let dm = set_status(&x.ledger, &x.paths, "d1", Status::Dormant, Some("12 matched")).unwrap();
        assert_eq!(dm.path, x.paths.archive.join("dormant/d1"));
        let back = set_status(&x.ledger, &x.paths, "d1", Status::Provisional, None).unwrap();
        assert_eq!(back.path, x.paths.staging.join("repo/draft-one"));
        let dm2 = set_status(&x.ledger, &x.paths, "d1", Status::Dormant, None).unwrap();
        assert!(dm2.path.exists());

        stage(&x.ledger, &x.paths, &entry("p1", "pinned-one", "repo", Status::Provisional), &[]).unwrap();
        pin(&x.ledger, "p1").unwrap();
        assert!(set_status(&x.ledger, &x.paths, "p1", Status::Retired, None).is_err(), "pinned refuses automation");
        assert!(load_entry(&x.paths.staging.join("repo/pinned-one")).unwrap().pinned);
        let r = disable(&x.ledger, &x.paths, "p1", "noisy").unwrap();
        assert_eq!(r.status, Status::Retired);
        assert_eq!(r.status_note.as_deref(), Some("disabled: noisy"));
        assert!(!r.pinned);
        assert!(set_status(&x.ledger, &x.paths, "p1", Status::Provisional, None).is_err(), "retired is final");

        stage(&x.ledger, &x.paths, &entry("u1", "unpinned", "repo", Status::Provisional), &[]).unwrap();
        pin(&x.ledger, "u1").unwrap();
        unpin(&x.ledger, "u1").unwrap();
        assert!(set_status(&x.ledger, &x.paths, "u1", Status::Stale, None).is_ok());
    }

    fn hook_entry(id: &str) -> Entry {
        let mut e = entry(id, &format!("hook-{id}"), "repo", Status::Provisional);
        e.kind = Kind::Hook;
        e.hook = Some(HookRule {
            tool: "bash".into(),
            field: "command".into(),
            regex: r"(?i)git\s+push\s+--force".into(),
            message: "Force push blocked: use --force-with-lease.".into(),
            pass_condition: "commands without a bare --force pass".into(),
        });
        e
    }

    #[test]
    fn approve_scripts_and_hooks_then_decline() {
        let x = env();
        stage(&x.ledger, &x.paths, &entry("s1", "scripted", "repo", Status::Provisional), &[w("scripts/run.sh", "echo ok\n")]).unwrap();
        let rc = approve(&x.ledger, &x.paths, "s1", Approval::Scripts).unwrap();
        assert_eq!(rc.activation, Activation::Prompted);
        assert!(x.ledger.get_blob(&rc.certificate).unwrap().is_some());
        assert_eq!(x.ledger.list_receipts(Some("s1")).unwrap().len(), 1);
        let e = x.ledger.get_entry("s1").unwrap().unwrap();
        assert_eq!(e.approved_scripts, vec!["scripts/run.sh".to_string()]);
        #[cfg(unix)]
        assert_eq!(mode(&e.path.join("scripts/run.sh")), 0o755);
        // approved mode survives a status move
        let dm = set_status(&x.ledger, &x.paths, "s1", Status::Dormant, None).unwrap();
        #[cfg(unix)]
        assert_eq!(mode(&dm.path.join("scripts/run.sh")), 0o755);

        stage(&x.ledger, &x.paths, &hook_entry("h1"), &[]).unwrap();
        assert!(x.paths.staging.join("repo/hook-h1/hook.json").is_file());
        approve(&x.ledger, &x.paths, "h1", Approval::Hook).unwrap();
        let module = std::fs::read_to_string(x.paths.hooks_dir().join("h1.ts")).unwrap();
        assert!(module.contains("export default {"));
        assert!(module.contains(r#"new RegExp("git\\s+push\\s+--force", "i")"#), "{module}");
        assert!(module.contains(r#"pass_condition: "commands without a bare --force pass""#));
        assert!(module.contains("test(input)"));
        assert!(x.paths.hooks_dir().join("README.md").is_file());

        let m = list_for_match(&x.ledger, &x.paths, Some(Path::new("/w/repo")), Path::new("/w/repo")).unwrap();
        assert_eq!(m.hooks, vec![x.paths.hooks_dir().join("h1.ts")]);

        let d = decline(&x.ledger, &x.paths, "h1", Approval::Hook).unwrap();
        assert_eq!(d.status, Status::Retired);
        assert!(!x.paths.hooks_dir().join("h1.ts").exists(), "retire removes the compiled hook");
        assert!(approve(&x.ledger, &x.paths, "h1", Approval::Hook).is_err());

        let mut bad = hook_entry("h2");
        bad.hook.as_mut().unwrap().pass_condition = " ".into();
        stage(&x.ledger, &x.paths, &bad, &[]).unwrap();
        assert!(approve(&x.ledger, &x.paths, "h2", Approval::Hook).is_err());
    }

    #[test]
    fn match_contributes_branch_dirs_on_glob_hit() {
        let x = env();
        let a = entry("a", "alpha", "repo", Status::Provisional); // glob /w/repo
        let mut b = entry("b", "beta", "repo", Status::Provisional);
        b.predicate.repo_glob = "/elsewhere".into();
        let mut c = entry("c", "gamma", "other", Status::Provisional);
        c.predicate.repo_glob = "/w/other".into();
        for e in [&a, &b, &c] {
            stage(&x.ledger, &x.paths, e, &[]).unwrap();
        }
        let mut k = entry("k", "kept-one", "other", Status::Provisional);
        k.predicate.repo_glob = "**".into();
        stage(&x.ledger, &x.paths, &k, &[]).unwrap();
        set_status(&x.ledger, &x.paths, "k", Status::Kept, None).unwrap();

        let m = list_for_match(&x.ledger, &x.paths, None, Path::new("/w/repo/src")).unwrap();
        assert_eq!(m.branch, "src");
        assert_eq!(m.skill_dirs, vec![x.paths.staging.join("repo")]);
        let got: Vec<(&str, bool)> = m.entries.iter().map(|e| (e.id.as_str(), e.predicate_hit)).collect();
        assert_eq!(got, vec![("a", true), ("b", false), ("k", true)]);

        let m = list_for_match(&x.ledger, &x.paths, Some(Path::new("/nowhere")), Path::new("/nowhere")).unwrap();
        assert!(m.skill_dirs.is_empty());
        assert_eq!(m.entries.len(), 1, "kept entries load natively");

        std::fs::write(&x.paths.off_marker, "").unwrap();
        let m = list_for_match(&x.ledger, &x.paths, None, Path::new("/w/repo")).unwrap();
        assert!(m.off && m.skill_dirs.is_empty() && m.entries.is_empty());
    }

    #[test]
    fn uninstall_removes_only_what_it_wrote_and_restores_originals() {
        let x = env();
        let skills = x.paths.skills_dir();
        // user's own skills: one untouched, one replaced by a kept entry
        for n in ["mine", "replaced"] {
            std::fs::create_dir_all(skills.join(n)).unwrap();
            std::fs::write(skills.join(n).join("SKILL.md"), format!("---\nname: {n}\ndescription: user\n---\nuser {n}\n")).unwrap();
        }
        // a foreign skill with a rematch block whose id the ledger does not know
        std::fs::create_dir_all(skills.join("foreign")).unwrap();
        std::fs::write(skills.join("foreign/SKILL.md"), "---\nname: foreign\nrematch:\n  id: zzz\n  status: kept\n  branch: b\n  family: a/x\n  version: 1\n  predicate_hash: h\n---\nx\n").unwrap();
        for (id, name) in [("e1", "replaced"), ("e2", "fresh")] {
            stage(&x.ledger, &x.paths, &entry(id, name, "repo", Status::Provisional), &[w("SKILL.md", &skill(name))]).unwrap();
            set_status(&x.ledger, &x.paths, id, Status::Kept, None).unwrap();
        }
        stage(&x.ledger, &x.paths, &hook_entry("h1"), &[]).unwrap();
        approve(&x.ledger, &x.paths, "h1", Approval::Hook).unwrap();

        let plan = uninstall_plan(&x.paths, &x.ledger).unwrap();
        assert_eq!(
            plan.remove,
            vec![skills.join("fresh"), skills.join("replaced"), x.paths.hooks_dir(), x.paths.root.clone()]
        );
        assert_eq!(plan.restore, vec![(x.paths.predecessors.join("replaced/0"), skills.join("replaced"))]);
        drop(x.ledger);
        apply_uninstall(&plan).unwrap();
        assert!(!x.paths.root.exists() && !x.paths.hooks_dir().exists() && !skills.join("fresh").exists());
        assert_eq!(std::fs::read_to_string(skills.join("replaced/SKILL.md")).unwrap(), "---\nname: replaced\ndescription: user\n---\nuser replaced\n");
        assert!(skills.join("mine/SKILL.md").is_file());
        assert!(skills.join("foreign/SKILL.md").is_file());
    }
}
