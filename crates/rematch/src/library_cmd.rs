//! `library ls|why|disable|rollback|pin|unpin|approve|decline|set-fork-path|export|import-export`
//! and the `off|on` kill switch.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use rematch_core::ledger::{enum_str, redirect_key, EntryFilter, JournalRow, SessionFilter};
use rematch_core::library::{self, Approval};
use rematch_core::predicate;
use rematch_core::redirect;
use rematch_core::types::{Entry, Kind, SessionRecord, Status};
use serde_json::{json, Value};

use crate::app::AppCtx;
use crate::lake_read::{self, Lookup, Source};
use crate::reflect::{is_foreign, record_status};

#[derive(Subcommand, Debug, Clone)]
pub enum LibraryCmd {
    /// Every entry with raw counters.
    Ls,
    /// Loaded entries, predicate hits, redirects and counters of a session (default: latest for --cwd).
    Why {
        session: Option<String>,
        /// Also the status each loaded entry and the class each redirect had at this time
        /// (ms, RFC 3339, `YYYY-MM-DDTHH:MM[:SS]` UTC, or `YYYY-MM-DD` = 00:00 UTC), from the
        /// lake's `entries`/`redirects` or this ledger's journal.
        #[arg(long, value_name = "TS")]
        asof: Option<String>,
    },
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
}

pub fn cmd_library(ctx: &AppCtx, cmd: LibraryCmd) -> Result<Value> {
    match cmd {
        LibraryCmd::Ls => ls(ctx),
        LibraryCmd::Why { session, asof } => why(ctx, session.as_deref(), asof.as_deref()),
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
    }
}

fn get(ctx: &AppCtx, id: &str) -> Result<Entry> {
    ctx.ledger.find_entry(id)?.ok_or_else(|| anyhow!("no entry {id:?}"))
}

/// Pinning a foreign entry is a ledger flag only (its files are never rewritten). A pin links
/// the entry into the shared skills dir whatever cwd synced; an unpin leaves it to its status.
fn set_pinned(ctx: &AppCtx, id: &str, pinned: bool) -> Result<Entry> {
    let e = get(ctx, id)?;
    let e = if is_foreign(ctx, &e) {
        let mut u = e;
        u.pinned = pinned;
        u.updated_at = ctx.now;
        ctx.ledger.upsert_entry(&u)?;
        u
    } else {
        library::set_pinned(&ctx.ledger, &e.id, pinned)?
    };
    library::sync_shared_skills(&ctx.ledger, &ctx.paths, None)?;
    Ok(e)
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

/// `--asof` → ms UTC: an integer ms, RFC 3339, `YYYY-MM-DDTHH:MM[:SS]` (UTC) or a bare
/// `YYYY-MM-DD` (00:00 UTC).
pub fn parse_asof(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    for f in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, f) {
            return Ok(t.and_utc().timestamp_millis());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).expect("midnight").and_utc().timestamp_millis());
    }
    Err(crate::envelope::Usage(format!("--asof {s:?}: expected ms, RFC 3339, YYYY-MM-DDTHH:MM[:SS] or YYYY-MM-DD")).into())
}

/// What this ledger held at `ts` for a session's loaded entries and redirects (ADR 001
/// §"Bitemporal semantics": the row with the greatest `known ≤ ts`). From the lake's
/// `entries` (`temporal2:entry_id`) and `redirects` (`temporal:session_id,redirect_seq`)
/// when it holds this ledger's whole journal, else from the ledger's journal: the same
/// rows either way.
struct AsOf {
    ts: i64,
    source: Source,
    entries: BTreeMap<String, Entry>,
    redirects: BTreeMap<u32, rematch_core::types::Redirect>,
}

