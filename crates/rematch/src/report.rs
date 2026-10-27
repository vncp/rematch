//! `rematch report` (ADR 001 L4; SPEC §"Kill criterion for the project"). Over a window:
//! (a) kept entries with their evidence: matched sessions (observational), sessions with
//! actual use, clean actual uses and use unknown (legacy rows, unknown completion), and the
//! engine's retention reason; an entry is kept in real use with ≥ `KEEP_MIN_USED` clean
//! actual uses, while a comparative-support keep is reported apart (bounded comparison,
//! not real-use gain); (b) per trigger predicate,
//! the recurrence of its redirect class on matched sessions that loaded an entry of the
//! trigger (with-arm) against matched sessions that loaded none (before-arm), grouped by the
//! trigger's domain (DESIGN §6 "Domain") with a per-domain summary; a predicate tune's
//! sibling joins its parent's trigger, each session judged by the predicate of the version
//! it loaded; (c) every predicate tune in the window, parent and sibling with-arms side by
//! side (DESIGN §6 "Predicate tuning"); (d) the verdict line. From the lake alone when there
//! is one — every machine's history, at whatever lag the last export left — else from this
//! ledger's journal (`"source": "ledger"`). Both read ADR 001's as-of rule: each row as its
//! origin ledger held it at the window's end. Built-in entries are reported but never decide
//! the verdict.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};
use clap::Args;
use rematch_core::counters::{class_recurs, redirect_class_of, retention_label, DAY_MS, KEEP_MIN_USED};
use rematch_core::types::RetentionReason;
use rematch_core::ledger::{redirect_key, CounterRow, JournalRow};
use rematch_core::library::is_builtin;
use rematch_core::predicate::{count_match, predicate_hash, Place};
use rematch_core::redirect::class_name;
use rematch_core::types::{Entry, RedirectClass, SessionRecord, Status};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::app::AppCtx;
use crate::lake_read::{self, as_of, Lookup, Source};

#[derive(Args, Debug, Clone)]
pub struct ReportArgs {
    /// Window ending now: `<n>w`, `<n>d` or `<n>h`.
    #[arg(long, default_value = "6w")]
    pub window: String,
    /// Start the window at this UTC day (`YYYY-MM-DD`) instead of `now − window`.
    #[arg(long, value_name = "DAY")]
    pub since: Option<String>,
}

/// `6w` / `30d` / `12h` → ms.
pub fn parse_window(s: &str) -> Result<i64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = match n.parse() {
        Ok(n) if n > 0 => n,
        _ => bail!("bad --window {s:?}: expected <n>w, <n>d or <n>h"),
    };
    Ok(n * match unit {
        "w" => 7 * DAY_MS,
        "d" => DAY_MS,
        "h" => DAY_MS / 24,
        _ => bail!("bad --window {s:?}: expected <n>w, <n>d or <n>h"),
    })
}

/// `YYYY-MM-DD` → ms at 00:00 UTC.
pub fn parse_day(s: &str) -> Result<i64> {
    match chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
        Ok(d) => Ok(d.and_hms_opt(0, 0, 0).expect("midnight").and_utc().timestamp_millis()),
        Err(_) => bail!("bad day {s:?}: expected YYYY-MM-DD"),
    }
}

pub(crate) fn utc(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()
}

/// The as-of rows of one table, decoded; rows that do not decode are skipped (stderr).
pub(crate) fn decode<T: DeserializeOwned>(rows: Vec<JournalRow>) -> Vec<(String, T)> {
    rows.into_iter()
        .filter_map(|r| match serde_json::from_value::<T>(r.json) {
            Ok(v) => Some((r.machine, v)),
            Err(e) => {
                eprintln!("rematch report: {} row {} seq {} skipped: {e}", r.table, r.row_key, r.seq);
                None
            }
        })
        .collect()
}

/// A counter row as the journal holds it: the ledger row plus the entry's content hash at
/// fold time (ADR 001: cross-machine aggregation is by content).
#[derive(serde::Deserialize)]
struct CounterJson {
    #[serde(flatten)]
    row: CounterRow,
    #[serde(default)]
    content_sha256: Option<String>,
}

/// What the report reads, every row as of the window's end.
pub struct Data {
    pub entries: Vec<(String, Entry)>,
    /// Counter rows whose session time falls in the window.
    counters: Vec<(String, CounterJson)>,
    /// Sessions started in the window, redirect labels as of the window's end.
    pub sessions: Vec<(String, SessionRecord)>,
    pub start: i64,
    pub end: i64,
}

