//! Admission rate limits and caps (DESIGN §8; SPEC "Caps and rates").

use crate::ledger::{EntryFilter, Ledger};
use crate::types::{Status, UnixMs};

pub const MAX_ADMISSIONS_PER_DAY: u32 = 1;
pub const MAX_PROVISIONAL_PER_BRANCH: usize = 10;
pub const MAX_KEPT_GENERAL: usize = 5;
pub const MAX_KEPT_PER_BRANCH: usize = 5;
/// Branch holding entries kept in two or more branches ("general" kept cap).
pub const SHARED_BRANCH: &str = "shared";

#[derive(Debug, thiserror::Error)]
pub enum RateLimit {
    /// A provisional entry with the same predicate is still a canary.
    #[error("pending canary {entry_id} on this trigger predicate")]
    PendingCanary { entry_id: String },
    #[error("already admitted on this trigger predicate today ({day})")]
    DailyPredicate { day: String },
    #[error("branch {branch} already has {count} provisional entries (max {MAX_PROVISIONAL_PER_BRANCH})")]
    BranchProvisionalCap { branch: String, count: usize },
    /// Kept cap reached: the candidate must merge into or revise an existing entry.
    #[error("kept cap reached (general {general}/{MAX_KEPT_GENERAL}, branch {branch} {branch_kept}/{MAX_KEPT_PER_BRANCH}); needs_merge")]
    NeedsMerge { branch: String, general: usize, branch_kept: usize },
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}

impl RateLimit {
    /// The candidate is routed to merge/revise instead of being dropped.
    pub fn needs_merge(&self) -> bool {
        matches!(self, RateLimit::NeedsMerge { .. })
    }
}

impl From<rusqlite::Error> for RateLimit {
    fn from(e: rusqlite::Error) -> Self {
        RateLimit::Storage(e.into())
    }
}

/// UTC day key for admissions.
pub fn utc_day(now: UnixMs) -> String {
    chrono::DateTime::from_timestamp_millis(now)
        .unwrap_or_default()
        .format("%Y-%m-%d")
        .to_string()
}

fn count(ledger: &Ledger, status: Status, branch: &str) -> Result<usize, RateLimit> {
    Ok(ledger
        .list_entries(&EntryFilter { status: Some(status), branch: Some(branch.into()), ..Default::default() })?
        .len())
}

/// Checks, in order: pending canary on the predicate; ≤ 1 admission per predicate per
/// UTC day; ≤ 10 provisional in the branch; kept caps (5 general in `shared` + 5 per
/// branch) ⇒ `NeedsMerge`.
pub fn can_admit(ledger: &Ledger, predicate_hash: &str, branch: &str, now: UnixMs) -> Result<(), RateLimit> {
    let pending = ledger.list_entries(&EntryFilter {
        status: Some(Status::Provisional),
        predicate_hash: Some(predicate_hash.into()),
        ..Default::default()
    })?;
    if let Some(e) = pending.first() {
        return Err(RateLimit::PendingCanary { entry_id: e.id.clone() });
    }
    let day = utc_day(now);
    if ledger.admissions_on(predicate_hash, &day)? >= MAX_ADMISSIONS_PER_DAY {
        return Err(RateLimit::DailyPredicate { day });
    }
    let prov = count(ledger, Status::Provisional, branch)?;
    if prov >= MAX_PROVISIONAL_PER_BRANCH {
        return Err(RateLimit::BranchProvisionalCap { branch: branch.into(), count: prov });
    }
    let general = count(ledger, Status::Kept, SHARED_BRANCH)?;
    let branch_kept = if branch == SHARED_BRANCH { general } else { count(ledger, Status::Kept, branch)? };
    let full = if branch == SHARED_BRANCH {
        general >= MAX_KEPT_GENERAL
    } else {
        branch_kept >= MAX_KEPT_PER_BRANCH
    };
    if full {
        return Err(RateLimit::NeedsMerge { branch: branch.into(), general, branch_kept });
    }
    Ok(())
}

