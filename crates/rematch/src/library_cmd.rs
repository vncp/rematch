//! `library ls|why|disable|rollback|pin|unpin|approve|decline|set-fork-path|export|import-export|sync`
//! and the `off|on` kill switch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use rematch_core::import_codex::CODEX_COPY_MARKER;
use rematch_core::ledger::{enum_str, EntryFilter, SessionFilter};
use rematch_core::library::{self, Approval, REMATCH_JSON, SKILL_MD};
use rematch_core::predicate;
use rematch_core::redirect;
use rematch_core::types::{Entry, Kind, SessionRecord, Status};
use serde_json::{json, Value};

use crate::app::AppCtx;
use crate::reflect::{is_foreign, record_status};

#[derive(Subcommand, Debug, Clone)]
pub enum LibraryCmd {
    /// Every entry with raw counters.
    Ls,
    /// Loaded entries, predicate hits, redirects and counters of a session (default: latest for --cwd).
    Why { session: Option<String> },
    /// Retire an entry now (user disable; counts as harm for that session).
    Disable {
        id: String,
        #[arg(long, default_value = "user request")]
        reason: String,
    },
    /// Restore the predecessor of a kept skill or agent; retire the current one.
    Rollback { skill: String },
    Pin { id: String },
    Unpin { id: String },
    /// One-tap approval of an entry's scripts or hook (or an `export:<id>` item).
    Approve { id: String },
    /// Decline an approval (retires the entry; `export:<id>` only suppresses the offer).
    Decline { id: String },
    /// Set the repo-relative file `ship --scope fork` writes for an entry (and, with
    /// --repo, the fork checkout it goes to).
    SetForkPath {
        id: String,
        path: String,
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Work → home export of invariant entries.
    Export(crate::scope::export::ExportArgs),
    /// Import an export directory.
    ImportExport(crate::scope::export::ImportExportArgs),
    /// Deliver entries to another harness's skill directories (DESIGN §18.2.3).
    Sync {
        /// Target harness.
        #[arg(long, value_enum)]
        harness: SyncHarness,
    },
}

/// Harnesses `library sync` delivers to. omp loads entries itself (`match`); Claude Code
/// through its plugin.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncHarness {
    Codex,
}

pub fn cmd_library(ctx: &AppCtx, cmd: LibraryCmd) -> Result<Value> {
    match cmd {
        LibraryCmd::Ls => ls(ctx),
        LibraryCmd::Why { session } => why(ctx, session.as_deref()),
        LibraryCmd::Disable { id, reason } => {
            let e = get(ctx, &id)?;
            let e = if is_foreign(ctx, &e) {
                let mut u = e.clone();
                u.pinned = false;
                record_status(ctx, &u, Status::Retired, &format!("disabled: {reason} (files left in place)"))?
            } else {
                library::disable(&ctx.ledger, &ctx.paths, &e.id, &reason)?
            };
            Ok(json!({"entry": e, "lines": [format!("disabled {} ({})", e.name, e.id)]}))
        }
        LibraryCmd::Rollback { skill } => {
            if let Some(cur) = ctx
                .ledger
                .list_entries(&EntryFilter { name: Some(skill.clone()), ..Default::default() })?
                .into_iter()
                .rev()
                .find(|e| matches!(e.status, Status::Kept | Status::Pinned))
            {
                if is_foreign(ctx, &cur) {
                    bail!("{skill} is a registered skill whose files rematch does not own; nothing to roll back");
                }
            }
            let r = library::rollback(&ctx.ledger, &ctx.paths, &skill)?;
            Ok(json!({
                "rollback": r,
                "lines": [format!("rolled back {skill} to v{}; retired {}", r.restored_version, r.retired.id)],
            }))
        }
        LibraryCmd::Pin { id } => {
            let e = set_pinned(ctx, &id, true)?;
            Ok(json!({"entry": e, "lines": [format!("pinned {}", e.name)]}))
        }
        LibraryCmd::Unpin { id } => {
            let e = set_pinned(ctx, &id, false)?;
            Ok(json!({"entry": e, "lines": [format!("unpinned {}", e.name)]}))
        }
        LibraryCmd::Approve { id } => {
            if id.starts_with("export:") {
                return crate::scope::export::approve(ctx, &id);
            }
            let e = get(ctx, &id)?;
            if is_foreign(ctx, &e) {
                bail!("{} is not a rematch-owned entry; nothing to approve", e.name);
            }
            let r = library::approve(&ctx.ledger, &ctx.paths, &e.id, approval_for(&e))?;
            Ok(json!({"receipt": r, "lines": [format!("approved {} of {}", enum_str(&approval_for(&e)), e.name)]}))
        }
        LibraryCmd::Decline { id } => {
            if id.starts_with("export:") {
                return crate::scope::export::decline(ctx, &id);
            }
            let e = get(ctx, &id)?;
            if is_foreign(ctx, &e) {
                bail!("{} is not a rematch-owned entry; nothing to decline", e.name);
            }
            let r = library::decline(&ctx.ledger, &ctx.paths, &e.id, approval_for(&e))?;
            Ok(json!({"entry": r, "lines": [format!("declined {}; entry retired", e.name)]}))
        }
        LibraryCmd::SetForkPath { id, path, repo } => set_fork_path(ctx, &id, &path, repo.as_deref()),
        LibraryCmd::Export(a) => crate::scope::export::cmd_export(ctx, a),
        LibraryCmd::ImportExport(a) => crate::scope::export::cmd_import_export(ctx, a),
        LibraryCmd::Sync { harness: SyncHarness::Codex } => sync_codex(ctx, &codex_user_skills()?),
    }
}

