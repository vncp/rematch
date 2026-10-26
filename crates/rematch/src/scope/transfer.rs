//! Cross-domain transfer and `shared` promotion (SPEC §Cross-domain transfer,
//! §Branches; branch facet vocabulary in `branches/<branch>/domain.yml`, documented in
//! `docs/domain.yml.md`).
//!
//! - `suggest`: kept invariant lessons not yet tried in a branch whose facet structure
//!   overlaps the source branch's on ≥ 2 keys.
//! - `apply`: stage a provisional copy in the target branch (new id, `transfers_from`
//!   link, origin `Import{source:"transfer:<id>"}`), linted with `transfer: true` so
//!   domain lessons and facts are blocked; workspace `facts/` never travel.
//! - `promote_shared`: an entry kept in ≥ 2 branches (or kept in one plus a clean
//!   canary with matched ≥ 4 in another) is copied to `shared` as kept, no new canary.
//!   When the copy takes a kept entry's slot, the anchor replay (`scope::anchor`) runs
//!   first: fail ⇒ the copy is retired with the note and that entry's promotion is not
//!   retried; replay error (task busy, omp missing) ⇒ the copy is retired as deferred and
//!   the next promotion pass tries again; no verifier ⇒ the keep proceeds.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Result};
use clap::{Args, Subcommand};
use rematch_core::dedup::{entry_content, near_duplicate};
use rematch_core::ledger::{EntryFilter, SessionFilter};
use rematch_core::library::{set_status, stage, validate_name, SHARED_BRANCH};
use rematch_core::lint::{branch_always_on_bytes, LintContext};
use rematch_core::predicate::{derive, predicate_hash, ANY_REPO};
use rematch_core::rate::{can_admit, record_admission, RateLimit};
use rematch_core::{Edit, Entry, LessonTag, Link, LinkRel, Origin, Predicate, Status};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::anchor::{self, AnchorOutcome};
use super::{branch_anchor, lint_edits, lint_errors, owns_files, package_edits, resolve_entry};
use crate::app::{new_id, AppCtx};
use crate::lake_read::{self, Source};

/// Minimum shared structural facet keys for a transfer suggestion.
pub const MIN_OVERLAP: usize = 2;
/// A second branch's canary counts toward `shared` promotion from this many clean matches.
pub const PROMOTE_CANARY_MATCHED: u32 = 4;

#[derive(Debug, Clone, Args)]
pub struct TransferArgs {
    #[command(subcommand)]
    pub cmd: TransferCmd,
}

#[derive(Debug, Clone, Subcommand)]
pub enum TransferCmd {
    /// List kept invariant lessons not yet tried in a structurally overlapping branch.
    Suggest,
    /// Stage a provisional copy of a lesson in another branch.
    Apply {
        lesson_id: String,
        #[arg(long)]
        branch: String,
    },
    /// Copy entries kept in two or more branches to `shared` (kept, no new canary).
    Promote,
}

