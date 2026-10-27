//! Predicate tuning (DESIGN §6 "Predicate tuning"): a trigger predicate learns from which
//! sessions read its entry, under the same counters, with no model call.
//!
//! Signals per entry over a window (default 6w), from the lake when it holds every journal
//! row this ledger wrote, else from the ledger's journal (the `sessions` rows `report` reads):
//! * **read-but-missed** — the session read the entry (`skill_reads` holds its name, or a
//!   `retrieval` record credited one of its units) while it was visible (`visible_entries`,
//!   loaded, or a native skill dir the user registered), at its place, in a compatible domain,
//!   and the predicate did not hit (`count_match` false): the model opened it, the stems missed;
//! * **matched-but-unread** — the predicate hit and the entry was loaded, but nothing read it.
//!
//! Reads in another domain (`cross_domain_reads`) or at a place the predicate does not cover
//! (`cross_place_reads`) are listed and never used: a tune never widens a domain or a place.
//!
//! Proposer: read-but-missed ≥ 3 ⇒ stems = current ∪ (stems present in ≥ 2 of those sessions,
//! one domain only), ranked by tf-idf against the window's sessions, ≤ 10; `file_exts` = current
//! ∪ observed, ≤ 8 (an empty list stays empty: it already matches every extension). Else
//! matched-but-unread ≥ 4 ⇒ drop the stems found in the unread sessions and in none of the read
//! ones, keeping ≥ 2. An entry without a domain whose ≥ 3 read sessions all carry one domain gets
//! it (a narrowing). No change ⇒ no proposal.
//!
//! A proposal is staged as a provisional sibling (same files, `version + 1`, `sibling_of`,
//! origin `Import{source: "predicate-tune"}`, the evidence in `Entry.tune` and the proposal row)
//! — never an in-place edit — under the guards of [`check`]. The sibling is a new canary: the
//! counters keep or retire it, and its keep replaces a kept parent through the anchor path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Result;
use clap::Args;
use rematch_core::counters::DAY_MS;
use rematch_core::domain::domain_matches;
use rematch_core::features::rank_stems;
use rematch_core::ledger::{EntryFilter, JournalRow};
use rematch_core::library::{self, dir_for, is_builtin, REMATCH_JSON};
use rematch_core::lint::{branch_always_on_bytes, LintContext};
use rematch_core::predicate::{count_hit, count_match, matches_place, predicate_hash, Place, ANY_REPO, MAX_EXTS, MAX_STEMS};
use rematch_core::rate;
use rematch_core::trace::TraceRecord;
use rematch_core::types::{Edit, Entry, Kind, Origin, Predicate, PredicateTune, SessionRecord, Status, TuneEvidence};
use serde::Serialize;
use serde_json::{json, Value};

use crate::app::{new_id, AppCtx};
use crate::lake_read::{self, as_of, Lookup, Source};
use crate::report::{decode, parse_window, utc};
use crate::scope::{lint_edits, lint_errors, package_edits, rename_skill, resolve_entry, save_entry};

/// `Origin::Import { source }` of a tune sibling.
pub const TUNE_SOURCE: &str = "predicate-tune";
pub const DEFAULT_WINDOW: &str = "6w";
/// At most one tune per entry per this interval (`Entry.last_tune_at`).
pub const TUNE_INTERVAL_MS: i64 = 7 * DAY_MS;
/// Read-but-missed sessions that make a widening.
pub const WIDEN_MIN_READ_MISSED: usize = 3;
/// A stem joins only when this many of those sessions carry it.
pub const WIDEN_MIN_SHARED: usize = 2;
/// Matched-but-unread sessions that make a narrowing (with fewer read-but-missed than
/// [`WIDEN_MIN_READ_MISSED`]).
pub const NARROW_MIN_MATCHED_UNREAD: usize = 4;
/// Stems a narrowing always leaves.
pub const NARROW_KEEP_STEMS: usize = 2;
/// Read sessions sharing one domain that set the domain of an entry without one.
pub const DOMAIN_MIN_READS: usize = 3;

#[derive(Args, Debug, Clone)]
pub struct TuneArgs {
    /// Only this entry (id or name).
    #[arg(long)]
    pub entry: Option<String>,
    /// Report signals and proposals (and whether each would stage); write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Window ending now: `<n>w`, `<n>d` or `<n>h`.
    #[arg(long, default_value = DEFAULT_WINDOW)]
    pub window: String,
}

/// What the tuner keeps of a session.
#[derive(Debug, Clone)]
struct Seen {
    stems: Vec<String>,
    exts: Vec<String>,
    domain: Option<String>,
}

/// The sessions of one window, read once for every entry a pass looks at.
pub struct Window {
    pub source: Source,
    pub start: i64,
    pub end: i64,
    sessions: Vec<(String, SessionRecord)>,
    corpus: Rc<Vec<Vec<String>>>,
}