fn get(ctx: &AppCtx, id: &str) -> Result<Entry> {
    ctx.ledger.find_entry(id)?.ok_or_else(|| anyhow!("no entry {id:?}"))
}

/// Pinning a foreign entry is a ledger flag only (its files are never rewritten).
fn set_pinned(ctx: &AppCtx, id: &str, pinned: bool) -> Result<Entry> {
    let e = get(ctx, id)?;
    if is_foreign(ctx, &e) {
        let mut u = e;
        u.pinned = pinned;
        u.updated_at = ctx.now;
        ctx.ledger.upsert_entry(&u)?;
        return Ok(u);
    }
    library::set_pinned(&ctx.ledger, &e.id, pinned)
}

fn approval_for(e: &Entry) -> Approval {
    if e.kind == Kind::Hook {
        Approval::Hook
    } else {
        Approval::Scripts
    }
}

/// `fork_path` (+ `fork_repo`) into the ledger and the entry's `rematch.json`. A changed
/// target clears `fork_commit`: the drift baseline belonged to the old one.
fn set_fork_path(ctx: &AppCtx, id: &str, path: &str, repo: Option<&std::path::Path>) -> Result<Value> {
    library::validate_fork_path(path)?;
    let mut e = crate::scope::resolve_entry(ctx, id)?;
    let repo = repo.map(crate::app::absolute).transpose()?;
    let before = (e.fork_path.clone(), e.fork_repo.clone());
    e.fork_path = Some(path.to_string());
    if repo.is_some() {
        e.fork_repo = repo;
    }
    if (e.fork_path.clone(), e.fork_repo.clone()) != before {
        e.fork_commit = None;
    }
    e.updated_at = ctx.now;
    crate::scope::save_entry(ctx, &e)?;
    let to = e.fork_repo.as_ref().map(|r| format!(" in {}", r.display())).unwrap_or_default();
    Ok(json!({"entry": e, "lines": [format!("{} ships to {path}{to} with `ship --scope fork`", e.name)]}))
}

fn ls(ctx: &AppCtx) -> Result<Value> {
    let mut entries = Vec::new();
    let mut lines = Vec::new();
    for e in ctx.ledger.list_entries(&EntryFilter::default())? {
        let c = ctx.ledger.counters(&e.id)?;
        lines.push(format!(
            "{:<11} {:<6} {} [{}] matched {} · recurrence {} · harm {}{}{}",
            enum_str(&e.status),
            enum_str(&e.kind),
            e.name,
            e.branch,
            c.matched,
            c.recurrence,
            c.harm,
            if e.pinned { " · pinned" } else { "" },
            e.fork_path.as_deref().map(|p| format!(" · fork:{p}")).unwrap_or_default()
        ));
        entries.push(json!({
            "id": e.id, "name": e.name, "kind": e.kind, "status": e.status, "branch": e.branch, "family": e.family,
            "pinned": e.pinned, "origin": e.origin, "trigger_line": e.trigger_line, "path": e.path,
            "predicate": e.predicate, "counters": c, "evidence": e.evidence.len(), "version": e.version,
            "fork_path": e.fork_path, "fork_repo": e.fork_repo,
        }));
    }
    if lines.is_empty() {
        lines.push("library is empty".into());
    }
    Ok(json!({"entries": entries, "lines": lines}))
}