/// `branches/<branch>/domain.yml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainFile {
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub facets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Suggestion {
    pub lesson_id: String,
    pub name: String,
    pub from_branch: String,
    pub to_branch: String,
    pub overlapping_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Promotion {
    pub entry: String,
    pub name: String,
    pub branches: Vec<String>,
    pub shared_id: Option<String>,
    pub blocked: Option<String>,
    /// Anchor replay before the shared copy replaced a kept entry: `{kept, replay,
    /// certificate?}`, plus `retired` (the copy's id) when it did not pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Value>,
}

pub fn read_domain(ctx: &AppCtx, branch: &str) -> Result<Option<DomainFile>> {
    let p = ctx.paths.branches.join(branch).join("domain.yml");
    match std::fs::read_to_string(&p) {
        Ok(t) => Ok(Some(serde_yaml::from_str(&t).map_err(|e| anyhow!("{}: {e}", p.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Facet keys of a branch: `domain.yml` `facets:` when present, else the distinct keys of
/// the branch's outcome facets ([`outcome_keys`]).
pub fn branch_facets(ctx: &AppCtx, branch: &str, outcome_keys: &OutcomeKeys) -> Result<BTreeSet<String>> {
    if let Some(d) = read_domain(ctx, branch)? {
        if !d.facets.is_empty() {
            return Ok(d.facets.into_iter().map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect());
        }
    }
    Ok(outcome_keys.get(branch).cloned().unwrap_or_default())
}

/// Outcome branch → the bare facet keys of its outcomes.
pub type OutcomeKeys = BTreeMap<String, BTreeSet<String>>;

/// [`OutcomeKeys`] for `transfer suggest`, and where they came from: the lake's `outcomes`
/// (`list:facets` holds each facet's bare key; every machine's outcomes, latest row per
/// outcome) when the lake holds this ledger's whole journal, else [`ledger_outcome_keys`].
pub fn outcome_keys(ctx: &AppCtx) -> Result<(OutcomeKeys, Source)> {
    if lake_read::current_lake(ctx).is_some() {
        match lake_read::lake_rows(ctx, "outcomes", None) {
            Ok(rows) => {
                let mut out = OutcomeKeys::new();
                for r in lake_read::as_of(rows, i64::MAX) {
                    let Ok(o) = serde_json::from_value::<rematch_core::Outcome>(r.json) else { continue };
                    out.entry(o.branch).or_default().extend(o.facets.into_iter().map(|f| f.key));
                }
                return Ok((out, Source::Lake));
            }
            Err(e) => eprintln!("rematch transfer: lake outcomes unreadable, reading the ledger: {e:#}"),
        }
    }
    Ok((ledger_outcome_keys(ctx)?, Source::Ledger))
}

/// [`OutcomeKeys`] from this ledger's outcome facet rows (the session-start digest reads
/// these: it never touches the lake).
pub fn ledger_outcome_keys(ctx: &AppCtx) -> Result<OutcomeKeys> {
    let mut st = ctx.ledger.conn().prepare(
        "SELECT o.branch, f.key FROM outcomes o LEFT JOIN outcome_facets f ON f.outcome_id = o.id",
    )?;
    let mut out = OutcomeKeys::new();
    for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))? {
        let (branch, key) = row?;
        out.entry(branch).or_default().extend(key);
    }
    Ok(out)
}

/// Every branch the library knows: `branches/` dirs, staging dirs, entry, session and
/// outcome branches.
fn known_branches(ctx: &AppCtx, outcome_keys: &OutcomeKeys) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for dir in [&ctx.paths.branches, &ctx.paths.staging] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            out.extend(rd.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()));
        }
    }
    out.extend(ctx.ledger.list_entries(&EntryFilter::default())?.into_iter().map(|e| e.branch));
    out.extend(ctx.ledger.list_sessions(&SessionFilter::default())?.into_iter().map(|s| s.branch));
    out.extend(outcome_keys.keys().cloned());
    out.remove(SHARED_BRANCH);
    out.remove("");
    Ok(out)
}

fn content_text(e: &Entry) -> String {
    entry_content(&e.path).map(|c| c.text).unwrap_or_default()
}

/// Already tried in `branch`: an entry there (any status) with the same name, a
/// `transfers_from` link to the lesson, or near-duplicate text.
fn tried_in(all: &[Entry], lesson: &Entry, lesson_text: &str, branch: &str) -> bool {
    all.iter().filter(|x| x.branch == branch && x.id != lesson.id).any(|x| {
        x.name == lesson.name
            || x.links.iter().any(|l| l.rel == LinkRel::TransfersFrom && l.id == lesson.id)
            || near_duplicate(lesson_text, &content_text(x))
    })
}

/// Kept invariant lessons not yet tried in a branch whose facet keys overlap theirs on
/// ≥ [`MIN_OVERLAP`] (outcome facet keys from [`outcome_keys`] or [`ledger_outcome_keys`]).
pub fn suggest(ctx: &AppCtx, keys: &OutcomeKeys) -> Result<Vec<Suggestion>> {
    let all = ctx.ledger.list_entries(&EntryFilter::default())?;
    let branches = known_branches(ctx, keys)?;
    let mut facets: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for b in &branches {
        facets.insert(b.clone(), branch_facets(ctx, b, keys)?);
    }
    let mut out = Vec::new();
    for lesson in all.iter().filter(|e| e.status == Status::Kept && e.lesson_tag == Some(LessonTag::Invariant)) {
        let Some(src) = facets.get(&lesson.branch) else { continue };
        let text = content_text(lesson);
        for (b, keys) in &facets {
            if *b == lesson.branch {
                continue;
            }
            let overlap: Vec<String> = src.intersection(keys).cloned().collect();
            if overlap.len() < MIN_OVERLAP || tried_in(&all, lesson, &text, b) {
                continue;
            }
            out.push(Suggestion {
                lesson_id: lesson.id.clone(),
                name: lesson.name.clone(),
                from_branch: lesson.branch.clone(),
                to_branch: b.clone(),
                overlapping_keys: overlap,
            });
        }
    }
    Ok(out)
}

/// Where a copy in `branch` loads (repo glob, remote, subpath; no content rules): derived
/// from the branch's latest session repo, else the place of a live entry already there.
fn branch_place(ctx: &AppCtx, branch: &str) -> Result<Option<Predicate>> {
    if let Some(anchor) = branch_anchor(ctx, branch)? {
        return Ok(Some(derive(Some(&anchor), &[], &[])));
    }
    Ok(ctx
        .ledger
        .list_entries(&EntryFilter { branch: Some(branch.to_string()), ..Default::default() })?
        .into_iter()
        .rev()
        .map(|e| Predicate { repo_glob: e.predicate.repo_glob, repo_remote: e.predicate.repo_remote, subpath: e.predicate.subpath, ..Default::default() })
        .find(|p| !p.repo_glob.trim().is_empty()))
}

/// A copy of `src` for another branch: new id, zeroed counters, re-approval required,
/// workspace facts dropped, loading where `place` says (its repo glob, remote and subpath; the
/// content rules stay `src`'s). Returns the entry, its files and the dropped fact paths.
pub(crate) fn copy_for(src: &Entry, branch: &str, place: Predicate, origin: Origin) -> Result<(Entry, Vec<Edit>, Vec<String>)> {
    let mut files = package_edits(&src.path)?;
    let mut dropped = Vec::new();
    files.retain(|e| match e {
        Edit::Write { path, .. } if path.starts_with("facts/") => {
            dropped.push(path.clone());
            false
        }
        _ => true,
    });
    if let Some(Edit::Write { content, .. }) = files.iter_mut().find(|e| matches!(e, Edit::Write { path, .. } if path == "SKILL.md")) {
        let mut doc = rematch_core::package::SkillDoc::parse(content)?;
        doc.frontmatter.remove("rematch");
        *content = doc.render();
    }
    let e = Entry {
        id: new_id(),
        branch: branch.to_string(),
        status: Status::Provisional,
        helpful: 0,
        harmful: 0,
        origin,
        predicate: Predicate {
            repo_glob: place.repo_glob,
            repo_remote: place.repo_remote,
            subpath: place.subpath,
            ..src.predicate.clone()
        },
        predecessor: None,
        version: 1,
        created_at: 0,
        updated_at: 0,
        pinned: false,
        approved_scripts: vec![],
        sibling_of: None,
        status_since: None,
        status_note: None,
        pending_next_run: false,
        fork_commit: None,
        families_excluded: vec![],
        links: vec![Link { rel: LinkRel::TransfersFrom, id: src.id.clone() }],
        ..src.clone()
    };
    Ok((e, files, dropped))
}

fn transfer_lint(ctx: &AppCtx, e: &Entry, files: &[Edit]) -> Result<rematch_core::LintReport> {
    let branch_dir = ctx.paths.staging_branch(&e.branch);
    let lctx = LintContext {
        branch_other_bytes: branch_always_on_bytes(&branch_dir, &branch_dir.join(&e.name)),
        transfer: true,
        ..Default::default()
    };
    lint_edits(ctx, e, files, &lctx)
}

/// `transfer apply <lesson> --branch <b>`: provisional copy in the target branch.
pub fn apply(ctx: &AppCtx, lesson_id: &str, branch: &str) -> Result<Value> {
    validate_name(branch)?;
    if branch == SHARED_BRANCH {
        bail!("`shared` is reached by promotion (kept in two branches), not by transfer");
    }
    let src = resolve_entry(ctx, lesson_id)?;
    if src.status != Status::Kept {
        bail!("only kept lessons transfer; {} is {:?}", src.id, src.status);
    }
    if src.branch == branch {
        bail!("{} already lives in branch {branch}", src.name);
    }
    let all = ctx.ledger.list_entries(&EntryFilter::default())?;
    if tried_in(&all, &src, &content_text(&src), branch) {
        bail!("{} was already tried in branch {branch} (same name, link or near-duplicate text)", src.name);
    }
    let place = branch_place(ctx, branch)?
        .ok_or_else(|| anyhow!("branch {branch} has no sessions or entries to anchor the copy's repo glob"))?;
    let origin = Origin::Import { source: format!("transfer:{}", src.id) };
    let (e, files, dropped) = copy_for(&src, branch, place, origin)?;
    let rep = transfer_lint(ctx, &e, &files)?;
    if !rep.ok {
        bail!("transfer blocked by lint: {}", lint_errors(&rep));
    }
    if src.lesson_tag != Some(LessonTag::Invariant) {
        bail!("only lessons tagged invariant transfer; {} is untagged", src.name);
    }
    let hash = predicate_hash(&e.predicate);
    match can_admit(&ctx.ledger, &hash, branch, ctx.now) {
        Ok(()) => {}
        Err(RateLimit::NeedsMerge { .. }) => {} // the copy is no duplicate (checked above); it earns its place by counters
        Err(err) => bail!("rate limit: {err}"),
    }
    let staged = stage(&ctx.ledger, &ctx.paths, &e, &files)?;
    record_admission(&ctx.ledger, &hash, &staged.id, ctx.now)?;
    Ok(json!({
        "entry": staged.id, "name": staged.name, "branch": staged.branch, "transfers_from": src.id,
        "path": staged.path, "dropped_facts": dropped, "lint": rep,
    }))
}

/// Clean canary: provisional, matched ≥ 4, no recurrence, no harm.
fn clean_canary(ctx: &AppCtx, e: &Entry) -> Result<bool> {
    if e.status != Status::Provisional {
        return Ok(false);
    }
    let c = ctx.ledger.counters(&e.id)?;
    Ok(c.matched >= PROMOTE_CANARY_MATCHED && c.recurrence == 0 && c.harm == 0)
}

/// Copy entries kept in ≥ 2 branches to `shared` as kept. The shared copy takes the
/// kept slot (`library` keeps the branch version as its predecessor for rollback).
pub fn promote_shared(ctx: &AppCtx) -> Result<Vec<Promotion>> {
    let all = ctx.ledger.list_entries(&EntryFilter::default())?;
    let mut out = Vec::new();
    let mut done: BTreeSet<String> = BTreeSet::new();
    // Only entries whose kept files are ours: keeping the shared copy supersedes the
    // `skills/<name>/` slot, and a user's own skill dir is never moved automatically.
    for e in all.iter().filter(|e| e.status == Status::Kept && e.branch != SHARED_BRANCH && !e.pinned && owns_files(ctx, e)) {
        if done.contains(&e.name) {
            continue;
        }
        // A live shared copy, or one whose anchor replay failed, settles this entry; a
        // copy retired otherwise (counters, a deferred replay) is tried again.
        let in_shared = all.iter().any(|x| {
            x.branch == SHARED_BRANCH
                && (x.name == e.name || x.links.iter().any(|l| l.rel == LinkRel::TransfersFrom && l.id == e.id))
                && (x.status != Status::Retired || anchor_failed_for(x, &e.id))
        });
        if in_shared {
            continue;
        }
        let text = content_text(e);
        let mut branches: BTreeSet<String> = BTreeSet::from([e.branch.clone()]);
        for x in all.iter().filter(|x| x.branch != e.branch && x.branch != SHARED_BRANCH && x.id != e.id) {
            let same = x.name == e.name
                || x.links.iter().any(|l| l.rel == LinkRel::TransfersFrom && l.id == e.id)
                || near_duplicate(&text, &content_text(x));
            if same && (x.status == Status::Kept || clean_canary(ctx, x)?) {
                branches.insert(x.branch.clone());
            }
        }
        if branches.len() < 2 {
            continue;
        }
        done.insert(e.name.clone());
        let mut p = Promotion {
            entry: e.id.clone(),
            name: e.name.clone(),
            branches: branches.iter().cloned().collect(),
            shared_id: None,
            blocked: None,
            anchor: None,
        };
        let place = derive(None, &[], &[]);
        debug_assert_eq!(place.repo_glob, ANY_REPO);
        let origin = Origin::Import { source: format!("shared:{}", e.id) };
        let (mut copy, files, dropped) = copy_for(e, SHARED_BRANCH, place, origin)?;
        copy.verified_on = e.verified_on.clone();
        copy.version = e.version + 1;
        copy.approved_scripts = e.approved_scripts.clone();
        let rep = transfer_lint(ctx, &copy, &files)?;
        if !rep.ok {
            p.blocked = Some(format!("lint: {}", lint_errors(&rep)));
            out.push(p);
            continue;
        }
        if !dropped.is_empty() {
            p.blocked = Some(format!("carries workspace facts ({}); facts stay in their branch", dropped.join(", ")));
            out.push(p);
            continue;
        }
        let staged = stage(&ctx.ledger, &ctx.paths, &copy, &files)?;
        let mut anchor_pass = None;
        if let Some(kept) = anchor::replaced_kept(ctx, &staged)? {
            let (retire, json) = match anchor::replay_before_replace(ctx, &staged, &kept) {
                Ok(outcome) => {
                    let json = json!({"kept": kept.id, "replay": outcome});
                    match outcome {
                        AnchorOutcome::Replayed { pass: false, receipt } => (Some(anchor::failure_note(&receipt)), json),
                        AnchorOutcome::Replayed { pass: true, receipt } => {
                            anchor_pass = Some(receipt);
                            (None, json)
                        }
                        AnchorOutcome::NoVerifier => (None, json),
                    }
                }
                Err(err) => (Some(format!("anchor replay deferred: {err:#}")), json!({"kept": kept.id, "error": format!("{err:#}")})),
            };
            p.anchor = Some(json);
            if let Some(why) = retire {
                set_status(&ctx.ledger, &ctx.paths, &staged.id, Status::Retired, Some(&why))?;
                if let Some(a) = p.anchor.as_mut() {
                    a["retired"] = json!(staged.id);
                }
                p.blocked = Some(why);
                out.push(p);
                continue;
            }
        }
        let note = format!("promoted to shared: kept in {}", p.branches.join(", "));
        let kept = set_status(&ctx.ledger, &ctx.paths, &staged.id, Status::Kept, Some(&note))?;
        if let (Some(r), Some(a)) = (&anchor_pass, p.anchor.as_mut()) {
            a["certificate"] = json!(anchor::certify(ctx, &kept, r, None)?);
        }
        p.shared_id = Some(kept.id);
        out.push(p);
    }
    Ok(out)
}

/// A shared copy of `src` retired by a failed anchor replay (not a deferred one).
fn anchor_failed_for(x: &Entry, src: &str) -> bool {
    x.status == Status::Retired
        && x.links.iter().any(|l| l.rel == LinkRel::TransfersFrom && l.id == src)
        && x.status_note.as_deref().is_some_and(|n| n.starts_with(anchor::FAILED_NOTE))
}

pub fn cmd_transfer(ctx: &AppCtx, args: TransferArgs) -> Result<Value> {
    match args.cmd {
        TransferCmd::Suggest => {
            let (keys, source) = outcome_keys(ctx)?;
            let suggestions = suggest(ctx, &keys)?;
            Ok(json!({ "suggestions": suggestions, "source": source }))
        }
        TransferCmd::Apply { lesson_id, branch } => apply(ctx, &lesson_id, &branch),
        TransferCmd::Promote => Ok(json!({ "promotions": promote_shared(ctx)? })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::testutil::{entry, env, keep, session, stage as stage_t};
    use rematch_core::Kind;

    fn domain(ctx: &AppCtx, branch: &str, facets: &str) {
        let d = ctx.paths.branches.join(branch);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("domain.yml"), format!("domain: {branch}\nfacets: [{facets}]\n")).unwrap();
    }

    #[test]
    fn transfer_suggests_only_on_two_overlapping_keys_and_applies() {
        let t = env();
        let ctx = &t.ctx;
        domain(ctx, "quant", "mechanism, universe, horizon, data_snapshot");
        domain(ctx, "options", "mechanism, horizon, strike");
        domain(ctx, "web", "module, horizon");
        ctx.ledger.upsert_session(&session("s-opt", "options", Some("/w/options"), "anthropic/claude-opus", 1_000)).unwrap();
        let e = stage_t(ctx, &entry("e-inv-1", "verify-before-claim", "quant"), &[("facts/rows.md", "---\nas_of: 2026-10-01\n---\n\nRows: 3\n")]);
        keep(ctx, &e.id);
        let s = suggest(ctx, &ledger_outcome_keys(ctx).unwrap()).unwrap();
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].to_branch, "options");
        assert_eq!(s[0].overlapping_keys, vec!["horizon".to_string(), "mechanism".to_string()]);

        let out = apply(ctx, &e.id, "options").unwrap();
        let copy = ctx.ledger.get_entry(out["entry"].as_str().unwrap()).unwrap().unwrap();
        assert_eq!(copy.status, Status::Provisional);
        assert_eq!(copy.branch, "options");
        assert_eq!(copy.links, vec![Link { rel: LinkRel::TransfersFrom, id: e.id.clone() }]);
        assert_eq!(copy.origin, Origin::Import { source: format!("transfer:{}", e.id) });
        assert_eq!(copy.predicate.repo_glob, "/w/options");
        assert!(!copy.path.join("facts").exists(), "workspace facts never transfer");
        assert_eq!(out["dropped_facts"], json!(["facts/rows.md"]));
        assert!(suggest(ctx, &ledger_outcome_keys(ctx).unwrap()).unwrap().is_empty(), "already tried in options");
        assert!(apply(ctx, &e.id, "options").is_err());
    }

    #[test]
    fn transfer_falls_back_to_outcome_facet_keys() {
        let t = env();
        let ctx = &t.ctx;
        domain(ctx, "quant", "mechanism, universe");
        let o = rematch_core::outcome::new_outcome_at(
            rematch_core::outcome::NewOutcome {
                session_id: "s".into(),
                model_family: rematch_core::Family("a/x".into()),
                repo: "/w/b".into(),
                branch: "bee".into(),
                kind: rematch_core::OutcomeKind::Decision,
                subject: rematch_core::Subject { domain: "quant.b".into(), entities: vec![] },
                claim: "c".into(),
                params: Default::default(),
                facets: vec![
                    rematch_core::Facet { key: "mechanism".into(), value: "carry".into() },
                    rematch_core::Facet { key: "universe".into(), value: "spx".into() },
                ],
                evidence: vec![],
                result: rematch_core::OutcomeResult { status: rematch_core::ResultStatus::Kept, metrics: Default::default(), kill_criterion: None },
                links: vec![],
                provenance: Default::default(),
            },
            "o1".into(),
            1,
            "m".into(),
        );
        ctx.ledger.insert_outcome(&o).unwrap();
        let keys = ledger_outcome_keys(ctx).unwrap();
        assert_eq!(branch_facets(ctx, "bee", &keys).unwrap(), BTreeSet::from(["mechanism".to_string(), "universe".to_string()]));
        let e = stage_t(ctx, &entry("e-inv-2", "check-receipts", "quant"), &[]);
        keep(ctx, &e.id);
        let s = suggest(ctx, &keys).unwrap();
        assert_eq!(s.iter().map(|s| s.to_branch.as_str()).collect::<Vec<_>>(), vec!["bee"]);
        assert_eq!(outcome_keys(ctx).unwrap(), (keys.clone(), Source::Ledger), "no lake: the ledger");

        // The same seed through the lake: the same keys and suggestions, from the lake.
        #[cfg(feature = "lake")]
        {
            crate::lake_read::export_all(ctx);
            let (lake_keys, source) = outcome_keys(ctx).unwrap();
            assert_eq!((&lake_keys, source), (&keys, Source::Lake));
            assert_eq!(suggest(ctx, &lake_keys).unwrap(), s);
            let out = cmd_transfer(ctx, TransferArgs { cmd: TransferCmd::Suggest }).unwrap();
            assert_eq!(out["source"], "lake", "{out}");
            // A ledger write the lake has not seen yet: back to the ledger.
            ctx.ledger.insert_outcome(&rematch_core::Outcome { id: "o2".into(), ..o.clone() }).unwrap();
            assert_eq!(outcome_keys(ctx).unwrap().1, Source::Ledger);
        }
    }

    #[test]
    fn transfer_lint_blocks_domain_lessons_and_facts() {
        let t = env();
        let ctx = &t.ctx;
        domain(ctx, "quant", "mechanism, universe");
        domain(ctx, "options", "mechanism, universe");
        ctx.ledger.upsert_session(&session("s-opt", "options", Some("/w/options"), "anthropic/claude-opus", 1_000)).unwrap();
        let mut d = entry("e-dom-1", "spx-quirks", "quant");
        d.lesson_tag = Some(LessonTag::Domain);
        let d = stage_t(ctx, &d, &[]);
        keep(ctx, &d.id);
        let err = format!("{:#}", apply(ctx, &d.id, "options").unwrap_err());
        assert!(err.contains("[transfer]") && err.contains("never transfer"), "{err}");
        let mut f = entry("e-fact-2", "spx-calendar", "quant");
        f.kind = Kind::Fact;
        f.lesson_tag = None;
        let f = stage_t(ctx, &f, &[]);
        keep(ctx, &f.id);
        let err = format!("{:#}", apply(ctx, &f.id, "options").unwrap_err());
        assert!(err.contains("[transfer]"), "{err}");
        assert!(ctx.ledger.list_entries(&EntryFilter { branch: Some("options".into()), ..Default::default() }).unwrap().is_empty());
    }

    #[test]
    fn promote_shared_when_kept_in_two_branches() {
        let t = env();
        let ctx = &t.ctx;
        let a = stage_t(ctx, &entry("e-a", "verify-before-claim", "quant"), &[]);
        let a = keep(ctx, &a.id);
        // second branch: a clean canary (copy via transfer link) with 4 clean matches
        let mut b = entry("e-b", "verify-before-claim-b", "options");
        b.links = vec![Link { rel: LinkRel::TransfersFrom, id: a.id.clone() }];
        let b = stage_t(ctx, &b, &[]);
        assert!(promote_shared(ctx).unwrap().is_empty(), "canary without matches does not count");
        for i in 0..4 {
            ctx.ledger
                .add_counter(&rematch_core::ledger::CounterRow {
                    entry_id: b.id.clone(),
                    session_id: format!("m{i}"),
                    family: rematch_core::Family("anthropic/claude-opus".into()),
                    matched: 1,
                    recurrence: 0,
                    harm: 0,
                    tokens: 0,
                    turns: 0,
                    ts: 1_000 + i,
                })
                .unwrap();
        }
        let p = promote_shared(ctx).unwrap();
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].branches, vec!["options".to_string(), "quant".to_string()]);
        let shared = ctx.ledger.get_entry(p[0].shared_id.as_deref().unwrap()).unwrap().unwrap();
        assert_eq!(shared.status, Status::Kept);
        assert_eq!(shared.branch, SHARED_BRANCH);
        assert_eq!(shared.predicate.repo_glob, ANY_REPO);
        assert_eq!(shared.predecessor.as_deref(), Some(a.id.as_str()));
        assert_eq!(ctx.ledger.get_entry(&a.id).unwrap().unwrap().status, Status::Dormant);
        assert!(promote_shared(ctx).unwrap().is_empty(), "idempotent");
    }
}
