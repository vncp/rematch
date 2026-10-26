//! Counters and decisions (DESIGN §7, SPEC loop 7–8). Counts are raw; a model never
//! rewrites them. Folding is idempotent per (entry, session).

use anyhow::Result;

use crate::ledger::{CounterRow, Ledger};
use crate::predicate::count_match;
use crate::types::{
    Counters, Decision, Entry, EntryId, Family, RedirectClass, SessionRecord, Status, UnixMs,
};

pub const DAY_MS: UnixMs = 24 * 60 * 60 * 1000;
pub const WINDOW_MS: UnixMs = 30 * DAY_MS;
pub const KEEP_MIN_MATCHED: u32 = 4;
pub const RETIRE_AT: u32 = 2;
pub const DORMANT_MATCHED: u32 = 12;
pub const DORMANT_IDLE_MATCHED: u32 = 3;
pub const UNKNOWN_FAMILY: &str = "unknown/unknown";

/// Redirect class of a redirect: the classifier's when present, else the prelabel
/// mapping `revert|negation|mark` → correction, `mark:<class>` → that class.
/// `interrupt`, `reprompt` and `none` carry no class.
pub fn redirect_class_of(class: Option<RedirectClass>, prelabel: &str) -> Option<RedirectClass> {
    if let Some(c) = class {
        return (c != RedirectClass::None).then_some(c);
    }
    match prelabel {
        "revert" | "negation" | "mark" => Some(RedirectClass::Correction),
        p => match p.strip_prefix("mark:") {
            Some("correction") => Some(RedirectClass::Correction),
            Some("preference") => Some(RedirectClass::Preference),
            Some("scope") => Some(RedirectClass::Scope),
            _ => None,
        },
    }
}

/// Did the entry's motivating redirect class recur in this session? An entry without
/// a class (motivated by an error cluster or takeover) recurs on any classed redirect.
pub fn recurred(entry: &Entry, s: &SessionRecord) -> bool {
    let want = entry.redirect_class.filter(|c| *c != RedirectClass::None);
    s.redirects.iter().any(|r| match redirect_class_of(r.class, &r.prelabel) {
        Some(c) => want.is_none_or(|w| w == c),
        None => false,
    })
}

/// Harm: a tool-error cluster, a takeover (interrupt), or the user disabling the entry.
pub fn harmed(s: &SessionRecord, disabled: bool) -> bool {
    s.error_clusters > 0 || s.takeovers > 0 || disabled
}

/// Fold one finished session into the counters of the entries that were loaded in it
/// (the caller resolves `s.loaded_entries` to entries). An entry counts only on a
/// count-time predicate hit. `disabled` lists entries the user disabled during the
/// session. Marks the session counted and refreshes each touched entry's
/// helpful/harmful tallies. Returns the rows written.
pub fn fold_session(
    ledger: &Ledger,
    s: &SessionRecord,
    entries_loaded: &[Entry],
    disabled: &[EntryId],
) -> Result<Vec<CounterRow>> {
    let place = s.repo.as_deref().unwrap_or(&s.cwd);
    let family = s.family.clone().unwrap_or_else(|| Family(UNKNOWN_FAMILY.into()));
    let mut rows = Vec::new();
    for e in entries_loaded {
        if !s.loaded_entries.is_empty() && !s.loaded_entries.contains(&e.id) {
            continue;
        }
        if !count_match(&e.predicate, place, &s.exts, &s.stems) {
            continue;
        }
        let row = CounterRow {
            entry_id: e.id.clone(),
            session_id: s.id.clone(),
            family: family.clone(),
            matched: 1,
            recurrence: recurred(e, s) as u32,
            harm: harmed(s, disabled.contains(&e.id)) as u32,
            tokens: s.tokens_in + s.tokens_out,
            turns: s.turns,
            ts: s.ended_at.unwrap_or(s.started_at),
        };
        ledger.add_counter(&row)?;
        refresh_tallies(ledger, &e.id)?;
        rows.push(row);
    }
    ledger.set_session_counted(&s.id, true)?;
    Ok(rows)
}

/// helpful = clean matched sessions, harmful = sessions with recurrence or harm.
fn refresh_tallies(ledger: &Ledger, entry_id: &str) -> Result<()> {
    let Some(mut e) = ledger.get_entry(entry_id)? else {
        return Ok(());
    };
    let rows = ledger.counter_rows(entry_id)?;
    let helpful = rows.iter().filter(|r| r.matched > 0 && r.recurrence == 0 && r.harm == 0).count() as u32;
    let harmful = rows.iter().filter(|r| r.recurrence > 0 || r.harm > 0).count() as u32;
    if (e.helpful, e.harmful) != (helpful, harmful) {
        e.helpful = helpful;
        e.harmful = harmful;
        ledger.upsert_entry(&e)?;
    }
    Ok(())
}