/// Latest session for the cwd (repo or cwd match), preferring our own traces.
fn latest_session(ctx: &AppCtx) -> Result<Option<SessionRecord>> {
    let cwd = ctx.cwd.to_string_lossy().to_string();
    let repo = rematch_core::import::repo_of(&ctx.cwd).map(|r| r.to_string_lossy().to_string());
    let all = ctx.ledger.list_sessions(&SessionFilter::default())?;
    let here = |s: &SessionRecord| s.cwd == cwd || (repo.is_some() && s.repo == repo);
    Ok(all
        .iter()
        .rev()
        .find(|s| here(s) && s.source == "trace")
        .or_else(|| all.iter().rev().find(|s| here(s)))
        .cloned())
}

fn why(ctx: &AppCtx, session: Option<&str>) -> Result<Value> {
    let s = match session {
        Some(id) => ctx.ledger.get_session(id)?.ok_or_else(|| anyhow!("no session {id} in the ledger (run `rematch count` after it ends)"))?,
        None => latest_session(ctx)?.ok_or_else(|| anyhow!("no session recorded for {}", ctx.cwd.display()))?,
    };
    let place = s.repo.clone().unwrap_or_else(|| s.cwd.clone());
    let rows: Vec<_> = ctx.ledger.list_counters()?;
    let mut loaded = Vec::new();
    let mut lines = vec![format!(
        "session {} · {} · {} turns · {} tool calls ({} errors, {} clusters) · {} takeovers",
        s.id,
        s.model.as_ref().map(|m| format!("{}/{}", m.provider, m.id)).unwrap_or_else(|| "model unknown".into()),
        s.turns,
        s.tool_calls,
        s.tool_errors,
        s.error_clusters,
        s.takeovers
    )];
    for id in &s.loaded_entries {
        let Some(e) = ctx.ledger.get_entry(id)? else {
            loaded.push(json!({"id": id, "missing": true}));
            continue;
        };
        let glob = predicate::matches_repo(&e.predicate, &place);
        let hit = predicate::count_match(&e.predicate, &place, &s.exts, &s.stems);
        let this = ctx.ledger.counter_rows(&e.id)?.into_iter().find(|r| r.session_id == s.id);
        let agg = rows.iter().find(|(eid, _)| eid == &e.id).map(|(_, c)| c.clone()).unwrap_or_default();
        lines.push(format!(
            "loaded {} ({}): glob {} · predicate {} · this session {} · total matched {} recurrence {} harm {}",
            e.name,
            enum_str(&e.status),
            if glob { "hit" } else { "miss" },
            if hit { "hit" } else { "miss" },
            this.as_ref().map(|r| format!("recurrence {} harm {}", r.recurrence, r.harm)).unwrap_or_else(|| "not counted".into()),
            agg.matched,
            agg.recurrence,
            agg.harm
        ));
        loaded.push(json!({
            "id": e.id, "name": e.name, "status": e.status, "predicate": e.predicate, "glob_hit": glob,
            "predicate_hit": hit, "session_counters": this, "counters": agg,
            "stems_overlap": e.predicate.stems.iter().filter(|x| s.stems.contains(x)).collect::<Vec<_>>(),
        }));
    }
    if s.loaded_entries.is_empty() {
        lines.push("no rematch entries were loaded".into());
    }
    let redirects: Vec<Value> = s
        .redirects
        .iter()
        .map(|r| {
            let class = redirect::effective_class(r);
            lines.push(format!(
                "redirect #{} {} → {}: {}",
                r.seq,
                r.prelabel,
                class.map(redirect::class_name).unwrap_or("unclassified"),
                rematch_core::features::head(&r.text_head, 120)
            ));
            json!({"seq": r.seq, "prelabel": r.prelabel, "class": class, "confidence": r.confidence, "text_head": r.text_head})
        })
        .collect();
    Ok(json!({
        "session": {"id": s.id, "cwd": s.cwd, "repo": s.repo, "family": s.family, "model": s.model, "started_at": s.started_at,
                     "ended_at": s.ended_at, "turns": s.turns, "tokens_in": s.tokens_in, "tokens_out": s.tokens_out,
                     "tool_calls": s.tool_calls, "tool_errors": s.tool_errors, "error_clusters": s.error_clusters,
                     "takeovers": s.takeovers, "skill_reads": s.skill_reads, "source": s.source, "counted": s.counted},
        "loaded": loaded,
        "redirects": redirects,
        "lines": lines,
    }))
}

