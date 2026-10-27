//! Entry lifecycle on disk and in the ledger (DESIGN §1, §4, §6; SPEC "Library and
//! admission"). `rematch.json` is authoritative for files, the ledger for counters.
//!
//! Locations by status:
//! - draft → `drafts/<name>/` (never loaded)
//! - provisional → `staging/<branch>/<name>/` (loaded through the shared skills dir once a
//!   synced cwd matched it)
//! - kept → `<agent_dir>/skills/<name>/` (loaded natively by omp; other harnesses through the
//!   shared skills dir); with a profile (DESIGN §20) `profile/skills/<name>/` instead, linked
//!   from `<agent_dir>/skills/<name>`; a replaced kept dir is saved to
//!   `predecessors/<name>/<version>/` (version 0 = a skill the plugin did not write)
//! - retired | dormant | stale → `archive/<status>/<id>/` (never loaded, never deleted)
//! - every loadable skill entry is linked as `skills/<name> → <entry dir>` by
//!   [`sync_shared_skills`], which every status change, stage and rollback runs (DESIGN §16
//!   "Shared skills")
//! - agents (kind `agent`: `AGENT.md` + `rematch.json`) take the same locations except
//!   kept, which is `kept-agents/<name>/` with `AGENT.md` copied to
//!   `<agent_dir>/agents/<name>.md` (with a profile: copied to `profile/agents/<name>.md`
//!   and linked from there); a provisional agent is also linked as
//!   `agents/<name>.md → <entry dir>/AGENT.md`, which omp discovers through the plugin's
//!   `extension/agents → <root>/agents` symlink. Agent predecessors live in
//!   `predecessors/.agents/<name>/<version>/` (entry names never start with `.`).
//!
//! `pinned` is a flag (human-owned): automatic transitions refuse pinned entries; the
//! entry keeps its status and location. `Status::Pinned` is accepted as a status too.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::ledger::{now_ms, EntryFilter, Ledger};
use crate::package::{RematchBlock, SkillDoc};
use crate::paths::RematchPaths;
use crate::ledger::sha256_hex;
use crate::predicate::{matches_place, predicate_hash, Place};
use crate::types::{Activation, Edit, Entry, EntryId, HookMode, Kind, Receipt, Sha256Hex, Status, UnixMs};

pub const SHARED_BRANCH: &str = "shared";
pub const REMATCH_JSON: &str = "rematch.json";
pub const HOOKS_README: &str = "README.md";
pub const SKILL_MD: &str = "SKILL.md";
pub const AGENT_MD: &str = "AGENT.md";
/// Text an entry contributes to `<repo>/AGENTS.md`, only through `ship --scope team`.
pub const AGENTS_FRAGMENT: &str = "AGENTS.fragment.md";
/// `Origin::Import { source }` of the entries the binary ships (`rematch builtins`).
pub const BUILTIN_SOURCE: &str = "builtin";

/// Shipped built-in entry (context policies as entries, DESIGN §10): counted and retired
/// like any entry, but outside the admission caps that ration proposals.
pub fn is_builtin(e: &Entry) -> bool {
    matches!(&e.origin, crate::types::Origin::Import { source } if source == BUILTIN_SOURCE)
}

/// The frontmatter document omp loads for an entry of this kind.
pub fn primary_doc(kind: Kind) -> &'static str {
    if kind == Kind::Agent {
        AGENT_MD
    } else {
        SKILL_MD
    }
}

/// `fork_path` (the file `ship --scope fork` writes) is repo-relative without traversal.
pub fn validate_fork_path(rel: &str) -> Result<PathBuf> {
    let p = PathBuf::from(rel);
    if rel.trim().is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("fork_path must be repo-relative without traversal: {rel:?}");
    }
    Ok(p)
}

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
        Status::Kept | Status::Pinned if e.kind == Kind::Agent => paths.kept_agents.join(&e.name),
        Status::Kept | Status::Pinned if crate::profile::present(paths) => crate::profile::skills_dir(paths).join(&e.name),
        Status::Kept | Status::Pinned => paths.skills_dir().join(&e.name),
        Status::Retired => paths.archive.join("retired").join(&e.id),
        Status::Dormant => paths.archive.join("dormant").join(&e.id),
        Status::Stale => paths.archive.join("stale").join(&e.id),
    }
}

/// omp's native path of a kept skill: `<agent_dir>/skills/<name>` (the kept dir itself, or with
/// a profile a link to `profile/skills/<name>`).
pub fn native_skill(paths: &RematchPaths, name: &str) -> PathBuf {
    paths.skills_dir().join(name)
}

/// Whether `path` is where a kept entry of `e`'s kind and name lives: [`dir_for`] kept, or
/// for a skill kept before the profile existed its native dir.
pub fn is_kept_slot(paths: &RematchPaths, e: &Entry, path: &Path) -> bool {
    path == dir_for(paths, e, Status::Kept) || (e.kind != Kind::Agent && path == native_skill(paths, &e.name))
}

/// With the kept dir in the profile: `<agent_dir>/skills/<name>` → it (a real dir there is
/// left alone; `promote_into` has saved it as a predecessor).
fn link_native_skill(paths: &RematchPaths, e: &Entry) -> Result<()> {
    if e.kind == Kind::Agent || !e.path.starts_with(crate::profile::dir(paths)) {
        return Ok(());
    }
    let link = native_skill(paths, &e.name);
    std::fs::create_dir_all(paths.skills_dir())?;
    match std::fs::symlink_metadata(&link) {
        Ok(md) if !md.file_type().is_symlink() => Ok(()),
        Ok(_) if std::fs::read_link(&link).is_ok_and(|t| t == e.path) => Ok(()),
        _ => relink(&e.path, &link),
    }
}

/// Remove `<agent_dir>/skills/<name>` while it is a link to `dir`.
fn unlink_native_skill(paths: &RematchPaths, name: &str, dir: &Path) -> Result<()> {
    let link = native_skill(paths, name);
    if std::fs::read_link(&link).is_ok_and(|t| t == dir) {
        remove_link(&link)?;
    }
    Ok(())
}

/// Staging link of a provisional agent (`<root>/agents/<name>.md`).
pub fn agent_link(paths: &RematchPaths, name: &str) -> PathBuf {
    paths.agents.join(format!("{name}.md"))
}

/// omp's copy of a kept agent (`<agent_dir>/agents/<name>.md`).
pub fn kept_agent_file(paths: &RematchPaths, name: &str) -> PathBuf {
    paths.agents_dir().join(format!("{name}.md"))
}

fn predecessor_root(paths: &RematchPaths, kind: Kind, name: &str) -> PathBuf {
    match kind {
        Kind::Agent => paths.predecessors.join(".agents").join(name),
        _ => paths.predecessors.join(name),
    }
}

fn remove_link(p: &Path) -> Result<()> {
    if std::fs::symlink_metadata(p).is_ok() {
        std::fs::remove_file(p).with_context(|| format!("remove {}", p.display()))?;
    }
    Ok(())
}