impl AsOf {
    fn load(ctx: &AppCtx, s: &SessionRecord, ts: i64) -> Result<AsOf> {
        let ids: Vec<String> = s.loaded_entries.clone();
        let keys: Vec<String> = s.redirects.iter().map(|r| redirect_key(&s.id, r.seq)).collect();
        let lake_lookups = (
            Lookup { key: "temporal2:entry_id", values: ids.clone(), window: None },
            Lookup {
                key: "temporal:session_id,redirect_seq",
                values: s.redirects.iter().map(|r| format!("{}\t{}", s.id, r.seq)).collect(),
                window: None,
            },
        );
        let read = |src: Source| -> Result<(Vec<JournalRow>, Vec<JournalRow>)> {
            let e = if ids.is_empty() { Vec::new() } else { lake_read::rows(ctx, src, "entries", Some(&lake_lookups.0), Some(&ids))? };
            let r = if keys.is_empty() { Vec::new() } else { lake_read::rows(ctx, src, "redirects", Some(&lake_lookups.1), Some(&keys))? };
            Ok((e, r))
        };
        let (source, (e, r)) = match lake_read::current_lake(ctx).map(|_| read(Source::Lake)) {
            Some(Ok(rows)) => (Source::Lake, rows),
            Some(Err(err)) => {
                eprintln!("rematch library why: lake unreadable, reading the ledger's journal: {err:#}");
                (Source::Ledger, read(Source::Ledger)?)
            }
            None => (Source::Ledger, read(Source::Ledger)?),
        };
        let me = ctx.ledger.machine_id();
        let mine = |rows: Vec<JournalRow>, wanted: &[String]| -> Vec<JournalRow> {
            lake_read::as_of(rows.into_iter().filter(|r| r.machine == me && wanted.contains(&r.row_key)).collect(), ts)
        };
        let entries = mine(e, &ids)
            .into_iter()
            .filter_map(|r| serde_json::from_value::<Entry>(r.json).ok().map(|e| (e.id.clone(), e)))
            .collect();
        let redirects = mine(r, &keys)
            .into_iter()
            .filter_map(|r| serde_json::from_value::<rematch_core::types::Redirect>(r.json).ok().map(|x| (x.seq, x)))
            .collect();
        Ok(AsOf { ts, source, entries, redirects })
    }
}

fn utc(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string()).unwrap_or_default()
}