impl Window {
    /// Sessions started in `[now − window_ms, now]`, each its latest row (including the rows
    /// this process wrote, e.g. the sessions `count` just folded): from the lake when it is
    /// current, else this ledger's journal.
    pub fn load(ctx: &AppCtx, window_ms: i64) -> Result<Window> {
        // Quietly: inside `count` the lake always lags (the sessions it just folded).
        let current = lake_read::lake(ctx).is_some() && lake_read::lag(ctx).is_ok_and(|n| n == 0);
        let source = if current { Source::Lake } else { Source::Ledger };
        let (start, end) = (ctx.now - window_ms, ctx.now);
        let lookup = Lookup { key: "range:started_at", values: Vec::new(), window: Some((start, end)) };
        let rows: Vec<JournalRow> = as_of(lake_read::rows(ctx, source, "sessions", Some(&lookup), None)?, i64::MAX);
        let mut sessions: Vec<(String, SessionRecord)> =
            decode::<SessionRecord>(rows).into_iter().filter(|(_, s)| (start..=end).contains(&s.started_at)).collect();
        sessions.sort_by(|a, b| (a.1.started_at, &a.1.id).cmp(&(b.1.started_at, &b.1.id)));
        let corpus = Rc::new(sessions.iter().map(|(_, s)| s.stems.clone()).collect());
        Ok(Window { source, start, end, sessions, corpus })
    }
}

/// Session ids per signal (DESIGN §6 "Predicate tuning").
#[derive(Debug, Clone, Serialize)]
pub struct TuneSignals {
    pub read_missed: Vec<String>,
    pub matched_unread: Vec<String>,
    /// Matched, loaded and read: the sessions a narrowing must keep.
    pub matched_read: Vec<String>,
    /// Reads whose session domain the entry's domain does not cover (informational).
    pub cross_domain_reads: Vec<String>,
    /// Reads at a place (repo, remote, subpath) the predicate does not cover (informational).
    pub cross_place_reads: Vec<String>,
    pub source: Source,
    pub window_ms: i64,
    #[serde(skip)]
    seen: BTreeMap<String, Seen>,
    #[serde(skip)]
    corpus: Rc<Vec<Vec<String>>>,
}

impl TuneSignals {
    fn any(&self) -> bool {
        !(self.read_missed.is_empty()
            && self.matched_unread.is_empty()
            && self.matched_read.is_empty()
            && self.cross_domain_reads.is_empty()
            && self.cross_place_reads.is_empty())
    }
}

/// A predicate change and the sessions behind it.
#[derive(Debug, Clone, Serialize)]
pub struct TuneProposal {
    pub predicate: Predicate,
    pub direction: String,
    /// Stems added / removed.
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub added_exts: Vec<String>,
    pub domain_set: Option<String>,
    pub evidence: TuneEvidence,
    /// Of the sessions the change was computed from, those the new predicate hits (a
    /// widening) or still hits (a narrowing).
    pub hits_after: usize,
}

impl TuneProposal {
    /// The sibling's `status_note`.
    pub fn note(&self) -> String {
        note(&self.added, &self.added_exts, self.domain_set.as_deref(), &self.removed, self.evidence.used.len())
    }
}

/// `predicate tune: +[a, b, .py, domain=quant.options] -[c] from N session(s)`.
pub fn note(added: &[String], added_exts: &[String], domain_set: Option<&str>, removed: &[String], sessions: usize) -> String {
    let mut plus: Vec<String> = added.to_vec();
    plus.extend(added_exts.iter().map(|e| format!(".{e}")));
    plus.extend(domain_set.map(|d| format!("domain={d}")));
    format!("predicate tune: +[{}] -[{}] from {sessions} session(s)", plus.join(", "), removed.join(", "))
}

/// [`note`] of a staged tune.
pub fn note_of(t: &PredicateTune) -> String {
    note(&t.added, &t.added_exts, t.domain_set.as_deref(), &t.removed, t.evidence.used.len())
}

/// A guard refused the tune: reported as `skipped`, never an error.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Skip(pub String);

fn skip<T>(reason: impl Into<String>) -> Result<T> {
    Err(Skip(reason.into()).into())
}

/// The signals of `entry` over `window` (`6w`, `30d`, …).
pub fn signals(ctx: &AppCtx, entry: &Entry, window: &str) -> Result<TuneSignals> {
    let w = Window::load(ctx, parse_window(window)?)?;
    signals_in(ctx, &w, entry)
}

/// Whether a `retrieval` record of the session credited a unit of `entry_id`.
fn retrieved(ctx: &AppCtx, s: &SessionRecord, entry_id: &str) -> bool {
    match crate::import_cmd::session_records(ctx, s) {
        Ok(Some((records, _))) => records.iter().any(|r| match r {
            TraceRecord::Retrieval { units, .. } => units.iter().any(|u| u.entry_id == entry_id),
            _ => false,
        }),
        _ => false,
    }
}