/// omp keeps the first agent of a name, so one name has one staging link: refuse when
/// another, still-present entry holds it.
fn check_agent_link(paths: &RematchPaths, name: &str, target: &Path) -> Result<()> {
    if let Ok(cur) = std::fs::read_link(agent_link(paths, name)) {
        if cur != target && cur.exists() {
            bail!("a provisional agent named {name} is already staged from {}", cur.display());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink_file(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).with_context(|| format!("link {}", link.display()))
}

/// Without symlinks the staged AGENT.md is copied (re-copied on every stage).
#[cfg(not(unix))]
fn symlink_file(target: &Path, link: &Path) -> Result<()> {
    std::fs::copy(target, link).with_context(|| format!("copy {}", link.display()))?;
    Ok(())
}

fn link_agent(paths: &RematchPaths, e: &Entry) -> Result<()> {
    let target = e.path.join(AGENT_MD);
    check_agent_link(paths, &e.name, &target)?;
    let link = agent_link(paths, &e.name);
    remove_link(&link)?;
    std::fs::create_dir_all(&paths.agents)?;
    symlink_file(&target, &link)
}

/// Remove the staging link of the agent staged at `dir` (or a dangling one).
fn unlink_agent(paths: &RematchPaths, name: &str, dir: &Path) -> Result<()> {
    let link = agent_link(paths, name);
    match std::fs::read_link(&link) {
        Ok(cur) if cur == dir.join(AGENT_MD) || !cur.exists() => remove_link(&link),
        Ok(_) => Ok(()),
        Err(_) if cfg!(not(unix)) => remove_link(&link),
        Err(_) => Ok(()),
    }
}

fn write_kept_agent(paths: &RematchPaths, e: &Entry) -> Result<()> {
    let f = kept_agent_file(paths, &e.name);
    std::fs::create_dir_all(paths.agents_dir())?;
    remove_link(&f)?;
    if crate::profile::present(paths) {
        let p = crate::profile::agents_dir(paths).join(format!("{}.md", e.name));
        std::fs::create_dir_all(crate::profile::agents_dir(paths))?;
        std::fs::copy(e.path.join(AGENT_MD), &p).with_context(|| format!("copy AGENT.md to {}", p.display()))?;
        return symlink_file(&p, &f);
    }
    std::fs::copy(e.path.join(AGENT_MD), &f).with_context(|| format!("copy AGENT.md to {}", f.display()))?;
    Ok(())
}

/// Remove omp's copy of a kept agent while it is still the one this entry wrote (with a
/// profile: the link and the profile's copy it points to).
fn remove_kept_agent(paths: &RematchPaths, e: &Entry) -> Result<()> {
    let f = kept_agent_file(paths, &e.name);
    let ours = std::fs::read_to_string(&f)
        .ok()
        .and_then(|t| SkillDoc::parse(&t).ok())
        .and_then(|d| d.rematch())
        .is_some_and(|b| b.id == e.id);
    if ours {
        if let Ok(t) = std::fs::read_link(&f) {
            if t.starts_with(crate::profile::agents_dir(paths)) {
                std::fs::remove_file(&t)?;
            }
        }
        std::fs::remove_file(&f)?;
    }
    Ok(())
}

/// `--adopt` of `profile init`: a kept/pinned entry rematch owns moves into the profile — a
/// skill's dir to `profile/skills/<name>/` (linked from `<agent_dir>/skills/<name>`), an agent's
/// omp copy to `profile/agents/<name>.md` (linked). `None` when there is nothing to move.
pub fn adopt_kept(ledger: &Ledger, paths: &RematchPaths, e: &Entry) -> Result<Option<Entry>> {
    if !crate::profile::present(paths) || !matches!(e.status, Status::Kept | Status::Pinned) {
        return Ok(None);
    }
    if e.kind == Kind::Agent {
        let f = kept_agent_file(paths, &e.name);
        if std::fs::read_link(&f).is_ok_and(|t| t.starts_with(crate::profile::dir(paths))) || !e.path.join(AGENT_MD).is_file() {
            return Ok(None);
        }
        write_kept_agent(paths, e)?;
        return Ok(Some(e.clone()));
    }
    let dst = dir_for(paths, e, e.status);
    if e.path == dst || !e.path.is_dir() || load_entry(&e.path).map_or(true, |x| x.id != e.id) {
        return Ok(None);
    }
    if dst.exists() {
        bail!("{} already holds {}", dst.display(), e.name);
    }
    let src = e.path.clone();
    move_dir(&src, &dst)?;
    let mut n = e.clone();
    n.path = dst;
    n.updated_at = now_ms();
    persist(ledger, &n)?;
    link_native_skill(paths, &n)?;
    sync_shared_skills(ledger, paths, None)?;
    Ok(Some(n))
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

/// Rewrite `rematch.json` and the `rematch:` block of SKILL.md / AGENT.md from the entry.
fn sync_files(e: &Entry) -> Result<()> {
    std::fs::write(e.path.join(REMATCH_JSON), serde_json::to_string_pretty(e)?)?;
    for name in [SKILL_MD, AGENT_MD] {
        let p = e.path.join(name);
        if p.is_file() {
            let mut doc = SkillDoc::parse(&std::fs::read_to_string(&p)?)?;
            doc.set_rematch(&rematch_block(e));
            std::fs::write(&p, doc.render())?;
        }
    }
    Ok(())
}

/// Content identity of an entry dir (ADR 001): sha256 of `{"files":[[path, sha256], …]}`
/// over its files sorted by `/`-separated relative path, leaving out `rematch.json` (the
/// ledger mirror), `EXPERIENCE.md` (the reflector's notes) and hidden files, with the
/// `rematch:` frontmatter block (machine-local id, status, version) removed from the
/// SKILL.md / AGENT.md bytes. Equal packages hash equal on every machine and status.
pub fn content_sha256(dir: &Path) -> Result<Sha256Hex> {
    let mut files: Vec<(String, Sha256Hex)> = Vec::new();
    for p in files_under(dir)? {
        let rel: Vec<String> = p
            .strip_prefix(dir)
            .unwrap_or(&p)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if rel.iter().any(|c| c.starts_with('.')) {
            continue;
        }
        let rel = rel.join("/");
        if rel == REMATCH_JSON || rel == crate::experience::EXPERIENCE_MD {
            continue;
        }
        let bytes = std::fs::read(&p).with_context(|| format!("read {}", p.display()))?;
        let sha = if rel == SKILL_MD || rel == AGENT_MD {
            match std::str::from_utf8(&bytes).ok().and_then(|t| SkillDoc::parse(t).ok()) {
                Some(mut doc) => {
                    doc.frontmatter.remove("rematch");
                    sha256_hex(doc.render().as_bytes())
                }
                None => sha256_hex(&bytes),
            }
        } else {
            sha256_hex(&bytes)
        };
        files.push((rel, sha));
    }
    files.sort();
    Ok(sha256_hex(serde_json::json!({ "files": files }).to_string().as_bytes()))
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
/// dir), the SKILL.md (agents: AGENT.md) frontmatter with the `rematch:` block merged,
/// `hook.json` for hooks and `rematch.json`; scripts get mode 0644; a provisional agent
/// gets its staging link. Re-staging the same id overwrites; a different entry already
/// at the location is an error.
pub fn stage(ledger: &Ledger, paths: &RematchPaths, entry: &Entry, files: &[Edit]) -> Result<Entry> {
    validate_name(&entry.name)?;
    validate_name(&entry.branch)?;
    if !matches!(entry.status, Status::Draft | Status::Provisional) {
        bail!("stage accepts draft or provisional entries, got {:?}", entry.status);
    }
    let now = now_ms();
    let mut e = entry.clone();
    let agent = e.kind == Kind::Agent;
    let dir = dir_for(paths, &e, e.status);
    if agent {
        let shipped = files.iter().any(|f| matches!(f, Edit::Write { path, .. } if path == AGENT_MD));
        if !shipped && !dir.join(AGENT_MD).is_file() {
            bail!("agent entry {} has no {AGENT_MD} (frontmatter name + description, body = system prompt)", e.name);
        }
        if e.status == Status::Provisional {
            check_agent_link(paths, &e.name, &dir.join(AGENT_MD))?;
        }
    }
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
    let primary = dir.join(primary_doc(e.kind));
    let mut doc = if primary.is_file() {
        SkillDoc::parse(&std::fs::read_to_string(&primary)?)?
    } else {
        SkillDoc { body: format!("\n# {}\n\n{}\n", e.name, e.trigger_line), ..Default::default() }
    };
    if doc.get_str("name").is_none() {
        doc.set("name", e.name.as_str());
    }
    if doc.get_str("description").is_none_or(|d| d.trim().is_empty()) {
        doc.set("description", e.trigger_line.as_str());
    }
    std::fs::write(&primary, doc.render())?;
    if e.kind == Kind::Hook {
        let hook = e.hook.as_ref().ok_or_else(|| anyhow!("hook entry {} has no hook rule", e.id))?;
        std::fs::write(dir.join("hook.json"), serde_json::to_string_pretty(hook)?)?;
    }
    e.content_sha256 = Some(content_sha256(&dir)?);
    e.path = dir;
    e.updated_at = now;
    if e.created_at == 0 {
        e.created_at = now;
    }
    e.status_since.get_or_insert(now);
    persist(ledger, &e)?;
    apply_script_modes(&e)?;
    if agent && e.status == Status::Provisional {
        link_agent(paths, &e)?;
    }
    sync_shared_skills(ledger, paths, None)?;
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
    let agent = e.kind == Kind::Agent;
    if agent && to == Status::Provisional {
        check_agent_link(paths, &e.name, &dir_for(paths, &e, to).join(AGENT_MD))?;
    }
    let now = now_ms();
    let src = e.path.clone();
    let mut dst = dir_for(paths, &e, to);
    if matches!(to, Status::Retired | Status::Dormant | Status::Stale) {
        dst = unique_archive_dir(dst, now);
    }
    if agent && from == Status::Provisional {
        unlink_agent(paths, &e.name, &src)?;
    }
    if agent && matches!(from, Status::Kept | Status::Pinned) && !matches!(to, Status::Kept | Status::Pinned) {
        remove_kept_agent(paths, &e)?;
    }
    if !agent && matches!(from, Status::Kept | Status::Pinned) && !matches!(to, Status::Kept | Status::Pinned) {
        unlink_native_skill(paths, &e.name, &src)?;
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
    if matches!(to, Status::Kept | Status::Pinned) {
        e.content_sha256 = Some(content_sha256(&dst)?);
    }
    e.path = dst;
    e.status = to;
    e.status_since = Some(now);
    e.status_note = note.map(str::to_string);
    e.updated_at = now;
    persist(ledger, &e)?;
    apply_script_modes(&e)?;
    if agent {
        match to {
            Status::Provisional => link_agent(paths, &e)?,
            Status::Kept | Status::Pinned => write_kept_agent(paths, &e)?,
            _ => {}
        }
    } else if matches!(to, Status::Kept | Status::Pinned) {
        link_native_skill(paths, &e)?;
    }
    sync_shared_skills(ledger, paths, None)?;
    Ok(e)
}

/// Before a provisional entry takes `<skills>/<name>/` (agents: `kept-agents/<name>/`):
/// save whatever is there to `predecessors/<name>/<version>/` (agents:
/// `predecessors/.agents/<name>/<version>/`; a user's own `<agent_dir>/agents/<name>.md`
/// is saved as version 0). With a profile the kept dir is `profile/skills/<name>/` and what
/// holds `<agent_dir>/skills/<name>` (a dir kept before the profile, a user's skill or link)
/// is the one replaced. A replaced rematch entry becomes dormant (its files live in
/// predecessors, ready for rollback); a pinned one blocks the keep.
fn promote_into(ledger: &Ledger, paths: &RematchPaths, e: &mut Entry, dst: &Path, now: UnixMs) -> Result<()> {
    let pred_root = predecessor_root(paths, e.kind, &e.name);
    let native_dir = native_skill(paths, &e.name);
    let occupant = if dst.exists() {
        dst.to_path_buf()
    } else if e.kind != Kind::Agent
        && dst.starts_with(crate::profile::dir(paths))
        && std::fs::symlink_metadata(&native_dir).is_ok_and(|md| {
            !md.file_type().is_symlink()
                || std::fs::read_link(&native_dir).is_ok_and(|t| !t.starts_with(crate::profile::dir(paths)))
        })
    {
        native_dir
    } else {
        let native = kept_agent_file(paths, &e.name);
        if e.kind == Kind::Agent && native.is_file() {
            let mut save = pred_root.join("0");
            if save.exists() {
                save = unique_archive_dir(save, now);
            }
            std::fs::create_dir_all(&save)?;
            std::fs::copy(&native, save.join(AGENT_MD))?;
        }
        return Ok(());
    };
    let dst = occupant.as_path();
    let old = load_entry(dst).ok().and_then(|o| ledger.get_entry(&o.id).ok().flatten());
    if let Some(o) = &old {
        if o.pinned || o.status == Status::Pinned {
            bail!("kept entry {} ({}) is pinned; not replacing it", o.id, o.name);
        }
    }
    let version = old.as_ref().map_or(0, |o| o.version);
    let mut save = pred_root.join(version.to_string());
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

/// `rollback <name>`: restore the newest predecessor into `<skills>/<name>/` (agents:
/// `kept-agents/<name>/` + `<agent_dir>/agents/<name>.md`) and retire the current kept
/// entry with a note.
pub fn rollback(ledger: &Ledger, paths: &RematchPaths, name: &str) -> Result<Rollback> {
    let current = ledger
        .list_entries(&EntryFilter { name: Some(name.into()), ..Default::default() })?
        .into_iter()
        .rev()
        .find(|e| matches!(e.status, Status::Kept | Status::Pinned))
        .ok_or_else(|| anyhow!("no kept entry named {name:?}"))?;
    let agent = current.kind == Kind::Agent;
    let pred_root = predecessor_root(paths, current.kind, name);
    let (version, pred_dir) = std::fs::read_dir(&pred_root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|d| d.ok())
        .filter_map(|d| Some((d.file_name().to_str()?.parse::<u32>().ok()?, d.path())))
        .max_by_key(|(v, _)| *v)
        .ok_or_else(|| anyhow!("no predecessor saved for {name:?}"))?;
    let home = dir_for(paths, &current, Status::Kept);
    let note = format!("rolled back to predecessor v{version}");
    let retired = transition(ledger, paths, current, Status::Retired, Some(&note))?;
    let prev = load_entry(&pred_dir).ok().and_then(|o| ledger.get_entry(&o.id).ok().flatten());
    if agent && prev.is_none() {
        // the user's own agent file that the keep replaced
        std::fs::create_dir_all(paths.agents_dir())?;
        std::fs::copy(pred_dir.join(AGENT_MD), kept_agent_file(paths, name))?;
        std::fs::remove_dir_all(&pred_dir)?;
    } else {
        move_dir(&pred_dir, &home)?;
        if !agent && home.starts_with(crate::profile::dir(paths)) && std::fs::symlink_metadata(native_skill(paths, name)).is_err() {
            std::fs::create_dir_all(paths.skills_dir())?;
            relink(&home, &native_skill(paths, name))?;
        }
    }
    let _ = std::fs::remove_dir(&pred_root);
    let restored = match prev {
        Some(mut o) => {
            let now = now_ms();
            o.status = Status::Kept;
            o.status_since = Some(now);
            o.status_note = Some(format!("restored by rollback of {}", retired.id));
            o.path = home;
            o.updated_at = now;
            persist(ledger, &o)?;
            if agent {
                write_kept_agent(paths, &o)?;
            }
            Some(o)
        }
        None => None,
    };
    sync_shared_skills(ledger, paths, None)?;
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
            // With a profile the module is written there and linked (DESIGN §20).
            if crate::profile::present(paths) {
                let target = crate::profile::hooks_dir(paths).join(format!("{}.ts", e.id));
                std::fs::create_dir_all(crate::profile::hooks_dir(paths))?;
                std::fs::write(&target, &module)?;
                remove_link(&file)?;
                symlink_file(&target, &file)?;
            } else {
                remove_link(&file)?;
                std::fs::write(&file, &module)?;
            }
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

/// The compiled hook: data only, `export default { id, mode, tool, field, regex, message,
/// pass_condition, params }`; the rematch extension implements every mode. `params` is
/// [`HookRule::params`], and for `block` the JS form of the regex (`{source, flags}`: a
/// leading Rust-style `(?i)` becomes the `i` flag). Refuses a rule with
/// [`crate::lint::hook_problems`] (benign probes of the default lint config).
pub fn hook_module(e: &Entry) -> Result<String> {
    let h = e.hook.as_ref().ok_or_else(|| anyhow!("entry {} has no hook rule", e.id))?;
    let probes = crate::lint::LintConfig::default().hooks.probes;
    if let Some(p) = crate::lint::hook_problems(h, &probes).into_iter().next() {
        bail!("hook {}: {p}", e.id);
    }
    let params = match h.mode {
        HookMode::Block => {
            let (source, flags) = match h.regex.strip_prefix("(?i)") {
                Some(rest) => (rest, "i"),
                None => (h.regex.as_str(), ""),
            };
            serde_json::json!({"source": source, "flags": flags})
        }
        _ => serde_json::to_value(h.params())?,
    };
    let j = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let mode = crate::ledger::enum_str(&h.mode);
    Ok(format!(
        "// Generated by rematch for entry {name} ({id}). Approved {mode} hook: data only, the rematch\n\
         // extension implements the mode. Do not edit; remove with `rematch library disable {id}`.\n\
         export default {{\n\
         \x20 id: {id_j},\n\
         \x20 mode: {mode_j},\n\
         \x20 tool: {tool},\n\
         \x20 field: {field},\n\
         \x20 regex: {regex},\n\
         \x20 message: {message},\n\
         \x20 pass_condition: {pass},\n\
         \x20 params: {params},\n\
         }};\n",
        name = e.name.replace('\n', " "),
        id = e.id.replace('\n', " "),
        id_j = j(&e.id),
        mode_j = j(&mode),
        tool = j(&h.tool),
        field = j(&h.field),
        regex = j(&h.regex),
        message = j(&h.message),
        pass = j(&h.pass_condition),
        params = serde_json::to_string(&params)?,
    ))
}

const HOOKS_README_TEXT: &str = "# rematch-hooks\n\nCompiled hooks written by rematch after a one-tap approval.\n\
     omp does not load these files itself (non-index `.ts` in an extension subdirectory);\n\
     the rematch extension imports the ones whose entry matches the session.\n\n\
     Each module is data only: `export default { id, mode, tool, field, regex, message, pass_condition, params }`.\n\
     The extension implements the modes: `block` and `stuck` on `tool_call`, `truncate` and\n\
     `retrieve` on `context` (docs/DESIGN.md §10).\n\n\
     Remove one with `rematch library disable <id>`; `rematch uninstall` removes this directory.\n";

fn write_hooks_readme(dir: &Path) -> Result<()> {
    let p = dir.join(HOOKS_README);
    if std::fs::read_to_string(&p).ok().as_deref() != Some(HOOKS_README_TEXT) {
        std::fs::write(&p, HOOKS_README_TEXT)?;
    }
    Ok(())
}

fn remove_hook(paths: &RematchPaths, id: &str) -> Result<()> {
    remove_link(&paths.hooks_dir().join(format!("{id}.ts")))?;
    remove_link(&crate::profile::hooks_dir(paths).join(format!("{id}.ts")))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchedEntry {
    pub id: EntryId,
    pub name: String,
    pub kind: Kind,
    pub status: Status,
    pub predicate_hit: bool,
    #[serde(default = "one_version")]
    pub version: u32,
    /// The entry's `content_sha256` (the trace's versioned `loaded_entries`, ADR 001).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<Sha256Hex>,
}

fn one_version() -> u32 {
    1
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
    /// The place's declared domain (`policy.json` `domains`, else the branch's `domain.yml`);
    /// the session's extensions and stems are unknown at load time, so no inference here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// `REMATCH_ROOT/profile/.git` exists: the extension runs `profile sync` at session start
    /// only then (DESIGN §20).
    #[serde(default)]
    pub profile_present: bool,
}

/// A hook entry is active only while its approved module exists: before approval
/// nothing of it runs, so it neither contributes its branch dir nor counts.
pub fn hook_active(paths: &RematchPaths, e: &Entry) -> bool {
    e.kind != Kind::Hook || paths.hooks_dir().join(format!("{}.ts", e.id)).is_file()
}

/// A provisional, non-agent, active entry whose predicate place matches `place`
/// ([`matches_place`]: remote + subpath, else the repo glob): what a load-time match contributes.
fn provisional_hit(paths: &RematchPaths, e: &Entry, place: &Place) -> bool {
    e.status == Status::Provisional && e.kind != Kind::Agent && hook_active(paths, e) && matches_place(&e.predicate, place)
}

/// Load-time match (DESIGN §6): a branch's staging dir is contributed when ≥ 1 of its
/// provisional non-agent, active ([`hook_active`]) entries' predicates match the place
/// ([`Place::locate`]: the repo's remote and the cwd's subpath, else the repo (else cwd)
/// against the glob; its declared domain gates entries that carry one). `entries` lists what
/// will load — provisional entries of contributed
/// branches, every provisional agent (omp discovers them through the plugin root in
/// every session) and every kept/pinned entry — with their glob hit (false for an
/// unapproved hook, which must not count). `hooks` lists approved hook modules of
/// loadable, matching entries. `skill_dirs` stays in the payload for compatibility;
/// harnesses load skills from the shared dir ([`sync_shared_skills`]). `domain` is the
/// place's declared domain (`domain::place_domain`).
pub fn list_for_match(ledger: &Ledger, paths: &RematchPaths, repo: Option<&Path>, cwd: &Path) -> Result<MatchResult> {
    let mut out = MatchResult {
        branch: branch_for(repo, cwd),
        repo: repo.map(|r| r.to_string_lossy().to_string()),
        off: paths.is_off(),
        profile_present: crate::profile::present(paths),
        ..Default::default()
    };
    if out.off {
        return Ok(out);
    }
    let mut place = Place::locate(repo, cwd);
    place.domain = crate::domain::place_domain(paths, &place, &out.branch);
    out.domain = place.domain.clone();
    let provisional =
        ledger.list_entries(&EntryFilter { status: Some(Status::Provisional), ..Default::default() })?;
    let mut branches: Vec<&str> = provisional
        .iter()
        .filter(|e| provisional_hit(paths, e, &place))
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
    let loaded = provisional.iter().filter(|e| e.kind == Kind::Agent || branches.contains(&e.branch.as_str()));
    for e in loaded.chain(kept.iter()) {
        let hit = matches_place(&e.predicate, &place) && hook_active(paths, e);
        out.entries.push(MatchedEntry {
            id: e.id.clone(),
            name: e.name.clone(),
            kind: e.kind,
            status: e.status,
            predicate_hit: hit,
            version: e.version,
            sha: e.content_sha256.clone(),
        });
        if hit && e.kind == Kind::Hook {
            out.hooks.push(paths.hooks_dir().join(format!("{}.ts", e.id)));
        }
    }
    Ok(out)
}

/// What [`sync_shared_skills`] did to `REMATCH_ROOT/skills/` (DESIGN §16 "Shared skills").
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SkillsSync {
    /// Names this sync linked or re-pointed.
    pub added: Vec<String>,
    /// Names whose link this sync removed.
    pub pruned: Vec<String>,
    /// Links in the directory after the sync.
    pub total: usize,
    /// Skill directories a harness loads: the shared dir (none while rematch is off).
    pub dirs: Vec<PathBuf>,
    /// `name → entry dir` of every link after the sync.
    pub links: BTreeMap<String, PathBuf>,
    /// Names in the directory that are not symlinks: never touched.
    pub untouched: Vec<String>,
    /// Entry ids behind the links after the sync: what a harness scanning the dir now sees
    /// (`session_start.visible_entries`).
    #[serde(default)]
    pub visible: Vec<EntryId>,
    pub off: bool,
}

impl SkillsSync {
    /// A harness that discovered its skills before this sync must re-scan to see it.
    pub fn changed(&self) -> bool {
        !self.added.is_empty() || !self.pruned.is_empty()
    }
}

/// An entry the shared dir may link: rematch-owned (its dir holds its own `rematch.json`, so a
/// user skill registered in place is never linked), with a `SKILL.md`, not an agent (reached
/// through `agents/`) or a lesson (a library record), and a hook only once approved.
fn shareable(paths: &RematchPaths, e: &Entry) -> bool {
    !matches!(e.kind, Kind::Agent | Kind::Lesson)
        && hook_active(paths, e)
        && e.path.join(SKILL_MD).is_file()
        && load_entry(&e.path).is_ok_and(|x| x.id == e.id)
}

/// Linked whatever cwd synced: kept and pinned entries (status or human pin).
fn always_shared(e: &Entry) -> bool {
    matches!(e.status, Status::Kept | Status::Pinned) || e.pinned
}

#[cfg(unix)]
pub fn symlink_dir(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).with_context(|| format!("link {} → {}", link.display(), target.display()))
}

#[cfg(windows)]
pub fn symlink_dir(target: &Path, link: &Path) -> Result<()> {
    std::os::windows::fs::symlink_dir(target, link).with_context(|| format!("link {} → {}", link.display(), target.display()))
}

/// Atomic per link: a hidden temporary symlink renamed over `link`, so a reader sees the old
/// target or the new one, never a missing name.
pub fn relink(target: &Path, link: &Path) -> Result<()> {
    let name = link.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let tmp = link.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()));
    symlink_dir(target, &tmp)?;
    std::fs::rename(&tmp, link).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!(err).context(format!("rename {} → {}", tmp.display(), link.display()))
    })
}

/// Make `REMATCH_ROOT/skills/` hold one symlink `<name> → <entry dir>` per loadable entry
/// ([`shareable`]): every kept/pinned entry, and every provisional entry whose predicate
/// matched a `place` (repo, else cwd) synced so far — union semantics: a provisional link is
/// never removed because this `place` does not match (another session elsewhere may load it),
/// only when its entry stops being provisional/kept/pinned (retired, dormant, stale, draft,
/// deleted, moved). With `place` = `None` (status changes) nothing provisional is added.
/// One name has one link: a provisional entry matched by this sync takes it; a provisional
/// candidate already linked shadows the kept entry of its name until it is kept or retired.
/// The profile's kept sources (`profile::kept_sources`: every `profile/skills/<name>/`, kept on
/// this machine or another, or adopted) fill names nothing else holds. Only symlinks are
/// touched; hidden names (another sync's temporary links) are skipped. While rematch is off
/// every link is removed and `dirs` is empty.
pub fn sync_shared_skills(ledger: &Ledger, paths: &RematchPaths, place: Option<&Place>) -> Result<SkillsSync> {
    let dir = &paths.skills_shared;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let off = paths.is_off();
    let live: Vec<Entry> = if off {
        Vec::new()
    } else {
        [Status::Provisional, Status::Kept, Status::Pinned]
            .into_iter()
            .map(|s| ledger.list_entries(&EntryFilter { status: Some(s), ..Default::default() }))
            .collect::<Result<Vec<_>>>()?
            .concat()
            .into_iter()
            .filter(|e| shareable(paths, e))
            .collect()
    };
    let (mut current, mut untouched) = (BTreeMap::new(), Vec::new());
    for ent in std::fs::read_dir(dir)? {
        let ent = ent?;
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if ent.file_type()?.is_symlink() {
            current.insert(name, std::fs::read_link(ent.path())?);
        } else {
            untouched.push(name);
        }
    }
    let sources = if off { Vec::new() } else { crate::profile::kept_sources(paths) };
    let holder = |name: &str, target: &Path| live.iter().find(|e| e.name == name && e.path == target);
    let from_profile = |name: &str, target: &Path| sources.iter().any(|(n, p)| n == name && p == target);
    let mut want: BTreeMap<String, PathBuf> = current
        .iter()
        .filter(|(n, t)| holder(n, t).is_some() || from_profile(n, t))
        .map(|(n, t)| (n.clone(), t.clone()))
        .collect();
    for e in live.iter().filter(|e| always_shared(e)) {
        let shadowed = want.get(&e.name).and_then(|t| holder(&e.name, t)).is_some_and(|h| !always_shared(h));
        if !shadowed {
            want.insert(e.name.clone(), e.path.clone());
        }
    }
    for (name, src) in &sources {
        want.entry(name.clone()).or_insert_with(|| src.clone());
    }
    if let Some(place) = place {
        for e in live.iter().filter(|e| provisional_hit(paths, e, place)) {
            want.insert(e.name.clone(), e.path.clone());
        }
    }
    for n in &untouched {
        want.remove(n);
    }
    let mut out = SkillsSync { off, untouched, ..Default::default() };
    for (name, seen) in &current {
        let link = dir.join(name);
        // Re-read before removing: a concurrent sync may have re-pointed it to a live entry.
        if !want.contains_key(name) && std::fs::read_link(&link).is_ok_and(|t| &t == seen) {
            remove_link(&link)?;
            out.pruned.push(name.clone());
        }
    }
    for (name, target) in &want {
        if current.get(name) != Some(target) {
            relink(target, &dir.join(name))?;
            out.added.push(name.clone());
        }
    }
    out.total = want.len();
    out.visible = want.iter().filter_map(|(n, t)| holder(n, t)).map(|e| e.id.clone()).collect();
    out.links = want;
    if !off {
        out.dirs.push(dir.clone());
    }
    Ok(out)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UninstallPlan {
    /// Paths to remove: kept skill dirs and kept agent files the plugin wrote, the
    /// provisional-agent link dir, the shared skills dir, `rematch-hooks/`, the root.
    pub remove: Vec<PathBuf>,
    /// `(from, to)` copies run before removal: original (non-rematch) skills and agent
    /// files that a kept entry replaced, restored from the version-0 predecessor.
    pub restore: Vec<(PathBuf, PathBuf)>,
}

fn sorted_children(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)?.filter_map(|d| d.ok().map(|d| d.path())).collect();
    v.sort();
    Ok(v)
}

/// The `rematch:` block of a markdown file names an entry the ledger knows.
fn written_by_us(ledger: &Ledger, md: &Path) -> Result<bool> {
    let Ok(text) = std::fs::read_to_string(md) else { return Ok(false) };
    let Some(block) = SkillDoc::parse(&text).ok().and_then(|doc| doc.rematch()) else { return Ok(false) };
    Ok(ledger.get_entry(&block.id)?.is_some())
}

/// The file carries a `rematch:` block (written by rematch on this machine or another).
fn has_rematch_block(md: &Path) -> bool {
    std::fs::read_to_string(md).ok().and_then(|t| SkillDoc::parse(&t).ok()).and_then(|d| d.rematch()).is_some()
}

/// Everything the plugin wrote and nothing else: kept skill dirs whose SKILL.md (and
/// kept `<agent_dir>/agents/*.md` files whose frontmatter) has a `rematch:` block with
/// an id known to the ledger, the provisional-agent link dir, the shared skills dir (links
/// only; removing it never follows them), the rematch-hooks dir, and the root. The profile
/// (`REMATCH_ROOT/profile/`, the user's git repo) stays: only the links into it go, and an
/// adopted hand-written skill or agent (no `rematch:` block) is restored as a copy in place
/// of its link.
pub fn uninstall_plan(paths: &RematchPaths, ledger: &Ledger) -> Result<UninstallPlan> {
    let mut plan = UninstallPlan::default();
    let profile_dir = crate::profile::dir(paths);
    let into_profile = |p: &Path| std::fs::read_link(p).ok().filter(|t| t.starts_with(&profile_dir));
    for d in sorted_children(&paths.skills_dir())? {
        let name = d.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        let original = paths.predecessors.join(&name).join("0");
        if let Some(target) = into_profile(&d) {
            if !has_rematch_block(&target.join(SKILL_MD)) {
                plan.restore.push((target, d.clone()));
            } else if original.is_dir() {
                plan.restore.push((original, d.clone()));
            }
            plan.remove.push(d);
            continue;
        }
        if !written_by_us(ledger, &d.join(SKILL_MD))? {
            continue;
        }
        if original.is_dir() {
            plan.restore.push((original, d.clone()));
        }
        plan.remove.push(d);
    }
    for f in sorted_children(&paths.agents_dir())? {
        if f.extension().is_none_or(|x| x != "md") {
            continue;
        }
        let name = f.file_stem().and_then(|n| n.to_str()).unwrap_or_default();
        let original = predecessor_root(paths, Kind::Agent, name).join("0").join(AGENT_MD);
        if let Some(target) = into_profile(&f) {
            if !has_rematch_block(&target) {
                plan.restore.push((target, f.clone()));
            } else if original.is_file() {
                plan.restore.push((original, f.clone()));
            }
            plan.remove.push(f);
            continue;
        }
        if !written_by_us(ledger, &f)? {
            continue;
        }
        if original.is_file() {
            plan.restore.push((original, f.clone()));
        }
        plan.remove.push(f);
    }
    if paths.agents.exists() {
        plan.remove.push(paths.agents.clone());
    }
    if paths.skills_shared.exists() {
        plan.remove.push(paths.skills_shared.clone());
    }
    let hooks = paths.hooks_dir();
    if hooks.exists() {
        plan.remove.push(hooks);
    }
    if profile_dir.exists() {
        plan.remove.extend(sorted_children(&paths.root)?.into_iter().filter(|p| p != &profile_dir));
    } else if paths.root.exists() {
        plan.remove.push(paths.root.clone());
    }
    Ok(plan)
}

fn remove_path(p: &Path) -> Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p)?,
        Ok(_) => std::fs::remove_file(p)?,
        Err(_) => {}
    }
    Ok(())
}