fn why(ctx: &AppCtx, session: Option<&str>, asof: Option<&str>) -> Result<Value> {
    let asof_ts = asof.map(parse_asof).transpose()?;
    let s = match session {
        Some(id) => ctx.ledger.get_session(id)?.ok_or_else(|| anyhow!("no session {id} in the ledger (run `rematch count` after it ends)"))?,
        None => latest_session(ctx)?.ok_or_else(|| anyhow!("no session recorded for {}", ctx.cwd.display()))?,
    };
    let past = asof_ts.map(|ts| AsOf::load(ctx, &s, ts)).transpose()?;
    let place = predicate::Place::of_session(&s);
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
    lines.push(format!(
        "repo {} · remote {} · subpath {} · head {}",
        s.repo.as_deref().unwrap_or("none"),
        s.repo_remote.as_deref().unwrap_or("unknown"),
        s.subpath.as_deref().map(|p| if p.is_empty() { "." } else { p }).unwrap_or("unknown"),
        s.head_sha.as_deref().map(|h| &h[..h.len().min(12)]).unwrap_or("unknown"),
    ));
    // The recorded domain, and what today's policy decides (they differ after a `policy
    // domain set` until `lake migrate` reapplies it).
    let now = rematch_core::domain::session_decision(&s, &ctx.domain_policy()?);
    lines.push(format!(
        "domain {} · policy now: {}",
        s.domain.as_deref().unwrap_or("none"),
        now.as_ref().map(|(d, src)| format!("{d} ({})", src.name())).unwrap_or_else(|| "undecided".into()),
    ));
    if let Some(p) = &past {
        lines.push(format!("as of {} (source {})", utc(p.ts), p.source.name()));
    }
    for id in &s.loaded_entries {
        let Some(e) = ctx.ledger.get_entry(id)? else {
            loaded.push(json!({"id": id, "missing": true}));
            continue;
        };
        let glob = predicate::matches_place(&e.predicate, &place);
        let domain_ok = rematch_core::domain::domain_matches(e.predicate.domain.as_deref(), s.domain.as_deref());
        let hit = predicate::count_match(&e.predicate, &place, &s.exts, &s.stems);
        let loaded_version = s.loaded_versions.get(&e.id).copied();
        let this = ctx.ledger.counter_rows(&e.id)?.into_iter().find(|r| r.session_id == s.id);
        let agg = rows.iter().find(|(eid, _)| eid == &e.id).map(|(_, c)| c.clone()).unwrap_or_default();
        let then = past.as_ref().map(|p| p.entries.get(&e.id));
        let status = match then {
            None => enum_str(&e.status),
            Some(Some(t)) => format!("{}; {} as of {}", enum_str(&e.status), enum_str(&t.status), utc(past.as_ref().map_or(0, |p| p.ts))),
            Some(None) => format!("{}; not in the ledger as of {}", enum_str(&e.status), utc(past.as_ref().map_or(0, |p| p.ts))),
        };
        lines.push(format!(
            "loaded {} v{} ({}) sha {}: domain {} {} · place {} · predicate {} · this session {} · total matched {} recurrence {} harm {}",
            e.name,
            loaded_version.unwrap_or(e.version),
            status,
            e.content_sha256.as_deref().map(|h| &h[..h.len().min(12)]).unwrap_or("none"),
            e.predicate.domain.as_deref().unwrap_or("any"),
            if domain_ok { "ok" } else { "mismatch" },
            if glob { "hit" } else { "miss" },
            if hit { "hit" } else { "miss" },
            this.as_ref().map(|r| format!("recurrence {} harm {}", r.recurrence, r.harm)).unwrap_or_else(|| "not counted".into()),
            agg.matched,
            agg.recurrence,
            agg.harm
        ));
        let mut row = json!({
            "id": e.id, "name": e.name, "status": e.status, "predicate": e.predicate, "glob_hit": glob,
            "domain": e.predicate.domain, "domain_ok": domain_ok,
            "version": e.version, "loaded_version": loaded_version, "content_sha256": e.content_sha256,
            "predicate_hit": hit, "session_counters": this, "counters": agg,
            "stems_overlap": e.predicate.stems.iter().filter(|x| s.stems.contains(x)).collect::<Vec<_>>(),
        });
        if let Some(t) = then {
            row["asof"] = json!({"status": t.map(|t| t.status), "version": t.map(|t| t.version),
                                 "content_sha256": t.and_then(|t| t.content_sha256.clone())});
        }
        if let Some(t) = crate::tune::pending_tune(ctx, &e)? {
            let c = ctx.ledger.counters(&t.id)?;
            lines.push(format!(
                "  predicate tune v{} pending ({} · {} · matched {} recurrence {} harm {})",
                t.version,
                t.id,
                t.tune.as_ref().map(crate::tune::note_of).unwrap_or_default(),
                c.matched,
                c.recurrence,
                c.harm
            ));
            row["tune_pending"] = json!({"id": t.id, "version": t.version, "predicate": t.predicate, "tune": t.tune, "counters": c});
        }
        loaded.push(row);
    }
    if s.loaded_entries.is_empty() {
        lines.push("no rematch entries were loaded".into());
    }
    let redirects: Vec<Value> = s
        .redirects
        .iter()
        .map(|r| {
            let class = redirect::effective_class(r);
            let then = past.as_ref().map(|p| p.redirects.get(&r.seq).map(redirect::effective_class));
            lines.push(format!(
                "redirect #{} {} → {}{}: {}",
                r.seq,
                r.prelabel,
                class.map(redirect::class_name).unwrap_or("unclassified"),
                match then {
                    None => String::new(),
                    Some(Some(c)) => format!(" ({} as of then)", c.map(redirect::class_name).unwrap_or("unclassified")),
                    Some(None) => " (not yet recorded then)".into(),
                },
                rematch_core::features::head(&r.text_head, 120)
            ));
            let mut row = json!({"seq": r.seq, "prelabel": r.prelabel, "class": class, "confidence": r.confidence, "text_head": r.text_head});
            if let Some(t) = then {
                row["asof"] = json!({"recorded": t.is_some(), "class": t.flatten()});
            }
            row
        })
        .collect();
    let source = past.as_ref().map_or(Source::Ledger, |p| p.source);
    Ok(json!({
        "session": {"id": s.id, "cwd": s.cwd, "repo": s.repo, "repo_remote": s.repo_remote, "subpath": s.subpath,
                     "domain": s.domain,
                     "domain_now": now.as_ref().map(|(d, src)| json!({"domain": d, "source": src})),
                     "head_sha": s.head_sha, "files_changed": s.files_changed, "loaded_entry_shas": s.loaded_entry_shas,
                     "visible_entries": s.visible_entries, "family": s.family, "model": s.model, "started_at": s.started_at,
                     "ended_at": s.ended_at, "turns": s.turns, "tokens_in": s.tokens_in, "tokens_out": s.tokens_out,
                     "tool_calls": s.tool_calls, "tool_errors": s.tool_errors, "error_clusters": s.error_clusters,
                     "takeovers": s.takeovers, "skill_reads": s.skill_reads, "source": s.source, "counted": s.counted},
        "loaded": loaded,
        "redirects": redirects,
        "source": source,
        "asof": past.as_ref().map(|p| json!({"ts": p.ts, "utc": utc(p.ts)})),
        "lines": lines,
    }))
}

