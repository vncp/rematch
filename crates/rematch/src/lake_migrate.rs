//! `rematch lake migrate` (ADR 001 L1, DESIGN §19): fills the identity fields on ledger rows
//! written before they existed and (re)applies the domain policy (DESIGN §6 "Domain").
//! Sessions whose repo is on this machine get `repo_remote` and `subpath`; every session's
//! `domain` is decided again from the current policy (`policy.json`, branch `domain.yml`,
//! `improver/domain_rules.toml` over its stored exts and stems), so a `policy domain set`
//! reaches recorded history here. Entries get `content_sha256` from their dir; entry
//! predicates whose repo glob names a git worktree on this machine get `repo_remote` and
//! `subpath` (the glob stays, as the fallback), and a predicate whose originating session
//! has a domain takes it. File reads only (`identity`), no git process, no model.
//! Idempotent: a field already set is not touched (the domain is recomputed but only
//! written when it changed), so a second run changes nothing.

use std::path::Path;

use anyhow::Result;
use rematch_core::domain;
use rematch_core::identity;
use rematch_core::import::repo_of;
use rematch_core::ledger::{EntryFilter, SessionFilter};
use rematch_core::library;
use rematch_core::predicate::{escape_glob, literal_path, ANY_REPO};
use rematch_core::types::{Origin, Predicate};
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
    let policy = ctx.domain_policy()?;
    let (mut sessions, mut session_domains) = (0usize, 0usize);
    // Sessions first: an entry takes its originating session's (re)decided domain.
    for s in ctx.ledger.list_sessions(&SessionFilter::default())? {
        let mut next = s.clone();
        if next.repo_remote.is_none() {
            if let Some(repo) = next.repo.as_deref().filter(|r| Path::new(r).is_dir()) {
                let (remote, subpath) = identity::session_place(Some(repo), &next.cwd);
                if remote.is_some() {
                    next.repo_remote = remote;
                    next.subpath = next.subpath.or(subpath);
                    sessions += 1;
                }
            }
        }
        next.domain = domain::session_domain(&next, &policy);
        if next.domain != s.domain {
            session_domains += 1;
        }
        if next != s {
            ctx.ledger.upsert_session(&next)?;
        }
    }
    let (mut entries, mut shas, mut predicates, mut entry_domains) = (0usize, 0usize, 0usize, 0usize);
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
        if let Origin::Session { session_id } = &next.origin {
            let origin_domain = ctx.ledger.get_session(session_id)?.and_then(|s| s.domain);
            if origin_domain.is_some() && origin_domain != next.predicate.domain {
                next.predicate.domain = origin_domain;
                entry_domains += 1;
            }
        }
        if next != e {
            crate::scope::save_entry(ctx, &next)?;
            entries += 1;
        }
    }
    let lines = vec![format!(
        "lake migrate: {entries} entries updated ({shas} content_sha256, {predicates} predicate remotes, {entry_domains} predicate domains), {sessions} sessions given a repo_remote, {session_domains} session domains (re)decided{}",
        if failed.is_empty() { String::new() } else { format!(", {} failed", failed.len()) }
    )];
    Ok(json!({
        "entries": entries,
        "content_sha256": shas,
        "predicates": predicates,
        "predicate_domains": entry_domains,
        "sessions": sessions,
        "session_domains": session_domains,
        "failed": failed,
        "journal_seq": ctx.ledger.journal_max_seq()?,
        "lines": lines,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy_cmd::{cmd_policy, DomainCmd, PolicyCmd};
    use crate::scope::testutil::{entry, env, session, stage};

    #[test]
    fn migrate_backfills_and_reapplies_domains_idempotently() {
        let t = env();
        let ctx = &t.ctx;
        // Pre-domain rows: a web session (the entry's origin, `s1`), a mobile one, an undecided one.
        let mut s1 = session("s1", "app", Some("/w/app"), "anthropic/claude-opus", 1_000);
        (s1.exts, s1.stems, s1.repo_remote) = (vec!["tsx".into(), "css".into()], vec!["layout".into()], Some("github.com/acme/app".into()));
        let mut s2 = session("s2", "ios", Some("/w/ios"), "anthropic/claude-opus", 2_000);
        (s2.exts, s2.stems) = (vec!["swift".into()], vec!["review".into()]);
        let mut s3 = session("s3", "notes", Some("/w/notes"), "anthropic/claude-opus", 3_000);
        (s3.exts, s3.stems) = (vec!["md".into()], vec!["note".into()]);
        for s in [&s1, &s2, &s3] {
            ctx.ledger.upsert_session(s).unwrap();
        }
        let e = stage(ctx, &entry("e1", "web-review", "app"), &[]);
        assert_eq!(e.predicate.domain, None);

        let m = cmd_lake_migrate(ctx).unwrap();
        assert_eq!((m["session_domains"].as_u64(), m["predicate_domains"].as_u64()), (Some(2), Some(1)), "{m}");
        let got = |id: &str| ctx.ledger.get_session(id).unwrap().unwrap().domain;
        assert_eq!((got("s1"), got("s2"), got("s3")), (Some("web.frontend".into()), Some("mobile".into()), None));
        let e1 = ctx.ledger.get_entry("e1").unwrap().unwrap();
        assert_eq!(e1.predicate.domain.as_deref(), Some("web.frontend"), "from the originating session");
        assert_eq!(rematch_core::library::load_entry(&e1.path).unwrap().predicate.domain.as_deref(), Some("web.frontend"), "rematch.json too");

        let seq = ctx.ledger.journal_max_seq().unwrap();
        let again = cmd_lake_migrate(ctx).unwrap();
        assert_eq!((again["entries"].as_u64(), again["session_domains"].as_u64(), again["predicate_domains"].as_u64()), (Some(0), Some(0), Some(0)), "{again}");
        assert_eq!(ctx.ledger.journal_max_seq().unwrap(), seq, "a second run writes nothing");

        // A declaration reaches recorded history on the next migrate, entries included.
        let set = cmd_policy(ctx, PolicyCmd::Domain { cmd: DomainCmd::Set { target: "github.com/acme/app".into(), domain: "quant".into(), subpath: None } }).unwrap();
        assert_eq!(set["sessions_to_reapply"], 1, "{set}");
        let m = cmd_lake_migrate(ctx).unwrap();
        assert_eq!((m["session_domains"].as_u64(), m["predicate_domains"].as_u64()), (Some(1), Some(1)), "{m}");
        assert_eq!(got("s1").as_deref(), Some("quant"));
        assert_eq!(ctx.ledger.get_entry("e1").unwrap().unwrap().predicate.domain.as_deref(), Some("quant"));
        assert_eq!(cmd_lake_migrate(ctx).unwrap()["entries"], 0);

        #[cfg(feature = "lake")]
        {
            // The sessions table carries the column; a file written before it reads alongside.
            use rematch_lake::{columns, io, tables};
            crate::lake_read::export_all(ctx);
            let old_cols: Vec<_> = tables::columns("sessions").unwrap().into_iter().filter(|c| c.name != "domain").collect();
            let mut row = vec![columns::Cell::I(1), columns::Cell::I(1), columns::Cell::S("old".into()), columns::Cell::S("insert".into()), columns::Cell::S("s-old".into())];
            row.push(columns::Cell::S("s-old".into())); // session_id
            row.extend(old_cols[6..old_cols.len() - 1].iter().map(|_| columns::Cell::Null));
            row.push(columns::Cell::S(r#"{"id":"s-old"}"#.into()));
            let lake = rematch_lake::lake_dir(&ctx.paths.root);
            io::write_once(&lake.join("sessions/2026-01-01/old-1.parquet"), &columns::batch(&old_cols, &[row]).unwrap()).unwrap();
            let rows = rematch_lake::query::scan(&ctx.paths.root, "sessions").unwrap();
            let last = |sid: &str| rows.iter().rfind(|r| r["session_id"] == sid).cloned().unwrap();
            assert_eq!(last("s1")["domain"], "quant");
            assert_eq!(last("s2")["domain"], "mobile");
            assert_eq!(last("s3")["domain"], Value::Null);
            assert!(last("s-old").get("domain").is_none_or(Value::is_null), "pre-domain file: no value");
        }
    }
}