pub fn cmd_off(ctx: &AppCtx) -> Result<Value> {
    std::fs::create_dir_all(&ctx.paths.root)?;
    std::fs::write(&ctx.paths.off_marker, "")?;
    Ok(json!({"off": true, "marker": ctx.paths.off_marker, "lines": ["rematch is off"]}))
}

pub fn cmd_on(ctx: &AppCtx) -> Result<Value> {
    if ctx.paths.off_marker.exists() {
        std::fs::remove_file(&ctx.paths.off_marker)?;
    }
    let env_off = matches!(std::env::var("REMATCH").ok().as_deref().map(str::trim), Some("0" | "false" | "off"));
    let mut lines = vec!["rematch is on".to_string()];
    if env_off {
        lines = vec!["marker removed, but REMATCH=0 is set in the environment".into()];
    }
    Ok(json!({"off": ctx.paths.is_off(), "marker": ctx.paths.off_marker, "env_off": env_off, "lines": lines}))
}

// ---- library sync --harness codex (DESIGN §18.2.3) ----

/// Directory under REMATCH_ROOT holding one symlink per provisional entry that matches the
/// repo of the last `library sync --harness codex`; Codex reaches it through a symlink in
/// one of its skill roots (`~/.agents/skills/rematch` or `<repo>/.agents/skills/rematch`).
pub const CODEX_LINKS_DIR: &str = "codex-skills";
/// Entry files that are bookkeeping or reach a repo only through `ship`, never a skill root.
const NOT_COPIED: &[&str] = &[REMATCH_JSON, rematch_core::experience::EXPERIENCE_MD, "AGENTS.fragment.md", "fork"];

/// Codex's user skill root, `$HOME/.agents/skills` (developers.openai.com/codex/skills).
fn codex_user_skills() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".agents").join("skills"))
}

/// Kept-entry copies `library sync --harness codex` wrote (directories under
/// `~/.agents/skills/` holding [`CODEX_COPY_MARKER`]); `uninstall` removes them.
pub fn codex_copies() -> Vec<PathBuf> {
    let Ok(dir) = codex_user_skills() else { return Vec::new() };
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join(CODEX_COPY_MARKER).is_file())
        .collect();
    out.sort();
    out
}

/// Entries Codex can load: a rematch-owned directory with a `SKILL.md`. Agents, hooks and
/// lessons have no Codex form (Codex runs no rematch hook and reads no rematch agent).
fn codex_loadable(ctx: &AppCtx, e: &Entry) -> bool {
    !matches!(e.kind, Kind::Agent | Kind::Hook | Kind::Lesson) && !is_foreign(ctx, e) && e.path.join(SKILL_MD).is_file()
}

