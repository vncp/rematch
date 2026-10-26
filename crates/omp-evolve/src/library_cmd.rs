//! `library ls|why|disable|rollback|pin|unpin|approve|decline|export|import-export`
//! and the `off|on` kill switch.

use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use evolve_core::ledger::{enum_str, EntryFilter, SessionFilter};
use evolve_core::library::{self, Approval};
use evolve_core::predicate;
use evolve_core::redirect;
use evolve_core::types::{Entry, Kind, SessionRecord, Status};
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
    /// Restore the predecessor of a kept skill; retire the current one.
    Rollback { skill: String },
    Pin { id: String },
    Unpin { id: String },
    /// One-tap approval of an entry's scripts or hook (or an `export:<id>` item).
    Approve { id: String },
    /// Decline an approval (retires the entry; `export:<id>` only suppresses the offer).
    Decline { id: String },
    /// Work → home export of invariant entries.
    Export(crate::scope::export::ExportArgs),
    /// Import an export directory.
    ImportExport(crate::scope::export::ImportExportArgs),
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
                    bail!("{skill} is a registered skill whose files omp-evolve does not own; nothing to roll back");
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
                bail!("{} is not an omp-evolve-owned entry; nothing to approve", e.name);
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
                bail!("{} is not an omp-evolve-owned entry; nothing to decline", e.name);
            }
            let r = library::decline(&ctx.ledger, &ctx.paths, &e.id, approval_for(&e))?;
            Ok(json!({"entry": r, "lines": [format!("declined {}; entry retired", e.name)]}))
        }
        LibraryCmd::Export(a) => crate::scope::export::cmd_export(ctx, a),
        LibraryCmd::ImportExport(a) => crate::scope::export::cmd_import_export(ctx, a),
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

fn ls(ctx: &AppCtx) -> Result<Value> {
    let mut entries = Vec::new();
    let mut lines = Vec::new();
    for e in ctx.ledger.list_entries(&EntryFilter::default())? {
        let c = ctx.ledger.counters(&e.id)?;
        lines.push(format!(
            "{:<11} {:<6} {} [{}] matched {} · recurrence {} · harm {}{}",
            enum_str(&e.status),
            enum_str(&e.kind),
            e.name,
            e.branch,
            c.matched,
            c.recurrence,
            c.harm,
            if e.pinned { " · pinned" } else { "" }
        ));
        entries.push(json!({
            "id": e.id, "name": e.name, "kind": e.kind, "status": e.status, "branch": e.branch, "family": e.family,
            "pinned": e.pinned, "origin": e.origin, "trigger_line": e.trigger_line, "path": e.path,
            "predicate": e.predicate, "counters": c, "evidence": e.evidence.len(), "version": e.version,
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
    let repo = evolve_core::import::repo_of(&ctx.cwd).map(|r| r.to_string_lossy().to_string());
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
        Some(id) => ctx.ledger.get_session(id)?.ok_or_else(|| anyhow!("no session {id} in the ledger (run `omp-evolve count` after it ends)"))?,
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
        lines.push("no omp-evolve entries were loaded".into());
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
                evolve_core::features::head(&r.text_head, 120)
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
    Ok(json!({"off": true, "marker": ctx.paths.off_marker, "lines": ["omp-evolve is off"]}))
}

pub fn cmd_on(ctx: &AppCtx) -> Result<Value> {
    if ctx.paths.off_marker.exists() {
        std::fs::remove_file(&ctx.paths.off_marker)?;
    }
    let env_off = matches!(std::env::var("OMP_EVOLVE").ok().as_deref().map(str::trim), Some("0" | "false" | "off"));
    let mut lines = vec!["omp-evolve is on".to_string()];
    if env_off {
        lines = vec!["marker removed, but OMP_EVOLVE=0 is set in the environment".into()];
    }
    Ok(json!({"off": ctx.paths.is_off(), "marker": ctx.paths.off_marker, "env_off": env_off, "lines": lines}))
}
