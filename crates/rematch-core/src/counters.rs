//! Counters and decisions (DESIGN §7, SPEC loop 7–8). Counts are raw; a model never
//! rewrites them. Folding is idempotent per (entry, session).
//!
//! Two evidence classes justify an automatic keep (`keep_reason`): accepted-in-use —
//! sustained relevant ACTUAL use ([`actual_use`]) in clean completed sessions — and
//! comparative support — an engine-run, content-bound reference/candidate comparison.
//! Matched sessions alone (a catalog listing, a loaded but unread skill) are observational
//! safety counters: they can retire or idle an entry, never keep it.

use anyhow::Result;
use serde::Serialize;

use crate::ledger::{CounterRow, Ledger};
use crate::predicate::{count_match, Place};
use crate::types::{
    ComparativeEvidence, ComparativeOutcome, Counters, Decision, Entry, EntryId, Family, HookMode, Kind,
    RedirectClass, RetentionReason, SessionRecord, Status, UnixMs,
};

pub const DAY_MS: UnixMs = 24 * 60 * 60 * 1000;
pub const WINDOW_MS: UnixMs = 30 * DAY_MS;
/// Matched sessions `report` measures kept entries over.
pub const KEEP_MIN_MATCHED: u32 = 4;
/// Clean sessions with actual use that make a provisional entry accepted-in-use.
pub const KEEP_MIN_USED: u32 = 4;
pub const RETIRE_AT: u32 = 2;
pub const DORMANT_MATCHED: u32 = 12;
pub const DORMANT_IDLE_MATCHED: u32 = 3;
pub const UNKNOWN_FAMILY: &str = "unknown/unknown";

/// How a session actually used an entry. Loaded, matched or listed in a catalog is none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UseKind {
    /// The skill body was read or the skill invoked (`/skill:`).
    SkillRead,
    /// A retrieve hook put units of the package into the context.
    Retrieved,
    /// The approved personal instruction section was in the session's AGENTS.md.
    InstructionExposed,
    /// The hook blocked a call, retrieved, or rewrote context.
    HookApplied,
    /// The hook evaluated a matching tool call (or every turn, for `truncate`).
    HookEvaluated,
    /// A shell command ran one of the entry's scripts.
    ScriptRun,
    /// The session invoked the agent through the task tool.
    AgentInvoked,
}

/// Actual use of `e` in `s`, by kind: skills (rules, facts, lessons) need a body read or
/// invocation (by resolved entry id; by name when the adapter resolved no ids), a
/// retrieval, or an approved instruction exposure; hooks an application or evaluation;
/// scripts an execution; agents an invocation.
pub fn actual_use(e: &Entry, s: &SessionRecord) -> Option<UseKind> {
    let id = &e.id;
    match e.kind {
        Kind::Hook => {
            if s.hooks_applied.contains(id) {
                Some(UseKind::HookApplied)
            } else if s.hooks_evaluated.contains(id) || truncate_evaluated(e, s) {
                Some(UseKind::HookEvaluated)
            } else {
                None
            }
        }
        Kind::Agent => s.agents_invoked.contains(&e.name).then_some(UseKind::AgentInvoked),
        Kind::Script => script_ran(e, s).then_some(UseKind::ScriptRun),
        Kind::Skill | Kind::Rule | Kind::Fact | Kind::Lesson => {
            let approved = s.instruction_shas.get(id).is_none_or(|sha| e.approved_fragment.as_ref().is_none_or(|a| a == sha));
            // Runtime insertion counts only for the text approved now.
            let sent = s.fragments_sent.contains(id)
                && s.fragment_shas.get(id).is_some_and(|sha| e.approved_fragment.as_ref() == Some(sha));
            if (s.instructions_exposed.contains(id) && approved) || sent {
                Some(UseKind::InstructionExposed)
            } else if skill_used(e, s) {
                Some(UseKind::SkillRead)
            } else if s.retrieved_entries.contains(id) {
                Some(UseKind::Retrieved)
            } else {
                None
            }
        }
    }
}

/// The skill body was read or invoked. A session from a scoping adapter (`exposure_view`)
/// credits only a read resolved to this entry id, or an invocation of its name while its
/// catalog listed this entry: a bare read name may be a native skill of the same name.
/// Older traces and imports keep name crediting when no ids were resolved.
fn skill_used(e: &Entry, s: &SessionRecord) -> bool {
    if s.skill_entries.contains(&e.id) {
        return true;
    }
    if s.exposure_view.is_some() {
        return s.skills_invoked.contains(&e.name) && s.visible_entries.contains(&e.id);
    }
    s.skill_entries.is_empty() && s.skill_reads.contains(&e.name)
}

/// A loaded `truncate` hook evaluates the context every turn; it has work once the session
/// is older than its age cutoff.
fn truncate_evaluated(e: &Entry, s: &SessionRecord) -> bool {
    let Some(h) = e.hook.as_ref().filter(|h| h.mode == HookMode::Truncate) else { return false };
    s.hooks_loaded.contains(&e.id) && s.turns > h.max_age_turns.unwrap_or(crate::types::DEFAULT_TRUNCATE_MAX_AGE_TURNS)
}

/// A shell command named one of the entry's scripts (`<name>/scripts/…`).
fn script_ran(e: &Entry, s: &SessionRecord) -> bool {
    let unix = format!("{}/scripts/", e.name);
    let win = format!("{}\\scripts\\", e.name);
    s.commands.iter().any(|c| c.contains(&unix) || c.contains(&win))
}