fn copy_entry(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for ent in std::fs::read_dir(src)? {
        let ent = ent?;
        let name = ent.file_name();
        if NOT_COPIED.iter().any(|n| name == *n) {
            continue;
        }
        let (s, d) = (ent.path(), dst.join(&name));
        if ent.file_type()?.is_dir() {
            copy_entry(&s, &d)?;
        } else {
            // fs::copy keeps the mode: approved scripts stay executable, unapproved ones do not.
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

fn marker_of(dir: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(CODEX_COPY_MARKER)).ok()?).ok()
}

/// `library sync --harness codex`: Codex has no per-session gating and no runtime hook, so
/// delivery is files. (1) `REMATCH_ROOT/codex-skills/<name>` → the staging dir of every
/// provisional entry `match` loads for `--cwd` with a predicate hit (stale links removed;
/// all removed while rematch is off). (2) Every kept/pinned entry copied to
/// `<user_skills>/<name>/` with [`CODEX_COPY_MARKER`]; a same-named directory without the
/// marker is the user's and is reported as a conflict; marked copies of entries no longer
/// kept are removed.
pub fn sync_codex(ctx: &AppCtx, user_skills: &Path) -> Result<Value> {
    let repo = rematch_core::import::repo_of(&ctx.cwd);
    let m = library::list_for_match(&ctx.ledger, &ctx.paths, repo.as_deref(), &ctx.cwd)?;
    let links = ctx.paths.root.join(CODEX_LINKS_DIR);
    std::fs::create_dir_all(&links)?;

    let mut want: BTreeMap<String, Entry> = BTreeMap::new();
    for me in m.entries.iter().filter(|me| me.status == Status::Provisional && me.predicate_hit) {
        if let Some(e) = ctx.ledger.get_entry(&me.id)?.filter(|e| codex_loadable(ctx, e)) {
            want.insert(e.name.clone(), e);
        }
    }
    let (mut unlinked, mut untouched) = (Vec::new(), Vec::new());
    for ent in std::fs::read_dir(&links)? {
        let p = ent?.path();
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !std::fs::symlink_metadata(&p)?.file_type().is_symlink() {
            untouched.push(name);
            continue;
        }
        if want.get(&name).is_some_and(|e| std::fs::read_link(&p).is_ok_and(|t| t == e.path)) {
            continue;
        }
        std::fs::remove_file(&p)?;
        if !want.contains_key(&name) {
            unlinked.push(name);
        }
    }
    unlinked.sort();
    untouched.sort();
    let mut linked = Vec::new();
    for (name, e) in &want {
        let p = links.join(name);
        if std::fs::symlink_metadata(&p).is_err() {
            std::os::unix::fs::symlink(&e.path, &p)?;
        }
        linked.push(json!({"name": name, "id": e.id, "target": e.path}));
    }

    let kept: Vec<Entry> = [Status::Kept, Status::Pinned]
        .into_iter()
        .map(|s| ctx.ledger.list_entries(&EntryFilter { status: Some(s), ..Default::default() }))
        .collect::<Result<Vec<_>>>()?
        .concat()
        .into_iter()
        .filter(|e| codex_loadable(ctx, e))
        .collect();
    std::fs::create_dir_all(user_skills)?;
    let (mut copied, mut current, mut conflicts, mut removed) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for e in &kept {
        let dst = user_skills.join(&e.name);
        let marker = json!({"id": e.id, "version": e.version, "updated_at": e.updated_at, "created_at": e.created_at});
        let ours = marker_of(&dst);
        if ours.is_none() && std::fs::symlink_metadata(&dst).is_ok() {
            conflicts.push(json!({"name": e.name, "id": e.id, "path": dst}));
            continue;
        }
        if ours.as_ref() == Some(&marker) {
            current.push(e.name.clone());
            continue;
        }
        if ours.is_some() {
            std::fs::remove_dir_all(&dst)?;
        }
        copy_entry(&e.path, &dst)?;
        std::fs::write(dst.join(CODEX_COPY_MARKER), serde_json::to_string(&marker)?)?;
        copied.push(json!({"name": e.name, "id": e.id, "path": dst}));
    }
    for ent in std::fs::read_dir(user_skills)? {
        let p = ent?.path();
        let Some(id) = marker_of(&p).and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string)) else {
            continue;
        };
        if !kept.iter().any(|e| e.id == id && p.file_name() == Some(e.name.as_ref())) {
            std::fs::remove_dir_all(&p)?;
            removed.push(p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        }
    }

    removed.sort();
    // Where Codex reaches the links dir from: the user root or the repo's root.
    let canon_links = std::fs::canonicalize(&links)?;
    let reached_from: Vec<PathBuf> = std::iter::once(user_skills.join("rematch"))
        .chain(repo.iter().map(|r| r.join(".agents").join("skills").join("rematch")))
        .filter(|p| std::fs::canonicalize(p).is_ok_and(|c| c == canon_links))
        .collect();
    let place = repo.as_deref().unwrap_or(&ctx.cwd).display().to_string();
    let mut lines = vec![format!(
        "codex: {} provisional link(s) for {place} in {}; {} kept cop(ies) written, {} current, {} removed",
        linked.len(),
        links.display(),
        copied.len(),
        current.len(),
        removed.len()
    )];
    for c in &conflicts {
        lines.push(format!("conflict: {} exists and is not rematch's; kept entry {} not copied", c["path"].as_str().unwrap_or_default(), c["id"].as_str().unwrap_or_default()));
    }
    if reached_from.is_empty() {
        lines.push(format!("Codex does not see {} yet: ln -s {} {}", links.display(), links.display(), user_skills.join("rematch").display()));
    }
    lines.push("Codex has no per-session gating: the links serve every Codex session until the next sync".into());
    Ok(json!({
        "harness": "codex",
        "repo": repo,
        "off": m.off,
        "links_dir": links,
        "linked": linked,
        "unlinked": unlinked,
        "untouched": untouched,
        "user_skills": user_skills,
        "kept": {"copied": copied, "current": current, "removed": removed, "conflicts": conflicts},
        "reached_from": reached_from,
        "lines": lines,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{new_entry, NewEntry};
    use rematch_core::ledger::Ledger;
    use rematch_core::paths::RematchPaths;
    use rematch_core::types::{Edit, Family, Origin, Predicate};

    fn ctx(dir: &Path, cwd: &Path) -> AppCtx {
        let paths = RematchPaths::with_roots(dir.join("rematch"), dir.join("agent"));
        paths.ensure_dirs().unwrap();
        let ledger = Ledger::open(&paths.ledger_file).unwrap();
        AppCtx { paths, ledger, now: rematch_core::ledger::now_ms(), cwd: cwd.to_path_buf(), text: false }
    }

    fn stage(c: &AppCtx, name: &str, repo: &Path) -> Entry {
        let e = new_entry(
            NewEntry {
                kind: Kind::Skill,
                name: name.into(),
                branch: "payments".into(),
                status: Status::Provisional,
                family: Family("openai/gpt".into()),
                origin: Origin::Description,
                predicate: Predicate { repo_glob: repo.to_string_lossy().into_owned(), file_exts: vec![], stems: vec![] },
                trigger_line: "Use when testing.".into(),
                path: PathBuf::new(),
            },
            c.now,
        )
        .unwrap();
        let body = format!("---\nname: {name}\ndescription: Use when testing.\n---\n\nUse when testing.\n");
        library::stage(&c.ledger, &c.paths, &e, &[Edit::Write { path: SKILL_MD.into(), content: body }]).unwrap()
    }

    #[test]
    fn sync_codex_links_provisional_copies_kept_and_respects_user_dirs() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let (repo, other) = (base.join("payments"), base.join("other"));
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(other.join(".git")).unwrap();
        let user = base.join("home/.agents/skills");
        let c = ctx(&base, &repo);
        let a = stage(&c, "ledger-tests", &repo);
        let b = stage(&c, "settle-first", &repo);

        let d = sync_codex(&c, &user).unwrap();
        let links = c.paths.root.join(CODEX_LINKS_DIR);
        assert_eq!(std::fs::read_link(links.join("ledger-tests")).unwrap(), a.path, "{d}");
        assert_eq!(d["linked"].as_array().unwrap().len(), 2);
        assert_eq!(d["reached_from"], json!([]));
        assert!(d["lines"].to_string().contains("ln -s"), "{d}");

        // Another repo: no predicate hit, links go; Codex reaching the dir is reported.
        std::os::unix::fs::symlink(&links, user.join("rematch")).unwrap();
        let c2 = ctx(&base, &other);
        let d2 = sync_codex(&c2, &user).unwrap();
        assert_eq!((d2["linked"].clone(), d2["unlinked"].clone()), (json!([]), json!(["ledger-tests", "settle-first"])), "{d2}");
        assert_eq!(d2["reached_from"], json!([user.join("rematch")]));

        // Kept: copied with the marker (no rematch.json); a user dir of the same name is left alone.
        library::set_status(&c.ledger, &c.paths, &a.id, Status::Kept, None).unwrap();
        library::set_status(&c.ledger, &c.paths, &b.id, Status::Kept, None).unwrap();
        std::fs::create_dir_all(user.join("settle-first")).unwrap();
        std::fs::write(user.join("settle-first/SKILL.md"), "mine").unwrap();
        let d3 = sync_codex(&c, &user).unwrap();
        let copy = user.join("ledger-tests");
        assert!(copy.join(SKILL_MD).is_file() && copy.join(CODEX_COPY_MARKER).is_file(), "{d3}");
        assert!(!copy.join(REMATCH_JSON).exists());
        assert_eq!(d3["kept"]["conflicts"][0]["name"], "settle-first", "{d3}");
        assert_eq!(std::fs::read_to_string(user.join("settle-first/SKILL.md")).unwrap(), "mine");
        assert_eq!(d3["linked"], json!([]), "kept entries are not provisional links");
        assert_eq!(sync_codex(&c, &user).unwrap()["kept"]["current"], json!(["ledger-tests"]), "idempotent");

        // Retired: the marked copy goes, the user's dir stays.
        library::disable(&c.ledger, &c.paths, &a.id, "test").unwrap();
        let d4 = sync_codex(&c, &user).unwrap();
        assert_eq!(d4["kept"]["removed"], json!(["ledger-tests"]), "{d4}");
        assert!(!copy.exists() && user.join("settle-first").is_dir());
    }
}