/// The kill switch also empties the shared skills dir at once (and `on` restores the kept and
/// pinned links; provisional ones return with the next sync of a matching cwd).
pub fn cmd_off(ctx: &AppCtx) -> Result<Value> {
    std::fs::create_dir_all(&ctx.paths.root)?;
    std::fs::write(&ctx.paths.off_marker, "")?;
    library::sync_shared_skills(&ctx.ledger, &ctx.paths, None)?;
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
    library::sync_shared_skills(&ctx.ledger, &ctx.paths, None)?;
    Ok(json!({"off": ctx.paths.is_off(), "marker": ctx.paths.off_marker, "env_off": env_off, "lines": lines}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::testutil::{entry, env, keep, session, stage};
    use rematch_core::ledger::now_ms;
    use rematch_core::types::{Redirect, RedirectClass};

    fn tick() -> i64 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        let t = now_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        t
    }

    fn asof_view(out: &Value) -> Value {
        json!({"entry": out["loaded"][0]["asof"], "status": out["loaded"][0]["status"],
               "redirect": out["redirects"][0]["asof"], "class": out["redirects"][0]["class"]})
    }

    /// A provisional entry loaded by a session whose redirect was unclassified; then the
    /// entry is kept and the redirect labelled a correction. `--asof` between the two
    /// writes shows the earlier status and class, from the ledger's journal and from the
    /// lake alike; after them, the current ones.
    #[test]
    fn why_asof_shows_the_status_and_class_before_a_transition() {
        let t = env();
        let ctx = &t.ctx;
        let e = stage(ctx, &entry("e-why", "run-tests-before-commit", "payments"), &[]);
        let mut s = session("s-why", "payments", Some("/w/payments"), "anthropic/claude-opus", 1_000);
        s.loaded_entries = vec![e.id.clone()];
        s.redirects = vec![Redirect {
            session_id: s.id.clone(),
            seq: 3,
            prelabel: "reprompt".into(),
            class: None,
            confidence: None,
            text_head: "try again".into(),
            deliverable_head: String::new(),
            t: 1_500,
        }];
        ctx.ledger.upsert_session(&s).unwrap();
        let before = tick();
        keep(ctx, &e.id);
        ctx.ledger.set_redirect_class(&s.id, 3, RedirectClass::Correction, Some(0.9)).unwrap();
        let after = tick();

        let then = json!({"entry": {"status": "provisional", "version": 1, "content_sha256": e.content_sha256},
                          "status": "kept", "redirect": {"recorded": true, "class": null}, "class": "correction"});
        let w = why(ctx, Some("s-why"), Some(&before.to_string())).unwrap();
        assert_eq!((asof_view(&w), w["source"].as_str()), (then.clone(), Some("ledger")), "{w}");
        assert!(w["lines"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("(kept; provisional as of ")), "{w}");
        let now = why(ctx, Some("s-why"), Some(&after.to_string())).unwrap();
        assert_eq!((now["loaded"][0]["asof"]["status"].as_str(), now["redirects"][0]["asof"]["class"].as_str()), (Some("kept"), Some("correction")));
        let first = why(ctx, Some("s-why"), Some("1970-01-01")).unwrap();
        assert_eq!((first["loaded"][0]["asof"]["status"].clone(), first["redirects"][0]["asof"]["recorded"].clone()), (Value::Null, json!(false)), "nothing written yet: {first}");
        let plain = why(ctx, Some("s-why"), None).unwrap();
        assert_eq!((plain["asof"].clone(), plain["loaded"][0].get("asof")), (Value::Null, None));
        assert!(parse_asof("yesterday").is_err());
        assert_eq!(parse_asof("2026-10-26T12:00:00Z").unwrap(), parse_asof("2026-10-26T12:00").unwrap());

        #[cfg(feature = "lake")]
        {
            crate::lake_read::export_all(ctx);
            let w = why(ctx, Some("s-why"), Some(&before.to_string())).unwrap();
            assert_eq!((asof_view(&w), w["source"].as_str()), (then, Some("lake")), "{w}");
            let now = why(ctx, Some("s-why"), Some(&after.to_string())).unwrap();
            assert_eq!((now["loaded"][0]["asof"]["status"].as_str(), now["source"].as_str()), (Some("kept"), Some("lake")));
        }
    }
}
