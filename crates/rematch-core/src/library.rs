//! Entry lifecycle on disk and in the ledger (DESIGN §1, §4, §6; SPEC "Library and
//! admission"). `rematch.json` is authoritative for files, the ledger for counters.
//!
//! Locations by status:
//! - draft → `drafts/<name>/` (never loaded)
//! - provisional → `staging/<branch>/<name>/` (loaded through the view of each place whose
//!   sessions its predicate matches, [`sync_view`]; never through the shared dir)
//! - kept → `<agent_dir>/skills/<name>/` (loaded natively by omp; other harnesses through the
//!   shared skills dir); with a profile (DESIGN §20) `profile/skills/<name>/` instead, linked
//!   from `<agent_dir>/skills/<name>`; a replaced kept dir is saved to
//!   `predecessors/<name>/<version>/` (version 0 = a skill the plugin did not write)
//! - retired | dormant | stale → `archive/<status>/<id>/` (never loaded, never deleted)
//! - every kept/pinned skill entry is linked as `skills/<name> → <entry dir>` by
//!   [`sync_shared_skills`], which every status change, stage and rollback runs (DESIGN §16
//!   "Shared skills"); a place's view `views/<key>/skills/` adds that place's provisional
//!   entries ([`sync_view`]), so a session never lists another repo's trial
//! - a skill name with a place scope (`policy.json` `scopes`, `domain::PlaceScope`) is never
//!   linked where every session looks (the shared dir, `<agent_dir>/skills`): only a matching
//!   place's view and repo trials link it, whatever holds it (provisional, kept, profile source)
//! - agents (kind `agent`: `AGENT.md` + `rematch.json`) take the same locations except
//!   kept, which is `kept-agents/<name>/` with `AGENT.md` copied to
//!   `<agent_dir>/agents/<name>.md` (with a profile: copied to `profile/agents/<name>.md`
//!   and linked from there); a provisional agent, once approved ([`agent_approved`]), is
//!   also linked as `agents/<name>.md → <entry dir>/AGENT.md`, which omp discovers through
//!   the plugin's `extension/agents → <root>/agents` symlink; a session outside the agent's
//!   places withholds it ([`agent_exposure`]) and `off` removes every link. Agent predecessors live in
//!   `predecessors/.agents/<name>/<version>/` (entry names never start with `.`).
//!
//! `pinned` is a flag (human-owned): automatic transitions refuse pinned entries; the
//! entry keeps its status and location. `Status::Pinned` is accepted as a status too.
//!
//! One-tap approvals ([`pending_parts`], [`approve`]) gate what runs or reaches a prompt
//! beyond a skill: scripts (mode 0755 once approved), a hook (its compiled module), a
//! provisional agent (`approved_agent`: the sha of its AGENT.md) and `AGENTS.fragment.md` as
//! personal system-prompt text (`approved_fragment`: its sha; [`prompt_fragments`]). An edit
//! to an approved AGENT.md or fragment makes it pending again.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::{scope_allows, Scopes};
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
/// An entry's eval suite dir (`evals/evals.json`, `rematch eval`): test material, never
/// loaded and not part of the content identity ([`content_sha256`]).
pub const EVALS_DIR: &str = "evals";
/// Text an entry contributes, after an exact-hash approval, to the user's personal
/// `<agent_dir>/AGENTS.md` (`instructions`, omp) and to the system prompt of matched main
/// sessions in harnesses without that file ([`prompt_fragments`]), or to `<repo>/AGENTS.md`
/// only through an explicit `ship --scope team`.
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

/// omp / host tool names that are strictly read-only: a provisional subagent whose `tools:`
/// frontmatter is a non-empty subset of this set is exposed in matching places without approval.
pub const READ_ONLY_AGENT_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "find",
    "lsp",
    "lsp_diagnostics",
    "lsp_hover",
    "lsp_goto_definition",
    "lsp_find_references",
    "lsp_document_symbols",
    "lsp_workspace_symbols",
    "lsp_call_hierarchy",
    "lsp_code_actions",
    "web_search",
    "fetch",
    "web_fetch",
];

/// Tools listed in an `AGENT.md` `tools:` frontmatter (comma-separated string or YAML sequence).
/// `None` when the frontmatter cannot be read or carries no `tools` key (which inherits the
/// session's tools and is therefore write-capable).
pub fn agent_tools_of(dir: &Path) -> Option<Vec<String>> {
    use serde_yaml::Value as Y;
    let text = std::fs::read_to_string(dir.join(AGENT_MD)).ok()?;
    let doc = SkillDoc::parse(&text).ok()?;
    let raw: Vec<String> = match doc.frontmatter.get("tools")? {
        Y::String(s) => s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        Y::Sequence(xs) => xs.iter().filter_map(Y::as_str).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        _ => return None,
    };
    (!raw.is_empty()).then_some(raw)
}

/// Whether a subagent's `AGENT.md` declares a non-empty `tools:` list that is a subset of
/// [`READ_ONLY_AGENT_TOOLS`].
pub fn agent_is_read_only(e: &Entry) -> bool {
    if e.kind != Kind::Agent {
        return false;
    }
    agent_tools_of(&e.path).is_some_and(|tools| {
        !tools.is_empty()
            && tools.iter().all(|t| READ_ONLY_AGENT_TOOLS.iter().any(|ro| ro.eq_ignore_ascii_case(t)))
    })
}

/// Every `HookMode` variant (`block`, `stuck`, `truncate`, `retrieve`, `check`) compiles to a
/// data-only module that the rematch extension evaluates: no arbitrary code runs.
pub fn is_data_only_hook(e: &Entry) -> bool {
    e.kind == Kind::Hook && e.hook.is_some()
}

/// Compile and write `e`'s data-only hook module into `<agent_dir>/extensions/rematch-hooks/<id>.ts`
/// (or into `profile/hooks/<id>.ts` linked from there when a profile is present). Returns the
/// `(module_path, sha256)` written.
pub fn compile_hook(paths: &RematchPaths, e: &Entry) -> Result<(PathBuf, Sha256Hex)> {
    let module = hook_module(e)?;
    let dir = paths.hooks_dir();
    std::fs::create_dir_all(&dir)?;
    write_hooks_readme(&dir)?;
    let file = dir.join(format!("{}.ts", e.id));
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
    let sha = crate::ledger::sha256_hex(module.as_bytes());
    Ok((file, sha))
}

/// `fork_path` (the file `ship --scope fork` writes) is repo-relative without traversal.
pub fn validate_fork_path(rel: &str) -> Result<PathBuf> {
    let p = PathBuf::from(rel);
    if rel.trim().is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("fork_path must be repo-relative without traversal: {rel:?}");
    }
    Ok(p)
}

/// What a one-tap approval enables (one part of an entry; [`pending_parts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    /// Files under `scripts/` become executable.
    Scripts,
    /// The hook rule is compiled into `rematch-hooks/<id>.ts`.
    Hook,
    /// A provisional subagent loads (its staging link, Claude Code's plugin copy, `match`).
    Agent,
    /// `AGENTS.fragment.md` joins the system prompt of main sessions the entry matches.
    Prompt,
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
/// left alone; `promote_into` has saved it as a predecessor). A name with a place scope
/// (`policy.json` `scopes`) is not linked: omp loads that dir in every session.
fn link_native_skill(paths: &RematchPaths, e: &Entry) -> Result<()> {
    if e.kind == Kind::Agent || !e.path.starts_with(crate::profile::dir(paths)) || crate::domain::load_scopes(paths)?.contains_key(&e.name) {
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
        unlink(p).with_context(|| format!("remove {}", p.display()))?;
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

/// Windows: needs Developer Mode (or admin) like every other rematch link.
#[cfg(windows)]
fn symlink_file(target: &Path, link: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(target, link).with_context(|| format!("link {}", link.display()))
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
        Err(_) => Ok(()),
    }
}

/// Remove `agents/<name>.md` while it is this entry's AGENT.md (a link to it, or a copy
/// carrying its `rematch:` block without symlinks): a provisional agent that is not, or no
/// longer, approved. `true` when removed.
fn unlink_unapproved_agent(paths: &RematchPaths, e: &Entry) -> Result<bool> {
    let link = agent_link(paths, &e.name);
    let ours = std::fs::read_link(&link).is_ok_and(|t| t == e.path.join(AGENT_MD))
        || std::fs::read_to_string(&link)
            .ok()
            .and_then(|t| SkillDoc::parse(&t).ok())
            .and_then(|d| d.rematch())
            .is_some_and(|b| b.id == e.id);
    if ours {
        remove_link(&link)?;
    }
    Ok(ours)
}

/// Remove the staging links of provisional agents that are not approved ([`agent_approved`]):
/// links made before approvals existed and agents whose AGENT.md changed since. `skills sync`
/// runs it; stage and status moves gate their own entry. Returns the names unlinked.
pub fn unlink_unapproved_agents(ledger: &Ledger, paths: &RematchPaths) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for e in ledger.list_entries(&EntryFilter { status: Some(Status::Provisional), ..Default::default() })? {
        if e.kind == Kind::Agent && !agent_approved(&e) && unlink_unapproved_agent(paths, &e)? {
            out.push(e.name);
        }
    }
    Ok(out)
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
        unlink(&f)?;
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
    sync_shared_skills(ledger, paths)?;
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
/// ledger mirror), `EXPERIENCE.md` (the reflector's notes), `evals/` (the eval suite: test
/// material, never loaded) and hidden files, with the `rematch:` frontmatter block
/// (machine-local id, status, version) removed from the SKILL.md / AGENT.md bytes. Equal
/// packages hash equal on every machine and status.
pub fn content_sha256(dir: &Path) -> Result<Sha256Hex> {
    let mut files: Vec<(String, Sha256Hex)> = Vec::new();
    for p in files_under(dir)? {
        let rel: Vec<String> = p
            .strip_prefix(dir)
            .unwrap_or(&p)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if rel.iter().any(|c| c.starts_with('.')) || rel.first().is_some_and(|c| c == EVALS_DIR) {
            continue;
        }
        let rel = rel.join("/");
        if rel == REMATCH_JSON || rel == crate::experience::EXPERIENCE_MD {
            continue;
        }
        let bytes = std::fs::read(&p).with_context(|| format!("read {}", p.display()))?;
        let sha = if rel == SKILL_MD || rel == AGENT_MD { doc_sha(&bytes) } else { sha256_hex(&bytes) };
        files.push((rel, sha));
    }
    files.sort();
    Ok(sha256_hex(serde_json::json!({ "files": files }).to_string().as_bytes()))
}

/// sha256 of a SKILL.md / AGENT.md without its `rematch:` frontmatter block (machine-local
/// id, status, version), so a status move leaves it unchanged; unparseable text hashes whole.
fn doc_sha(bytes: &[u8]) -> Sha256Hex {
    match std::str::from_utf8(bytes).ok().and_then(|t| SkillDoc::parse(t).ok()) {
        Some(mut doc) => {
            doc.frontmatter.remove("rematch");
            sha256_hex(doc.render().as_bytes())
        }
        None => sha256_hex(bytes),
    }
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
/// gets its staging link once approved ([`agent_approved`]; an unapproved one loses a link it
/// had). Re-staging the same id overwrites; a different entry already
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
        // A description rematch writes is catalog text: one plain bounded line.
        let limit = crate::lint::LintConfig::default().limits.description_bytes;
        doc.set("description", crate::package::catalog_line(&e.trigger_line, limit));
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
    if matches!(e.status, Status::Provisional | Status::Kept | Status::Pinned) {
        if is_data_only_hook(&e) {
            compile_hook(paths, &e)?;
        }
        e = apply_entry_auto(ledger, paths, e)?;
    }
    apply_script_modes(&e)?;
    if agent && e.status == Status::Provisional {
        if agent_approved(&e) {
            link_agent(paths, &e)?;
        } else {
            unlink_unapproved_agent(paths, &e)?;
        }
    }
    sync_shared_skills(ledger, paths)?;
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
    if matches!(to, Status::Provisional | Status::Kept | Status::Pinned) {
        if is_data_only_hook(&e) {
            compile_hook(paths, &e)?;
        }
        e = apply_entry_auto(ledger, paths, e)?;
    }
    apply_script_modes(&e)?;
    if agent {
        match to {
            Status::Provisional if agent_approved(&e) => link_agent(paths, &e)?,
            Status::Provisional => {
                unlink_unapproved_agent(paths, &e)?;
            }
            Status::Kept | Status::Pinned => write_kept_agent(paths, &e)?,
            _ => {}
        }
    } else if matches!(to, Status::Kept | Status::Pinned) {
        link_native_skill(paths, &e)?;
    }
    sync_shared_skills(ledger, paths)?;
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
    sync_shared_skills(ledger, paths)?;
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

/// Declined approval retires the entry (SPEC "Statuses": retired ← approval declined); `what`
/// is the parts that were pending, named in the status note.
pub fn decline(ledger: &Ledger, paths: &RematchPaths, id: &str, what: &[Approval]) -> Result<Entry> {
    let e = get(ledger, id)?;
    let note = match what {
        [] => "approval declined".to_string(),
        parts => format!("approval declined ({})", parts_label(parts)),
    };
    transition(ledger, paths, e, Status::Retired, Some(&note))
}

/// Parts as the digest names them: `scripts`, `hook`, `agent`, `prompt` joined with `+`.
pub fn parts_label(parts: &[Approval]) -> String {
    parts.iter().map(crate::ledger::enum_str).collect::<Vec<_>>().join("+")
}

/// Whether rematch owns the entry's directory: it lives under REMATCH_ROOT, or it is a kept
/// copy whose SKILL.md carries this entry's `rematch:` block. Hand-written skills registered
/// by `improve`/`import --forge` (even under `~/.omp/agent/skills`) are not.
pub fn owns_dir(paths: &RematchPaths, e: &Entry) -> bool {
    if e.path.starts_with(&paths.root) {
        return true;
    }
    std::fs::read_to_string(e.path.join(SKILL_MD))
        .ok()
        .and_then(|t| SkillDoc::parse(&t).ok())
        .and_then(|d| d.rematch())
        .is_some_and(|b| b.id == e.id)
}

/// Files under the entry's `scripts/` that are not in `approved_scripts`, as
/// `(relative path with '/', file)`, sorted.
pub fn unapproved_scripts(e: &Entry) -> Vec<(String, PathBuf)> {
    files_under(&e.path.join("scripts"))
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.strip_prefix(&e.path).unwrap_or(&p).to_string_lossy().replace('\\', "/"), p))
        .filter(|(rel, _)| !e.approved_scripts.contains(rel))
        .collect()
}

