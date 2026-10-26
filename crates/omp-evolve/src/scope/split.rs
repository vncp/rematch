//! Per-family split and the capability floor (SPEC §Models, DESIGN §7 Phase 3).
//!
//! Split: when an entry's counters diverge by family (`counters::family_divergence`:
//! one family harmed, another matched ≥ 2 clean), the harmed family gets a sibling
//! proposal — a provisional entry `<name>--<family-slug>` in the same branch with
//! `family` = harmed, `sibling_of` = original, version + 1, the original's files plus
//! `units/family-note.md` (no model call). The original keeps serving the clean
//! family and records the harmed one in `families_excluded`; entries still load
//! regardless of family, so the exclusion is informational (`match` reports it).
//!
//! Capability floor: a family whose recurrence or harm rose across several entries in
//! the last window (vs the window before) is listed for review in the digest; nothing
//! is retired automatically.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use clap::Args;
use evolve_core::counters::{family_divergence, DAY_MS};
use evolve_core::ledger::{EntryFilter, now_ms};
use evolve_core::library::stage;
use evolve_core::lint::{branch_always_on_bytes, LintContext};
use evolve_core::package::slug;
use evolve_core::rate::MAX_PROVISIONAL_PER_BRANCH;
use evolve_core::{Entry, Family, FamilyCounters, Status};
use serde::Serialize;
use serde_json::{json, Value};

use super::{lint_edits, lint_errors, package_edits, put_edit, rename_skill, save_entry};
use crate::app::{new_id, AppCtx};

pub const FAMILY_NOTE: &str = "units/family-note.md";
/// "Several entries" for the capability floor.
pub const REVIEW_MIN_ENTRIES: usize = 2;