/// Whether the session completed model work: ≥ 1 model turn and a settled run
/// (`SessionRecord::run_completed`: a terminal and yielded `agent_end` after the last turn,
/// which did not stop on a tool call, an abort or an error). `None` when the record has no
/// run-end evidence (an import, an adapter without `agent_end`): unknown, never positive.
/// A session that only started and shut down, or broke off mid-tool, is no evidence of use.
pub fn completed_work(s: &SessionRecord) -> Option<bool> {
    if s.turns == 0 {
        return Some(false);
    }
    s.run_completed
}

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

/// Whether a redirect of class `got` ([`redirect_class_of`]) is a recurrence for an entry
/// motivated by `want`. An entry without a class (motivated by an error cluster or
/// takeover, and the built-ins) recurs on a correction or preference redirect only: a
/// scope redirect (the user widened or changed the task) carries no failure signal
/// (DESIGN §7).
pub fn class_recurs(want: Option<RedirectClass>, got: Option<RedirectClass>) -> bool {
    match (got, want.filter(|c| *c != RedirectClass::None)) {
        (Some(c), Some(w)) => c == w,
        (Some(c), None) => matches!(c, RedirectClass::Correction | RedirectClass::Preference),
        (None, _) => false,
    }
}

/// Did the entry's motivating redirect class recur in this session ([`class_recurs`])?
pub fn recurred(entry: &Entry, s: &SessionRecord) -> bool {
    s.redirects.iter().any(|r| class_recurs(entry.redirect_class, redirect_class_of(r.class, &r.prelabel)))
}

/// Harm: a tool-error cluster or a takeover (interrupt) in a session that actually used the
/// entry, or the user disabling the entry. A session's errors are never blamed on an entry
/// that only matched it: an always-on hook matches every session, and would otherwise be
/// retired by any two long sessions with tool errors it had nothing to do with.
pub fn harmed(s: &SessionRecord, used: bool, disabled: bool) -> bool {
    disabled || (used && (s.error_clusters > 0 || s.takeovers > 0))
}

/// Whether a `block` hook fired in `s` and was followed in the same session by a user
/// correction/preference redirect (or any non-none redirect that was not classified as `Scope`
/// or `None`).
pub fn block_hook_harmed(entry: &Entry, s: &SessionRecord) -> bool {
    if entry.kind != Kind::Hook || entry.hook.as_ref().is_none_or(|h| h.mode != HookMode::Block) {
        return false;
    }
    if !s.hooks_applied.contains(&entry.id) {
        return false;
    }
    let blocked_seq = s.hook_blocked_at.get(&entry.id).copied().unwrap_or(0);
    s.redirects.iter().any(|r| {
        r.seq > blocked_seq
            && (matches!(redirect_class_of(r.class, &r.prelabel), Some(RedirectClass::Correction | RedirectClass::Preference))
                || (r.class.is_none()
                    && !r.prelabel.is_empty()
                    && r.prelabel != "none"
                    && !r.prelabel.starts_with("mark:scope")))
    })
}

/// Fold one finished session into the counters of the entries that were loaded in it
/// (the caller resolves `s.loaded_entries` to entries). An entry counts only on a
/// count-time predicate hit ([`Place`]: the session's remote and subpath when both sides
/// have a remote, else its repo or cwd against the glob), and only at the version and
/// content it was loaded with when `session_start` recorded them (a changed entry never
/// inherits the old content's use). Each row records whether the session actually used the
/// entry ([`actual_use`]). `disabled` lists entries the user disabled during the session.
/// Marks the session counted and refreshes each touched entry's helpful/harmful tallies.
/// Returns the rows written.
pub fn fold_session(
    ledger: &Ledger,
    s: &SessionRecord,
    entries_loaded: &[Entry],
    disabled: &[EntryId],
) -> Result<Vec<CounterRow>> {
    let place = Place::of_session(s);
    let family = s.family.clone().unwrap_or_else(|| Family(UNKNOWN_FAMILY.into()));
    let mut rows = Vec::new();
    for e in entries_loaded {
        if !s.loaded_entries.is_empty() && !s.loaded_entries.contains(&e.id) {
            continue;
        }
        if s.loaded_versions.get(&e.id).is_some_and(|v| *v != e.version) {
            continue;
        }
        if s.loaded_shas.get(&e.id).is_some_and(|sha| e.content_sha256.as_ref().is_some_and(|c| c != sha)) {
            continue;
        }
        if !count_match(&e.predicate, &place, &s.exts, &s.stems) {
            continue;
        }
        let used = actual_use(e, s).is_some();
        let row = CounterRow {
            entry_id: e.id.clone(),
            session_id: s.id.clone(),
            family: family.clone(),
            matched: 1,
            recurrence: recurred(e, s) as u32,
            harm: (harmed(s, used, disabled.contains(&e.id)) || block_hook_harmed(e, s)) as u32,
            tokens: s.tokens_in + s.tokens_out,
            turns: s.turns,
            ts: s.ended_at.unwrap_or(s.started_at),
            // Use in a session whose completion is unknown stays unknown (`None`), never positive.
            used: match (used, completed_work(s)) {
                (false, _) | (true, Some(false)) => Some(0),
                (true, Some(true)) => Some(1),
                (true, None) => None,
            },
        };
        ledger.add_counter(&row)?;
        refresh_tallies(ledger, &e.id)?;
        rows.push(row);
    }
    ledger.set_session_counted(&s.id, true)?;
    Ok(rows)
}