/// sha256 of the entry's AGENT.md without its `rematch:` block (status moves rewrite that
/// block; they do not change what was approved). `None` without the file.
fn agent_sha(dir: &Path) -> Option<Sha256Hex> {
    std::fs::read(dir.join(AGENT_MD)).ok().map(|b| doc_sha(&b))
}

/// sha256 of the entry's `AGENTS.fragment.md`; `None` without the file.
fn fragment_sha(dir: &Path) -> Option<Sha256Hex> {
    std::fs::read(dir.join(AGENTS_FRAGMENT)).ok().map(|b| sha256_hex(&b))
}

/// The entry's `AGENTS.fragment.md` is the text approved for the personal system prompt
/// (its sha equals `approved_fragment`).
pub fn fragment_approved(e: &Entry) -> bool {
    e.approved_fragment.is_some() && fragment_sha(&e.path) == e.approved_fragment
}

/// The entry's AGENT.md is the one approved (`approved_agent`: its sha without the
/// `rematch:` block).
pub fn agent_approved(e: &Entry) -> bool {
    (e.approved_agent.is_some() && agent_sha(&e.path) == e.approved_agent) || agent_is_read_only(e)
}

/// Apply automatic entry approvals on stage or status transition: read-only subagent exposure,
/// standing `policy.json` `auto` rules for `agents`, `scripts`, and `instructions`.
pub fn apply_entry_auto(ledger: &Ledger, paths: &RematchPaths, mut e: Entry) -> Result<Entry> {
    if !matches!(e.status, Status::Provisional | Status::Kept | Status::Pinned) || e.kind == Kind::Lesson {
        return Ok(e);
    }
    let rules = crate::domain::load_auto_rules(paths).unwrap_or_default();
    if e.kind == Kind::Agent && e.status == Status::Provisional {
        let sha = agent_sha(&e.path);
        if e.approved_agent != sha || sha.is_none() {
            let approver = if agent_is_read_only(&e) {
                Some("read-only".to_string())
            } else {
                crate::domain::matching_auto_rule(&rules, crate::domain::AutoSurface::Agents, &e.predicate)
                    .map(|r| format!("rule:{}:{}", r.surface.as_str(), r.scope))
            };
            if let Some(why) = approver {
                if approve_with(ledger, paths, &e.id, Approval::Agent, Some(&why), None).is_ok() {
                    e = get(ledger, &e.id)?;
                }
            }
        }
    }
    let owned = owns_dir(paths, &e);
    if owned && !unapproved_scripts(&e).is_empty() {
        if let Some(r) = crate::domain::matching_auto_rule(&rules, crate::domain::AutoSurface::Scripts, &e.predicate) {
            let why = format!("rule:{}:{}", r.surface.as_str(), r.scope);
            if approve_with(ledger, paths, &e.id, Approval::Scripts, Some(&why), None).is_ok() {
                e = get(ledger, &e.id)?;
            }
        } else {
            let _ = crate::sandbox::run_entry_scripts(ledger, &e);
        }
    }
    if owned
        && e.path.join(AGENTS_FRAGMENT).is_file()
        && crate::domain::matching_auto_rule(&rules, crate::domain::AutoSurface::Instructions, &e.predicate).is_some()
    {
        // Over budget or unwritable: the fragment stays a review item (`instructions:<id>`).
        if let Err(err) = crate::instructions::apply_auto_rules(ledger, paths) {
            eprintln!("rematch: auto rule for {} not applied: {err:#}", e.name);
        }
        e = get(ledger, &e.id)?;
    }
    Ok(e)
}

/// Every part of a provisional, kept or pinned entry that waits for the one-tap approval,
/// in the order scripts, hook, agent, prompt: unapproved files under `scripts/` (rematch-owned
/// dirs only, [`owns_dir`]), a hook that is not data-only and lacks its compiled module, a
/// provisional write-capable agent whose AGENT.md is not approved (read-only, kept and pinned
/// agents load as they are), and an `AGENTS.fragment.md` (rematch-owned dirs only) that is not
/// approved. Lessons and entries that cannot load have none.
pub fn pending_parts(paths: &RematchPaths, e: &Entry) -> Vec<Approval> {
    let mut out = Vec::new();
    if e.kind == Kind::Lesson || !matches!(e.status, Status::Provisional | Status::Kept | Status::Pinned) {
        return out;
    }
    let owned = owns_dir(paths, e);
    if owned && !unapproved_scripts(e).is_empty() {
        out.push(Approval::Scripts);
    }
    if e.kind == Kind::Hook && !is_data_only_hook(e) && !hook_active(paths, e) {
        out.push(Approval::Hook);
    }
    if e.kind == Kind::Agent && e.status == Status::Provisional && !agent_approved(e) {
        out.push(Approval::Agent);
    }
    if owned && e.path.join(AGENTS_FRAGMENT).is_file() && !fragment_approved(e) {
        out.push(Approval::Prompt);
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub entry: String,
    pub what: Approval,
    pub files: Vec<(String, String)>,
    pub at: UnixMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<serde_json::Value>,
}

/// Latest [`ApprovalRecord`] recorded for `(entry_id, what)`.
pub fn latest_approval_record(ledger: &Ledger, entry_id: &str, what: Approval) -> Option<ApprovalRecord> {
    let receipts = ledger.list_receipts(Some(entry_id)).ok()?;
    for r in receipts.into_iter().rev() {
        let Some(blob) = ledger.get_blob(&r.receipt.certificate).ok().flatten() else { continue };
        let Ok(rec) = serde_json::from_slice::<ApprovalRecord>(&blob) else { continue };
        if rec.what == what {
            return Some(rec);
        }
    }
    None
}
pub fn approve(ledger: &Ledger, paths: &RematchPaths, id: &str, what: Approval) -> Result<Receipt> {
    approve_with(ledger, paths, id, what, None, None)
}

/// [`approve`] recording an optional `approver` (`"read-only"`, `"rule:…"`)
/// and `observed` sandbox evidence in the receipt certificate blob.
pub fn approve_with(
    ledger: &Ledger,
    paths: &RematchPaths,
    id: &str,
    what: Approval,
    approver: Option<&str>,
    observed: Option<serde_json::Value>,
) -> Result<Receipt> {
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
            let (file, sha) = compile_hook(paths, &e)?;
            hashed.push((file.to_string_lossy().to_string(), sha));
        }
        Approval::Agent => {
            if e.kind != Kind::Agent {
                bail!("entry {} is not an agent", e.id);
            }
            let sha = agent_sha(&e.path).ok_or_else(|| anyhow!("entry {} has no {AGENT_MD}", e.id))?;
            // Approval makes its task-tool line always on where it matches: the budget again,
            // against what is exposed now (other approvals may have filled it since staging).
            let cost = crate::lint::entry_cost(&e.path, Kind::Agent).catalog;
            check_always_on(ledger, paths, &e, cost, "agent's task-tool line")?;
            hashed.push((AGENT_MD.to_string(), sha.clone()));
            e.approved_agent = Some(sha);
        }
        Approval::Prompt => {
            let sha = fragment_sha(&e.path).ok_or_else(|| anyhow!("entry {} has no {AGENTS_FRAGMENT}", e.id))?;
            let c = crate::lint::entry_cost(&e.path, e.kind);
            let cost = c.catalog + c.fragment;
            check_always_on(ledger, paths, &e, cost, "AGENTS.fragment.md")?;
            hashed.push((AGENTS_FRAGMENT.to_string(), sha.clone()));
            e.approved_fragment = Some(sha);
        }
    }
    let now = now_ms();
    e.updated_at = now;
    persist(ledger, &e)?;
    apply_script_modes(&e)?;
    if what == Approval::Agent && e.status == Status::Provisional {
        link_agent(paths, &e)?;
    }
    let record = ApprovalRecord {
        entry: e.id.clone(),
        what,
        files: hashed,
        at: now,
        approver: approver.map(str::to_string),
        observed,
    };
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