pub fn signals_in(ctx: &AppCtx, w: &Window, entry: &Entry) -> Result<TuneSignals> {
    let me = ctx.ledger.machine_id();
    let pred = &entry.predicate;
    // Only a package's units are retrieved; a user's skill registered in place loads natively
    // in every session, so a read of it was a read of something visible.
    let package = entry.path.join("units").is_dir() || entry.path.join("facts").is_dir();
    let native = crate::reflect::is_foreign(ctx, entry);
    let mut sig = TuneSignals {
        read_missed: Vec::new(),
        matched_unread: Vec::new(),
        matched_read: Vec::new(),
        cross_domain_reads: Vec::new(),
        cross_place_reads: Vec::new(),
        source: w.source,
        window_ms: w.end - w.start,
        seen: BTreeMap::new(),
        corpus: w.corpus.clone(),
    };
    for (m, s) in &w.sessions {
        let same = m == me;
        // Entry ids are machine-local: a session of another machine loaded it by content.
        let loaded = if same {
            s.loaded_entries.contains(&entry.id)
        } else {
            entry.content_sha256.as_ref().is_some_and(|h| s.loaded_entry_shas.contains(h))
        };
        let visible = loaded || native || (same && s.visible_entries.contains(&entry.id));
        let named = s.skill_reads.iter().any(|n| n == &entry.name);
        let read = named || (package && loaded && retrieved(ctx, s, &entry.id));
        let place = Place::of_session(s);
        let hit = count_match(pred, &place, &s.exts, &s.stems);
        let bucket = if hit && loaded {
            Some(if read { &mut sig.matched_read } else { &mut sig.matched_unread })
        } else if read && visible && !hit {
            Some(if !domain_matches(pred.domain.as_deref(), s.domain.as_deref()) {
                &mut sig.cross_domain_reads
            } else if !matches_place(pred, &Place { domain: None, ..place }) {
                &mut sig.cross_place_reads
            } else {
                &mut sig.read_missed
            })
        } else {
            None
        };
        if let Some(b) = bucket {
            b.push(s.id.clone());
            sig.seen.insert(s.id.clone(), Seen { stems: s.stems.clone(), exts: s.exts.clone(), domain: s.domain.clone() });
        }
    }
    Ok(sig)
}

/// Distinct items of each session, counted per session; ordered by count desc, then first
/// occurrence (sessions in start order).
fn counted<'a>(lists: impl Iterator<Item = &'a [String]>) -> Vec<(String, usize)> {
    let mut v: Vec<(String, usize)> = Vec::new();
    for list in lists {
        let mut seen = BTreeSet::new();
        for x in list.iter().filter(|x| seen.insert(*x)) {
            match v.iter_mut().find(|(y, _)| y == x) {
                Some(c) => c.1 += 1,
                None => v.push((x.clone(), 1)),
            }
        }
    }
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    v
}

/// The sessions `ids` names, with what the tuner kept of each.
fn view<'a>(sig: &'a TuneSignals, ids: &'a [String]) -> Vec<(&'a String, &'a Seen)> {
    ids.iter().filter_map(|id| sig.seen.get(id).map(|s| (id, s))).collect()
}

/// The proposer (no model): `None` when nothing would change.
pub fn propose(entry: &Entry, sig: &TuneSignals) -> Option<TuneProposal> {
    let old = &entry.predicate;
    let reads: Vec<(&String, &Seen)> = view(sig, &sig.matched_read).into_iter().chain(view(sig, &sig.read_missed)).collect();

    // Domain: set on an entry without one when ≥ 3 read sessions carry one domain and no read
    // session carries another. An existing domain never changes.
    let domain_set = match &old.domain {
        Some(_) => None,
        None => {
            let known: BTreeSet<&str> = reads.iter().filter_map(|(_, s)| s.domain.as_deref()).collect();
            let n = reads.iter().filter(|(_, s)| s.domain.is_some()).count();
            (known.len() == 1 && n >= DOMAIN_MIN_READS).then(|| known.into_iter().next().expect("one").to_string())
        }
    };
    let target = old.domain.clone().or_else(|| domain_set.clone());

    let mut stems = old.stems.clone();
    let mut exts = old.file_exts.clone();
    let (mut added, mut removed, mut added_exts) = (Vec::new(), Vec::new(), Vec::new());
    let mut used: Vec<String> = Vec::new();

    if sig.read_missed.len() >= WIDEN_MIN_READ_MISSED {
        // One domain only: the target's, else the single domain the sessions carry.
        let missed = view(sig, &sig.read_missed);
        let domains: BTreeSet<&str> = missed.iter().filter_map(|(_, s)| s.domain.as_deref()).collect();
        let group: Vec<(&String, &Seen)> = match &target {
            Some(d) => missed.into_iter().filter(|(_, s)| domain_matches(Some(d), s.domain.as_deref())).collect(),
            None if domains.len() > 1 => Vec::new(),
            None => missed,
        };
        if group.len() >= WIDEN_MIN_READ_MISSED {
            used = group.iter().map(|(id, _)| (*id).clone()).collect();
            let shared: BTreeSet<String> = counted(group.iter().map(|(_, s)| s.stems.as_slice()))
                .into_iter()
                .filter(|(x, n)| *n >= WIDEN_MIN_SHARED && !stems.contains(x))
                .map(|(x, _)| x)
                .collect();
            // tf = sessions carrying the stem; idf over the window's sessions.
            let doc: Vec<String> = group
                .iter()
                .flat_map(|(_, s)| s.stems.iter().collect::<BTreeSet<_>>().into_iter().filter(|x| shared.contains(*x)).cloned().collect::<Vec<_>>())
                .collect();
            for x in rank_stems(&doc, &sig.corpus) {
                if stems.len() >= MAX_STEMS {
                    break;
                }
                stems.push(x.clone());
                added.push(x);
            }
            // An empty list matches every extension: adding one would narrow it.
            if !exts.is_empty() {
                for (x, _) in counted(group.iter().map(|(_, s)| s.exts.as_slice())) {
                    if exts.len() >= MAX_EXTS {
                        break;
                    }
                    if !exts.contains(&x) {
                        exts.push(x.clone());
                        added_exts.push(x);
                    }
                }
            }
        }
    } else if sig.matched_unread.len() >= NARROW_MIN_MATCHED_UNREAD && stems.len() > NARROW_KEEP_STEMS {
        let unread = view(sig, &sig.matched_unread);
        let read_stems: BTreeSet<&String> = reads.iter().flat_map(|(_, s)| s.stems.iter()).collect();
        let in_unread: BTreeMap<String, usize> = counted(unread.iter().map(|(_, s)| s.stems.as_slice())).into_iter().collect();
        let mut drop: Vec<(usize, usize, String)> = stems
            .iter()
            .enumerate()
            .filter(|(_, x)| !read_stems.contains(x))
            .filter_map(|(i, x)| in_unread.get(x).map(|n| (*n, i, x.clone())))
            .collect();
        drop.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        drop.truncate(stems.len() - NARROW_KEEP_STEMS);
        if !drop.is_empty() {
            used = unread.iter().map(|(id, _)| (*id).clone()).collect();
            removed = stems.iter().filter(|x| drop.iter().any(|d| &d.2 == *x)).cloned().collect();
            stems.retain(|x| !removed.contains(x));
        }
    }
    let direction = if !added.is_empty() || !added_exts.is_empty() {
        "widen"
    } else if !removed.is_empty() {
        "narrow"
    } else {
        used = reads.iter().filter(|(_, s)| s.domain.is_some() && s.domain == domain_set).map(|(id, _)| (*id).clone()).collect();
        "domain"
    };
    let predicate = Predicate { stems, file_exts: exts, domain: old.domain.clone().or_else(|| domain_set.clone()), ..old.clone() };
    if &predicate == old {
        return None;
    }
    let hits_after = used
        .iter()
        .filter_map(|id| sig.seen.get(id))
        .filter(|s| domain_matches(predicate.domain.as_deref(), s.domain.as_deref()) && count_hit(&predicate, &s.exts, &s.stems))
        .count();
    Some(TuneProposal {
        predicate,
        direction: direction.into(),
        added,
        removed,
        added_exts,
        domain_set,
        evidence: TuneEvidence {
            read_missed: sig.read_missed.clone(),
            matched_unread: sig.matched_unread.clone(),
            used,
            source: sig.source.name().into(),
            window_ms: sig.window_ms,
        },
        hits_after,
    })
}