/// helpful = clean sessions with actual use (never bare matches), harmful = sessions with
/// recurrence or harm.
fn refresh_tallies(ledger: &Ledger, entry_id: &str) -> Result<()> {
    let Some(mut e) = ledger.get_entry(entry_id)? else {
        return Ok(());
    };
    let c = ledger.counters(entry_id)?;
    let rows = ledger.counter_rows(entry_id)?;
    let helpful = c.used_clean;
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

/// The evidence that justifies keeping a provisional entry now, if any: a comparison bound
/// to its current version and content that supports it ⇒ comparative support; else ≥
/// [`KEEP_MIN_USED`] clean sessions with actual use ⇒ accepted-in-use. Either requires no
/// recurrence and no harm over every matched session.
pub fn keep_reason(e: &Entry, c: &Counters, probe: Option<&ComparativeEvidence>) -> Option<RetentionReason> {
    if c.recurrence > 0 || c.harm > 0 {
        return None;
    }
    if probe.is_some_and(|p| p.outcome == ComparativeOutcome::Supports && p.binds(e)) {
        return Some(RetentionReason::ComparativeSupport);
    }
    (c.used_clean >= KEEP_MIN_USED).then_some(RetentionReason::AcceptedInUse)
}

/// What a surface reports as the evidence behind an entry's status: pinned ⇒ the user's
/// decision; kept ⇒ its engine record, else `legacy` (kept before evidence was recorded).
/// `None` for an entry that is not kept or pinned.
pub fn retention_label(e: &Entry) -> Option<RetentionReason> {
    if e.pinned || e.status == Status::Pinned {
        return Some(RetentionReason::UserDecision);
    }
    (e.status == Status::Kept).then(|| e.retention.as_ref().map_or(RetentionReason::Legacy, |r| r.reason))
}

/// Decision rules (DESIGN §7), evaluated in order:
/// pinned ⇒ Continue (human-owned);
/// provisional: recurrence ≥ 2 ∨ harm ≥ 2 ⇒ Retire; a bound comparison that regresses ⇒
/// Retire; [`keep_reason`] ⇒ Keep; matched ≥ 12 ⇒ Dormant; provisional ≥ 30 days ∧
/// matched < 3 ⇒ Dormant;
/// kept: ≥ 2 harm events within the last 30 days (and since it was kept) ⇒ Retire;
/// otherwise Continue. Ties, cost-worse and incomplete comparisons decide nothing.
/// `decide` runs for entries that got a counter row; the provisional rest is swept by
/// [`idle_dormant`].
pub fn decide(e: &Entry, c: &Counters, probe: Option<&ComparativeEvidence>, now: UnixMs) -> Decision {
    if e.pinned || e.status == Status::Pinned {
        return Decision::Continue;
    }
    match e.status {
        Status::Provisional => {
            let regressed = probe.is_some_and(|p| p.outcome == ComparativeOutcome::Regresses && p.binds(e));
            if c.recurrence >= RETIRE_AT || c.harm >= RETIRE_AT || regressed {
                Decision::Retire
            } else if keep_reason(e, c, probe).is_some() {
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

/// The idle dormancy rule for a provisional entry that `decide` never sees because it got
/// no counter row this run (a predicate that never hits gets none at all): Dormant once it
/// has been provisional ≥ 30 days with matched < 3 **and** at least 12 counted sessions
/// ran in its scope since then (`sessions_in_scope`: sessions passing [`in_idle_scope`]).
/// Idle time alone never makes an entry dormant: a user away for a month runs no sessions.
/// Pinned, built-in and lesson entries (library records, never loaded) are exempt.
pub fn idle_dormant(e: &Entry, c: &Counters, sessions_in_scope: usize, now: UnixMs) -> bool {
    e.status == Status::Provisional
        && !e.pinned
        && !crate::library::is_builtin(e)
        && e.kind != Kind::Lesson
        && now - status_since(e) >= WINDOW_MS
        && c.matched < DORMANT_IDLE_MATCHED
        && sessions_in_scope >= DORMANT_MATCHED as usize
}

/// A session that gave the entry its chance to match ([`idle_dormant`]): counted, started
/// since the entry's current status, in the entry's branch (any branch for `shared`).
pub fn in_idle_scope(e: &Entry, s: &SessionRecord) -> bool {
    s.counted && s.started_at >= status_since(e) && (e.branch == crate::library::SHARED_BRANCH || s.branch == e.branch)
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

    /// `m` matched sessions of which `u` actually used the entry and were clean.
    fn used(m: u32, u: u32) -> Counters {
        Counters { matched: m, used: u, used_clean: u, ..Default::default() }
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
    fn unclassed_entries_recur_on_correction_or_preference_never_scope() {
        use RedirectClass::{Correction, Preference, Scope};
        let mut e = entry("e", "n", "b", Status::Provisional);
        e.redirect_class = None;
        let mut s = session("s", "b", 0);
        for (redirect, want) in [
            (red("none", Some(Scope)), false),
            (red("mark:scope", None), false),
            (red("interrupt", None), false),
            (red("none", Some(Correction)), true),
            (red("mark:preference", None), true),
        ] {
            s.redirects = vec![redirect];
            assert_eq!(recurred(&e, &s), want, "{:?}", s.redirects[0]);
        }
        // an explicit class still matches only itself, scope included
        e.redirect_class = Some(Scope);
        s.redirects = vec![red("mark:scope", None)];
        assert!(recurred(&e, &s));
        s.redirects = vec![red("none", Some(Preference))];
        assert!(!recurred(&e, &s));
    }

    #[test]
    fn decide_provisional_boundaries() {
        let day = DAY_MS;
        let mut e = entry("e", "n", "b", Status::Provisional);
        e.created_at = 0;
        let now = 5 * day;
        assert_eq!(decide(&e, &counters(4, 0, 0), None, now), Decision::Continue, "4 matched, never used: no keep");
        assert_eq!(decide(&e, &used(4, 3), None, now), Decision::Continue, "3 clean uses: not yet");
        assert_eq!(decide(&e, &used(4, 4), None, now), Decision::Keep);
        assert_eq!(keep_reason(&e, &used(4, 4), None), Some(RetentionReason::AcceptedInUse));
        assert_eq!(decide(&e, &Counters { recurrence: 1, ..used(5, 4) }, None, now), Decision::Continue, "one recurrence blocks keep");
        assert_eq!(decide(&e, &Counters { harm: 1, ..used(5, 4) }, None, now), Decision::Continue, "one harm blocks keep");
        assert_eq!(decide(&e, &counters(2, 2, 0), None, now), Decision::Retire);
        assert_eq!(decide(&e, &counters(2, 0, 2), None, now), Decision::Retire);
        assert_eq!(decide(&e, &counters(20, 2, 0), None, now), Decision::Retire, "retire beats dormant");
        assert_eq!(decide(&e, &counters(11, 1, 0), None, now), Decision::Continue);
        assert_eq!(decide(&e, &counters(12, 1, 1), None, now), Decision::Dormant);
        assert_eq!(decide(&e, &counters(12, 0, 0), None, now), Decision::Dormant, "matched often, never used");
    }

    #[test]
    fn decide_uses_only_bound_comparisons() {
        let mut e = entry("e", "n", "b", Status::Provisional);
        e.content_sha256 = Some("c1".into());
        let probe = |outcome, version, sha: &str| ComparativeEvidence {
            probe: "p".into(),
            outcome,
            version,
            content_sha256: Some(sha.into()),
            at: 0,
        };
        let c = counters(0, 0, 0);
        let sup = probe(ComparativeOutcome::Supports, 1, "c1");
        assert_eq!(decide(&e, &c, Some(&sup), 0), Decision::Keep, "support without recorded regressions");
        assert_eq!(keep_reason(&e, &c, Some(&sup)), Some(RetentionReason::ComparativeSupport));
        assert_eq!(decide(&e, &counters(1, 1, 0), Some(&sup), 0), Decision::Continue, "a recorded recurrence blocks it");
        let stale = probe(ComparativeOutcome::Supports, 1, "other");
        assert_eq!(decide(&e, &c, Some(&stale), 0), Decision::Continue, "changed content inherits nothing");
        let old = probe(ComparativeOutcome::Supports, 0, "c1");
        assert_eq!(decide(&e, &c, Some(&old), 0), Decision::Continue, "another version inherits nothing");
        assert_eq!(decide(&e, &c, Some(&probe(ComparativeOutcome::Regresses, 1, "c1")), 0), Decision::Retire);
        for o in [ComparativeOutcome::Tie, ComparativeOutcome::CostWorse, ComparativeOutcome::Incomplete] {
            assert_eq!(decide(&e, &c, Some(&probe(o, 1, "c1")), 0), Decision::Continue, "{o:?}");
            assert_eq!(decide(&e, &used(4, 4), Some(&probe(o, 1, "c1")), 0), Decision::Keep, "{o:?}: observational acceptance still keeps");
        }
    }

    #[test]
    fn retention_labels_are_honest() {
        let mut e = entry("e", "n", "b", Status::Kept);
        assert_eq!(retention_label(&e), Some(RetentionReason::Legacy), "kept before evidence was recorded");
        e.retention = Some(crate::types::Retention {
            reason: RetentionReason::AcceptedInUse,
            at: 1,
            version: 1,
            content_sha256: None,
            matched: 5,
            used: 4,
            used_clean: 4,
            probe: None,
            detail: String::new(),
        });
        assert_eq!(retention_label(&e), Some(RetentionReason::AcceptedInUse));
        e.pinned = true;
        assert_eq!(retention_label(&e), Some(RetentionReason::UserDecision));
        assert_eq!(retention_label(&entry("p", "n", "b", Status::Provisional)), None);
    }

    #[test]
    fn decide_thirty_day_dormant_uses_status_since() {
        let mut e = entry("e", "n", "b", Status::Provisional);
        e.created_at = 0;
        assert_eq!(decide(&e, &counters(2, 0, 0), None, WINDOW_MS - 1), Decision::Continue);
        assert_eq!(decide(&e, &counters(2, 0, 0), None, WINDOW_MS), Decision::Dormant);
        assert_eq!(decide(&e, &counters(3, 1, 0), None, WINDOW_MS * 2), Decision::Continue, "≥3 matches stays");
        // re-provisioned recently: clock restarts
        e.status_since = Some(WINDOW_MS);
        assert_eq!(decide(&e, &counters(0, 0, 0), None, WINDOW_MS + DAY_MS), Decision::Continue);
    }

    #[test]
    fn decide_kept_retires_on_two_recent_harms_only() {
        let mut e = entry("e", "n", "b", Status::Kept);
        e.created_at = 0;
        e.status_since = Some(10 * DAY_MS);
        let now = 100 * DAY_MS;
        let mut c = counters(10, 0, 2);
        c.harm_times = vec![now - 40 * DAY_MS, now - DAY_MS];
        assert_eq!(decide(&e, &c, None, now), Decision::Continue, "one harm outside 30 days");
        c.harm_times = vec![now - 29 * DAY_MS, now - DAY_MS];
        assert_eq!(decide(&e, &c, None, now), Decision::Retire);
        // harms recorded before it was kept do not count
        e.status_since = Some(now - 2 * DAY_MS);
        assert_eq!(decide(&e, &c, None, now), Decision::Continue);
        // kept entries are not re-kept or made dormant
        assert_eq!(decide(&e, &used(50, 50), None, now), Decision::Continue);
    }

    #[test]
    fn decide_pinned_and_other_statuses_continue() {
        let mut e = entry("e", "n", "b", Status::Provisional);
        e.pinned = true;
        assert_eq!(decide(&e, &counters(9, 5, 5), None, 0), Decision::Continue);
        for st in [Status::Draft, Status::Dormant, Status::Retired, Status::Stale, Status::Pinned] {
            let e = entry("e", "n", "b", st);
            assert_eq!(decide(&e, &used(9, 9), None, 0), Decision::Continue, "{st:?}");
        }
    }

    #[test]
    fn idle_dormant_boundaries() {
        let n = DORMANT_MATCHED as usize;
        let mut e = entry("e", "n", "b", Status::Provisional);
        e.created_at = 0;
        let c = counters(0, 0, 0);
        assert!(idle_dormant(&e, &c, n, WINDOW_MS), "30 days, 12 sessions, matched 0");
        assert!(!idle_dormant(&e, &c, n, WINDOW_MS - 1), "29 days and change");
        assert!(!idle_dormant(&e, &c, n, 29 * DAY_MS));
        assert!(!idle_dormant(&e, &c, n - 1, WINDOW_MS), "11 sessions in scope");
        assert!(!idle_dormant(&e, &c, 0, 365 * DAY_MS), "idle time alone (user away) never");
        assert!(idle_dormant(&e, &counters(2, 0, 0), n, WINDOW_MS), "matched 2");
        assert!(!idle_dormant(&e, &counters(3, 0, 0), n, WINDOW_MS), "matched 3");
        // the clock runs from the current status
        e.status_since = Some(DAY_MS);
        assert!(!idle_dormant(&e, &c, n, WINDOW_MS));
        assert!(idle_dormant(&e, &c, n, WINDOW_MS + DAY_MS));

        let mut pinned = e.clone();
        pinned.pinned = true;
        assert!(!idle_dormant(&pinned, &c, n, WINDOW_MS + DAY_MS), "pinned exempt");
        let mut builtin = e.clone();
        builtin.origin = crate::types::Origin::Import { source: crate::library::BUILTIN_SOURCE.into() };
        assert!(!idle_dormant(&builtin, &c, n, WINDOW_MS + DAY_MS), "built-in exempt");
        let mut lesson = e.clone();
        lesson.kind = Kind::Lesson;
        assert!(!idle_dormant(&lesson, &c, n, WINDOW_MS + DAY_MS), "lesson exempt");
        for st in [Status::Kept, Status::Pinned, Status::Dormant, Status::Draft] {
            let mut o = e.clone();
            o.status = st;
            assert!(!idle_dormant(&o, &c, n, WINDOW_MS + DAY_MS), "{st:?}");
        }
    }

    #[test]
    fn idle_scope_is_counted_sessions_of_the_branch_since_the_status() {
        let mut e = entry("e", "n", "b", Status::Provisional);
        e.status_since = Some(100);
        let mut s = session("s", "b", 100);
        s.counted = true;
        assert!(in_idle_scope(&e, &s));
        s.started_at = 99;
        assert!(!in_idle_scope(&e, &s), "started before the status");
        s.started_at = 200;
        s.counted = false;
        assert!(!in_idle_scope(&e, &s), "not counted yet");
        s.counted = true;
        s.branch = "other".into();
        assert!(!in_idle_scope(&e, &s), "another branch");
        e.branch = crate::library::SHARED_BRANCH.into();
        assert!(in_idle_scope(&e, &s), "shared spans every branch");
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

        // session 1: hit and read, a negation (→ correction) and an error cluster
        let mut s1 = session("s1", "repo", 1_000);
        s1.cwd = "/w/repo/crates".into();
        s1.repo = None;
        s1.exts = vec!["rs".into()];
        s1.stems = vec!["ledger".into(), "migrat".into(), "x".into()];
        s1.loaded_entries = vec!["e1".into(), "e2".into()];
        s1.skill_reads = vec!["ledger-skill".into()];
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

        // session 5: matched with an error cluster and a takeover, but never used: no harm
        let mut s5 = session("s5", "repo", 4_000);
        s5.exts = vec!["rs".into()];
        s5.stems = vec!["ledger".into(), "migrat".into()];
        s5.loaded_entries = vec!["e1".into()];
        s5.error_clusters = 2;
        s5.takeovers = 1;
        let rows = fold_session(&l, &s5, &[e.clone()], &[]).unwrap();
        assert_eq!((rows[0].matched, rows[0].harm), (1, 0), "a session's errors are not blamed on an entry it never used");

        let c = l.counters("e1").unwrap();
        assert_eq!((c.matched, c.recurrence, c.harm), (3, 1, 2));
        assert_eq!(decide(&l.get_entry("e1").unwrap().unwrap(), &c, None, 3_000), Decision::Retire);
    }

    #[test]
    fn four_matched_unread_sessions_stay_provisional() {
        let (_t, l) = ledger();
        let e = entry("e1", "n", "repo", Status::Provisional); // exts [rs], no stems
        l.upsert_entry(&e).unwrap();
        for i in 0..4 {
            let mut s = session(&format!("s{i}"), "repo", i * 10);
            s.exts = vec!["rs".into()];
            s.loaded_entries = vec!["e1".into()];
            s.visible_entries = vec!["e1".into()]; // listed in the catalog, never read
            fold_session(&l, &s, std::slice::from_ref(&e), &[]).unwrap();
        }
        let c = l.counters("e1").unwrap();
        assert_eq!((c.matched, c.used, c.used_clean), (4, 0, 0));
        assert_eq!(decide(&e, &c, None, 100), Decision::Continue, "catalog-only matches keep nothing");
        assert_eq!(l.get_entry("e1").unwrap().unwrap().helpful, 0, "no use, no helpful credit");
    }

    #[test]
    fn four_clean_relevant_uses_keep_and_irrelevant_sessions_do_not_advance() {
        let (_t, l) = ledger();
        let e = entry("e1", "n", "repo", Status::Provisional); // exts [rs], no stems
        l.upsert_entry(&e).unwrap();
        let s_at = |i: i64, ext: &str| {
            let mut s = session(&format!("s{i}"), "repo", i * 10);
            s.exts = vec![ext.into()];
            s.loaded_entries = vec!["e1".into()];
            s.skill_reads = vec!["n".into()];
            s.turns = 2;
            s.run_completed = Some(true);
            s
        };
        // A read in a session the predicate misses (python work) is no relevant use.
        for i in 0..4 {
            assert!(fold_session(&l, &s_at(100 + i, "py"), std::slice::from_ref(&e), &[]).unwrap().is_empty());
        }
        for i in 0..3 {
            fold_session(&l, &s_at(i, "rs"), std::slice::from_ref(&e), &[]).unwrap();
        }
        let c = l.counters("e1").unwrap();
        assert_eq!(decide(&e, &c, None, 100), Decision::Continue, "no decision at 3 clean uses");
        let mut s = s_at(9, "rs");
        // an interrupt prelabel (no class) is not a recurrence
        s.redirects = vec![red("interrupt", None)];
        fold_session(&l, &s, std::slice::from_ref(&e), &[]).unwrap();
        let c = l.counters("e1").unwrap();
        assert_eq!((c.matched, c.used, c.used_clean, c.recurrence, c.harm), (4, 4, 4, 0, 0));
        assert_eq!(decide(&e, &c, None, 100), Decision::Keep);
        assert_eq!(keep_reason(&e, &c, None), Some(RetentionReason::AcceptedInUse));
        assert_eq!(l.get_entry("e1").unwrap().unwrap().helpful, 4);
    }

    #[test]
    fn changed_content_and_other_entries_inherit_no_use() {
        let (_t, l) = ledger();
        let mut e = entry("e1", "n", "repo", Status::Provisional);
        e.content_sha256 = Some("new".into());
        l.upsert_entry(&e).unwrap();
        let mut s = session("s1", "repo", 10);
        s.exts = vec!["rs".into()];
        s.loaded_entries = vec!["e1".into()];
        s.loaded_shas.insert("e1".into(), "old".into());
        s.skill_reads = vec!["n".into()];
        s.turns = 2;
        s.run_completed = Some(true);
        assert!(fold_session(&l, &s, std::slice::from_ref(&e), &[]).unwrap().is_empty(), "loaded at other content");
        // A sibling with the same name is credited only through its own resolved id.
        s.id = "s2".into();
        s.loaded_shas.clear();
        s.skill_entries = vec!["e-sibling".into()];
        let rows = fold_session(&l, &s, std::slice::from_ref(&e), &[]).unwrap();
        assert_eq!(rows[0].used, Some(0));
    }

    #[test]
    fn exposure_needs_completed_work() {
        let (_t, l) = ledger();
        let mut e = entry("r1", "personal-rule", "repo", Status::Kept);
        e.approved_fragment = Some("f1".into());
        l.upsert_entry(&e).unwrap();
        let mut s = session("s1", "repo", 10);
        s.exts = vec!["rs".into()];
        s.loaded_entries = vec!["r1".into()];
        s.instructions_exposed = vec!["r1".into()];
        s.instruction_shas.insert("r1".into(), "f1".into());
        // startup + shutdown, no model turn: exposure alone is no use
        assert_eq!(fold_session(&l, &s, std::slice::from_ref(&e), &[]).unwrap()[0].used, Some(0));
        s.id = "s2".into();
        s.turns = 3;
        s.last_stop_reason = Some("aborted".into());
        s.run_completed = Some(false);
        assert_eq!(fold_session(&l, &s, std::slice::from_ref(&e), &[]).unwrap()[0].used, Some(0), "aborted run");
        s.id = "s3".into();
        s.last_stop_reason = Some("stop".into());
        s.run_completed = None;
        assert_eq!(fold_session(&l, &s, std::slice::from_ref(&e), &[]).unwrap()[0].used, None, "no run-end evidence: unknown");
        s.id = "s5".into();
        s.run_completed = Some(true);
        assert_eq!(fold_session(&l, &s, std::slice::from_ref(&e), &[]).unwrap()[0].used, Some(1));
        s.id = "s4".into();
        s.instruction_shas.insert("r1".into(), "older".into());
        assert_eq!(fold_session(&l, &s, std::slice::from_ref(&e), &[]).unwrap()[0].used, Some(0), "not the approved text");
        let c = l.counters("r1").unwrap();
        assert_eq!((c.used, c.used_clean, c.use_unknown), (1, 1, 1));
    }

    #[test]
    fn interrupted_mid_tool_runs_are_not_completed() {
        use crate::trace::{fold_trace, parse_trace};
        let start = r#"{"seq":0,"t":1,"ev":"session_start","session_id":"S","cwd":"/w/repo","repo":"/w/repo"}
{"seq":1,"t":2,"ev":"input","text":"do it","prelabel":"none"}
{"seq":2,"t":3,"ev":"tool_result","tool":"read","call_id":"c","input":{"path":"skill://n"},"is_error":false,"skill_read":"n"}"#;
        // A turn that stopped on a tool call, then shutdown with no agent_end and no error text.
        let crashed = format!(
            "{start}\n{}\n{}",
            r#"{"seq":3,"t":4,"ev":"turn_end","stop_reason":"toolUse","text_head":""}"#,
            r#"{"seq":4,"t":5,"ev":"session_shutdown","reasons":[],"flagged":false}"#
        );
        let s = fold_trace(&parse_trace(&crashed), "S");
        assert_eq!(completed_work(&s), None, "no run-end evidence");
        // The same with an unsettled agent_end: not completed.
        let unsettled = format!(
            "{start}\n{}\n{}\n{}",
            r#"{"seq":3,"t":4,"ev":"turn_end","stop_reason":"toolUse","text_head":""}"#,
            r#"{"seq":4,"t":5,"ev":"agent_end","turns":1,"is_terminal":false,"yielded":false}"#,
            r#"{"seq":5,"t":6,"ev":"session_shutdown","reasons":[],"flagged":false}"#
        );
        assert_eq!(completed_work(&fold_trace(&parse_trace(&unsettled), "S")), Some(false));
        // A settled run whose last turn stopped normally.
        let done = format!(
            "{start}\n{}\n{}\n{}",
            r#"{"seq":3,"t":4,"ev":"turn_end","stop_reason":"stop","text_head":"done"}"#,
            r#"{"seq":4,"t":5,"ev":"agent_end","turns":1,"is_terminal":true,"yielded":true}"#,
            r#"{"seq":5,"t":6,"ev":"session_shutdown","reasons":[],"flagged":false}"#
        );
        assert_eq!(completed_work(&fold_trace(&parse_trace(&done), "S")), Some(true));
        // A turn after the settled run (then a crash) undoes it.
        let after = format!("{done}\n{}", r#"{"seq":6,"t":7,"ev":"turn_end","stop_reason":"toolUse","text_head":""}"#);
        assert_eq!(completed_work(&fold_trace(&parse_trace(&after), "S")), Some(false));
        // An adapter closing an open run at shutdown: terminal but not yielded.
        let closed = format!(
            "{start}\n{}\n{}",
            r#"{"seq":3,"t":4,"ev":"turn_end","stop_reason":"stop","text_head":"half"}"#,
            r#"{"seq":4,"t":5,"ev":"agent_end","turns":1,"is_terminal":true,"yielded":false}"#
        );
        assert_eq!(completed_work(&fold_trace(&parse_trace(&closed), "S")), Some(false));
        // A new input after a settled run, then shutdown: the new run never settled.
        let reprompt = format!("{done}\n{}", r#"{"seq":6,"t":7,"ev":"input","text":"and also","prelabel":"none"}"#);
        assert_eq!(completed_work(&fold_trace(&parse_trace(&reprompt), "S")), Some(false));
        // An importer's synthesized agent_end proves nothing: unknown.
        let imported = done.replacen(r#""repo":"/w/repo"}"#, r#""repo":"/w/repo","mode":"import"}"#, 1);
        assert_eq!(completed_work(&fold_trace(&parse_trace(&imported), "S")), None);
    }

    #[test]
    fn scoped_sessions_credit_resolved_ids_or_listed_invocations_only() {
        let e = entry("e1", "pdf-tools", "repo", Status::Provisional);
        let mut s = session("s", "repo", 0);
        s.exposure_view = Some("view-1".into());
        s.skill_reads = vec!["pdf-tools".into()]; // may be a native skill of the same name
        assert_eq!(actual_use(&e, &s), None);
        s.skills_invoked = vec!["pdf-tools".into()];
        assert_eq!(actual_use(&e, &s), None, "invoked, but this entry was not in the catalog");
        s.visible_entries = vec!["e1".into()];
        assert_eq!(actual_use(&e, &s), Some(UseKind::SkillRead));
        let mut t = session("t", "repo", 0);
        t.exposure_view = Some("view-1".into());
        t.skill_entries = vec!["e1".into()];
        assert_eq!(actual_use(&e, &t), Some(UseKind::SkillRead), "resolved id");
        let mut legacy = session("l", "repo", 0);
        legacy.skill_reads = vec!["pdf-tools".into()];
        assert_eq!(actual_use(&e, &legacy), Some(UseKind::SkillRead), "old trace: name crediting");
    }

    #[test]
    fn actual_use_by_kind() {
        let mut s = session("s", "repo", 0);
        let mut hook = entry("h", "guard", "repo", Status::Provisional);
        hook.kind = Kind::Hook;
        s.hooks_loaded = vec!["h".into()];
        assert_eq!(actual_use(&hook, &s), None, "loaded alone is no use");
        s.hooks_evaluated = vec!["h".into()];
        assert_eq!(actual_use(&hook, &s), Some(UseKind::HookEvaluated));
        s.hooks_applied = vec!["h".into()];
        assert_eq!(actual_use(&hook, &s), Some(UseKind::HookApplied));
        let mut agent = entry("a", "reviewer", "repo", Status::Provisional);
        agent.kind = Kind::Agent;
        assert_eq!(actual_use(&agent, &s), None);
        s.agents_invoked = vec!["reviewer".into()];
        assert_eq!(actual_use(&agent, &s), Some(UseKind::AgentInvoked));
        let mut script = entry("x", "fmt-check", "repo", Status::Provisional);
        script.kind = Kind::Script;
        s.skill_reads = vec!["fmt-check".into()];
        assert_eq!(actual_use(&script, &s), None, "reading a script's router is no execution");
        s.commands = vec!["sh ~/.omp/agent/skills/fmt-check/scripts/check.sh".into()];
        assert_eq!(actual_use(&script, &s), Some(UseKind::ScriptRun));
        let rule = entry("r", "personal-rule", "repo", Status::Kept);
        assert_eq!(actual_use(&rule, &s), None);
        s.instructions_exposed = vec!["r".into()];
        assert_eq!(actual_use(&rule, &s), Some(UseKind::InstructionExposed));
        let pkg = entry("p", "pkg", "repo", Status::Provisional);
        s.retrieved_entries = vec!["p".into()];
        assert_eq!(actual_use(&pkg, &s), Some(UseKind::Retrieved));
    }

    #[test]
    fn web_entry_is_not_counted_in_a_mobile_session_with_identical_stems() {
        let (_t, l) = ledger();
        let mut e = entry("e-web", "review-checklist", "repo", Status::Provisional);
        e.predicate.file_exts = vec![];
        e.predicate.stems = vec!["review".into(), "checklist".into()];
        e.predicate.domain = Some("web".into());
        l.upsert_entry(&e).unwrap();
        let at = |id: &str, domain: Option<&str>| {
            let mut s = session(id, "repo", 1_000);
            s.stems = vec!["review".into(), "checklist".into()];
            s.loaded_entries = vec!["e-web".into()];
            s.domain = domain.map(str::to_string);
            s
        };
        assert!(fold_session(&l, &at("s-mobile", Some("mobile")), std::slice::from_ref(&e), &[]).unwrap().is_empty());
        assert_eq!(fold_session(&l, &at("s-web", Some("web.frontend")), std::slice::from_ref(&e), &[]).unwrap().len(), 1);
        assert_eq!(fold_session(&l, &at("s-none", None), std::slice::from_ref(&e), &[]).unwrap().len(), 1, "undecided: not tested");
        assert_eq!(l.counters("e-web").unwrap().matched, 2);
    }

    #[test]
    fn block_hook_retires_after_two_harmful_fires_and_credits_clean_completion_as_use() {
        let (_t, l) = ledger();
        let mut e = entry("h-block", "no-force-push", "repo", Status::Provisional);
        e.kind = Kind::Hook;
        e.hook = Some(crate::types::HookRule::block("bash", "command", "git push --force", "use --force-with-lease", "safe push passes"));
        e.predicate.file_exts = vec![];
        e.predicate.stems = vec![];
        l.upsert_entry(&e).unwrap();

        // Clean block: hook fires, agent completes without a redirect -> clean use, 0 harm.
        let mut clean = session("s-clean", "repo", 1_000);
        clean.loaded_entries = vec!["h-block".into()];
        clean.hooks_applied = vec!["h-block".into()];
        clean.hook_blocked_at.insert("h-block".into(), 2);
        clean.turns = 2;
        clean.run_completed = Some(true);
        fold_session(&l, &clean, std::slice::from_ref(&e), &[]).unwrap();
        let c = l.counters("h-block").unwrap();
        assert_eq!((c.matched, c.used, c.used_clean, c.harm), (1, 1, 1, 0));

        // Harmful fire 1: block fires at seq 2, followed by a user correction redirect at seq 4.
        let mut h1 = session("s-harm-1", "repo", 2_000);
        h1.loaded_entries = vec!["h-block".into()];
        h1.hooks_applied = vec!["h-block".into()];
        h1.hook_blocked_at.insert("h-block".into(), 2);
        h1.redirects = vec![Redirect {
            session_id: "s-harm-1".into(),
            seq: 4,
            prelabel: "negation".into(),
            class: Some(RedirectClass::Correction),
            confidence: Some(1.0),
            text_head: "No, I needed that force push on this scratch branch".into(),
            deliverable_head: String::new(),
            t: 2_005,
        }];
        fold_session(&l, &h1, std::slice::from_ref(&e), &[]).unwrap();
        let c = l.counters("h-block").unwrap();
        assert_eq!(c.harm, 1);
        assert_eq!(decide(&e, &c, None, 2_010), Decision::Continue);

        // Harmful fire 2: block fires, followed by user disable -> 2nd harm -> Retire.
        let mut h2 = session("s-harm-2", "repo", 3_000);
        h2.loaded_entries = vec!["h-block".into()];
        h2.hooks_applied = vec!["h-block".into()];
        h2.hook_blocked_at.insert("h-block".into(), 2);
        fold_session(&l, &h2, std::slice::from_ref(&e), &["h-block".to_string()]).unwrap();
        let c = l.counters("h-block").unwrap();
        assert_eq!(c.harm, 2);
        assert_eq!(decide(&e, &c, None, 3_010), Decision::Retire);
    }
}