/// Remove an entry's compiled hook (`rematch-hooks/<id>.ts` and, with a profile, the
/// `profile/hooks/<id>.ts` it links to): what retiring, dormancy and staleness undo.
pub fn remove_hook(paths: &RematchPaths, id: &str) -> Result<()> {
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
    /// The personal `<agent_dir>/AGENTS.md` sections omp loads now (`instructions::exposure`;
    /// empty while off). Only omp reads that file.
    #[serde(default)]
    pub instructions: crate::instructions::Exposure,
}

/// A hook entry is active only while its approved module exists: before approval
/// nothing of it runs, so it neither contributes its branch dir nor counts.
pub fn hook_active(paths: &RematchPaths, e: &Entry) -> bool {
    !paths.is_off() && (e.kind != Kind::Hook || paths.hooks_dir().join(format!("{}.ts", e.id)).is_file())
}

/// A provisional, non-agent, active entry whose predicate place matches `place`
/// ([`matches_place`]: remote + subpath, else the repo glob) and whose name's place scope, if
/// any, matches it too (`domain::scope_allows`): what a load-time match contributes.
fn provisional_hit(paths: &RematchPaths, e: &Entry, place: &Place, scopes: &Scopes) -> bool {
    e.status == Status::Provisional
        && e.kind != Kind::Agent
        && hook_active(paths, e)
        && matches_place(&e.predicate, place)
        && scope_allows(scopes, &e.name, place)
}

/// Load-time match (DESIGN §6): what a session at the place sees. `entries` lists every
/// provisional non-agent, active ([`hook_active`]) entry whose predicate matches the place
/// ([`Place::locate`]: the repo's remote and the cwd's subpath, else the repo (else cwd)
/// against the glob; its declared domain gates entries that carry one) — the place's view,
/// [`sync_view`] — plus human-pinned provisional entries (in the shared dir), every approved
/// provisional agent ([`agent_approved`]) that matches (others are withheld, [`agent_exposure`])
/// and every kept/pinned entry with its glob hit (false for an unapproved hook, which must
/// not count). A skill-like entry whose name has a place scope (`policy.json` `scopes`) is
/// listed only where the scope matches: elsewhere nothing exposes it. `hooks` lists approved
/// hook modules of loadable, matching entries. `skill_dirs`
/// (staging dirs of the branches with a hit) stays in the payload for compatibility;
/// harnesses load skills from the view. `domain` is the place's declared domain
/// (`domain::place_domain`); `instructions` the personal AGENTS.md exposure.
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
    let scopes = crate::domain::load_scopes(paths)?;
    let provisional =
        ledger.list_entries(&EntryFilter { status: Some(Status::Provisional), ..Default::default() })?;
    let mut branches: Vec<&str> = provisional
        .iter()
        .filter(|e| provisional_hit(paths, e, &place, &scopes))
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
    // A native user/project agent of the same name is what omp loads: the candidate is not.
    let native_agents = native_agent_names(paths, cwd);
    let loaded = provisional.iter().filter(|e| {
        if e.kind == Kind::Agent {
            agent_approved(e) && matches_place(&e.predicate, &place) && scope_allows(&scopes, &e.name, &place) && !native_agents.contains(&e.name)
        } else {
            provisional_hit(paths, e, &place, &scopes) || (e.pinned && shareable(paths, e) && scope_allows(&scopes, &e.name, &place))
        }
    });
    let kept = kept.iter().filter(|e| e.kind == Kind::Agent || scope_allows(&scopes, &e.name, &place));
    for e in loaded.chain(kept) {
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
    out.instructions = crate::instructions::exposure(ledger, paths)?;
    Ok(out)
}

/// An approved `AGENTS.fragment.md` of a matched entry: personal system-prompt text
/// ([`render_fragments`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptFragment {
    pub id: EntryId,
    pub name: String,
    /// The fragment, trimmed.
    pub text: String,
    /// The approved sha256 of `AGENTS.fragment.md` (`approved_fragment`): what a host records
    /// when it actually sends the text (`fragments_sent`).
    #[serde(default)]
    pub sha: Sha256Hex,
}

/// The personal system-prompt text of a match: the approved ([`fragment_approved`])
/// `AGENTS.fragment.md` of every listed entry with a predicate hit that is provisional, kept
/// or pinned, by name. Their total text stays within the lint always-on budget
/// (`limits.always_on_bytes` of `improver/lint.toml`, else the built-in 8192 B); a fragment
/// that would exceed it is dropped. An approved fragment whose file fails to read is skipped
/// (fail open) and stays visible as [`unreadable_fragments`].
pub fn prompt_fragments(ledger: &Ledger, paths: &RematchPaths, entries: &[MatchedEntry]) -> Result<Vec<PromptFragment>> {
    let mut out = Vec::new();
    for me in entries {
        if !me.predicate_hit || me.kind == Kind::Lesson || !matches!(me.status, Status::Provisional | Status::Kept | Status::Pinned) {
            continue;
        }
        let Some(e) = ledger.get_entry(&me.id)? else { continue };
        let bytes = match std::fs::read(e.path.join(AGENTS_FRAGMENT)) {
            Ok(b) => b,
            Err(err) => {
                if e.approved_fragment.is_some() {
                    eprintln!("rematch: approved personal instructions {} not sent: {err}", e.name);
                }
                continue;
            }
        };
        if e.approved_fragment.as_deref() != Some(sha256_hex(&bytes).as_str()) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes).trim().to_string();
        if !text.is_empty() {
            out.push(PromptFragment { id: e.id, name: e.name, text, sha: sha256_hex(&bytes) });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    let budget = crate::lint::LintConfig::load(&paths.improver.join("lint.toml")).unwrap_or_default().limits.always_on_bytes;
    let mut used = 0;
    out.retain(|f| {
        let fits = used + f.text.len() <= budget;
        if fits {
            used += f.text.len();
        }
        fits
    });
    Ok(out)
}

/// An approved `AGENTS.fragment.md` the runtime hosts (Claude Code, Pi, OpenCode) are not given
/// because its file fails to read ([`prompt_fragments`] skips it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadableFragment {
    pub id: EntryId,
    pub name: String,
    pub path: PathBuf,
    pub error: String,
}

/// Every live (provisional, kept, pinned; not a lesson) entry with an approved fragment whose
/// `AGENTS.fragment.md` fails to read: approved text no runtime host receives, until the file
/// reads again or the approval is withdrawn.
pub fn unreadable_fragments(ledger: &Ledger) -> Result<Vec<UnreadableFragment>> {
    let mut out = Vec::new();
    for status in [Status::Provisional, Status::Kept, Status::Pinned] {
        for e in ledger.list_entries(&EntryFilter { status: Some(status), ..Default::default() })? {
            if e.kind == Kind::Lesson || e.approved_fragment.is_none() {
                continue;
            }
            let path = e.path.join(AGENTS_FRAGMENT);
            if let Err(err) = std::fs::read(&path) {
                out.push(UnreadableFragment { id: e.id, name: e.name, error: format!("cannot read {}: {err}", path.display()), path });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// Fragments as system-prompt text, identical in every harness: `## rematch: <name>`, a blank
/// line and the text per fragment (the heading `ship --scope team` gives the AGENTS.md
/// section), joined by a blank line.
pub fn render_fragments(fragments: &[PromptFragment]) -> String {
    fragments.iter().map(|f| format!("## rematch: {}\n\n{}", f.name, f.text.trim())).collect::<Vec<_>>().join("\n\n")
}

/// What [`sync_shared_skills`] did to `REMATCH_ROOT/skills/` (DESIGN §16 "Shared skills"), or
/// [`sync_view`] to one place's view `REMATCH_ROOT/views/<key>/skills/`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SkillsSync {
    /// Names this sync linked or re-pointed.
    pub added: Vec<String>,
    /// Names whose link this sync removed.
    pub pruned: Vec<String>,
    /// Links in the directory after the sync.
    pub total: usize,
    /// Skill directories a harness session loads: the view (or the shared dir); none while
    /// rematch is off.
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
    /// The view key ([`view_key`]); `None` for the shared dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    /// Provisional entries this view links that the shared dir does not: what sessions of
    /// this place see and sessions elsewhere do not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provisional: Vec<EntryId>,
    /// Names of kept/pinned entries and profile kept sources with a place scope that this view
    /// links because the scope matches its place (`policy.json` `scopes`): like `provisional`,
    /// what sessions here see and sessions elsewhere (and the shared dir) do not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scoped: Vec<String>,
    /// Provisional agents of a view's session ([`agent_exposure`]): exposed, withheld (omp
    /// disables those names for the process, `task.disabledAgents`), shadowed by a native
    /// definition. Empty for the shared dir and while off.
    #[serde(default)]
    pub agents: AgentExposure,
}

impl SkillsSync {
    /// A harness that discovered its skills before this sync must re-scan to see it.
    pub fn changed(&self) -> bool {
        !self.added.is_empty() || !self.pruned.is_empty()
    }
}

/// An entry the shared dir or a view may link as a skill: rematch-owned (its dir holds its own
/// `rematch.json`, so a user skill registered in place is never linked), with a `SKILL.md`, not
/// an agent (reached through `agents/`), a lesson (a library record), or a hook (reached
/// through `rematch-hooks/`).
fn shareable(paths: &RematchPaths, e: &Entry) -> bool {
    !matches!(e.kind, Kind::Agent | Kind::Lesson | Kind::Hook)
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

/// Windows distinguishes file and directory symlinks; a file target (an agent `.md`) gets a
/// file symlink.
#[cfg(windows)]
pub fn symlink_dir(target: &Path, link: &Path) -> Result<()> {
    let r = if target.is_file() {
        std::os::windows::fs::symlink_file(target, link)
    } else {
        std::os::windows::fs::symlink_dir(target, link)
    };
    r.with_context(|| format!("link {} → {}", link.display(), target.display()))
}

/// Remove a link (or file) without following it. Windows removes a directory symlink with
/// `RemoveDirectory`, so `remove_file` alone fails on one.
pub fn unlink(p: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if std::fs::symlink_metadata(p)?.file_type().is_symlink_dir() {
            return std::fs::remove_dir(p);
        }
    }
    std::fs::remove_file(p)
}

/// A hidden temporary symlink renamed over `link`. Unix: atomic, a reader sees the old target
/// or the new one, never a missing name. Windows refuses to rename over a directory, which a
/// directory symlink or junction is: an existing link there is unlinked first (never followed;
/// a real directory or file is not removed and the rename fails), so a reader may briefly find
/// the name missing.
pub fn relink(target: &Path, link: &Path) -> Result<()> {
    let name = link.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let tmp = link.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()));
    symlink_dir(target, &tmp)?;
    let r = std::fs::rename(&tmp, link);
    #[cfg(windows)]
    let r = match r {
        Err(_) if std::fs::symlink_metadata(link).is_ok_and(|m| m.file_type().is_symlink()) => {
            unlink(link).and_then(|()| std::fs::rename(&tmp, link))
        }
        r => r,
    };
    r.map_err(|err| {
        let _ = unlink(&tmp);
        anyhow!(err).context(format!("rename {} → {}", tmp.display(), link.display()))
    })
}

/// Root of the per-place catalog views: `REMATCH_ROOT/views/`.
pub fn views_dir(paths: &RematchPaths) -> PathBuf {
    paths.root.join("views")
}

/// The view of a place: `<branch>-<16 hex of sha256(path, remote, subpath, domain)>`. Sessions
/// of the same place share it (their links are the same function of the ledger); sessions of
/// different places never write each other's.
pub fn view_key(place: &Place, branch: &str) -> String {
    let id = format!(
        "{}\0{}\0{}\0{}",
        place.path,
        place.remote.as_deref().unwrap_or(""),
        place.subpath.as_deref().unwrap_or(""),
        place.domain.as_deref().unwrap_or("")
    );
    format!("{branch}-{}", &sha256_hex(id.as_bytes())[..16])
}

/// Skills directory of view `key`.
pub fn view_skills(paths: &RematchPaths, key: &str) -> PathBuf {
    views_dir(paths).join(key).join("skills")
}

/// `views/<key>/<this>/`: only what the place alone gets ([`sync_trials`]), for a host that
/// mounts the kept library elsewhere and reaches trials through a repo-local root.
pub const TRIALS: &str = "trials";

/// What [`sync_trials`] linked for a repo's top.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Trials {
    /// `views/<key>/trials/`.
    pub dir: PathBuf,
    /// Names linked: the provisional entries that match the top, and the kept/pinned entries
    /// and profile kept sources whose place scope matches it.
    pub linked: Vec<String>,
    /// Names that would match below the top only (a predicate or scope `subpath`): left out,
    /// a repo root cannot express a subpath.
    pub subpath_only: Vec<String>,
}