fn status_since(e: &Entry) -> UnixMs {
    e.status_since.unwrap_or(e.created_at)
}

/// Decision rules (DESIGN §7), evaluated in order:
/// pinned ⇒ Continue (human-owned);
/// provisional: recurrence ≥ 2 ∨ harm ≥ 2 ⇒ Retire; matched ≥ 4 ∧ recurrence = 0 ∧ harm = 0 ⇒ Keep;
/// matched ≥ 12 ⇒ Dormant; provisional ≥ 30 days ∧ matched < 3 ⇒ Dormant;
/// kept: ≥ 2 harm events within the last 30 days (and since it was kept) ⇒ Retire;
/// otherwise Continue.
pub fn decide(e: &Entry, c: &Counters, now: UnixMs) -> Decision {
    if e.pinned || e.status == Status::Pinned {
        return Decision::Continue;
    }
    match e.status {
        Status::Provisional => {
            if c.recurrence >= RETIRE_AT || c.harm >= RETIRE_AT {
                Decision::Retire
            } else if c.matched >= KEEP_MIN_MATCHED && c.recurrence == 0 && c.harm == 0 {
                Decision::Keep
            } else if c.matched >= DORMANT_MATCHED
                || (now - status_since(e) >= WINDOW_MS && c.matched < DORMANT_IDLE_MATCHED)
            {
                Decision::Dormant
            } else {
                Decision::Continue
            }
        }
        Status::Kept => {
            let from = (now - WINDOW_MS).max(status_since(e));
            let recent = c.harm_times.iter().filter(|t| **t >= from && **t <= now).count() as u32;
            if recent >= RETIRE_AT {
                Decision::Retire
            } else {
                Decision::Continue
            }
        }
        _ => Decision::Continue,
    }
}