/// Entries a tune never touches: built-ins, hooks, pinned entries, anything but a live
/// skill (or rule) entry.
pub fn ineligible(e: &Entry) -> Option<String> {
    if is_builtin(e) {
        return Some("built-in entry".into());
    }
    if e.kind == Kind::Hook || e.hook.is_some() {
        return Some("hook entry".into());
    }
    if e.pinned || e.status == Status::Pinned {
        return Some("pinned (human-owned)".into());
    }
    if !matches!(e.kind, Kind::Skill | Kind::Rule) {
        return Some(format!("{} entry: only skill entries are tuned", rematch_core::ledger::enum_str(&e.kind)));
    }
    if !matches!(e.status, Status::Kept | Status::Provisional) {
        return Some(format!("{} entry: only provisional and kept entries are tuned", rematch_core::ledger::enum_str(&e.status)));
    }
    None
}

/// The predicate guards: same place, never `**`, the caps, never empty, ≥ 2 stems after a
/// narrowing, a domain only ever set.
pub fn guard_predicate(old: &Predicate, new: &Predicate) -> Result<(), String> {
    if (&old.repo_glob, &old.repo_remote, &old.subpath) != (&new.repo_glob, &new.repo_remote, &new.subpath) {
        return Err("a tune never changes the place (repo_glob, repo_remote, subpath)".into());
    }
    if (new.repo_glob.trim() == ANY_REPO && old.repo_glob.trim() != ANY_REPO)
        || new.stems.iter().chain(&new.file_exts).any(|x| x.contains('*'))
    {
        return Err("a tune never produces `**`".into());
    }
    if new.stems.len() > MAX_STEMS || new.file_exts.len() > MAX_EXTS {
        return Err(format!("{} stems / {} exts over the caps ({MAX_STEMS} / {MAX_EXTS})", new.stems.len(), new.file_exts.len()));
    }
    if (!old.stems.is_empty() && new.stems.is_empty()) || (!old.file_exts.is_empty() && new.file_exts.is_empty()) {
        return Err("a tune never empties the stems or the extensions".into());
    }
    if new.stems.len() < old.stems.len() && new.stems.len() < NARROW_KEEP_STEMS {
        return Err(format!("a narrowing keeps ≥ {NARROW_KEEP_STEMS} stems"));
    }
    if old.domain.is_some() && new.domain != old.domain {
        return Err("a tune never changes an existing domain".into());
    }
    Ok(())
}

/// The provisional tune sibling of `e` still waiting for its counters, if any.
pub fn pending_tune(ctx: &AppCtx, e: &Entry) -> Result<Option<Entry>> {
    Ok(ctx
        .ledger
        .list_entries(&EntryFilter { status: Some(Status::Provisional), name: Some(e.name.clone()), ..Default::default() })?
        .into_iter()
        .find(|s| s.sibling_of.as_deref() == Some(e.id.as_str()) && s.tune.is_some()))
}

/// A tune that passed every guard, ready to stage.
pub struct Prepared {
    pub sibling: Entry,
    pub files: Vec<Edit>,
}