/// A subpath glob narrower than the whole repo.
fn narrow(subpath: Option<&str>) -> bool {
    subpath.is_some_and(|s| !s.trim().is_empty() && s.trim() != crate::predicate::ANY_REPO)
}

/// What a repo's top (`Place::locate(repo, repo)`, its declared domain) alone gets, linked under
/// `views/<key>/trials/`: the provisional entries that match it, plus the kept/pinned entries
/// and profile sources whose place scope (`policy.json` `scopes`) matches it — the scoped kept
/// library the host's global mount no longer carries. What a host with repo-level discovery
/// (Codex's `<repo>/.agents/skills`) loads in that repo. Keyed by the repo's top, so sessions
/// in any subdir of one repo write the same links; an entry or scope narrowed to a subpath is
/// not a top-level hit and stays out ([`Trials::subpath_only`]). Empty while off.
pub fn sync_trials(ledger: &Ledger, paths: &RematchPaths, top: &Place, branch: &str) -> Result<Trials> {
    let dir = views_dir(paths).join(view_key(top, branch)).join(TRIALS);
    let scopes = crate::domain::load_scopes(paths)?;
    let live = live_shareable(ledger, paths)?;
    let sources = if paths.is_off() { Vec::new() } else { crate::profile::kept_sources(paths) };
    let kept_names: Vec<String> = kept_want(&live, &sources, &Scopes::new(), None).into_keys().collect();
    let mut want: BTreeMap<String, PathBuf> =
        kept_want(&live, &sources, &scopes, Some(top)).into_iter().filter(|(n, _)| scopes.contains_key(n)).collect();
    let mut subpath_only = Vec::new();
    let widened = |s: &crate::domain::PlaceScope| crate::domain::PlaceScope { subpath: None, ..s.clone() };
    for e in live.iter().filter(|e| !always_shared(e) && e.status == Status::Provisional && e.kind != Kind::Agent) {
        if provisional_hit(paths, e, top, &scopes) {
            want.insert(e.name.clone(), e.path.clone());
            continue;
        }
        let scope = scopes.get(&e.name);
        let below = (narrow(e.predicate.subpath.as_deref()) || scope.is_some_and(|s| narrow(s.subpath.as_deref())))
            && hook_active(paths, e)
            && matches_place(&crate::types::Predicate { subpath: None, ..e.predicate.clone() }, top)
            && scope.is_none_or(|s| widened(s).matches(top));
        if below && !subpath_only.contains(&e.name) {
            subpath_only.push(e.name.clone());
        }
    }
    for (name, s) in &scopes {
        if kept_names.contains(name) && !want.contains_key(name) && !s.matches(top) && narrow(s.subpath.as_deref()) && widened(s).matches(top) && !subpath_only.contains(name) {
            subpath_only.push(name.clone());
        }
    }
    apply_links(&dir, &mut want)?;
    Ok(Trials { dir, linked: want.into_keys().collect(), subpath_only })
}

/// What one directory of links did.
#[derive(Default)]
struct Applied {
    added: Vec<String>,
    pruned: Vec<String>,
    untouched: Vec<String>,
}

/// Make `dir` hold exactly the symlinks `want` (names that are not symlinks there are never
/// touched and drop out of `want`; hidden names — another sync's temporary links — are
/// skipped). A link is removed only while it still points where this sync read it.
fn apply_links(dir: &Path, want: &mut BTreeMap<String, PathBuf>) -> Result<Applied> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut current = BTreeMap::new();
    let mut out = Applied::default();
    for ent in std::fs::read_dir(dir)? {
        let ent = ent?;
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if ent.file_type()?.is_symlink() {
            current.insert(name, std::fs::read_link(ent.path())?);
        } else {
            out.untouched.push(name);
        }
    }
    for n in &out.untouched {
        want.remove(n);
    }
    for (name, seen) in &current {
        let link = dir.join(name);
        if !want.contains_key(name) && std::fs::read_link(&link).is_ok_and(|t| &t == seen) {
            remove_link(&link)?;
            out.pruned.push(name.clone());
        }
    }
    for (name, target) in want.iter() {
        if current.get(name) != Some(target) {
            relink(target, &dir.join(name))?;
            out.added.push(name.clone());
        }
    }
    Ok(out)
}

/// Loadable skill-like entries ([`shareable`]) — none while rematch is off.
fn live_shareable(ledger: &Ledger, paths: &RematchPaths) -> Result<Vec<Entry>> {
    if paths.is_off() {
        return Ok(Vec::new());
    }
    Ok([Status::Provisional, Status::Kept, Status::Pinned]
        .into_iter()
        .map(|s| ledger.list_entries(&EntryFilter { status: Some(s), ..Default::default() }))
        .collect::<Result<Vec<_>>>()?
        .concat()
        .into_iter()
        .filter(|e| shareable(paths, e))
        .collect())
}

/// The kept library: kept/pinned entries (status or human pin), then the profile's kept
/// sources (`profile::kept_sources`) for names nothing else holds. A name with a place scope
/// (`policy.json` `scopes`) is left out of the library every session sees (`place` `None`) and
/// kept in a place's library only when its scope matches that place.
fn kept_want(live: &[Entry], sources: &[(String, PathBuf)], scopes: &Scopes, place: Option<&Place>) -> BTreeMap<String, PathBuf> {
    let allowed = |name: &str| match place {
        None => !scopes.contains_key(name),
        Some(p) => scope_allows(scopes, name, p),
    };
    let mut want = BTreeMap::new();
    for e in live.iter().filter(|e| always_shared(e) && allowed(&e.name)) {
        want.entry(e.name.clone()).or_insert_with(|| e.path.clone());
    }
    for (name, src) in sources.iter().filter(|(n, _)| allowed(n)) {
        want.entry(name.clone()).or_insert_with(|| src.clone());
    }
    want
}

/// Make `REMATCH_ROOT/skills/` the kept library: one symlink `<name> → <entry dir>` per
/// kept/pinned entry ([`shareable`]) and per profile kept source, except names with a place
/// scope ([`kept_want`]; their rematch-made `<agent_dir>/skills/<name>` link into the profile is
/// removed too, `profile::unlink_scoped`). Provisional entries are never
/// linked here: they reach sessions only through their place's view ([`sync_view`]), so a
/// harness that mounts this one directory for every session (Claude Code's plugin, Codex)
/// never lists another repo's trial. Every status change runs it; it also keeps the other
/// exposures consistent: links of views whose entry left (or all of them, while off) are
/// removed, provisional agent links follow off/on and approval ([`sync_agent_links`]), and the
/// personal AGENTS.md follows its approvals (`instructions::sync`). While rematch is off every
/// link is removed and `dirs` is empty.
pub fn sync_shared_skills(ledger: &Ledger, paths: &RematchPaths) -> Result<SkillsSync> {
    let dir = &paths.skills_shared;
    let off = paths.is_off();
    let scopes = crate::domain::load_scopes(paths)?;
    let live = live_shareable(ledger, paths)?;
    let sources = if off { Vec::new() } else { crate::profile::kept_sources(paths) };
    let mut want = kept_want(&live, &sources, &scopes, None);
    crate::profile::unlink_scoped(paths, &scopes)?;
    let applied = apply_links(dir, &mut want)?;
    let mut out = SkillsSync { off, untouched: applied.untouched, added: applied.added, pruned: applied.pruned, ..Default::default() };
    out.total = want.len();
    out.visible = want.iter().filter_map(|(n, t)| live.iter().find(|e| &e.name == n && &e.path == t)).map(|e| e.id.clone()).collect();
    out.links = want;
    if !off {
        out.dirs.push(dir.clone());
    }
    prune_views(paths, &live, &sources)?;
    if off {
        for p in crate::project_views::withdraw_all(paths)? {
            eprintln!("rematch: off: removed repo-local trial delivery {}", p.display());
        }
    } else {
        let may_load: Vec<EntryId> = ledger
            .list_entries(&EntryFilter { status: Some(Status::Provisional), ..Default::default() })?
            .into_iter()
            .filter(|e| e.kind == Kind::Agent && agent_approved(e))
            .map(|e| e.id)
            .collect();
        crate::project_views::prune_copies(paths, |id| may_load.iter().any(|x| x == id))?;
    }
    sync_agent_links(ledger, paths)?;
    sync_hook_modules(ledger, paths)?;
    let _ = crate::instructions::apply_auto_rules(ledger, paths);
    // A read-only or locked personal AGENTS.md never blocks a keep, retire or stage; edited
    // blocks are already reported as conflicts (`instructions-conflict:<name>` actions).
    if let Err(e) = crate::instructions::sync(ledger, paths) {
        eprintln!("rematch: personal AGENTS.md not synced: {e:#}");
    }
    Ok(out)
}

/// Remove from every view (its `skills/` and `trials/`) the links whose target is no longer a
/// loadable entry dir or profile source (every link while off). Only removes: a view gains
/// links when its own place syncs.
fn prune_views(paths: &RematchPaths, live: &[Entry], sources: &[(String, PathBuf)]) -> Result<()> {
    let Ok(rd) = std::fs::read_dir(views_dir(paths)) else { return Ok(()) };
    for (view, sub) in rd.flatten().flat_map(|v| [(v.path(), "skills"), (v.path(), TRIALS)]) {
        let Ok(links) = std::fs::read_dir(view.join(sub)) else { continue };
        for l in links.flatten() {
            let name = l.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !l.file_type().is_ok_and(|t| t.is_symlink()) {
                continue;
            }
            let Ok(target) = std::fs::read_link(l.path()) else { continue };
            let alive = live.iter().any(|e| e.name == name && e.path == target) || sources.iter().any(|(n, p)| n == &name && p == &target);
            if !alive {
                remove_link(&l.path())?;
            }
        }
    }
    Ok(())
}