/// Counters diverge by family (Phase 3 split): one family harmed (harm ≥ 1) while
/// another has matched ≥ 2 with zero harm and zero recurrence.
/// Returns `(harmed, clean)`; ties resolve to the most-harmed / most-matched family.
pub fn family_divergence(c: &Counters) -> Option<(Family, Family)> {
    let harmed = c
        .per_family
        .iter()
        .filter(|(_, f)| f.harm >= 1)
        .max_by(|a, b| a.1.harm.cmp(&b.1.harm).then_with(|| b.0.cmp(a.0)))?;
    let clean = c
        .per_family
        .iter()
        .filter(|(fam, f)| *fam != harmed.0 && f.matched >= 2 && f.harm == 0 && f.recurrence == 0)
        .max_by(|a, b| a.1.matched.cmp(&b.1.matched).then_with(|| b.0.cmp(a.0)))?;
    Some((harmed.0.clone(), clean.0.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::tests::{entry, session};
    use crate::types::{FamilyCounters, Redirect};

    fn ledger() -> (tempfile::TempDir, Ledger) {
        let t = tempfile::tempdir().unwrap();
        let l = Ledger::open(&t.path().join("ledger.sqlite")).unwrap();
        (t, l)
    }

    fn red(prelabel: &str, class: Option<RedirectClass>) -> Redirect {
        Redirect {
            session_id: "s".into(),
            seq: 1,
            prelabel: prelabel.into(),
            class,
            confidence: None,
            text_head: String::new(),
            deliverable_head: String::new(),
            t: 0,
        }
    }

    fn counters(m: u32, r: u32, h: u32) -> Counters {
        Counters { matched: m, recurrence: r, harm: h, ..Default::default() }
    }

    #[test]
    fn prelabel_mapping() {
        use RedirectClass::{Correction, Preference, Scope};
        assert_eq!(redirect_class_of(None, "revert"), Some(Correction));
        assert_eq!(redirect_class_of(None, "negation"), Some(Correction));
        assert_eq!(redirect_class_of(None, "mark"), Some(Correction));
        assert_eq!(redirect_class_of(None, "mark:preference"), Some(Preference));
        assert_eq!(redirect_class_of(None, "mark:scope"), Some(Scope));
        assert_eq!(redirect_class_of(None, "interrupt"), None);
        assert_eq!(redirect_class_of(None, "reprompt"), None);
        assert_eq!(redirect_class_of(None, "none"), None);
        // classifier wins over prelabel, including an explicit None
        assert_eq!(redirect_class_of(Some(Preference), "revert"), Some(Preference));
        assert_eq!(redirect_class_of(Some(RedirectClass::None), "revert"), None);
    }

    #[test]
    fn decide_provisional_boundaries() {
        let day = DAY_MS;
        let mut e = entry("e", "n", "b", Status::Provisional);
        e.created_at = 0;
        let now = 5 * day;
        assert_eq!(decide(&e, &counters(3, 0, 0), now), Decision::Continue, "3 matched: not yet");
        assert_eq!(decide(&e, &counters(4, 0, 0), now), Decision::Keep);
        assert_eq!(decide(&e, &counters(4, 1, 0), now), Decision::Continue, "one recurrence blocks keep");
        assert_eq!(decide(&e, &counters(4, 0, 1), now), Decision::Continue, "one harm blocks keep");
        assert_eq!(decide(&e, &counters(2, 2, 0), now), Decision::Retire);
        assert_eq!(decide(&e, &counters(2, 0, 2), now), Decision::Retire);
        assert_eq!(decide(&e, &counters(20, 2, 0), now), Decision::Retire, "retire beats dormant");
        assert_eq!(decide(&e, &counters(11, 1, 0), now), Decision::Continue);
        assert_eq!(decide(&e, &counters(12, 1, 1), now), Decision::Dormant);
    }

    #[test]
    fn decide_thirty_day_dormant_uses_status_since() {
        let mut e = entry("e", "n", "b", Status::Provisional);
        e.created_at = 0;
        assert_eq!(decide(&e, &counters(2, 0, 0), WINDOW_MS - 1), Decision::Continue);
        assert_eq!(decide(&e, &counters(2, 0, 0), WINDOW_MS), Decision::Dormant);
        assert_eq!(decide(&e, &counters(3, 1, 0), WINDOW_MS * 2), Decision::Continue, "≥3 matches stays");
        // re-provisioned recently: clock restarts
        e.status_since = Some(WINDOW_MS);
        assert_eq!(decide(&e, &counters(0, 0, 0), WINDOW_MS + DAY_MS), Decision::Continue);
    }

    #[test]
    fn decide_kept_retires_on_two_recent_harms_only() {
        let mut e = entry("e", "n", "b", Status::Kept);
        e.created_at = 0;
        e.status_since = Some(10 * DAY_MS);
        let now = 100 * DAY_MS;
        let mut c = counters(10, 0, 2);
        c.harm_times = vec![now - 40 * DAY_MS, now - DAY_MS];
        assert_eq!(decide(&e, &c, now), Decision::Continue, "one harm outside 30 days");
        c.harm_times = vec![now - 29 * DAY_MS, now - DAY_MS];
        assert_eq!(decide(&e, &c, now), Decision::Retire);
        // harms recorded before it was kept do not count
        e.status_since = Some(now - 2 * DAY_MS);
        assert_eq!(decide(&e, &c, now), Decision::Continue);
        // kept entries are not re-kept or made dormant
        assert_eq!(decide(&e, &counters(50, 0, 0), now), Decision::Continue);
    }

    #[test]
    fn decide_pinned_and_other_statuses_continue() {
        let mut e = entry("e", "n", "b", Status::Provisional);
        e.pinned = true;
        assert_eq!(decide(&e, &counters(9, 5, 5), 0), Decision::Continue);
        for st in [Status::Draft, Status::Dormant, Status::Retired, Status::Stale, Status::Pinned] {
            let e = entry("e", "n", "b", st);
            assert_eq!(decide(&e, &counters(9, 0, 0), 0), Decision::Continue, "{st:?}");
        }
    }

    #[test]
    fn family_divergence_rules() {
        let mut c = Counters::default();
        let fam = |s: &str| Family(s.into());
        c.per_family.insert(fam("a/x"), FamilyCounters { matched: 3, recurrence: 0, harm: 1 });
        assert_eq!(family_divergence(&c), None, "no clean family");
        c.per_family.insert(fam("b/y"), FamilyCounters { matched: 1, recurrence: 0, harm: 0 });
        assert_eq!(family_divergence(&c), None, "clean family needs ≥2 matched");
        c.per_family.insert(fam("c/z"), FamilyCounters { matched: 2, recurrence: 1, harm: 0 });
        assert_eq!(family_divergence(&c), None, "recurrence is not clean");
        c.per_family.insert(fam("d/w"), FamilyCounters { matched: 2, recurrence: 0, harm: 0 });
        assert_eq!(family_divergence(&c), Some((fam("a/x"), fam("d/w"))));
    }

    #[test]
    fn fold_counts_matched_sessions_and_is_idempotent() {
        let (_t, l) = ledger();
        let mut e = entry("e1", "ledger-skill", "repo", Status::Provisional);
        e.predicate.file_exts = vec!["rs".into()];
        e.predicate.stems = vec!["ledger".into(), "migrat".into()];
        e.redirect_class = Some(RedirectClass::Correction);
        l.upsert_entry(&e).unwrap();
        let mut other = entry("e2", "unrelated", "repo", Status::Provisional);
        other.predicate.file_exts = vec!["py".into()];
        l.upsert_entry(&other).unwrap();

        // session 1: hit, a negation (→ correction) and an error cluster
        let mut s1 = session("s1", "repo", 1_000);
        s1.cwd = "/w/repo/crates".into();
        s1.repo = None;
        s1.exts = vec!["rs".into()];
        s1.stems = vec!["ledger".into(), "migrat".into(), "x".into()];
        s1.loaded_entries = vec!["e1".into(), "e2".into()];
        s1.redirects = vec![red("negation", None)];
        s1.error_clusters = 1;
        s1.tokens_in = 10;
        s1.tokens_out = 5;
        s1.turns = 3;
        l.upsert_session(&s1).unwrap();
        let rows = fold_session(&l, &s1, &[e.clone(), other.clone()], &[]).unwrap();
        assert_eq!(rows.len(), 1, "e2 predicate misses");
        assert_eq!((rows[0].matched, rows[0].recurrence, rows[0].harm, rows[0].tokens), (1, 1, 1, 15));
        assert!(l.get_session("s1").unwrap().unwrap().counted);
        fold_session(&l, &s1, &[e.clone(), other.clone()], &[]).unwrap();
        assert_eq!(l.counters("e1").unwrap().matched, 1, "refold is idempotent");
        assert_eq!(l.get_entry("e1").unwrap().unwrap().harmful, 1);

        // session 2: hit, preference redirect (different class) and user disable → harm only
        let mut s2 = session("s2", "repo", 2_000);
        s2.exts = vec!["rs".into()];
        s2.stems = vec!["ledger".into(), "migrat".into()];
        s2.loaded_entries = vec!["e1".into()];
        s2.redirects = vec![red("none", Some(RedirectClass::Preference))];
        let rows = fold_session(&l, &s2, &[e.clone()], &["e1".to_string()]).unwrap();
        assert_eq!((rows[0].recurrence, rows[0].harm), (0, 1));

        // session 3: stems overlap only 1 of 2 → no match; not loaded entry → skipped
        let mut s3 = session("s3", "repo", 3_000);
        s3.exts = vec!["rs".into()];
        s3.stems = vec!["ledger".into()];
        s3.loaded_entries = vec!["e1".into()];
        assert!(fold_session(&l, &s3, &[e.clone()], &[]).unwrap().is_empty());
        let mut s4 = s2.clone();
        s4.id = "s4".into();
        s4.loaded_entries = vec!["e2".into()];
        assert!(fold_session(&l, &s4, &[e.clone()], &[]).unwrap().is_empty());

        let c = l.counters("e1").unwrap();
        assert_eq!((c.matched, c.recurrence, c.harm), (2, 1, 2));
        assert_eq!(decide(&l.get_entry("e1").unwrap().unwrap(), &c, 3_000), Decision::Retire);
    }

    #[test]
    fn fold_clean_sessions_reach_keep_and_no_decision_case() {
        let (_t, l) = ledger();
        let e = entry("e1", "n", "repo", Status::Provisional); // exts [rs], no stems
        l.upsert_entry(&e).unwrap();
        for i in 0..3 {
            let mut s = session(&format!("s{i}"), "repo", i * 10);
            s.exts = vec!["rs".into()];
            s.loaded_entries = vec!["e1".into()];
            fold_session(&l, &s, std::slice::from_ref(&e), &[]).unwrap();
        }
        let c = l.counters("e1").unwrap();
        assert_eq!(decide(&e, &c, 100), Decision::Continue, "no decision at 3 clean sessions");
        let mut s = session("s9", "repo", 99);
        s.exts = vec!["rs".into()];
        s.loaded_entries = vec!["e1".into()];
        // an interrupt prelabel (no class) is not a recurrence; takeovers are harm
        s.redirects = vec![red("interrupt", None)];
        fold_session(&l, &s, std::slice::from_ref(&e), &[]).unwrap();
        let c = l.counters("e1").unwrap();
        assert_eq!((c.matched, c.recurrence, c.harm), (4, 0, 0));
        assert_eq!(decide(&e, &c, 100), Decision::Keep);
        assert_eq!(l.get_entry("e1").unwrap().unwrap().helpful, 4);
    }
}