/// Every guard, in order, without writing anything: eligibility; the predicate guards; one
/// tune per entry per 7 days; one pending tune per entry; the sibling's staging slot free;
/// the rate limits on the new predicate hash (a kept-cap `NeedsMerge` aside: a tune revises
/// an existing entry, which is what that signal asks for); lint on the sibling.
pub fn check(ctx: &AppCtx, e: &Entry, p: &TuneProposal) -> Result<Prepared> {
    if let Some(r) = ineligible(e) {
        return skip(r);
    }
    if let Err(r) = guard_predicate(&e.predicate, &p.predicate) {
        return skip(r);
    }
    if let Some(at) = e.last_tune_at.filter(|t| ctx.now - t < TUNE_INTERVAL_MS) {
        return skip(format!("tuned {}; the next tune is allowed after {}", utc(at), utc(at + TUNE_INTERVAL_MS)));
    }
    if let Some(s) = pending_tune(ctx, e)? {
        return skip(format!("predicate tune v{} pending ({})", s.version, s.id));
    }
    let mut sib = Entry {
        id: new_id(),
        status: Status::Provisional,
        verified_on: vec![],
        helpful: 0,
        harmful: 0,
        origin: Origin::Import { source: TUNE_SOURCE.into() },
        predicate: p.predicate.clone(),
        predecessor: None,
        version: e.version + 1,
        path: PathBuf::new(),
        created_at: 0,
        updated_at: 0,
        pinned: false,
        approved_scripts: vec![],
        approved_fragment: None,
        approved_agent: None,
        sibling_of: Some(e.id.clone()),
        status_since: None,
        status_note: Some(p.note()),
        pending_next_run: false,
        fork_commit: None,
        families_excluded: vec![],
        content_sha256: None,
        last_tune_at: None,
        tune: None,
        ..e.clone()
    };
    sib.tune = Some(PredicateTune {
        parent: e.id.clone(),
        parent_version: e.version,
        at: ctx.now,
        direction: p.direction.clone(),
        from: e.predicate.clone(),
        added: p.added.clone(),
        removed: p.removed.clone(),
        added_exts: p.added_exts.clone(),
        domain_set: p.domain_set.clone(),
        evidence: p.evidence.clone(),
    });
    let slot = dir_for(&ctx.paths, &sib, Status::Provisional);
    if slot.join(REMATCH_JSON).is_file() {
        let holder = library::load_entry(&slot).map(|h| h.id).unwrap_or_else(|_| "an unreadable entry".into());
        if holder == e.id {
            return skip(format!("the parent is provisional at {}: its own canary decides before a tune (one pending version per name)", slot.display()));
        }
        return skip(format!("{} holds {holder}: one pending version per name, the counters decide it first", slot.display()));
    }
    let hash = predicate_hash(&p.predicate);
    let kept_per_domain = ctx.domain_policy()?.kept_per_domain();
    match rate::can_admit(&ctx.ledger, &hash, &e.branch, p.predicate.domain.as_deref(), kept_per_domain, ctx.now) {
        Ok(()) => {}
        Err(r) if r.needs_merge() => {}
        Err(r) => return skip(format!("rate limit: {r}")),
    }
    let mut files = match package_edits(&e.path) {
        Ok(f) => f,
        Err(err) => return skip(format!("entry files: {err:#}")),
    };
    rename_skill(&mut files, &e.name)?;
    let branch_dir = ctx.paths.staging_branch(&e.branch);
    let lctx = LintContext { branch_other_bytes: branch_always_on_bytes(&branch_dir, &slot), ..Default::default() };
    let rep = lint_edits(ctx, &sib, &files, &lctx)?;
    if !rep.ok {
        return skip(format!("lint: {}", lint_errors(&rep)));
    }
    Ok(Prepared { sibling: sib, files })
}

/// Stage the tune of `entry` as its provisional sibling (the guards of [`check`] first): the
/// admission is recorded on the new predicate hash, the proposal row carries the evidence and
/// the parent records `last_tune_at`; nothing else of the parent changes.
pub fn apply(ctx: &AppCtx, entry: &Entry, p: &TuneProposal) -> Result<Entry> {
    let prep = check(ctx, entry, p)?;
    let staged = library::stage(&ctx.ledger, &ctx.paths, &prep.sibling, &prep.files)?;
    crate::scope::stale::snapshot_sources(&staged.path)?;
    rate::record_admission(&ctx.ledger, &predicate_hash(&staged.predicate), &staged.id, ctx.now)?;
    ctx.ledger.insert_proposal(
        Some(&staged.id),
        None,
        &json!({"decision": "PREDICATE_TUNE", "entry": staged, "tune": staged.tune, "from": entry.predicate, "to": staged.predicate}),
        true,
    )?;
    let mut parent = ctx.ledger.get_entry(&entry.id)?.unwrap_or_else(|| entry.clone());
    parent.last_tune_at = Some(ctx.now);
    save_entry(ctx, &parent)?;
    Ok(staged)
}

