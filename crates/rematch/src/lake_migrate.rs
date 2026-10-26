//! `rematch lake migrate` (ADR 001 L1, DESIGN §19): fills the identity fields on ledger rows
//! written before they existed. Entries get `content_sha256` from their dir; entry predicates
//! whose repo glob names a git worktree on this machine get `repo_remote` and `subpath` (the
//! glob stays, as the fallback); sessions whose repo is on this machine get `repo_remote` and
//! `subpath`. File reads only (`identity`), no git process, no model. Idempotent: a row that
//! already carries a field is not touched, so a second run changes nothing.

use std::path::Path;

use anyhow::Result;
use rematch_core::identity;
use rematch_core::import::repo_of;
use rematch_core::ledger::{EntryFilter, SessionFilter};
use rematch_core::library;
use rematch_core::predicate::{escape_glob, literal_path, ANY_REPO};
use rematch_core::types::Predicate;
use serde_json::{json, Value};

use crate::app::AppCtx;

/// `repo_remote` + `subpath` for a predicate whose glob names a directory inside a worktree
/// here: the toplevel's identity, and `**` (the toplevel itself) or the escaped relative path.
fn predicate_place(p: &Predicate) -> Option<(String, String)> {
    let dir = literal_path(&p.repo_glob).filter(|d| d.is_dir())?;
    let top = repo_of(&dir)?;
    let remote = identity::repo_remote_of(&top)?;
    let sub = identity::subpath_of(&top, &dir)?;
    Some((remote, if sub.is_empty() { ANY_REPO.to_string() } else { escape_glob(&sub) }))
}

pub fn cmd_lake_migrate(ctx: &AppCtx) -> Result<Value> {
    let (mut entries, mut shas, mut predicates, mut sessions) = (0usize, 0usize, 0usize, 0usize);
    let mut failed: Vec<Value> = Vec::new();
    for e in ctx.ledger.list_entries(&EntryFilter::default())? {
        let mut next = e.clone();
        if next.content_sha256.is_none() && next.path.is_dir() {
            match library::content_sha256(&next.path) {
                Ok(h) => {
                    next.content_sha256 = Some(h);
                    shas += 1;
                }
                Err(err) => failed.push(json!({"entry": e.id, "error": format!("{err:#}")})),
            }
        }
        if next.predicate.repo_remote.is_none() {
            if let Some((remote, subpath)) = predicate_place(&next.predicate) {
                next.predicate.repo_remote = Some(remote);
                next.predicate.subpath = Some(subpath);
                predicates += 1;
            }
        }
        if next != e {
            crate::scope::save_entry(ctx, &next)?;
            entries += 1;
        }
    }
    for s in ctx.ledger.list_sessions(&SessionFilter::default())? {
        if s.repo_remote.is_some() {
            continue;
        }
        let Some(repo) = s.repo.as_deref().filter(|r| Path::new(r).is_dir()) else { continue };
        let (remote, subpath) = identity::session_place(Some(repo), &s.cwd);
        if remote.is_none() {
            continue;
        }
        let mut next = s.clone();
        next.repo_remote = remote;
        next.subpath = next.subpath.or(subpath);
        ctx.ledger.upsert_session(&next)?;
        sessions += 1;
    }
    let lines = vec![format!(
        "lake migrate: {entries} entries updated ({shas} content_sha256, {predicates} predicate remotes), {sessions} sessions given a repo_remote{}",
        if failed.is_empty() { String::new() } else { format!(", {} failed", failed.len()) }
    )];
    Ok(json!({
        "entries": entries,
        "content_sha256": shas,
        "predicates": predicates,
        "sessions": sessions,
        "failed": failed,
        "journal_seq": ctx.ledger.journal_max_seq()?,
        "lines": lines,
    }))
}