#[derive(Debug, Clone, Args)]
pub struct SplitArgs {
    /// Capability-floor window in days.
    #[arg(long, default_value_t = 7)]
    pub review_days: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SplitOutcome {
    pub original: String,
    pub name: String,
    pub harmed: Family,
    pub clean: Family,
    /// Sibling entry id when staged.
    pub sibling: Option<String>,
    pub sibling_name: String,
    /// Why no sibling was staged (lint, cap, name taken).
    pub blocked: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FamilyReview {
    pub family: Family,
    pub entries_affected: usize,
    pub recurrence_delta: i64,
    pub harm_delta: i64,
    pub entries: Vec<String>,
}

pub fn sibling_name(name: &str, family: &Family) -> String {
    format!("{name}--{}", slug(&family.0.replace('/', " ")))
}

fn family_note(e: &Entry, harmed: &Family, clean: &Family, h: &FamilyCounters, c: &FamilyCounters) -> String {
    format!(
        "# Family note\n\n\
         This variant serves `{harmed}`; `{name}` keeps serving `{clean}`.\n\n\
         While `{name}` (v{version}) was loaded, `{harmed}` showed harm {hh} and recurrence {hr} over {hm} matched sessions, \
         while `{clean}` matched {cm} sessions with zero harm and zero recurrence. \
         The counters diverge by family, so this sibling starts its own canary under `{harmed}`; \
         revise the steps above where they assume what `{clean}` does unprompted.\n",
        harmed = harmed.0,
        clean = clean.0,
        name = e.name,
        version = e.version,
        hh = h.harm,
        hr = h.recurrence,
        hm = h.matched,
        cm = c.matched,
    )
}

/// Create sibling proposals for every kept/provisional entry whose counters diverge by
/// family. Idempotent: an entry that already excludes the harmed family, or already has
/// a sibling for it, is skipped.
pub fn check(ctx: &AppCtx) -> Result<Vec<SplitOutcome>> {
    let all = ctx.ledger.list_entries(&EntryFilter::default())?;
    let mut out = Vec::new();
    for e in all.iter().filter(|e| matches!(e.status, Status::Kept | Status::Provisional) && !e.pinned) {
        let counters = ctx.ledger.counters(&e.id)?;
        let Some((harmed, clean)) = family_divergence(&counters) else { continue };
        let has_sibling = all.iter().any(|s| s.sibling_of.as_deref() == Some(e.id.as_str()) && s.family == harmed);
        if e.families_excluded.contains(&harmed) || has_sibling {
            continue;
        }
        let name = sibling_name(&e.name, &harmed);
        let mut res = SplitOutcome {
            original: e.id.clone(),
            name: e.name.clone(),
            harmed: harmed.clone(),
            clean: clean.clone(),
            sibling: None,
            sibling_name: name.clone(),
            blocked: None,
        };
        let provisional = all.iter().filter(|x| x.branch == e.branch && x.status == Status::Provisional).count();
        if provisional >= MAX_PROVISIONAL_PER_BRANCH {
            res.blocked = Some(format!("branch {} holds {provisional} provisional entries (cap)", e.branch));
            out.push(res);
            continue;
        }
        if all.iter().any(|x| x.name == name && x.branch == e.branch && x.status != Status::Retired) {
            res.blocked = Some(format!("an entry named {name} already exists in {}", e.branch));
            out.push(res);
            continue;
        }
        let hc = counters.per_family.get(&harmed).cloned().unwrap_or_default();
        let cc = counters.per_family.get(&clean).cloned().unwrap_or_default();
        let mut files = package_edits(&e.path)?;
        rename_skill(&mut files, &name)?;
        put_edit(&mut files, FAMILY_NOTE, family_note(e, &harmed, &clean, &hc, &cc));
        let sib = Entry {
            id: new_id(),
            name: name.clone(),
            status: Status::Provisional,
            family: harmed.clone(),
            verified_on: vec![],
            helpful: 0,
            harmful: 0,
            predecessor: None,
            version: e.version + 1,
            created_at: 0,
            updated_at: 0,
            pinned: false,
            approved_scripts: vec![],
            sibling_of: Some(e.id.clone()),
            status_since: None,
            status_note: Some(format!("family split of {}: harmed on {}", e.id, harmed.0)),
            pending_next_run: false,
            fork_commit: None,
            families_excluded: vec![],
            ..e.clone()
        };
        let branch_dir = ctx.paths.staging_branch(&e.branch);
        let lctx = LintContext {
            branch_other_bytes: branch_always_on_bytes(&branch_dir, &branch_dir.join(&name)),
            ..Default::default()
        };
        let rep = lint_edits(ctx, &sib, &files, &lctx)?;
        if !rep.ok {
            res.blocked = Some(format!("lint: {}", lint_errors(&rep)));
            out.push(res);
            continue;
        }
        let staged = stage(&ctx.ledger, &ctx.paths, &sib, &files)?;
        super::stale::snapshot_sources(&staged.path)?;
        let mut orig = e.clone();
        orig.families_excluded.push(harmed.clone());
        orig.updated_at = now_ms();
        save_entry(ctx, &orig)?;
        res.sibling = Some(staged.id);
        out.push(res);
    }
    Ok(out)
}

fn sum(rows: &[(String, FamilyCounters)]) -> BTreeMap<String, FamilyCounters> {
    rows.iter().cloned().collect()
}

/// Families whose recurrence or harm rose on ≥ `REVIEW_MIN_ENTRIES` entries in the last
/// `window_days` compared with the window before. Review list only.
pub fn capability_review(ctx: &AppCtx, window_days: u32) -> Result<Vec<FamilyReview>> {
    let w = i64::from(window_days.max(1)) * DAY_MS;
    let (cur_from, prev_from) = (ctx.now - w, ctx.now - 2 * w);
    let families: BTreeSet<Family> = ctx
        .ledger
        .list_counters()?
        .into_iter()
        .flat_map(|(_, c)| c.per_family.into_keys())
        .collect();
    let mut out = Vec::new();
    for fam in families {
        let cur = sum(&ctx.ledger.counters_for_family(&fam, cur_from)?);
        let since_prev = sum(&ctx.ledger.counters_for_family(&fam, prev_from)?);
        let (mut rd, mut hd) = (0i64, 0i64);
        let mut rising = Vec::new();
        for (id, all) in &since_prev {
            let c = cur.get(id).cloned().unwrap_or_default();
            let p = FamilyCounters {
                matched: all.matched - c.matched,
                recurrence: all.recurrence - c.recurrence,
                harm: all.harm - c.harm,
            };
            let (dr, dh) = (i64::from(c.recurrence) - i64::from(p.recurrence), i64::from(c.harm) - i64::from(p.harm));
            rd += dr;
            hd += dh;
            if dr > 0 || dh > 0 {
                rising.push(id.clone());
            }
        }
        if rising.len() >= REVIEW_MIN_ENTRIES && (rd > 0 || hd > 0) {
            out.push(FamilyReview { family: fam, entries_affected: rising.len(), recurrence_delta: rd, harm_delta: hd, entries: rising });
        }
    }
    Ok(out)
}

pub fn cmd_split(ctx: &AppCtx, args: SplitArgs) -> Result<Value> {
    let splits = check(ctx)?;
    let review = capability_review(ctx, args.review_days)?;
    Ok(json!({ "splits": splits, "review": review }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::testutil::{entry, env, stage as stage_t};
    use evolve_core::ledger::CounterRow;

    fn row(entry: &str, sid: &str, fam: &str, rec: u32, harm: u32, ts: i64) -> CounterRow {
        CounterRow {
            entry_id: entry.into(),
            session_id: sid.into(),
            family: Family(fam.into()),
            matched: 1,
            recurrence: rec,
            harm,
            tokens: 0,
            turns: 0,
            ts,
        }
    }

    #[test]
    fn split_creates_family_sibling_once() {
        let t = env();
        let ctx = &t.ctx;
        let e = stage_t(ctx, &entry("e-split-1", "ledger-migrations", "repo"), &[("units/steps.md", "## Procedure\n\n1. Run the test.\n")]);
        for (sid, fam, h) in [("a1", "openai/gpt-sol", 0), ("a2", "openai/gpt-sol", 0), ("b1", "github-copilot/gemini-argon", 1)] {
            ctx.ledger.add_counter(&row(&e.id, sid, fam, 0, h, 1_000)).unwrap();
        }
        let out = check(ctx).unwrap();
        assert_eq!(out.len(), 1, "{out:?}");
        let s = &out[0];
        assert_eq!(s.harmed, Family("github-copilot/gemini-argon".into()));
        assert_eq!(s.clean, Family("openai/gpt-sol".into()));
        assert_eq!(s.sibling_name, "ledger-migrations--github-copilot-gemini-argon");
        assert!(s.blocked.is_none(), "{:?}", s.blocked);
        let sib = ctx.ledger.get_entry(s.sibling.as_deref().unwrap()).unwrap().unwrap();
        assert_eq!(sib.status, Status::Provisional);
        assert_eq!(sib.family, s.harmed);
        assert_eq!(sib.sibling_of.as_deref(), Some(e.id.as_str()));
        assert_eq!(sib.version, e.version + 1);
        assert_eq!(sib.branch, "repo");
        assert!(sib.path.join("units/steps.md").is_file());
        let note = std::fs::read_to_string(sib.path.join(FAMILY_NOTE)).unwrap();
        assert!(note.contains("harm 1"), "{note}");
        let md = std::fs::read_to_string(sib.path.join("SKILL.md")).unwrap();
        assert!(md.contains("name: ledger-migrations--github-copilot-gemini-argon"), "{md}");
        let orig = ctx.ledger.get_entry(&e.id).unwrap().unwrap();
        assert_eq!(orig.families_excluded, vec![s.harmed.clone()]);
        assert_eq!(evolve_core::library::load_entry(&orig.path).unwrap().families_excluded, vec![s.harmed.clone()]);
        assert!(check(ctx).unwrap().is_empty(), "idempotent");
    }

    #[test]
    fn capability_review_flags_family_rising_on_several_entries() {
        let t = env();
        let ctx = &t.ctx;
        let now = ctx.now;
        let day = DAY_MS;
        for id in ["x1", "x2", "x3"] {
            // previous window clean, current window harmed for x1/x2; x3 flat
            ctx.ledger.add_counter(&row(id, &format!("{id}-old"), "github-copilot/gemini-argon", 0, 0, now - 10 * day)).unwrap();
            let harm = u32::from(id != "x3");
            ctx.ledger.add_counter(&row(id, &format!("{id}-new"), "github-copilot/gemini-argon", 0, harm, now - day)).unwrap();
            ctx.ledger.add_counter(&row(id, &format!("{id}-sol"), "openai/gpt-sol", 0, 0, now - day)).unwrap();
        }
        let r = capability_review(ctx, 7).unwrap();
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].family, Family("github-copilot/gemini-argon".into()));
        assert_eq!(r[0].entries_affected, 2);
        assert_eq!(r[0].harm_delta, 2);
        assert_eq!(r[0].recurrence_delta, 0);
    }
}