/// Remove the link `name` from every view (`skills/` and `trials/`): what a new place scope
/// (`policy scope set`) withdraws, since a view's key does not tell its place. Each place's
/// next sync ([`sync_view`], [`sync_trials`]) links it again where the scope matches. Returns
/// the links removed.
pub fn unlink_from_views(paths: &RematchPaths, name: &str) -> Result<usize> {
    validate_name(name)?;
    let Ok(rd) = std::fs::read_dir(views_dir(paths)) else { return Ok(0) };
    let mut n = 0;
    for (view, sub) in rd.flatten().flat_map(|v| [(v.path(), "skills"), (v.path(), TRIALS)]) {
        let link = view.join(sub).join(name);
        if std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
            remove_link(&link)?;
            n += 1;
        }
    }
    Ok(n)
}

/// `REMATCH_ROOT/agents/` (what omp reads through `extension/agents`): while off, no links at
/// all; otherwise a link per approved provisional agent ([`agent_approved`]) and none for an
/// unapproved one. Per-place exposure of the linked ones is the session's job
/// ([`SkillsSync::agents`]).
pub fn sync_agent_links(ledger: &Ledger, paths: &RematchPaths) -> Result<()> {
    if paths.is_off() {
        let Ok(rd) = std::fs::read_dir(&paths.agents) else { return Ok(()) };
        for l in rd.flatten() {
            if l.file_type().is_ok_and(|t| t.is_symlink()) {
                remove_link(&l.path())?;
            }
        }
        return Ok(());
    }
    unlink_unapproved_agents(ledger, paths)?;
    for e in ledger.list_entries(&EntryFilter { status: Some(Status::Provisional), ..Default::default() })? {
        if e.kind == Kind::Agent && agent_approved(&e) && std::fs::symlink_metadata(agent_link(paths, &e.name)).is_err() && check_agent_link(paths, &e.name, &e.path.join(AGENT_MD)).is_ok() {
            link_agent(paths, &e)?;
        }
    }
    Ok(())
}

/// Ensure `rematch-hooks/` matches on/off and live data-only hook entries: while off, remove
/// every compiled hook module in `paths.hooks_dir()`; while on, compile any live data-only hook
/// whose module is missing.
pub fn sync_hook_modules(ledger: &Ledger, paths: &RematchPaths) -> Result<()> {
    let dir = paths.hooks_dir();
    if paths.is_off() {
        let Ok(rd) = std::fs::read_dir(&dir) else { return Ok(()) };
        for l in rd.flatten() {
            let name = l.file_name().to_string_lossy().into_owned();
            if name.ends_with(".ts") {
                remove_link(&l.path())?;
            }
        }
        return Ok(());
    }
    let profile = crate::profile::present(paths);
    for status in [Status::Provisional, Status::Kept, Status::Pinned] {
        for e in ledger.list_entries(&EntryFilter { status: Some(status), ..Default::default() })? {
            if !is_data_only_hook(&e) {
                continue;
            }
            let file = dir.join(format!("{}.ts", e.id));
            let need = !file.is_file()
                || (profile && !std::fs::symlink_metadata(&file).is_ok_and(|m| m.file_type().is_symlink()));
            if need {
                let _ = compile_hook(paths, &e);
            }
        }
    }
    Ok(())
}

/// The catalog view of `place` (a session there mounts `dirs`): the kept library of
/// [`sync_shared_skills`] plus the scoped kept names whose place scope matches here
/// ([`kept_want`], listed in [`SkillsSync::scoped`]), plus every provisional entry whose
/// predicate (and scope) matches the place ([`provisional_hit`]), which shadows a kept entry of
/// its name in this view only. Written under `views/<key>/skills/` ([`view_key`]): concurrent
/// sessions of other places never touch it and nothing global is pruned per cwd. Also names the
/// approved provisional agents the place exposes and the linked ones it must withhold. While
/// off the view is emptied.
pub fn sync_view(ledger: &Ledger, paths: &RematchPaths, place: &Place, branch: &str, cwd: &Path) -> Result<SkillsSync> {
    let key = view_key(place, branch);
    let dir = view_skills(paths, &key);
    let off = paths.is_off();
    let scopes = crate::domain::load_scopes(paths)?;
    let live = live_shareable(ledger, paths)?;
    let sources = if off { Vec::new() } else { crate::profile::kept_sources(paths) };
    let mut want = kept_want(&live, &sources, &scopes, Some(place));
    let mut provisional = Vec::new();
    for e in live.iter().filter(|e| !always_shared(e) && provisional_hit(paths, e, place, &scopes)) {
        want.insert(e.name.clone(), e.path.clone());
        provisional.push(e.id.clone());
    }
    let applied = apply_links(&dir, &mut want)?;
    let mut out = SkillsSync { off, view: Some(key), untouched: applied.untouched, added: applied.added, pruned: applied.pruned, ..Default::default() };
    out.total = want.len();
    out.visible = want.iter().filter_map(|(n, t)| live.iter().find(|e| &e.name == n && &e.path == t)).map(|e| e.id.clone()).collect();
    out.provisional = provisional.into_iter().filter(|id| out.visible.contains(id)).collect();
    out.scoped = want
        .iter()
        .filter(|(n, t)| scopes.contains_key(*n) && !live.iter().any(|e| out.provisional.contains(&e.id) && &e.path == *t))
        .map(|(n, _)| n.clone())
        .collect();
    out.links = want;
    if off {
        return Ok(out);
    }
    out.dirs.push(dir);
    out.agents = agent_exposure(ledger, paths, place, cwd)?;
    Ok(out)
}

/// Provisional agents for a session at `place` / `cwd` ([`AgentExposure`]): an approved one
/// whose predicate (and place scope, `policy.json` `scopes`) matches is exposed unless a native
/// definition of its name wins in omp ([`native_agent_names`]: then it is shadowed — not
/// offered, not credited); every other name linked under `REMATCH_ROOT/agents/` is withheld,
/// except a name a native definition holds (`task.disabledAgents` works by name and would hide
/// the user's agent).
pub fn agent_exposure(ledger: &Ledger, paths: &RematchPaths, place: &Place, cwd: &Path) -> Result<AgentExposure> {
    let native = native_agent_names(paths, cwd);
    let scopes = crate::domain::load_scopes(paths)?;
    let provisional = ledger.list_entries(&EntryFilter { status: Some(Status::Provisional), ..Default::default() })?;
    let mut out = AgentExposure::default();
    for e in provisional
        .iter()
        .filter(|e| e.kind == Kind::Agent && agent_approved(e) && matches_place(&e.predicate, place) && scope_allows(&scopes, &e.name, place))
    {
        if !std::fs::read_link(agent_link(paths, &e.name)).is_ok_and(|t| t == e.path.join(AGENT_MD)) {
            continue;
        }
        if native.contains(&e.name) {
            out.shadowed.push(e.name.clone());
        } else {
            out.exposed.push(e.name.clone());
        }
    }
    for l in std::fs::read_dir(&paths.agents).into_iter().flatten().flatten() {
        let file = l.file_name().to_string_lossy().into_owned();
        let Some(name) = file.strip_suffix(".md").filter(|n| !n.starts_with('.')) else { continue };
        if out.exposed.iter().any(|n| n == name) || native.iter().any(|n| n == name) {
            continue;
        }
        out.withheld.push(name.to_string());
    }
    out.exposed.sort();
    out.withheld.sort();
    out.shadowed.sort();
    Ok(out)
}

/// What [`agent_exposure`] decided for one session's place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentExposure {
    /// Approved provisional agents of the place that the host offers as the candidate.
    pub exposed: Vec<String>,
    /// Linked names the session must not offer (other places' agents); never a name a native
    /// definition holds, which disabling by name would hide too.
    pub withheld: Vec<String>,
    /// Approved provisional agents of the place whose name a user or project agent holds: omp
    /// loads that definition instead, so the candidate is neither offered nor credited.
    pub shadowed: Vec<String>,
}