pub fn load(ctx: &AppCtx, src: Source, start: i64, end: i64) -> Result<Data> {
    let rows = |table: &str, lookup: Option<&Lookup>| -> Result<Vec<JournalRow>> {
        Ok(as_of(lake_read::rows(ctx, src, table, lookup, None)?, end))
    };
    let entries = decode::<Entry>(rows("entries", None)?);
    let counters: Vec<(String, CounterJson)> =
        decode::<CounterJson>(rows("counters", None)?).into_iter().filter(|(_, c)| (start..=end).contains(&c.row.ts)).collect();
    let window = Lookup { key: "range:started_at", values: Vec::new(), window: Some((start, end)) };
    let mut sessions: Vec<(String, SessionRecord)> =
        decode::<SessionRecord>(rows("sessions", Some(&window))?).into_iter().filter(|(_, s)| (start..=end).contains(&s.started_at)).collect();
    let labels: BTreeMap<(String, String), rematch_core::types::Redirect> = decode(rows("redirects", None)?)
        .into_iter()
        .map(|(m, r): (String, rematch_core::types::Redirect)| ((m, redirect_key(&r.session_id, r.seq)), r))
        .collect();
    for (m, s) in &mut sessions {
        for r in &mut s.redirects {
            if let Some(l) = labels.get(&(m.clone(), redirect_key(&r.session_id, r.seq))) {
                *r = l.clone();
            }
        }
    }
    Ok(Data { entries, counters, sessions, start, end })
}

#[derive(Default)]
struct Arm {
    sessions: usize,
    recurred: usize,
    redirects: usize,
}

/// One domain's triggers: their arms summed (a session matching two triggers counts twice).
#[derive(Default)]
struct DomainSum {
    triggers: usize,
    covered: usize,
    fell: usize,
    with: Arm,
    before: Arm,
    lines: Vec<String>,
}

/// Label of a trigger's domain group.
const NO_DOMAIN: &str = "(none)";

impl Arm {
    fn rate(&self) -> Option<f64> {
        (self.sessions > 0).then(|| self.recurred as f64 / self.sessions as f64)
    }

    fn add(&mut self, o: &Arm) {
        self.sessions += o.sessions;
        self.recurred += o.recurred;
        self.redirects += o.redirects;
    }

    /// One matched session: recurred when it holds a redirect of `class`.
    fn add_session(&mut self, s: &SessionRecord, class: Option<RedirectClass>) {
        let n = s.redirects.iter().filter(|r| class_recurs(class, redirect_class_of(r.class, &r.prelabel))).count();
        self.sessions += 1;
        self.recurred += (n > 0) as usize;
        self.redirects += n;
    }

    fn json(&self) -> Value {
        json!({"sessions": self.sessions, "recurred": self.recurred, "redirects": self.redirects, "rate": self.rate()})
    }

    fn text(&self) -> String {
        match self.rate() {
            Some(r) => format!("{}/{} recurred ({} redirect(s), rate {r:.2})", self.recurred, self.sessions, self.redirects),
            None => "no sessions".into(),
        }
    }
}

fn names(entries: &[&(String, Entry)]) -> String {
    let set: BTreeSet<&str> = entries.iter().map(|(_, e)| e.name.as_str()).collect();
    set.into_iter().collect::<Vec<_>>().join("/")
}

/// Whether session `s` (of machine `sm`) loaded `e` (of machine `em`): by id on the entry's
/// own machine (ids are machine-local, and a predicate tune's sibling shares its parent's
/// content hash, so only the id tells the versions apart), by content hash on another.
fn loaded_in(sm: &str, s: &SessionRecord, em: &str, e: &Entry) -> bool {
    if sm == em {
        s.loaded_entries.contains(&e.id)
    } else {
        e.content_sha256.as_ref().is_some_and(|h| s.loaded_entry_shas.contains(h))
    }
}