/// Record an admission for the day of `now`.
pub fn record_admission(ledger: &Ledger, predicate_hash: &str, entry_id: &str, now: UnixMs) -> anyhow::Result<()> {
    ledger.record_admission(predicate_hash, &utc_day(now), entry_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::tests::{entry, ledger};
    use crate::predicate::predicate_hash;

    const DAY: UnixMs = 86_400_000;
    // 2026-10-25T00:00:00Z
    const T0: UnixMs = 1_792_886_400_000;

    #[test]
    fn utc_day_boundaries() {
        assert_eq!(utc_day(T0), "2026-10-25");
        assert_eq!(utc_day(T0 - 1), "2026-10-24");
        assert_eq!(utc_day(T0 + DAY - 1), "2026-10-25");
    }

    #[test]
    fn daily_admission_per_predicate() {
        let (_t, l) = ledger();
        assert!(can_admit(&l, "h", "repo", T0).is_ok());
        record_admission(&l, "h", "e1", T0 + 5).unwrap();
        assert!(matches!(can_admit(&l, "h", "repo", T0 + DAY - 1), Err(RateLimit::DailyPredicate { .. })));
        assert!(can_admit(&l, "other", "repo", T0).is_ok(), "per predicate");
        assert!(can_admit(&l, "h", "repo", T0 + DAY).is_ok(), "next UTC day");
    }

    #[test]
    fn pending_canary_blocks_same_predicate() {
        let (_t, l) = ledger();
        let e = entry("e1", "n", "repo", Status::Provisional);
        l.upsert_entry(&e).unwrap();
        let h = predicate_hash(&e.predicate);
        match can_admit(&l, &h, "repo", T0) {
            Err(RateLimit::PendingCanary { entry_id }) => assert_eq!(entry_id, "e1"),
            other => panic!("{other:?}"),
        }
        let mut kept = e.clone();
        kept.status = Status::Kept;
        l.upsert_entry(&kept).unwrap();
        assert!(can_admit(&l, &h, "repo", T0).is_ok(), "decided canary no longer blocks");
    }

    #[test]
    fn provisional_cap_per_branch() {
        let (_t, l) = ledger();
        for i in 0..9 {
            let mut e = entry(&format!("e{i}"), "n", "repo", Status::Provisional);
            e.predicate.stems = vec![format!("s{i}")];
            l.upsert_entry(&e).unwrap();
        }
        assert!(can_admit(&l, "new", "repo", T0).is_ok(), "9 < 10");
        let mut e = entry("e9", "n", "repo", Status::Provisional);
        e.predicate.stems = vec!["s9".into()];
        l.upsert_entry(&e).unwrap();
        match can_admit(&l, "new", "repo", T0) {
            Err(RateLimit::BranchProvisionalCap { count, .. }) => assert_eq!(count, 10),
            other => panic!("{other:?}"),
        }
        assert!(can_admit(&l, "new", "elsewhere", T0).is_ok());
    }

    #[test]
    fn kept_caps_signal_needs_merge() {
        let (_t, l) = ledger();
        for i in 0..5 {
            l.upsert_entry(&entry(&format!("k{i}"), "n", "repo", Status::Kept)).unwrap();
        }
        for i in 0..4 {
            l.upsert_entry(&entry(&format!("g{i}"), "n", SHARED_BRANCH, Status::Kept)).unwrap();
        }
        let err = can_admit(&l, "new", "repo", T0).unwrap_err();
        assert!(err.needs_merge());
        assert!(can_admit(&l, "new", "other", T0).is_ok(), "other branch has room");
        assert!(can_admit(&l, "new", SHARED_BRANCH, T0).is_ok(), "4 general < 5");
        l.upsert_entry(&entry("g4", "n", SHARED_BRANCH, Status::Kept)).unwrap();
        match can_admit(&l, "new", SHARED_BRANCH, T0) {
            Err(RateLimit::NeedsMerge { general, .. }) => assert_eq!(general, 5),
            other => panic!("{other:?}"),
        }
    }
}