/// One entry through signals → proposal → guards (→ stage unless `dry_run`). Returns the
/// entry's JSON, its text line, and whether the line is worth printing in a listing (a read
/// signal or a proposal).
fn run_one(ctx: &AppCtx, w: &Window, e: &Entry, dry_run: bool) -> Result<(Value, String, bool)> {
    let sig = signals_in(ctx, w, e)?;
    let proposal = propose(e, &sig);
    let mut out = json!({
        "id": e.id, "name": e.name, "version": e.version, "status": e.status,
        "predicate_hash": predicate_hash(&e.predicate), "signals": sig, "proposal": proposal,
    });
    let counts = format!(
        "read-but-missed {} · matched-but-unread {} · matched-and-read {}{}{}",
        sig.read_missed.len(),
        sig.matched_unread.len(),
        sig.matched_read.len(),
        if sig.cross_domain_reads.is_empty() { String::new() } else { format!(" · cross-domain reads {}", sig.cross_domain_reads.len()) },
        if sig.cross_place_reads.is_empty() { String::new() } else { format!(" · other-place reads {}", sig.cross_place_reads.len()) },
    );
    let head = format!("{} v{}: {counts}", e.name, e.version);
    let notable = sig.any() || proposal.is_some();
    if let Some(reason) = ineligible(e) {
        out["outcome"] = json!("skipped");
        out["reason"] = json!(reason);
        let note = proposal.as_ref().map(|p| format!(" → {}", p.note())).unwrap_or_default();
        return Ok((out, format!("{head}{note} → skipped: {reason}"), notable));
    }
    let Some(p) = proposal else {
        out["outcome"] = json!("no_change");
        // Say why the signal did not become a proposal: the reader must be able to tell
        // "no evidence" from "evidence the guards rejected".
        let why = if sig.read_missed.len() >= WIDEN_MIN_READ_MISSED {
            "read-but-missed sessions share no task stem (each needs ≥ 2 sessions) or span several domains"
        } else if sig.matched_unread.len() >= NARROW_MIN_MATCHED_UNREAD {
            "matched-but-unread sessions share every stem with the read ones; nothing to drop"
        } else {
            "below the signal thresholds"
        };
        out["reason"] = json!(why);
        return Ok((out, format!("{head} → no change ({why})"), notable));
    };
    let result = if dry_run { check(ctx, e, &p).map(|prep| prep.sibling) } else { apply(ctx, e, &p) };
    let tail = match result {
        Ok(sib) => {
            out["outcome"] = json!(if dry_run { "would_stage" } else { "staged" });
            if dry_run {
                format!("would stage v{} (provisional, predicate {})", sib.version, &predicate_hash(&sib.predicate)[..12])
            } else {
                out["staged"] = json!({"id": sib.id, "version": sib.version, "path": sib.path,
                                       "predicate_hash": predicate_hash(&sib.predicate), "status_note": sib.status_note});
                format!("staged v{} {} (provisional, predicate {})", sib.version, sib.id, &predicate_hash(&sib.predicate)[..12])
            }
        }
        Err(err) => match err.downcast::<Skip>() {
            Ok(Skip(reason)) => {
                out["outcome"] = json!("skipped");
                out["reason"] = json!(reason);
                format!("skipped: {reason}")
            }
            Err(err) => return Err(err),
        },
    };
    Ok((out, format!("{head} → {} → {tail}", p.note()), true))
}

fn candidates(ctx: &AppCtx) -> Result<Vec<Entry>> {
    let mut v = Vec::new();
    for st in [Status::Kept, Status::Provisional, Status::Pinned] {
        v.extend(ctx.ledger.list_entries(&EntryFilter { status: Some(st), ..Default::default() })?);
    }
    v.sort_by(|a, b| (&a.name, a.version, &a.id).cmp(&(&b.name, b.version, &b.id)));
    Ok(v)
}

/// `rematch tune [--dry-run] [--entry <id|name>] [--window 6w]`.
pub fn cmd_tune(ctx: &AppCtx, a: TuneArgs) -> Result<Value> {
    let w = Window::load(ctx, parse_window(&a.window)?)?;
    let entries = match &a.entry {
        Some(r) => vec![resolve_entry(ctx, r)?],
        None => candidates(ctx)?,
    };
    let mut lines = vec![format!(
        "tune · source {} · window {} ({} → {}) · {} session(s){}",
        w.source.name(),
        a.window,
        utc(w.start),
        utc(w.end),
        w.sessions.len(),
        if a.dry_run { " · dry run" } else { "" }
    )];
    let mut out = Vec::new();
    for e in &entries {
        let (v, line, notable) = run_one(ctx, &w, e, a.dry_run)?;
        if notable || a.entry.is_some() {
            lines.push(line);
        }
        out.push(v);
    }
    if lines.len() == 1 {
        lines.push(format!("{} entr(ies): no read signal in the window", entries.len()));
    }
    Ok(json!({"source": w.source, "window": {"start": w.start, "end": w.end, "spec": a.window}, "dry_run": a.dry_run,
              "entries": out, "lines": lines}))
}