/// Names of the agent definitions omp prefers over the plugin's `agents/` (task/discovery.ts:
/// project `.omp/agents/*.md` of the nearest ancestors of `cwd`, then the user's
/// `<agent_dir>/agents/*.md`): each file's frontmatter `name`, else its stem.
pub fn native_agent_names(paths: &RematchPaths, cwd: &Path) -> Vec<String> {
    let mut dirs = vec![paths.agents_dir()];
    dirs.extend(cwd.ancestors().map(|a| a.join(".omp").join("agents")));
    let mut out = Vec::new();
    for d in dirs {
        for f in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = f.path();
            if p.extension().is_none_or(|x| x != "md") {
                continue;
            }
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let name = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| SkillDoc::parse(&t).ok())
                .and_then(|d| d.get_str("name").map(|n| n.trim().to_string()))
                .filter(|n| !n.is_empty())
                .unwrap_or(stem);
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// Refuse an approval that would make `cost` more bytes always on where `candidate` loads than
/// `limits.always_on_bytes` leaves room for next to what is exposed now
/// ([`catalog_other_bytes`]; `improver/lint.toml`, else the built-in budget). Staging linted
/// each candidate alone, so pending approvals can each fit and still overflow together; the
/// approval is where they become always on. `what` names the text in the error.
pub fn check_always_on(ledger: &Ledger, paths: &RematchPaths, candidate: &Entry, cost: usize, what: &str) -> Result<()> {
    let limit = crate::lint::LintConfig::load(&paths.improver.join("lint.toml")).unwrap_or_default().limits.always_on_bytes;
    let other = catalog_other_bytes(ledger, paths, candidate, &candidate.path)?;
    if other + cost > limit {
        bail!(
            "approving {} would put {} B always on where it loads ({what} {cost} B + {other} B already exposed) > {limit} B: retire or shorten something exposed there first (`rematch skills sync` itemizes the catalog)",
            candidate.name,
            other + cost
        );
    }
    Ok(())
}

/// Always-on bytes other rematch content already puts in front of a session where `candidate`
/// would load (`lint::LintContext::catalog_other_bytes`): the catalog line
/// (`lint::entry_cost`) of every kept/pinned entry and profile kept source (every session sees
/// them), of every active provisional entry that can share a place with the candidate (same
/// branch, the same repo glob or remote, or a glob that matches every repo), of every approved
/// provisional agent among those, plus the personal instructions applied now
/// (`instructions::exposure`) and the approved fragments injected at runtime in hosts without
/// that file. A skill name with a place scope (`policy.json` `scopes`) is not global: it counts
/// only when its scope may hold a session of the candidate (`PlaceScope::may_share`). `exclude`
/// (the candidate's own dir) and entries of the candidate's id or name are left out: the
/// candidate is counted by lint itself, and a successor replaces its predecessor. Hand-written
/// skills rematch did not write are not rematch's to budget.
pub fn catalog_other_bytes(ledger: &Ledger, paths: &RematchPaths, candidate: &Entry, exclude: &Path) -> Result<usize> {
    let scopes = crate::domain::load_scopes(paths)?;
    let in_scope = |name: &str| scopes.get(name).is_none_or(|s| s.may_share(&candidate.predicate));
    let shares_place = |e: &Entry| {
        let (a, b) = (&e.predicate, &candidate.predicate);
        e.branch == candidate.branch
            || a.repo_glob == crate::predicate::ANY_REPO
            || b.repo_glob == crate::predicate::ANY_REPO
            || a.repo_glob == b.repo_glob
            || (a.repo_remote.is_some() && a.repo_remote == b.repo_remote)
    };
    let exposure = crate::instructions::exposure(ledger, paths)?;
    let mut total = 0;
    let mut paths_seen: Vec<PathBuf> = Vec::new();
    for status in [Status::Kept, Status::Pinned, Status::Provisional] {
        for e in ledger.list_entries(&EntryFilter { status: Some(status), ..Default::default() })? {
            if e.path == exclude || e.id == candidate.id || e.name == candidate.name || e.kind == Kind::Lesson || !hook_active(paths, &e) {
                continue;
            }
            // A kept agent is global (`<agent_dir>/agents`); every other name follows its scope.
            let reach = (e.kind == Kind::Agent && always_shared(&e)) || in_scope(&e.name);
            let relevant = reach && (always_shared(&e) || (shares_place(&e) && (e.kind != Kind::Agent || agent_approved(&e))));
            if !relevant {
                continue;
            }
            total += crate::lint::entry_cost(&e.path, e.kind).catalog;
            if fragment_approved(&e) && !exposure.exposed.iter().any(|x| x.name == e.name) {
                total += crate::lint::entry_cost(&e.path, e.kind).fragment;
            }
            paths_seen.push(e.path);
        }
    }
    for (name, dir) in crate::profile::kept_sources(paths) {
        if name != candidate.name && !paths_seen.contains(&dir) && in_scope(&name) {
            total += crate::lint::entry_cost(&dir, Kind::Skill).catalog;
        }
    }
    total += exposure.exposed.iter().filter(|x| x.name != candidate.name).map(|x| x.bytes).sum::<usize>();
    Ok(total)
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
        Ok(_) => unlink(p)?,
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
            &[w("SKILL.md", &skill("ledger-skill")), w("scripts/check.sh", "#!/bin/sh\necho x > \"$HOME/outside.txt\"\n"), w("units/a.md", "unit")],
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
        drop(x.ledger);
        apply_uninstall(&plan).unwrap();
        assert!(profile.join(".git").is_dir() && home.join("SKILL.md").is_file());
        assert!(std::fs::symlink_metadata(&user_dir).unwrap().is_dir());
        assert!(!x.paths.ledger_file.exists());
    }

    /// Links are re-pointed, removed and uninstalled as links: never followed into their
    /// targets, never replacing a real directory (Windows: directory symlinks, file symlinks).
    #[test]
    fn links_repoint_and_go_without_touching_targets() {
        let t = tempfile::tempdir().unwrap();
        let (a, b, real) = (t.path().join("a"), t.path().join("b"), t.path().join("real"));
        for d in [&a, &b, &real] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("SKILL.md"), d.display().to_string()).unwrap();
        }
        let views = t.path().join("views");
        std::fs::create_dir_all(&views).unwrap();
        let link = views.join("s");
        relink(&a, &link).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), a);
        relink(&b, &link).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), b, "an existing directory link is re-pointed");
        assert_eq!(std::fs::read_to_string(link.join("SKILL.md")).unwrap(), b.display().to_string());
        // A file target gets a link that reads as the file.
        let md = views.join("agent.md");
        relink(&a.join("SKILL.md"), &md).unwrap();
        relink(&b.join("SKILL.md"), &md).unwrap();
        assert_eq!(std::fs::read_to_string(&md).unwrap(), b.display().to_string());
        // A real directory in the way is the user's: the rename fails and it stays whole.
        assert!(relink(&a, &real).is_err());
        assert!(std::fs::symlink_metadata(&real).unwrap().is_dir() && real.join("SKILL.md").is_file());
        let names: Vec<String> = std::fs::read_dir(&views).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "no temporary link left: {names:?}");
        // Removal unlinks; uninstalling a link dir removes the links, not what they point at.
        unlink(&md).unwrap();
        assert!(std::fs::symlink_metadata(&md).is_err());
        apply_uninstall(&UninstallPlan { remove: vec![link.clone(), views.clone()], restore: Vec::new() }).unwrap();
        assert!(!views.exists());
        for d in [&a, &b, &real] {
            assert_eq!(std::fs::read_to_string(d.join("SKILL.md")).unwrap(), d.display().to_string(), "{}", d.display());
        }
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
        std::fs::create_dir_all(kept.path.join(EVALS_DIR)).unwrap();
        std::fs::write(kept.path.join(EVALS_DIR).join("evals.json"), "{\"cases\":[]}").unwrap();
        assert_eq!(content_sha256(&kept.path).unwrap(), changed, "EXPERIENCE.md, evals/ and hidden files are not content");
        // The shared dir reports what it exposes.
        let s = sync_shared_skills(&x.ledger, &x.paths).unwrap();
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
        stage(
            &x.ledger,
            &x.paths,
            &entry("s1", "scripted", "repo", Status::Provisional),
            &[w("scripts/run.sh", "#!/bin/sh\necho x > \"$HOME/outside.txt\"\n")],
        )
        .unwrap();
        let rc = approve(&x.ledger, &x.paths, "s1", Approval::Scripts).unwrap();
        assert_eq!(rc.activation, Activation::Prompted);
        assert!(x.ledger.get_blob(&rc.certificate).unwrap().is_some());
        let e = x.ledger.get_entry("s1").unwrap().unwrap();
        assert_eq!(e.approved_scripts, vec!["scripts/run.sh".to_string()]);
        #[cfg(unix)]
        assert_eq!(mode(&e.path.join("scripts/run.sh")), 0o755);
        // approved mode survives a status move
        #[cfg_attr(not(unix), allow(unused_variables))]
        let dm = set_status(&x.ledger, &x.paths, "s1", Status::Dormant, None).unwrap();
        #[cfg(unix)]
        assert_eq!(mode(&dm.path.join("scripts/run.sh")), 0o755);

        // Data-only hooks compile and become active automatically on stage.
        stage(&x.ledger, &x.paths, &hook_entry("h1"), &[]).unwrap();
        assert!(x.paths.staging.join("repo/hook-h1/hook.json").is_file());
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

        let d = decline(&x.ledger, &x.paths, "h1", &[Approval::Hook]).unwrap();
        assert_eq!(d.status_note.as_deref(), Some("approval declined (hook)"));
        assert_eq!(d.status, Status::Retired);
        assert!(!x.paths.hooks_dir().join("h1.ts").exists(), "retire removes the compiled hook");
        assert!(approve(&x.ledger, &x.paths, "h1", Approval::Hook).is_err());

        let mut bad = hook_entry("h2");
        bad.hook.as_mut().unwrap().pass_condition = " ".into();
        assert!(stage(&x.ledger, &x.paths, &bad, &[]).is_err(), "invalid hook fails to compile on stage");
    }

    #[test]
    fn fragment_approval_feeds_prompt_fragments_until_the_fragment_changes() {
        let x = env();
        let here = |x: &Env| list_for_match(&x.ledger, &x.paths, Some(Path::new("/w/repo")), Path::new("/w/repo")).unwrap();
        let fragment = "Run the crate's tests before committing.\n\n### Why\n\nCI runs them.\n";
        let e = stage(
            &x.ledger,
            &x.paths,
            &entry("f1", "tests-first", "repo", Status::Provisional),
            &[w("scripts/run.sh", "#!/bin/sh\necho x > \"$HOME/outside.txt\"\n"), w(AGENTS_FRAGMENT, fragment)],
        )
        .unwrap();
        let parts = pending_parts(&x.paths, &e);
        assert_eq!((parts.clone(), parts_label(&parts)), (vec![Approval::Scripts, Approval::Prompt], "scripts+prompt".to_string()));
        assert!(prompt_fragments(&x.ledger, &x.paths, &here(&x).entries).unwrap().is_empty(), "nothing before approval");

        for part in parts {
            approve(&x.ledger, &x.paths, "f1", part).unwrap();
        }
        assert!(x.ledger.list_receipts(Some("f1")).unwrap().len() >= 2, "receipts per part (+ optional sandbox dry run)");
        let e = x.ledger.get_entry("f1").unwrap().unwrap();
        assert!(pending_parts(&x.paths, &e).is_empty() && fragment_approved(&e));
        assert_eq!(load_entry(&e.path).unwrap().approved_fragment, e.approved_fragment, "rematch.json carries it");
        let got = prompt_fragments(&x.ledger, &x.paths, &here(&x).entries).unwrap();
        let want = PromptFragment { id: "f1".into(), name: "tests-first".into(), text: fragment.trim().into(), sha: crate::ledger::sha256_hex(fragment.as_bytes()) };
        assert_eq!(got, vec![want.clone()]);
        assert_eq!(render_fragments(&got), format!("## rematch: tests-first\n\n{}", fragment.trim()));
        let elsewhere = list_for_match(&x.ledger, &x.paths, Some(Path::new("/elsewhere")), Path::new("/elsewhere")).unwrap();
        assert!(prompt_fragments(&x.ledger, &x.paths, &elsewhere.entries).unwrap().is_empty(), "no predicate hit, no text");

        // kept: the approval moves with the files
        let kept = set_status(&x.ledger, &x.paths, "f1", Status::Kept, None).unwrap();
        assert!(fragment_approved(&kept));
        assert_eq!(prompt_fragments(&x.ledger, &x.paths, &here(&x).entries).unwrap(), vec![want.clone()]);

        // a second approved fragment sorts by name; the budget drops what does not fit
        let other = "Short rule.\n";
        stage(&x.ledger, &x.paths, &entry("f2", "alpha-rule", "repo", Status::Provisional), &[w(AGENTS_FRAGMENT, other)]).unwrap();
        approve(&x.ledger, &x.paths, "f2", Approval::Prompt).unwrap();
        let names = |x: &Env| prompt_fragments(&x.ledger, &x.paths, &here(x).entries).unwrap().into_iter().map(|f| f.name).collect::<Vec<_>>();
        assert_eq!(names(&x), vec!["alpha-rule", "tests-first"]);
        std::fs::write(x.paths.improver.join("lint.toml"), format!("[limits]\nalways_on_bytes = {}\n", other.trim().len() + 1)).unwrap();
        assert_eq!(names(&x), vec!["alpha-rule"], "over the always-on budget: dropped");
        std::fs::remove_file(x.paths.improver.join("lint.toml")).unwrap();

        // an edit makes it pending again and drops it from the prompt
        std::fs::write(kept.path.join(AGENTS_FRAGMENT), "Changed rule.\n").unwrap();
        let edited = x.ledger.get_entry("f1").unwrap().unwrap();
        assert!(!fragment_approved(&edited));
        assert_eq!(pending_parts(&x.paths, &edited), vec![Approval::Prompt]);
        assert_eq!(names(&x), vec!["alpha-rule"]);
        assert!(approve(&x.ledger, &x.paths, "f1", Approval::Agent).is_err(), "not an agent");

        // an approved fragment whose file no longer reads: skipped at runtime, listed as unreadable
        assert!(unreadable_fragments(&x.ledger).unwrap().is_empty());
        let f2 = x.ledger.get_entry("f2").unwrap().unwrap();
        std::fs::remove_file(f2.path.join(AGENTS_FRAGMENT)).unwrap();
        assert!(names(&x).is_empty(), "nothing sent, the match still answers");
        let gone = unreadable_fragments(&x.ledger).unwrap();
        assert_eq!(gone.iter().map(|u| (u.id.as_str(), u.name.as_str())).collect::<Vec<_>>(), vec![("f2", "alpha-rule")]);
        assert!(gone[0].error.starts_with("cannot read ") && gone[0].path == f2.path.join(AGENTS_FRAGMENT), "{gone:?}");
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
        assert_eq!(got, vec![("a", true), ("k", true)], "a provisional entry without a hit is not in this place's view");

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
    fn views_scope_provisional_skills_per_place_and_shared_holds_the_kept_library() {
        let x = env();
        let shared = x.paths.skills_shared.clone();
        let (repo, other) = (Place::path("/w/repo"), Place::path("/w/other"));
        let names = |s: &SkillsSync| s.links.keys().cloned().collect::<Vec<_>>();
        stage(&x.ledger, &x.paths, &entry("a", "alpha", "repo", Status::Provisional), &[w("SKILL.md", &skill("alpha"))]).unwrap();
        let mut b = entry("b", "beta", "other", Status::Provisional);
        b.predicate.repo_glob = "/w/other".into();
        stage(&x.ledger, &x.paths, &b, &[w("SKILL.md", &skill("beta"))]).unwrap();
        assert!(sync_shared_skills(&x.ledger, &x.paths).unwrap().links.is_empty(), "no provisional entry in the shared dir");

        // two places, interleaved as two concurrent sessions would: each view holds its own trial only
        let view = |p: &Place, b: &str| sync_view(&x.ledger, &x.paths, p, b, Path::new(&p.path)).unwrap();
        let vr = view(&repo, "repo");
        let vo = view(&other, "other");
        let vr2 = view(&repo, "repo");
        assert_eq!((names(&vr), vr.provisional.clone()), (vec!["alpha".to_string()], vec!["a".to_string()]));
        assert_eq!(names(&vo), vec!["beta".to_string()]);
        assert!(!vr2.changed() && names(&vr2) == names(&vr), "the other place's sync changed nothing here");
        assert_ne!(vr.dirs, vo.dirs);
        assert_eq!(vr.dirs, vec![view_skills(&x.paths, vr.view.as_deref().unwrap())]);
        assert_eq!(std::fs::read_link(vr.dirs[0].join("alpha")).unwrap(), x.paths.staging.join("repo/alpha"));
        assert!(!vo.dirs[0].join("alpha").exists() && !vr.dirs[0].join("beta").exists());
        assert_ne!(view_key(&repo, "repo"), view_key(&Place { subpath: Some("sub".into()), ..repo.clone() }, "repo"));
        // a directory that is not a link is never touched
        std::fs::create_dir_all(shared.join("mine")).unwrap();
        assert_eq!(sync_shared_skills(&x.ledger, &x.paths).unwrap().untouched, vec!["mine".to_string()]);

        // keep: shared to every place, the view link follows the entry to its kept dir
        set_status(&x.ledger, &x.paths, "a", Status::Kept, None).unwrap();
        assert_eq!(std::fs::read_link(shared.join("alpha")).unwrap(), x.paths.skills_dir().join("alpha"));
        assert!(!vr.dirs[0].join("alpha").exists(), "the stale view link went with the status change");
        assert_eq!(names(&view(&other, "other")), vec!["alpha".to_string(), "beta".to_string()]);
        // a matched provisional candidate of the same name shadows it in its place only
        let mut a2 = entry("a2", "alpha", "repo2", Status::Provisional);
        a2.predicate.repo_glob = "/w/repo".into();
        stage(&x.ledger, &x.paths, &a2, &[w("SKILL.md", &skill("alpha"))]).unwrap();
        let vr = view(&repo, "repo");
        assert_eq!(std::fs::read_link(vr.dirs[0].join("alpha")).unwrap(), x.paths.staging.join("repo2/alpha"));
        assert_eq!(std::fs::read_link(shared.join("alpha")).unwrap(), x.paths.skills_dir().join("alpha"), "shared keeps the kept one");
        set_status(&x.ledger, &x.paths, "a2", Status::Retired, None).unwrap();
        let vr = view(&repo, "repo");
        assert_eq!(std::fs::read_link(vr.dirs[0].join("alpha")).unwrap(), x.paths.skills_dir().join("alpha"));

        // off: every link of the shared dir and of every view goes, the user's dir stays
        std::fs::write(&x.paths.off_marker, "").unwrap();
        let s = sync_shared_skills(&x.ledger, &x.paths).unwrap();
        assert!(s.off && s.dirs.is_empty() && s.links.is_empty() && shared.join("mine").is_dir());
        assert!(std::fs::read_dir(&vo.dirs[0]).unwrap().next().is_none(), "views emptied");
        let v = view(&repo, "repo");
        assert!(v.off && v.dirs.is_empty() && v.links.is_empty());
    }

    #[test]
    fn place_scopes_keep_a_skill_out_of_every_global_location_and_in_its_places_views() {
        let x = env();
        std::fs::create_dir_all(crate::profile::dir(&x.paths).join(".git")).unwrap();
        let names = |s: &SkillsSync| s.links.keys().cloned().collect::<Vec<_>>();
        let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let web = |path: &str, sub: &str| Place { path: path.into(), remote: Some("github.com/acme/web".into()), subpath: Some(sub.into()), domain: None };
        let (top, worktree, deep) = (web("/w/web", ""), web("/w/web-wt", ""), web("/w/web-wt", "research/x"));
        let other = Place { path: "/w/api".into(), remote: Some("github.com/acme/api".into()), subpath: Some(String::new()), domain: None };
        let notes = Place::path("/w/notes/sub");
        let native = |n: &str| x.paths.skills_dir().join(n);
        // Profile sources with no entry here (kept on another machine), a kept entry (its dir in
        // the profile), a plain kept skill, and a provisional entry that matches every place.
        for name in ["bellwether-dev", "notes-skill"] {
            let src = crate::profile::skills_dir(&x.paths).join(name);
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(src.join(SKILL_MD), skill(name)).unwrap();
        }
        for (id, name) in [("k", "kept-scoped"), ("p", "plain")] {
            let mut e = entry(id, name, "repo", Status::Provisional);
            e.predicate.repo_glob = "**".into();
            stage(&x.ledger, &x.paths, &e, &[w("SKILL.md", &skill(name))]).unwrap();
            set_status(&x.ledger, &x.paths, id, Status::Kept, None).unwrap();
        }
        let mut t = entry("t", "trial-scoped", "shared", Status::Provisional);
        t.predicate.repo_glob = "**".into();
        stage(&x.ledger, &x.paths, &t, &[w("SKILL.md", &skill("trial-scoped"))]).unwrap();
        crate::profile::link_views(&x.paths).unwrap();
        assert!(std::fs::read_link(native("bellwether-dev")).is_ok() && std::fs::read_link(native("kept-scoped")).is_ok(), "unscoped: global");
        std::fs::create_dir_all(native("users-own")).unwrap();

        let policy = |v: serde_json::Value| std::fs::write(x.paths.root.join(crate::domain::POLICY_FILE), v.to_string()).unwrap();
        policy(serde_json::json!({"environment": "work", "scopes": {
            "bellwether-dev": {"remote": "github.com/acme/web", "subpath": "**"},
            "kept-scoped": {"remote": "github.com/acme/web", "subpath": "**"},
            "late": {"remote": "github.com/acme/web", "subpath": "**"},
            "trial-scoped": {"remote": "github.com/acme/web", "subpath": "research/**"},
            "notes-skill": {"repo_glob": "/w/notes"},
            "users-own": {"repo_glob": "/w/notes"},
        }}));
        let views = crate::profile::link_views(&x.paths).unwrap();
        assert_eq!(views.scoped, strs(&["skills/bellwether-dev", "skills/kept-scoped", "skills/notes-skill"]));
        for n in ["bellwether-dev", "kept-scoped", "notes-skill"] {
            assert!(std::fs::symlink_metadata(native(n)).is_err(), "{n} left <agent_dir>/skills");
        }
        assert!(native("users-own").is_dir(), "a real directory is the user's");
        let shared = sync_shared_skills(&x.ledger, &x.paths).unwrap();
        assert_eq!((names(&shared), shared.visible.clone()), (strs(&["plain"]), strs(&["p"])), "the shared dir holds no scoped name");
        // A keep under a scope links nothing natively.
        let mut late = entry("l", "late", "repo", Status::Provisional);
        late.predicate.repo_glob = "**".into();
        stage(&x.ledger, &x.paths, &late, &[w("SKILL.md", &skill("late"))]).unwrap();
        set_status(&x.ledger, &x.paths, "l", Status::Kept, None).unwrap();
        assert!(std::fs::symlink_metadata(native("late")).is_err() && std::fs::symlink_metadata(x.paths.skills_shared.join("late")).is_err());

        let view = |p: &Place| sync_view(&x.ledger, &x.paths, p, "b", Path::new(&p.path)).unwrap();
        assert_eq!(names(&view(&other)), strs(&["plain"]), "another repo sees none of them");
        for p in [&top, &worktree] {
            let v = view(p);
            assert_eq!(names(&v), strs(&["bellwether-dev", "kept-scoped", "late", "plain"]), "{}", p.path);
            assert_eq!((v.scoped.clone(), v.provisional.clone()), (strs(&["bellwether-dev", "kept-scoped", "late"]), vec![]));
        }
        let v = view(&deep);
        assert_eq!(names(&v), strs(&["bellwether-dev", "kept-scoped", "late", "plain", "trial-scoped"]), "the subpath scope matches below research/");
        assert_eq!(v.provisional, strs(&["t"]));
        assert_eq!(names(&view(&notes)), strs(&["notes-skill", "plain"]), "a path scope matches below its directory");
        assert_eq!(names(&view(&Place::path("/w/notes-x"))), strs(&["plain"]));
        // Repo trials (Codex): the scoped kept library of the top, not the subpath-only trial.
        let tr = sync_trials(&x.ledger, &x.paths, &top, "b").unwrap();
        assert_eq!((tr.linked.clone(), tr.subpath_only.clone()), (strs(&["bellwether-dev", "kept-scoped", "late"]), strs(&["trial-scoped"])));
        assert_eq!(std::fs::read_link(tr.dir.join("bellwether-dev")).unwrap(), crate::profile::skills_dir(&x.paths).join("bellwether-dev"));
        assert!(sync_trials(&x.ledger, &x.paths, &other, "b").unwrap().linked.is_empty());
        // Load-time match elsewhere: the scoped entries are not loaded.
        let m = list_for_match(&x.ledger, &x.paths, None, Path::new("/w/api")).unwrap();
        let matched: Vec<&str> = m.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(matched.contains(&"plain") && !matched.iter().any(|n| ["kept-scoped", "late", "trial-scoped"].contains(n)), "{matched:?}");
        // Always-on accounting: scoped names count only where their scope may hold the session.
        let cand = |remote: &str| Entry { predicate: crate::types::Predicate { repo_remote: Some(remote.into()), ..Default::default() }, ..entry("c", "cand", "repo", Status::Provisional) };
        let far = catalog_other_bytes(&x.ledger, &x.paths, &cand("github.com/acme/api"), Path::new("/none")).unwrap();
        let near = catalog_other_bytes(&x.ledger, &x.paths, &cand("github.com/acme/web"), Path::new("/none")).unwrap();
        let cost = |d: PathBuf| crate::lint::entry_cost(&d, Kind::Skill).catalog;
        let web_only: usize = ["bellwether-dev", "kept-scoped", "late"].iter().map(|n| cost(crate::profile::skills_dir(&x.paths).join(n))).sum::<usize>()
            + cost(x.paths.staging.join("shared/trial-scoped"));
        assert_eq!(near - far, web_only);

        // Unset: global again.
        policy(serde_json::json!({"environment": "work"}));
        crate::profile::link_views(&x.paths).unwrap();
        assert!(std::fs::read_link(native("bellwether-dev")).is_ok() && std::fs::read_link(native("kept-scoped")).is_ok());
        let shared = sync_shared_skills(&x.ledger, &x.paths).unwrap();
        assert_eq!(names(&shared), strs(&["bellwether-dev", "kept-scoped", "late", "notes-skill", "plain"]));
        assert_eq!(names(&view(&other)), strs(&["bellwether-dev", "kept-scoped", "late", "notes-skill", "plain", "trial-scoped"]));
    }

    #[test]
    fn approved_agents_are_exposed_only_where_they_match_and_off_unlinks_them() {
        let x = env();
        let mut a = agent_entry("g1", "log-triage");
        a.predicate.repo_glob = "/w/repo".into();
        stage(&x.ledger, &x.paths, &a, &[w("AGENT.md", &agent_md("log-triage", "v1"))]).unwrap();
        approve(&x.ledger, &x.paths, "g1", Approval::Agent).unwrap();
        let link = agent_link(&x.paths, "log-triage");
        assert!(std::fs::read_link(&link).is_ok());
        let x_ = |p: &str| agent_exposure(&x.ledger, &x.paths, &Place::path(p), Path::new(p)).unwrap();
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(x_("/w/repo"), AgentExposure { exposed: names(&["log-triage"]), ..Default::default() });
        assert_eq!(x_("/w/other"), AgentExposure { withheld: names(&["log-triage"]), ..Default::default() }, "withheld outside its place");
        let m = list_for_match(&x.ledger, &x.paths, Some(Path::new("/w/other")), Path::new("/w/other")).unwrap();
        assert!(m.entries.iter().all(|e| e.id != "g1"), "not loaded elsewhere: {m:?}");

        // Native-authored collisions: a user agent (file named otherwise, frontmatter name equal)
        // wins in omp; the name is never withheld (that would hide the user's agent) and the
        // candidate is shadowed in its own place: not offered, not loaded, not credited.
        std::fs::create_dir_all(x.paths.agents_dir()).unwrap();
        let mine = x.paths.agents_dir().join("my-triage.md");
        std::fs::write(&mine, agent_md("log-triage", "mine")).unwrap();
        assert_eq!(x_("/w/other"), AgentExposure::default());
        assert_eq!(x_("/w/repo"), AgentExposure { shadowed: names(&["log-triage"]), ..Default::default() });
        let m = list_for_match(&x.ledger, &x.paths, Some(Path::new("/w/repo")), Path::new("/w/repo")).unwrap();
        assert!(m.entries.iter().all(|e| e.id != "g1"), "a shadowed candidate does not load: {m:?}");
        std::fs::remove_file(&mine).unwrap();
        // the same for a project agent in an ancestor of the cwd (`.omp/agents/`)
        let t = tempfile::tempdir().unwrap();
        let proj = t.path().join("proj");
        std::fs::create_dir_all(proj.join(".omp/agents")).unwrap();
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        std::fs::write(proj.join(".omp/agents/log-triage.md"), agent_md("log-triage", "project")).unwrap();
        let sub = proj.join("sub");
        let got = agent_exposure(&x.ledger, &x.paths, &Place::path(sub.to_string_lossy()), &sub).unwrap();
        assert_eq!(got, AgentExposure::default(), "the project's agent wins and is never withheld");

        std::fs::write(&x.paths.off_marker, "").unwrap();
        sync_shared_skills(&x.ledger, &x.paths).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err(), "off withdraws provisional agents");
        std::fs::remove_file(&x.paths.off_marker).unwrap();
        sync_shared_skills(&x.ledger, &x.paths).unwrap();
        assert!(std::fs::read_link(&link).is_ok(), "on restores the approved link");
    }

    #[test]
    fn catalog_other_bytes_counts_what_shares_a_session_with_the_candidate() {
        let x = env();
        let cost = |e: &Entry| crate::lint::entry_cost(&e.path, e.kind).catalog;
        let mine = stage(&x.ledger, &x.paths, &entry("c", "candidate", "repo", Status::Provisional), &[w("SKILL.md", &skill("candidate"))]).unwrap();
        let same = stage(&x.ledger, &x.paths, &entry("s", "same-branch", "repo", Status::Provisional), &[w("SKILL.md", &skill("same-branch"))]).unwrap();
        let mut far = entry("f", "far", "elsewhere", Status::Provisional);
        far.predicate.repo_glob = "/w/elsewhere".into();
        stage(&x.ledger, &x.paths, &far, &[w("SKILL.md", &skill("far"))]).unwrap();
        let mut kept = entry("k", "kept-one", "elsewhere", Status::Provisional);
        kept.predicate.repo_glob = "/w/elsewhere".into();
        stage(&x.ledger, &x.paths, &kept, &[w("SKILL.md", &skill("kept-one"))]).unwrap();
        let kept = set_status(&x.ledger, &x.paths, "k", Status::Kept, None).unwrap();
        let got = catalog_other_bytes(&x.ledger, &x.paths, &mine, &mine.path).unwrap();
        assert_eq!(got, cost(&same) + cost(&kept), "kept from another branch counts, a far provisional one does not");
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
        // unapproved: no staging link, not listed by match, offered for approval
        assert!(std::fs::symlink_metadata(&link).is_err(), "a provisional agent loads only once approved");
        let m = list_for_match(&x.ledger, &x.paths, Some(Path::new("/w/repo")), Path::new("/w/repo")).unwrap();
        assert!(m.entries.is_empty(), "{m:?}");
        assert_eq!(pending_parts(&x.paths, &a1), vec![Approval::Agent]);
        let rc = approve(&x.ledger, &x.paths, "a1", Approval::Agent).unwrap();
        assert_eq!(rc.activation, Activation::Prompted);
        assert!(pending_parts(&x.paths, &x.ledger.get_entry("a1").unwrap().unwrap()).is_empty());
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
        assert!(m.entries.is_empty(), "the user's own log-triage.md wins in omp: the candidate does not load (shadowed): {m:?}");
        std::fs::rename(&native, native.with_extension("bak")).unwrap();
        let m = list_for_match(&x.ledger, &x.paths, Some(Path::new("/w/repo")), Path::new("/w/repo")).unwrap();
        let got: Vec<(&str, Kind, bool)> = m.entries.iter().map(|e| (e.id.as_str(), e.kind, e.predicate_hit)).collect();
        assert_eq!(got, vec![("a1", Kind::Agent, true)]);
        std::fs::rename(native.with_extension("bak"), &native).unwrap();
        let m = list_for_match(&x.ledger, &x.paths, Some(Path::new("/elsewhere")), Path::new("/elsewhere")).unwrap();
        assert!(m.entries.is_empty(), "another place withholds the approved agent: {m:?}");

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

    #[test]
    fn agent_links_follow_draft_activate_retire_and_kept_retire() {
        let x = env();
        stage(&x.ledger, &x.paths, &Entry { status: Status::Draft, ..agent_entry("d1", "draft-agent") }, &[w("AGENT.md", &agent_md("draft-agent", "d"))]).unwrap();
        let link = agent_link(&x.paths, "draft-agent");
        assert!(std::fs::symlink_metadata(&link).is_err(), "drafts never load");
        let p = activate_draft(&x.ledger, &x.paths, "draft-agent").unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err(), "activated, not yet approved");
        approve(&x.ledger, &x.paths, "d1", Approval::Agent).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), p.path.join("AGENT.md"));
        let d = set_status(&x.ledger, &x.paths, "d1", Status::Dormant, None).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err());
        let back = set_status(&x.ledger, &x.paths, "d1", Status::Provisional, None).unwrap();
        assert_ne!(d.path, back.path);
        assert_eq!(std::fs::read_link(&link).unwrap(), back.path.join("AGENT.md"), "the approval survives status moves");
        // an edited AGENT.md is pending again: the next sync unlinks it, approving relinks it
        std::fs::write(back.path.join("AGENT.md"), agent_md("draft-agent", "edited")).unwrap();
        let edited = x.ledger.get_entry("d1").unwrap().unwrap();
        assert!(!agent_approved(&edited));
        assert_eq!(pending_parts(&x.paths, &edited), vec![Approval::Agent]);
        assert_eq!(unlink_unapproved_agents(&x.ledger, &x.paths).unwrap(), vec!["draft-agent".to_string()]);
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert!(list_for_match(&x.ledger, &x.paths, None, Path::new("/w/repo")).unwrap().entries.is_empty());
        approve(&x.ledger, &x.paths, "d1", Approval::Agent).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), back.path.join("AGENT.md"));
        assert!(unlink_unapproved_agents(&x.ledger, &x.paths).unwrap().is_empty());
        disable(&x.ledger, &x.paths, "d1", "noisy").unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err(), "retired agents unload");

        stage(&x.ledger, &x.paths, &agent_entry("k1", "kept-agent"), &[w("AGENT.md", &agent_md("kept-agent", "k"))]).unwrap();
        set_status(&x.ledger, &x.paths, "k1", Status::Kept, None).unwrap();
        let native = kept_agent_file(&x.paths, "kept-agent");
        assert!(native.is_file());
        set_status(&x.ledger, &x.paths, "k1", Status::Stale, None).unwrap();
        assert!(!native.exists(), "a stale kept agent leaves omp's agent dir");
    }

    #[test]
    fn read_only_agent_is_exposed_without_approval_while_write_agent_stays_pending() {
        let x = env();
        let ro_md = "---\nname: ro-triage\ndescription: Use for reading logs.\ntools: [read, grep, glob, lsp_diagnostics, web_search]\n---\nRead the log.\n";
        let rw_md = "---\nname: rw-fixer\ndescription: Use for fixing logs.\ntools: [read, edit, bash]\n---\nFix the log.\n";
        let ro = stage(&x.ledger, &x.paths, &agent_entry("a-ro", "ro-triage"), &[w("AGENT.md", ro_md)]).unwrap();
        let rw = stage(&x.ledger, &x.paths, &agent_entry("a-rw", "rw-fixer"), &[w("AGENT.md", rw_md)]).unwrap();
        assert!(agent_is_read_only(&ro));
        assert!(!agent_is_read_only(&rw));
        assert!(agent_approved(&ro), "read-only agent is approved automatically");
        assert!(!agent_approved(&rw), "write-capable agent stays unapproved");
        assert!(pending_parts(&x.paths, &ro).is_empty());
        assert_eq!(pending_parts(&x.paths, &rw), vec![Approval::Agent]);
        assert!(std::fs::read_link(agent_link(&x.paths, "ro-triage")).is_ok(), "read-only agent linked on stage");
        assert!(std::fs::symlink_metadata(agent_link(&x.paths, "rw-fixer")).is_err(), "write agent not linked");
        let exp = agent_exposure(&x.ledger, &x.paths, &Place::path("/w/repo"), Path::new("/w/repo")).unwrap();
        assert_eq!(exp.exposed, vec!["ro-triage".to_string()]);
        let rec = latest_approval_record(&x.ledger, "a-ro", Approval::Agent).expect("approval receipt");
        assert_eq!(rec.approver.as_deref(), Some("read-only"));
    }
}