/// Execute a plan: remove each replaced kept path, copy restores into place, then
/// remove the rest (link dir, hooks dir, the root, which holds the predecessors).
pub fn apply_uninstall(plan: &UninstallPlan) -> Result<()> {
    let restored: Vec<&PathBuf> = plan.restore.iter().map(|(_, to)| to).collect();
    for d in plan.remove.iter().filter(|d| restored.contains(d)) {
        remove_path(d)?;
    }
    for (from, to) in &plan.restore {
        if from.is_dir() {
            copy_dir(from, to)?;
        } else {
            std::fs::copy(from, to).with_context(|| format!("restore {}", to.display()))?;
        }
    }
    for d in plan.remove.iter().filter(|d| !restored.contains(d)) {
        remove_path(d)?;
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
    fn with_a_profile_keep_writes_there_links_back_rolls_back_and_uninstall_keeps_it() {
        let x = env();
        let profile = crate::profile::dir(&x.paths);
        std::fs::create_dir_all(profile.join(".git")).unwrap();
        let user_dir = x.paths.skills_dir().join("ledger-skill");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(user_dir.join("SKILL.md"), "---\nname: ledger-skill\ndescription: mine\n---\nmine\n").unwrap();
        let home = profile.join("skills/ledger-skill");

        // The keep goes to the profile; the user's native dir is the replaced occupant.
        stage(&x.ledger, &x.paths, &entry("e1", "ledger-skill", "repo", Status::Provisional), &[w("SKILL.md", &skill("ledger-skill"))]).unwrap();
        let k1 = set_status(&x.ledger, &x.paths, "e1", Status::Kept, None).unwrap();
        assert_eq!(k1.path, home);
        assert_eq!(std::fs::read_link(&user_dir).unwrap(), home);
        assert!(std::fs::read_to_string(x.paths.predecessors.join("ledger-skill/0/SKILL.md")).unwrap().ends_with("mine\n"));
        assert_eq!(std::fs::read_link(x.paths.skills_shared.join("ledger-skill")).unwrap(), home);
        assert!(is_kept_slot(&x.paths, &k1, &home) && is_kept_slot(&x.paths, &k1, &user_dir));

        // A sibling replaces it in place; rollback restores e1 there and relinks.
        let mut e2 = entry("e2", "ledger-skill", "repo", Status::Provisional);
        e2.sibling_of = Some("e1".into());
        stage(&x.ledger, &x.paths, &e2, &[w("SKILL.md", &skill("ledger-skill"))]).unwrap();
        assert_eq!(set_status(&x.ledger, &x.paths, "e2", Status::Kept, None).unwrap().path, home);
        let rb = rollback(&x.ledger, &x.paths, "ledger-skill").unwrap();
        assert_eq!(rb.restored.unwrap().path, home);
        assert_eq!(std::fs::read_link(&user_dir).unwrap(), home);

        // An approved hook is written into the profile and linked.
        stage(&x.ledger, &x.paths, &hook_entry("h1"), &[]).unwrap();
        approve(&x.ledger, &x.paths, "h1", Approval::Hook).unwrap();
        let hook = x.paths.hooks_dir().join("h1.ts");
        assert_eq!(std::fs::read_link(&hook).unwrap(), profile.join("hooks/h1.ts"));
        assert!(hook_active(&x.paths, &x.ledger.get_entry("h1").unwrap().unwrap()));
        disable(&x.ledger, &x.paths, "h1", "test").unwrap();
        assert!(std::fs::symlink_metadata(&hook).is_err() && !profile.join("hooks/h1.ts").exists());

        // The user's original comes back into the profile; retiring a kept entry drops its link.
        let rb = rollback(&x.ledger, &x.paths, "ledger-skill").unwrap();
        assert_eq!(rb.restored_version, 0);
        assert!(std::fs::read_to_string(home.join("SKILL.md")).unwrap().ends_with("mine\n"));
        assert_eq!(std::fs::read_link(&user_dir).unwrap(), home);

        // Uninstall: the profile stays; the hand-written skill is restored as a copy.
        let plan = uninstall_plan(&x.paths, &x.ledger).unwrap();
        assert!(plan.restore.contains(&(home.clone(), user_dir.clone())), "{plan:?}");
        assert!(plan.remove.iter().all(|p| !p.starts_with(&profile) && p != &x.paths.root), "{plan:?}");
        apply_uninstall(&plan).unwrap();
        assert!(profile.join(".git").is_dir() && home.join("SKILL.md").is_file());
        assert!(std::fs::symlink_metadata(&user_dir).unwrap().is_dir());
        assert!(!x.paths.ledger_file.exists());
    }

    #[test]
    fn with_a_profile_a_kept_agent_is_copied_there_and_linked() {
        let x = env();
        let profile = crate::profile::dir(&x.paths);
        std::fs::create_dir_all(profile.join(".git")).unwrap();
        stage(&x.ledger, &x.paths, &agent_entry("a1", "log-triage"), &[w("AGENT.md", &agent_md("log-triage", "Triage."))]).unwrap();
        set_status(&x.ledger, &x.paths, "a1", Status::Kept, None).unwrap();
        let f = kept_agent_file(&x.paths, "log-triage");
        let copy = profile.join("agents/log-triage.md");
        assert_eq!(std::fs::read_link(&f).unwrap(), copy);
        assert!(std::fs::read_to_string(&copy).unwrap().contains("Triage."));
        set_status(&x.ledger, &x.paths, "a1", Status::Retired, None).unwrap();
        assert!(std::fs::symlink_metadata(&f).is_err() && !copy.exists());
    }

    #[test]
    fn content_sha256_is_set_at_stage_and_keep_and_ignores_status_and_mirror() {
        let x = env();
        let files = [w("SKILL.md", &skill("hashed")), w("units/a.md", "Unit.\n"), w("scripts/run.sh", "echo ok\n")];
        let a = stage(&x.ledger, &x.paths, &entry("h1", "hashed", "repo", Status::Provisional), &files).unwrap();
        let sha = a.content_sha256.clone().expect("set at stage");
        assert_eq!(content_sha256(&a.path).unwrap(), sha);
        assert_eq!(x.ledger.get_entry("h1").unwrap().unwrap().content_sha256.as_deref(), Some(sha.as_str()));
        // Same files under another id, branch and status: the same content.
        let b = stage(&x.ledger, &x.paths, &entry("h2", "hashed", "other", Status::Draft), &files).unwrap();
        assert_eq!(b.content_sha256.as_deref(), Some(sha.as_str()), "the rematch: block and rematch.json are left out");
        let kept = set_status(&x.ledger, &x.paths, "h1", Status::Kept, None).unwrap();
        assert_eq!(kept.content_sha256.as_deref(), Some(sha.as_str()), "recomputed at keep, unchanged content");
        std::fs::write(kept.path.join("units/a.md"), "Changed.\n").unwrap();
        std::fs::write(kept.path.join(".DS_Store"), "x").unwrap();
        let changed = content_sha256(&kept.path).unwrap();
        assert_ne!(changed, sha, "a unit edit changes it");
        std::fs::write(kept.path.join(crate::experience::EXPERIENCE_MD), "notes").unwrap();
        assert_eq!(content_sha256(&kept.path).unwrap(), changed, "EXPERIENCE.md and hidden files are not content");
        // The shared dir reports what it exposes.
        let s = sync_shared_skills(&x.ledger, &x.paths, None).unwrap();
        assert_eq!(s.visible, vec!["h1".to_string()]);
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
        e.hook = Some(HookRule::block(
            "bash",
            "command",
            r"(?i)git\s+push\s+--force",
            "Force push blocked: use --force-with-lease.",
            "commands without a bare --force pass",
        ));
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
        // unapproved: nothing of the hook loads or counts
        let m = list_for_match(&x.ledger, &x.paths, Some(Path::new("/w/repo")), Path::new("/w/repo")).unwrap();
        assert!(m.skill_dirs.is_empty() && m.hooks.is_empty(), "{m:?}");
        assert!(m.entries.iter().all(|e| e.id != "h1"), "{m:?}");
        approve(&x.ledger, &x.paths, "h1", Approval::Hook).unwrap();
        let module = std::fs::read_to_string(x.paths.hooks_dir().join("h1.ts")).unwrap();
        assert!(module.contains("export default {"));
        assert!(module.contains(r#"mode: "block","#), "{module}");
        assert!(module.contains(r#"params: {"flags":"i","source":"git\\s+push\\s+--force"},"#), "{module}");
        assert!(module.contains(r#"pass_condition: "commands without a bare --force pass""#));
        assert!(!module.contains("test("), "data only: {module}");
        assert!(std::fs::read_to_string(x.paths.hooks_dir().join("README.md")).unwrap().contains("data only"));

        let m = list_for_match(&x.ledger, &x.paths, Some(Path::new("/w/repo")), Path::new("/w/repo")).unwrap();
        assert_eq!(m.hooks, vec![x.paths.hooks_dir().join("h1.ts")]);
        assert_eq!(m.skill_dirs, vec![x.paths.staging.join("repo")]);
        assert!(m.entries.iter().any(|e| e.id == "h1" && e.predicate_hit), "{m:?}");

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
    fn hook_module_per_mode_is_data_only() {
        use crate::types::HookMode;
        let mut e = hook_entry("m1");
        let base = e.hook.clone().unwrap();
        let module = |e: &Entry| hook_module(e).unwrap();

        let mut tr = HookRule { mode: HookMode::Truncate, tool: "*".into(), field: String::new(), regex: String::new(), ..base.clone() };
        tr.max_age_turns = Some(6);
        tr.keep_chars = Some(1200);
        e.hook = Some(tr);
        let m = module(&e);
        assert!(m.contains(r#"mode: "truncate","#) && m.contains(r#"tool: "*","#), "{m}");
        assert!(m.contains(r#"params: {"keep_chars":1200,"max_age_turns":6},"#), "{m}");
        assert!(m.contains(r#"field: "","#) && m.contains(r#"regex: "","#), "{m}");

        let st = HookRule { mode: HookMode::Stuck, tool: "*".into(), field: String::new(), regex: String::new(), ..base.clone() };
        e.hook = Some(st);
        let m = module(&e);
        assert!(m.contains(r#"mode: "stuck","#) && m.contains(r#"params: {"repeats":3},"#), "defaults resolved: {m}");

        let mut rt = HookRule { mode: HookMode::Retrieve, tool: "*".into(), field: String::new(), regex: String::new(), ..base.clone() };
        rt.budget_bytes = Some(6000);
        e.hook = Some(rt);
        let m = module(&e);
        assert!(m.contains(r#"mode: "retrieve","#) && m.contains(r#"params: {"budget_bytes":6000,"min_overlap":2},"#), "{m}");
        assert!(!m.contains("function") && !m.contains("=>") && !m.contains("RegExp"), "no logic: {m}");

        // invalid parameters refuse to compile
        let mut bad = HookRule { mode: HookMode::Stuck, tool: "*".into(), field: String::new(), regex: String::new(), ..base };
        bad.repeats = Some(1);
        e.hook = Some(bad);
        assert!(hook_module(&e).unwrap_err().to_string().contains("repeats"));
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
        // agents: the user's own file, one the plugin replaced (original v0), one it wrote
        let agents = x.paths.agents_dir();
        std::fs::create_dir_all(&agents).unwrap();
        for n in ["mine-agent", "replaced-agent"] {
            std::fs::write(agents.join(format!("{n}.md")), agent_md(n, "user")).unwrap();
        }
        for (id, name) in [("a1", "replaced-agent"), ("a2", "fresh-agent")] {
            stage(&x.ledger, &x.paths, &agent_entry(id, name), &[w("AGENT.md", &agent_md(name, "ours"))]).unwrap();
            set_status(&x.ledger, &x.paths, id, Status::Kept, None).unwrap();
        }
        stage(&x.ledger, &x.paths, &agent_entry("a3", "staged-agent"), &[w("AGENT.md", &agent_md("staged-agent", "p"))]).unwrap();
        // kept skills are linked in the shared dir; removing it never follows the links
        assert_eq!(std::fs::read_link(x.paths.skills_shared.join("fresh")).unwrap(), skills.join("fresh"));

        let plan = uninstall_plan(&x.paths, &x.ledger).unwrap();
        assert_eq!(
            plan.remove,
            vec![
                skills.join("fresh"),
                skills.join("replaced"),
                agents.join("fresh-agent.md"),
                agents.join("replaced-agent.md"),
                x.paths.agents.clone(),
                x.paths.skills_shared.clone(),
                x.paths.hooks_dir(),
                x.paths.root.clone()
            ]
        );
        assert_eq!(
            plan.restore,
            vec![
                (x.paths.predecessors.join("replaced/0"), skills.join("replaced")),
                (x.paths.predecessors.join(".agents/replaced-agent/0/AGENT.md"), agents.join("replaced-agent.md")),
            ]
        );
        drop(x.ledger);
        apply_uninstall(&plan).unwrap();
        assert!(!x.paths.root.exists() && !x.paths.hooks_dir().exists() && !skills.join("fresh").exists());
        assert_eq!(std::fs::read_to_string(skills.join("replaced/SKILL.md")).unwrap(), "---\nname: replaced\ndescription: user\n---\nuser replaced\n");
        assert!(skills.join("mine/SKILL.md").is_file());
        assert!(skills.join("foreign/SKILL.md").is_file());
        assert!(!agents.join("fresh-agent.md").exists());
        assert_eq!(std::fs::read_to_string(agents.join("replaced-agent.md")).unwrap(), agent_md("replaced-agent", "user"));
        assert_eq!(std::fs::read_to_string(agents.join("mine-agent.md")).unwrap(), agent_md("mine-agent", "user"));
    }

    #[test]
    fn shared_skills_union_shadow_prune_and_off() {
        let x = env();
        let dir = x.paths.skills_shared.clone();
        let names = |s: &SkillsSync| (s.added.clone(), s.pruned.clone(), s.total);
        let link = |n: &str| std::fs::read_link(dir.join(n)).ok();
        stage(&x.ledger, &x.paths, &entry("a", "alpha", "repo", Status::Provisional), &[w("SKILL.md", &skill("alpha"))]).unwrap();
        let mut b = entry("b", "beta", "other", Status::Provisional);
        b.predicate.repo_glob = "/w/other".into();
        stage(&x.ledger, &x.paths, &b, &[w("SKILL.md", &skill("beta"))]).unwrap();
        assert_eq!(link("alpha"), None, "staging alone matched no cwd");

        let s = sync_shared_skills(&x.ledger, &x.paths, Some(&Place::path("/w/repo"))).unwrap();
        assert_eq!((names(&s), s.dirs.clone()), ((vec!["alpha".into()], vec![], 1), vec![dir.clone()]));
        // union: another cwd adds its own and keeps alpha
        let s = sync_shared_skills(&x.ledger, &x.paths, Some(&Place::path("/w/other"))).unwrap();
        assert_eq!(names(&s), (vec!["beta".into()], vec![], 2));
        assert!(!sync_shared_skills(&x.ledger, &x.paths, Some(&Place::path("/w/repo"))).unwrap().changed(), "idempotent");
        // a directory that is not a link is never touched
        std::fs::create_dir_all(dir.join("mine")).unwrap();
        assert_eq!(sync_shared_skills(&x.ledger, &x.paths, None).unwrap().untouched, vec!["mine".to_string()]);

        // keep: the link follows the entry to its kept dir
        set_status(&x.ledger, &x.paths, "a", Status::Kept, None).unwrap();
        assert_eq!(link("alpha"), Some(x.paths.skills_dir().join("alpha")));
        // a matched provisional candidate of the same name shadows it until retired
        let mut a2 = entry("a2", "alpha", "repo2", Status::Provisional);
        a2.predicate.repo_glob = "/w/repo".into();
        stage(&x.ledger, &x.paths, &a2, &[w("SKILL.md", &skill("alpha"))]).unwrap();
        sync_shared_skills(&x.ledger, &x.paths, Some(&Place::path("/w/repo"))).unwrap();
        assert_eq!(link("alpha"), Some(x.paths.staging.join("repo2/alpha")));
        sync_shared_skills(&x.ledger, &x.paths, None).unwrap();
        assert_eq!(link("alpha"), Some(x.paths.staging.join("repo2/alpha")), "a status-change sync keeps the candidate");
        set_status(&x.ledger, &x.paths, "a2", Status::Retired, None).unwrap();
        assert_eq!(link("alpha"), Some(x.paths.skills_dir().join("alpha")));
        // retire: pruned
        set_status(&x.ledger, &x.paths, "b", Status::Retired, None).unwrap();
        assert_eq!(link("beta"), None);

        // off: every link goes, the user's dir stays
        std::fs::write(&x.paths.off_marker, "").unwrap();
        let s = sync_shared_skills(&x.ledger, &x.paths, Some(&Place::path("/w/repo"))).unwrap();
        assert_eq!((names(&s), s.off, s.dirs.is_empty()), ((vec![], vec!["alpha".into()], 0), true, true));
        assert!(dir.join("mine").is_dir());
    }

    fn agent_md(name: &str, prompt: &str) -> String {
        format!("---\nname: {name}\ndescription: Use for a large build log; returns the first failing step.\nmodel: \"@smol\"\n---\n{prompt}\n")
    }

    fn agent_entry(id: &str, name: &str) -> Entry {
        let mut e = entry(id, name, "repo", Status::Provisional);
        e.kind = Kind::Agent;
        e
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn agents_link_while_provisional_copy_when_kept_and_roll_back() {
        let x = env();
        assert!(stage(&x.ledger, &x.paths, &agent_entry("a0", "log-triage"), &[]).is_err(), "AGENT.md required");
        // the user's own agent of that name, replaced by the keep
        let native = kept_agent_file(&x.paths, "log-triage");
        std::fs::create_dir_all(x.paths.agents_dir()).unwrap();
        std::fs::write(&native, agent_md("log-triage", "mine")).unwrap();

        let a1 = stage(&x.ledger, &x.paths, &agent_entry("a1", "log-triage"), &[w("AGENT.md", &agent_md("log-triage", "v1"))]).unwrap();
        let dir = x.paths.staging.join("repo/log-triage");
        assert_eq!(a1.path, dir);
        assert!(!dir.join("SKILL.md").exists(), "an agent entry is AGENT.md + rematch.json");
        let link = agent_link(&x.paths, "log-triage");
        assert_eq!(std::fs::read_link(&link).unwrap(), dir.join("AGENT.md"));
        let doc = SkillDoc::parse(&read(&link)).unwrap();
        assert_eq!(doc.get_str("name"), Some("log-triage"));
        assert_eq!(doc.get_str("model"), Some("@smol"), "unknown keys preserved");
        assert_eq!(doc.rematch().unwrap().id, "a1");
        assert!(doc.body.contains("v1"));

        let mut twin = agent_entry("a9", "log-triage");
        twin.branch = "other".into();
        let err = stage(&x.ledger, &x.paths, &twin, &[w("AGENT.md", &agent_md("log-triage", "x"))]).unwrap_err();
        assert!(err.to_string().contains("already staged"), "{err}");
        assert!(!x.paths.staging.join("other/log-triage").exists(), "refused before writing");

        let m = list_for_match(&x.ledger, &x.paths, Some(Path::new("/w/repo")), Path::new("/w/repo")).unwrap();
        assert!(m.skill_dirs.is_empty(), "agents contribute no skill dir");
        let got: Vec<(&str, Kind, bool)> = m.entries.iter().map(|e| (e.id.as_str(), e.kind, e.predicate_hit)).collect();
        assert_eq!(got, vec![("a1", Kind::Agent, true)]);
        let m = list_for_match(&x.ledger, &x.paths, Some(Path::new("/elsewhere")), Path::new("/elsewhere")).unwrap();
        assert_eq!(m.entries.iter().map(|e| e.predicate_hit).collect::<Vec<_>>(), vec![false], "listed (it loads), not counted");

        let k1 = set_status(&x.ledger, &x.paths, "a1", Status::Kept, None).unwrap();
        assert_eq!(k1.path, x.paths.kept_agents.join("log-triage"));
        assert!(std::fs::symlink_metadata(&link).is_err(), "staging link removed");
        assert!(!dir.exists());
        assert!(read(&native).contains("v1"));
        assert_eq!(SkillDoc::parse(&read(&native)).unwrap().rematch().unwrap().status, Status::Kept);
        assert_eq!(read(&x.paths.predecessors.join(".agents/log-triage/0/AGENT.md")), agent_md("log-triage", "mine"));

        stage(&x.ledger, &x.paths, &agent_entry("a2", "log-triage"), &[w("AGENT.md", &agent_md("log-triage", "v2"))]).unwrap();
        let k2 = set_status(&x.ledger, &x.paths, "a2", Status::Kept, None).unwrap();
        assert_eq!((k2.predecessor.as_deref(), k2.version), (Some("a1"), 2));
        assert!(read(&native).contains("v2"));
        let old = x.ledger.get_entry("a1").unwrap().unwrap();
        assert_eq!((old.status, old.path), (Status::Dormant, x.paths.predecessors.join(".agents/log-triage/1")));

        let rb = rollback(&x.ledger, &x.paths, "log-triage").unwrap();
        assert_eq!(rb.retired.path, x.paths.archive.join("retired/a2"));
        let r = rb.restored.unwrap();
        assert_eq!((r.id.as_str(), r.status, r.path), ("a1", Status::Kept, x.paths.kept_agents.join("log-triage")));
        assert!(read(&native).contains("v1"));
        let rb = rollback(&x.ledger, &x.paths, "log-triage").unwrap();
        assert_eq!((rb.restored_version, rb.restored.is_none()), (0, true));
        assert_eq!(read(&native), agent_md("log-triage", "mine"), "the user's own agent is back");
        assert!(rollback(&x.ledger, &x.paths, "log-triage").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn agent_links_follow_draft_activate_retire_and_kept_retire() {
        let x = env();
        stage(&x.ledger, &x.paths, &Entry { status: Status::Draft, ..agent_entry("d1", "draft-agent") }, &[w("AGENT.md", &agent_md("draft-agent", "d"))]).unwrap();
        let link = agent_link(&x.paths, "draft-agent");
        assert!(std::fs::symlink_metadata(&link).is_err(), "drafts never load");
        let p = activate_draft(&x.ledger, &x.paths, "draft-agent").unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), p.path.join("AGENT.md"));
        let d = set_status(&x.ledger, &x.paths, "d1", Status::Dormant, None).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err());
        let back = set_status(&x.ledger, &x.paths, "d1", Status::Provisional, None).unwrap();
        assert_ne!(d.path, back.path);
        assert_eq!(std::fs::read_link(&link).unwrap(), back.path.join("AGENT.md"));
        disable(&x.ledger, &x.paths, "d1", "noisy").unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err(), "retired agents unload");

        stage(&x.ledger, &x.paths, &agent_entry("k1", "kept-agent"), &[w("AGENT.md", &agent_md("kept-agent", "k"))]).unwrap();
        set_status(&x.ledger, &x.paths, "k1", Status::Kept, None).unwrap();
        let native = kept_agent_file(&x.paths, "kept-agent");
        assert!(native.is_file());
        set_status(&x.ledger, &x.paths, "k1", Status::Stale, None).unwrap();
        assert!(!native.exists(), "a stale kept agent leaves omp's agent dir");
    }
}