/// The tune pass inside `count` (after its decisions): every provisional/kept skill entry,
/// at most one tune each, guards as for `rematch tune`. Reported entries are those with a
/// proposal; a failing entry is reported, never fatal to counting.
pub fn pass(ctx: &AppCtx, lines: &mut Vec<String>) -> Result<Value> {
    let w = Window::load(ctx, parse_window(DEFAULT_WINDOW)?)?;
    let mut out = Vec::new();
    for e in candidates(ctx)?.iter().filter(|e| ineligible(e).is_none()) {
        match run_one(ctx, &w, e, false) {
            Ok((v, line, _)) => {
                if v["proposal"].is_null() {
                    continue;
                }
                lines.push(format!("tune: {line}"));
                out.push(v);
            }
            Err(err) => out.push(json!({"id": e.id, "name": e.name, "error": format!("{err:#}")})),
        }
    }
    Ok(json!({"source": w.source, "entries": out}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::testutil::{entry, env, keep, session, stage};
    use rematch_core::ledger::now_ms;

    fn sess(id: &str, days_ago: i64, stems: &[&str], domain: Option<&str>) -> SessionRecord {
        let mut s = session(id, "payments", Some("/w/payments"), "anthropic/claude-opus", now_ms() - days_ago * DAY_MS);
        s.stems = stems.iter().map(|x| x.to_string()).collect();
        s.domain = domain.map(str::to_string);
        s
    }

    /// Read while visible (the shared dir exposed it).
    fn read(mut s: SessionRecord, e: &Entry) -> SessionRecord {
        s.skill_reads = vec![e.name.clone()];
        s.visible_entries = vec![e.id.clone()];
        s
    }

    fn tune_sig(ctx: &AppCtx, e: &Entry) -> (TuneSignals, Option<TuneProposal>) {
        let w = Window::load(ctx, parse_window("6w").unwrap()).unwrap();
        let sig = signals_in(ctx, &w, e).unwrap();
        let p = propose(e, &sig);
        (sig, p)
    }

    fn kept(ctx: &AppCtx, id: &str, name: &str, domain: Option<&str>) -> Entry {
        let mut e = entry(id, name, "payments");
        e.predicate.stems = vec!["ledger".into(), "migrat".into(), "schema".into()];
        e.predicate.domain = domain.map(str::to_string);
        let e = stage(ctx, &e, &[]);
        keep(ctx, &e.id)
    }

    #[test]
    fn three_reads_the_stems_missed_widen_with_the_shared_stems() {
        let t = env();
        let ctx = &t.ctx;
        let e = kept(ctx, "e-w", "ledger-migrations", None);
        for (i, extra) in ["parser", "spread", "book"].iter().enumerate() {
            ctx.ledger.upsert_session(&read(sess(&format!("rm-{i}"), 3 + i as i64, &["orderbook", "recon", extra], None), &e)).unwrap();
        }
        // a reader in another repo: listed, never used
        let mut far = read(sess("far", 2, &["orderbook", "recon"], None), &e);
        far.repo = Some("/w/other".into());
        far.cwd = "/w/other".into();
        ctx.ledger.upsert_session(&far).unwrap();
        let mut e = e;
        e.predicate.repo_glob = "/w/payments".into();
        let (sig, p) = tune_sig(ctx, &e);
        assert_eq!(sig.read_missed, vec!["rm-2", "rm-1", "rm-0"]);
        assert_eq!(sig.cross_place_reads, vec!["far"]);
        let p = p.unwrap();
        assert_eq!(p.added, vec!["orderbook", "recon"], "only stems in ≥ 2 sessions: {p:?}");
        assert!(p.added_exts.is_empty() && p.removed.is_empty());
        assert_eq!(p.hits_after, 3);
        assert_eq!(p.note(), "predicate tune: +[orderbook, recon] -[] from 3 session(s)");
    }

    #[test]
    fn a_web_entry_read_in_three_mobile_sessions_is_not_widened() {
        let t = env();
        let ctx = &t.ctx;
        let e = kept(ctx, "e-web", "form-validation", Some("web.frontend"));
        for i in 0..3 {
            ctx.ledger.upsert_session(&read(sess(&format!("m-{i}"), 2 + i, &["swiftui", "form"], Some("mobile.ios")), &e)).unwrap();
        }
        let (sig, p) = tune_sig(ctx, &e);
        assert!(sig.read_missed.is_empty(), "{sig:?}");
        assert_eq!(sig.cross_domain_reads.len(), 3);
        assert!(p.is_none(), "{p:?}");
    }

    #[test]
    fn an_undomained_entry_read_in_three_quant_options_sessions_gets_the_domain() {
        let t = env();
        let ctx = &t.ctx;
        let e = kept(ctx, "e-q", "greeks-check", None);
        for i in 0..3 {
            ctx.ledger.upsert_session(&read(sess(&format!("q-{i}"), 2 + i, &["greek", "vega", "strike"], Some("quant.options")), &e)).unwrap();
        }
        let (_, p) = tune_sig(ctx, &e);
        let p = p.unwrap();
        assert_eq!(p.domain_set.as_deref(), Some("quant.options"));
        assert_eq!(p.predicate.domain.as_deref(), Some("quant.options"));
        assert_eq!(p.added, vec!["greek", "strike", "vega"], "equal tf-idf: first occurrence");
        assert!(p.note().contains("domain=quant.options"), "{}", p.note());
    }

    #[test]
    fn stems_are_never_unioned_across_domains() {
        let t = env();
        let ctx = &t.ctx;
        let e = kept(ctx, "e-x", "rounding-rules", None);
        let mut i = 0;
        for (d, stems) in [("quant.options", ["greek", "vega"]), ("web.frontend", ["react", "form"])] {
            for _ in 0..2 {
                ctx.ledger.upsert_session(&read(sess(&format!("x-{i}"), 2 + i, &stems, Some(d)), &e)).unwrap();
                i += 1;
            }
        }
        let (sig, p) = tune_sig(ctx, &e);
        assert_eq!(sig.read_missed.len(), 4);
        assert!(p.is_none(), "two domains, no consensus, no union: {p:?}");
    }

    #[test]
    fn matched_but_unread_narrows_and_never_below_two_stems() {
        let t = env();
        let ctx = &t.ctx;
        let mut e = entry("e-n", "ledger-migrations", "payments");
        e.predicate.stems = vec!["ledger".into(), "migrat".into(), "schema".into(), "test".into()];
        let e = keep(ctx, &stage(ctx, &e, &[]).id);
        let loaded = |mut s: SessionRecord| {
            s.loaded_entries = vec![e.id.clone()];
            s
        };
        // read sessions carry ledger + migrat; unread ones carry schema and test too
        for i in 0..2 {
            ctx.ledger.upsert_session(&read(loaded(sess(&format!("r-{i}"), 2 + i, &["ledger", "migrat"], None)), &e)).unwrap();
        }
        for i in 0..4 {
            ctx.ledger.upsert_session(&loaded(sess(&format!("u-{i}"), 5 + i, &["ledger", "migrat", "schema", "test"], None))).unwrap();
        }
        let (sig, p) = tune_sig(ctx, &e);
        assert_eq!((sig.matched_read.len(), sig.matched_unread.len()), (2, 4));
        let p = p.unwrap();
        assert_eq!((p.direction.as_str(), p.removed.clone()), ("narrow", vec!["schema".to_string(), "test".into()]));
        assert_eq!(p.predicate.stems, vec!["ledger", "migrat"]);

        // every stem only in unread sessions: still two left
        let mut f = entry("e-n2", "schema-only", "payments");
        f.predicate.stems = vec!["alpha".into(), "beta".into(), "gamma".into()];
        let f = keep(ctx, &stage(ctx, &f, &[]).id);
        for i in 0..4 {
            let mut s = sess(&format!("v-{i}"), 2 + i, &["alpha", "beta", "gamma"], None);
            s.loaded_entries = vec![f.id.clone()];
            ctx.ledger.upsert_session(&s).unwrap();
        }
        let (_, p) = tune_sig(ctx, &f);
        let p = p.unwrap();
        assert_eq!(p.predicate.stems.len(), NARROW_KEEP_STEMS, "{p:?}");
        assert!(guard_predicate(&f.predicate, &p.predicate).is_ok());

        // two stems: nothing to narrow
        let mut g = entry("e-n3", "two-stems", "payments");
        g.predicate.stems = vec!["alpha".into(), "beta".into()];
        let g = keep(ctx, &stage(ctx, &g, &[]).id);
        for i in 0..4 {
            let mut s = sess(&format!("w-{i}"), 2 + i, &["alpha", "beta"], None);
            s.loaded_entries = vec![g.id.clone()];
            ctx.ledger.upsert_session(&s).unwrap();
        }
        assert!(tune_sig(ctx, &g).1.is_none());
    }

    #[test]
    fn the_guards_refuse_place_glob_caps_emptying_and_domain_changes() {
        let old = Predicate { repo_glob: "/w/p".into(), stems: vec!["a".into(), "b".into(), "c".into()], file_exts: vec!["rs".into()], ..Default::default() };
        let ok = |f: &dyn Fn(&mut Predicate)| {
            let mut n = old.clone();
            f(&mut n);
            guard_predicate(&old, &n)
        };
        assert!(ok(&|_| {}).is_ok());
        assert!(ok(&|n| n.repo_glob = ANY_REPO.into()).is_err());
        assert!(ok(&|n| n.stems.push("**".into())).is_err());
        assert!(ok(&|n| n.stems = (0..11).map(|i| format!("s{i}")).collect()).is_err());
        assert!(ok(&|n| n.file_exts.clear()).is_err());
        assert!(ok(&|n| n.stems = vec!["a".into()]).is_err());
        assert!(ok(&|n| n.domain = Some("web".into())).is_ok(), "setting a domain is a narrowing");
        let mut dom = old.clone();
        dom.domain = Some("web".into());
        let mut other = dom.clone();
        other.domain = Some("mobile".into());
        assert!(guard_predicate(&dom, &other).is_err());
        let mut cleared = dom.clone();
        cleared.domain = None;
        assert!(guard_predicate(&dom, &cleared).is_err());
    }

    #[test]
    fn built_in_hook_pinned_and_non_skill_entries_are_never_tuned() {
        let mut e = entry("e-i", "ledger-migrations", "payments");
        assert!(ineligible(&e).is_none());
        e.pinned = true;
        assert_eq!(ineligible(&e).as_deref(), Some("pinned (human-owned)"));
        e.pinned = false;
        e.kind = Kind::Hook;
        assert_eq!(ineligible(&e).as_deref(), Some("hook entry"));
        e.origin = Origin::Import { source: library::BUILTIN_SOURCE.into() };
        assert_eq!(ineligible(&e).as_deref(), Some("built-in entry"));
        let mut a = entry("e-a", "reviewer", "payments");
        a.kind = Kind::Agent;
        assert!(ineligible(&a).unwrap().contains("only skill entries"));
        a.kind = Kind::Rule;
        assert!(ineligible(&a).is_none(), "rule entries are skills");
        a.status = Status::Dormant;
        assert!(ineligible(&a).unwrap().contains("only provisional and kept"));
    }
}