/// Every predicate tune sibling (`Entry.tune`) whose parent the rows hold → the entry its
/// lineage started from, both as `(machine, id)`.
fn tune_roots(entries: &[(String, Entry)]) -> BTreeMap<(&str, &str), (&str, &str)> {
    let by_id: BTreeMap<(&str, &str), &Entry> = entries.iter().map(|(m, e)| ((m.as_str(), e.id.as_str()), e)).collect();
    let mut out = BTreeMap::new();
    for (m, e) in entries {
        let mut cur = e;
        for _ in 0..entries.len() {
            match cur.tune.as_ref().and_then(|t| by_id.get(&(m.as_str(), t.parent.as_str()))) {
                Some(p) => cur = p,
                None => break,
            }
        }
        if cur.id != e.id {
            out.insert((m.as_str(), e.id.as_str()), (m.as_str(), cur.id.as_str()));
        }
    }
    out
}

/// The report over `data` for the window `[start, end]`.
pub fn compute(data: &Data) -> (Value, Vec<String>) {
    // (a) kept entries and their matched sessions in the window, by id on the entry's
    // machine or by content hash on any machine — except the rows of another version in its
    // own tune lineage, which shares its content hash but not its predicate.
    let root = tune_roots(&data.entries);
    let lineage_of = |m: &str, id: &str| -> (String, String) {
        let (rm, rid) = root.get(&(m, id)).copied().unwrap_or((m, id));
        (rm.to_string(), rid.to_string())
    };
    let mut kept = Vec::new();
    for (m, e) in data.entries.iter().filter(|(_, e)| e.status == Status::Kept) {
        let mine = lineage_of(m, &e.id);
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let (mut recurrence, mut harm, mut used, mut used_clean, mut unknown) = (0u32, 0u32, 0u32, 0u32, 0u32);
        for (cm, c) in &data.counters {
            let own_machine = cm == m;
            let ours = (own_machine && c.row.entry_id == e.id)
                || (e.content_sha256.is_some()
                    && c.content_sha256 == e.content_sha256
                    && !(own_machine && c.row.entry_id != e.id && lineage_of(cm, &c.row.entry_id) == mine));
            if ours && c.row.matched > 0 && seen.insert(c.row.session_id.as_str()) {
                recurrence += c.row.recurrence;
                harm += c.row.harm;
                match c.row.used {
                    Some(u) if u > 0 => {
                        used += 1;
                        if c.row.recurrence == 0 && c.row.harm == 0 {
                            used_clean += 1;
                        }
                    }
                    Some(_) => {}
                    None => unknown += 1,
                }
            }
        }
        let reason = retention_label(e);
        kept.push(json!({
            "machine": m, "id": e.id, "name": e.name, "content_sha256": e.content_sha256, "builtin": is_builtin(e),
            "matched": seen.len(), "used": used, "used_clean": used_clean, "use_unknown": unknown,
            "recurrence": recurrence, "harm": harm, "retention": reason,
            "in_use": used_clean >= KEEP_MIN_USED && recurrence == 0 && harm == 0,
            "comparative": reason == Some(RetentionReason::ComparativeSupport),
        }));
    }

    // (b) triggers: entries grouped by predicate hash; a predicate tune's sibling joins the
    // group of the entry it was tuned from (its tune lineage), so widening a predicate opens
    // no trigger of its own.
    let mut groups: BTreeMap<String, Vec<&(String, Entry)>> = BTreeMap::new();
    let live: Vec<&(String, Entry)> = data.entries.iter().filter(|(_, e)| e.status != Status::Draft).collect();
    for x in live.iter().filter(|(m, e)| !root.contains_key(&(m.as_str(), e.id.as_str()))) {
        groups.entry(predicate_hash(&x.1.predicate)).or_default().push(x);
    }
    let lineage = |es: &[&(String, Entry)]| -> Vec<&(String, Entry)> {
        let heads: BTreeSet<(&str, &str)> = es.iter().map(|(m, e)| (m.as_str(), e.id.as_str())).collect();
        live.iter().filter(|(m, e)| root.get(&(m.as_str(), e.id.as_str())).is_some_and(|r| heads.contains(r))).copied().collect()
    };
    let mut triggers = Vec::new();
    let mut fell_on: Vec<String> = Vec::new();
    let mut by_domain: BTreeMap<String, DomainSum> = BTreeMap::new();
    for (hash, mut es) in groups {
        es.sort_by_key(|(_, e)| (e.created_at, e.id.clone()));
        let class: Option<RedirectClass> = es.iter().find_map(|(_, e)| e.redirect_class.filter(|c| *c != RedirectClass::None));
        let pred = &es.iter().max_by_key(|(_, e)| e.updated_at).expect("non-empty group").1.predicate;
        let tunes = lineage(&es);
        let members: Vec<&(String, Entry)> = es.iter().chain(tunes.iter()).copied().collect();
        let builtin = es.iter().all(|(_, e)| is_builtin(e));
        let (mut with, mut before) = (Arm::default(), Arm::default());
        for (m, s) in &data.sessions {
            let place = Place::of_session(s);
            let loaded: Vec<&Entry> = members.iter().filter(|(em, e)| loaded_in(m, s, em, e)).map(|(_, e)| e).collect();
            // The with-arm tests the predicate of the version the session loaded; the
            // before-arm, the trigger's.
            let arm = if loaded.is_empty() {
                if !count_match(pred, &place, &s.exts, &s.stems) {
                    continue;
                }
                &mut before
            } else {
                if !loaded.iter().any(|e| count_match(&e.predicate, &place, &s.exts, &s.stems)) {
                    continue;
                }
                &mut with
            };
            arm.add_session(s, class);
        }
        let es: Vec<&(String, Entry)> = members;
        let delta = with.rate().zip(before.rate()).map(|(w, b)| w - b);
        let covered = with.sessions > 0;
        let fell = delta.is_some_and(|d| d < 0.0);
        let label = names(&es);
        let class_s = class.map(class_name).unwrap_or("correction|preference");
        if fell && !builtin {
            fell_on.push(label.clone());
        }
        let domain = pred.domain.clone().unwrap_or_else(|| NO_DOMAIN.to_string());
        let sum = by_domain.entry(domain).or_default();
        sum.triggers += 1;
        sum.covered += covered as usize;
        sum.fell += (fell && !builtin) as usize;
        sum.with.add(&with);
        sum.before.add(&before);
        if with.sessions + before.sessions > 0 {
            sum.lines.push(format!(
                "  trigger {label}{} [{}] class {class_s}: with {} · before {}{}",
                if builtin { " (built-in)" } else { "" },
                &hash[..12],
                with.text(),
                before.text(),
                delta.map(|d| format!(" · Δ {d:+.2}{}", if fell { " fell" } else { "" })).unwrap_or_default(),
            ));
        }
        triggers.push(json!({
            "predicate_hash": hash, "names": label, "predicate": pred, "domain": pred.domain, "redirect_class": class.map(class_name),
            "builtin": builtin, "covered": covered,
            "entries": es.iter().map(|(m, e)| json!({"machine": m, "id": e.id, "name": e.name, "status": e.status,
                                                      "content_sha256": e.content_sha256})).collect::<Vec<_>>(),
            "with": with.json(), "before": before.json(), "delta": delta, "fell": fell,
        }));
    }
    // Per-domain summary, then that domain's trigger lines; domains with no matched session
    // in the window print nothing.
    let mut trigger_lines = Vec::new();
    let mut domains = Vec::new();
    for (d, sum) in &by_domain {
        domains.push(json!({
            "domain": d, "triggers": sum.triggers, "covered": sum.covered, "fell": sum.fell,
            "with": sum.with.json(), "before": sum.before.json(),
        }));
        if sum.lines.is_empty() {
            continue;
        }
        trigger_lines.push(format!(
            "domain {d}: {} trigger(s), {} covered, {} fell · with {} · before {}",
            sum.triggers,
            sum.covered,
            sum.fell,
            sum.with.text(),
            sum.before.text()
        ));
        trigger_lines.extend(sum.lines.iter().cloned());
    }

    // Predicate tunes staged in the window: the parent's and the sibling's with-arms side by
    // side (each the sessions that loaded that version and that its own predicate hit) against
    // the before-arm of the parent's predicate, so a widening is read against the version each
    // session loaded and never against a predicate that did not exist then.
    let mut tunes = Vec::new();
    let mut tune_lines = Vec::new();
    for (m, t) in &data.entries {
        let Some(tune) = t.tune.as_ref().filter(|x| (data.start..=data.end).contains(&x.at)) else { continue };
        let Some((_, p)) = data.entries.iter().find(|(pm, p)| pm == m && p.id == tune.parent) else { continue };
        let class = [p, t].iter().find_map(|e| e.redirect_class.filter(|c| *c != RedirectClass::None));
        let (mut v1, mut v2, mut before) = (Arm::default(), Arm::default(), Arm::default());
        for (sm, s) in &data.sessions {
            let place = Place::of_session(s);
            let (lp, lt) = (loaded_in(sm, s, m, p), loaded_in(sm, s, m, t));
            if lp && count_match(&p.predicate, &place, &s.exts, &s.stems) {
                v1.add_session(s, class);
            }
            if lt && count_match(&t.predicate, &place, &s.exts, &s.stems) {
                v2.add_session(s, class);
            }
            if !lp && !lt && count_match(&p.predicate, &place, &s.exts, &s.stems) {
                before.add_session(s, class);
            }
        }
        tune_lines.push(format!(
            "tune {} v{}→v{} ({}, {}): {} · v{} with {} · v{} with {} · before {}",
            t.name,
            p.version,
            t.version,
            utc(tune.at),
            rematch_core::ledger::enum_str(&t.status),
            crate::tune::note_of(tune),
            p.version,
            v1.text(),
            t.version,
            v2.text(),
            before.text(),
        ));
        tunes.push(json!({
            "machine": m, "name": t.name, "at": tune.at, "direction": tune.direction,
            "parent": {"id": p.id, "version": p.version, "status": p.status, "predicate_hash": predicate_hash(&p.predicate), "predicate": p.predicate},
            "tune": {"id": t.id, "version": t.version, "status": t.status, "predicate_hash": predicate_hash(&t.predicate), "predicate": t.predicate},
            "added": tune.added, "removed": tune.removed, "added_exts": tune.added_exts, "domain_set": tune.domain_set,
            "evidence": tune.evidence, "redirect_class": class.map(class_name),
            "v1": v1.json(), "v2": v2.json(), "before": before.json(),
        }));
    }

    // (c) the verdict, built-ins aside.
    let names_where = |key: &str| -> Vec<String> {
        kept.iter()
            .filter(|k| k[key] == true && k["builtin"] == false)
            .map(|k| k["name"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    let in_use = names_where("in_use");
    let comparative = names_where("comparative");
    let yes = |b: bool| if b { "yes" } else { "no" };
    let list = |v: &[String]| if v.is_empty() { String::new() } else { format!(" ({})", v.join(", ")) };
    let verdict_line = format!(
        "verdict: an entry kept in real use (≥ {KEEP_MIN_USED} clean actual uses): {}{}; recurrence fell on a covered trigger: {}{}; kept on comparative support: {}{}",
        yes(!in_use.is_empty()),
        list(&in_use),
        yes(!fell_on.is_empty()),
        list(&fell_on),
        yes(!comparative.is_empty()),
        list(&comparative),
    );

    let mut lines = Vec::new();
    if kept.is_empty() {
        lines.push("kept: none".to_string());
    }
    for k in &kept {
        lines.push(format!(
            "kept {}{} [{}]: {} matched, {} used ({} clean, {} unknown) in the window, recurrence {}, harm {}",
            k["name"].as_str().unwrap_or_default(),
            if k["builtin"] == true { " (built-in)" } else { "" },
            k["retention"].as_str().unwrap_or("legacy"),
            k["matched"],
            k["used"],
            k["used_clean"],
            k["use_unknown"],
            k["recurrence"],
            k["harm"],
        ));
    }
    if trigger_lines.is_empty() {
        lines.push("triggers: no matched session in the window".into());
    }
    lines.extend(trigger_lines);
    lines.extend(tune_lines);
    lines.push(verdict_line);
    let out = json!({
        "kept": kept,
        "triggers": triggers,
        "domains": domains,
        "tunes": tunes,
        "verdict": {"kept_in_use": !in_use.is_empty(), "kept_entries": in_use,
                    "recurrence_fell": !fell_on.is_empty(), "fell_on": fell_on,
                    "kept_comparative": !comparative.is_empty(), "comparative_entries": comparative},
    });
    (out, lines)
}

pub fn cmd_report(ctx: &AppCtx, a: ReportArgs) -> Result<Value> {
    let end = ctx.now;
    let start = match &a.since {
        Some(d) => parse_day(d)?,
        None => end - parse_window(&a.window)?,
    };
    if start > end {
        bail!("--since {} is after now", a.since.as_deref().unwrap_or_default());
    }
    let src = if lake_read::lake(ctx).is_some() { Source::Lake } else { Source::Ledger };
    let data = load(ctx, src, start, end)?;
    let machines: BTreeSet<&str> =
        data.entries.iter().map(|(m, _)| m.as_str()).chain(data.sessions.iter().map(|(m, _)| m.as_str())).collect();
    let lag = match src {
        Source::Lake => Some(lake_read::lag(ctx)?),
        Source::Ledger => None,
    };
    let (mut out, body) = compute(&data);
    let mut lines = vec![format!(
        "report · source {}{} · machines {} · window {} → {}{}",
        src.name(),
        lag.map(|n| format!(" (lag {n} journal row(s))")).unwrap_or_default(),
        if machines.is_empty() { "none".to_string() } else { machines.iter().copied().collect::<Vec<_>>().join(", ") },
        utc(start),
        utc(end),
        if a.since.is_some() { String::new() } else { format!(" ({})", a.window) },
    )];
    // Skills kept on another machine load here but are counted only there (DESIGN §20).
    if rematch_core::profile::present(&ctx.paths) {
        let n = crate::profile_cmd::profile_kept(ctx)?.len();
        lines.push(format!("profile-kept (not counted on this machine): {n}"));
        out["profile_kept"] = json!(n);
    }
    lines.extend(body);
    out["source"] = json!(src);
    out["lake_lag"] = json!(lag);
    out["machines"] = json!(machines);
    out["window"] = json!({"start": start, "end": end, "spec": if a.since.is_some() { Value::Null } else { json!(a.window) }, "since": a.since});
    out["lines"] = json!(lines);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::testutil::{entry, env, keep, session, stage, Env};
    use rematch_core::ledger::now_ms;
    use rematch_core::types::Redirect;

    fn negation(sid: &str, t: i64) -> Redirect {
        Redirect {
            session_id: sid.into(),
            seq: 3,
            prelabel: "negation".into(),
            class: None,
            confidence: None,
            text_head: "No, that's wrong: run the migration test first".into(),
            deliverable_head: String::new(),
            t,
        }
    }

    /// One trigger (`run-tests-before-commit`, correction) kept over four clean with-arm
    /// sessions; four before-arm sessions of the same predicate, two with a correction and
    /// one whose negation the classifier later relabelled `scope`; a second trigger no
    /// session matches; a session outside the window.
    fn seed(t: &mut Env) -> Entry {
        let ctx = &t.ctx;
        let now = now_ms();
        let e = stage(ctx, &entry("e-tests", "run-tests-before-commit", "payments"), &[]);
        let mut doc = entry("e-doc", "docstring-style", "payments");
        doc.predicate.file_exts = vec!["py".into()];
        doc.predicate.stems = vec!["docstr".into(), "style".into()];
        stage(ctx, &doc, &[]);
        for i in 0..4i64 {
            let mut s = session(&format!("b-{i}"), "payments", Some("/w/payments"), "anthropic/claude-opus", now - (20 - i) * DAY_MS);
            if i < 3 {
                s.redirects = vec![negation(&s.id, s.started_at + 10)];
            }
            ctx.ledger.upsert_session(&s).unwrap();
        }
        ctx.ledger.set_redirect_class("b-2", 3, RedirectClass::Scope, Some(0.9)).unwrap();
        for i in 0..4i64 {
            let mut s = session(&format!("w-{i}"), "payments", Some("/w/payments"), "anthropic/claude-opus", now - (10 - i) * DAY_MS);
            s.loaded_entries = vec![e.id.clone()];
            s.loaded_entry_shas = e.content_sha256.clone().into_iter().collect();
            // Actual, completed use: the skill was read and the run settled.
            s.skill_entries = vec![e.id.clone()];
            s.turns = 2;
            s.run_completed = Some(true);
            ctx.ledger.upsert_session(&s).unwrap();
            rematch_core::counters::fold_session(&ctx.ledger, &s, std::slice::from_ref(&e), &[]).unwrap();
        }
        let mut old = session("old", "payments", Some("/w/payments"), "anthropic/claude-opus", now - 90 * DAY_MS);
        old.redirects = vec![negation("old", old.started_at + 10)];
        ctx.ledger.upsert_session(&old).unwrap();
        let kept = keep(ctx, &e.id);
        t.ctx.now = now_ms();
        kept
    }

    fn report(ctx: &AppCtx) -> Value {
        cmd_report(ctx, ReportArgs { window: "6w".into(), since: None }).unwrap()
    }

    fn check(out: &Value) {
        let kept = &out["kept"].as_array().unwrap()[0];
        assert_eq!(
            (kept["name"].as_str(), kept["matched"].as_u64(), kept["used_clean"].as_u64(), kept["in_use"].as_bool(), kept["retention"].as_str()),
            (Some("run-tests-before-commit"), Some(4), Some(4), Some(true), Some("legacy")),
            "{out}"
        );
        let t = out["triggers"].as_array().unwrap().iter().find(|t| t["names"] == "run-tests-before-commit").unwrap();
        assert_eq!(t["redirect_class"], "correction");
        assert_eq!(t["with"], json!({"sessions": 4, "recurred": 0, "redirects": 0, "rate": 0.0}), "{t}");
        assert_eq!(t["before"], json!({"sessions": 4, "recurred": 2, "redirects": 2, "rate": 0.5}), "the scope relabel is no recurrence: {t}");
        assert_eq!((t["delta"].as_f64(), t["fell"].as_bool(), t["covered"].as_bool()), (Some(-0.5), Some(true), Some(true)));
        let doc = out["triggers"].as_array().unwrap().iter().find(|t| t["names"] == "docstring-style").unwrap();
        assert_eq!((doc["with"]["sessions"].as_u64(), doc["before"]["sessions"].as_u64()), (Some(0), Some(0)));
        assert_eq!(out["verdict"], json!({"kept_in_use": true, "kept_entries": ["run-tests-before-commit"],
                                           "recurrence_fell": true, "fell_on": ["run-tests-before-commit"],
                                           "kept_comparative": false, "comparative_entries": []}));
        assert_eq!(
            out["lines"].as_array().unwrap().last().unwrap(),
            "verdict: an entry kept in real use (≥ 4 clean actual uses): yes (run-tests-before-commit); recurrence fell on a covered trigger: yes (run-tests-before-commit); kept on comparative support: no"
        );
    }

    #[test]
    fn report_from_the_ledger_and_from_the_lake_agree() {
        let mut t = env();
        seed(&mut t);
        let ctx = &t.ctx;
        let ledger = report(ctx);
        assert_eq!((ledger["source"].as_str(), &ledger["lake_lag"]), (Some("ledger"), &Value::Null), "{ledger}");
        check(&ledger);
        println!("{}", ledger["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect::<Vec<_>>().join("\n"));
        #[cfg(feature = "lake")]
        {
            lake_read::export_all(ctx);
            let lake = report(ctx);
            assert_eq!((lake["source"].as_str(), lake["lake_lag"].as_i64()), (Some("lake"), Some(0)), "{lake}");
            check(&lake);
            for k in ["kept", "triggers", "verdict", "machines"] {
                assert_eq!(lake[k], ledger[k], "{k}");
            }
            assert_eq!(lake["lines"].as_array().unwrap()[1..], ledger["lines"].as_array().unwrap()[1..]);
            println!("{}", lake["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect::<Vec<_>>().join("\n"));
        }
    }

    #[test]
    fn the_window_and_the_as_of_rule_bound_what_is_read() {
        let mut t = env();
        let e = seed(&mut t);
        // Three days back: every seeded session started earlier; nothing matched, nothing kept over 4.
        let short = cmd_report(&t.ctx, ReportArgs { window: "3d".into(), since: None }).unwrap();
        let tr = short["triggers"].as_array().unwrap().iter().find(|t| t["names"] == "run-tests-before-commit").unwrap().clone();
        assert_eq!((tr["with"]["sessions"].as_u64(), tr["before"]["sessions"].as_u64()), (Some(0), Some(0)), "{short}");
        assert_eq!(short["kept"][0]["matched"], 0);
        assert!(short["lines"].as_array().unwrap().last().unwrap().as_str().unwrap().contains("clean actual uses): no; recurrence fell on a covered trigger: no"), "{short}");
        // --since the day before the old session: it joins the before-arm (3 of 5 recurred).
        let since = utc(t.ctx.now - 91 * DAY_MS)[..10].to_string();
        let long = cmd_report(&t.ctx, ReportArgs { window: "6w".into(), since: Some(since) }).unwrap();
        let tr = long["triggers"].as_array().unwrap().iter().find(|t| t["names"] == "run-tests-before-commit").unwrap().clone();
        assert_eq!((tr["before"]["sessions"].as_u64(), tr["before"]["recurred"].as_u64()), (Some(5), Some(3)), "{long}");
        // A window ending just before the keep: the entry was provisional then, nothing kept.
        t.ctx.now = e.status_since.unwrap() - 1;
        let out = report(&t.ctx);
        assert_eq!(out["kept"], json!([]), "{out}");
        assert_eq!(out["verdict"]["kept_in_use"], false, "{out}");
        assert!(parse_window("6x").is_err() && parse_window("0w").is_err() && parse_day("2026-13-01").is_err());
    }

    #[test]
    fn a_comparative_keep_without_real_use_is_reported_apart() {
        let mut t = env();
        let mut e = seed(&mut t);
        // Same counters but none of the matched sessions used it; kept on comparative support.
        for i in 0..4 {
            let mut s = t.ctx.ledger.get_session(&format!("w-{i}")).unwrap().unwrap();
            s.skill_entries.clear();
            t.ctx.ledger.add_counter(&rematch_core::ledger::CounterRow {
                used: Some(0),
                ..t.ctx.ledger.counter_rows(&e.id).unwrap().into_iter().find(|r| r.session_id == s.id).unwrap()
            })
            .unwrap();
        }
        e.retention = Some(rematch_core::types::Retention {
            reason: RetentionReason::ComparativeSupport,
            at: e.status_since.unwrap(),
            version: e.version,
            content_sha256: e.content_sha256.clone(),
            matched: 4,
            used: 0,
            used_clean: 0,
            probe: Some("p".into()),
            detail: String::new(),
        });
        t.ctx.ledger.upsert_entry(&e).unwrap();
        t.ctx.now = now_ms(); // the window ends after these writes (as-of rule)
        let out = report(&t.ctx);
        assert_eq!(out["verdict"]["kept_in_use"], false, "4 unread matches are no real use: {out}");
        assert_eq!(out["verdict"]["comparative_entries"], json!(["run-tests-before-commit"]));
        assert!(out["lines"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("[comparative_support]: 4 matched, 0 used")), "{out}");
    }

    /// Two domains: a web and a mobile review trigger with identical stems; each counts only
    /// its own domain's sessions, and the lines group under a per-domain summary.
    #[test]
    fn triggers_group_by_domain_and_never_cross_it() {
        let mut t = env();
        let ctx = &t.ctx;
        let now = now_ms();
        let review = |id: &str, name: &str, domain: &str| {
            let mut e = entry(id, name, "app");
            e.predicate.file_exts = vec![];
            e.predicate.stems = vec!["review".into(), "checklist".into()];
            e.predicate.domain = Some(domain.into());
            stage(ctx, &e, &[])
        };
        review("e-web", "web-review", "web");
        review("e-mob", "mobile-review", "mobile");
        for (i, d) in ["web.frontend", "web", "mobile", "mobile", "mobile"].iter().enumerate() {
            let mut s = session(&format!("s-{i}"), "app", Some("/w/app"), "anthropic/claude-opus", now - (5 + i as i64) * DAY_MS);
            s.stems = vec!["review".into(), "checklist".into()];
            s.domain = Some(d.to_string());
            if i == 0 || i == 2 {
                s.redirects = vec![negation(&s.id, s.started_at + 10)];
            }
            ctx.ledger.upsert_session(&s).unwrap();
        }
        t.ctx.now = now_ms();
        let out = report(&t.ctx);
        let arm = |name: &str| {
            let t = out["triggers"].as_array().unwrap().iter().find(|t| t["names"] == name).unwrap().clone();
            (t["domain"].as_str().map(str::to_string), t["before"]["sessions"].as_u64(), t["before"]["recurred"].as_u64())
        };
        assert_eq!(arm("web-review"), (Some("web".into()), Some(2), Some(1)), "{out}");
        assert_eq!(arm("mobile-review"), (Some("mobile".into()), Some(3), Some(1)), "{out}");
        let lines: Vec<&str> = out["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
        let at = |p: &str| lines.iter().position(|l| l.starts_with(p)).unwrap_or_else(|| panic!("{p}: {lines:#?}"));
        assert_eq!(lines[at("domain mobile:")], "domain mobile: 1 trigger(s), 0 covered, 0 fell · with no sessions · before 1/3 recurred (1 redirect(s), rate 0.33)");
        assert!(lines[at("domain mobile:") + 1].starts_with("  trigger mobile-review"));
        assert!(lines[at("domain web:") + 1].starts_with("  trigger web-review"));
        assert!(at("domain mobile:") < at("domain web:"));
        assert_eq!(out["domains"].as_array().unwrap().len(), 2);
        println!("{}", lines.join("\n"));
    }
}
