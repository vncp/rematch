//! `rematch report` (ADR 001 L4; SPEC §"Kill criterion for the project"). Over a window:
//! (a) entries that reached `kept` with ≥ 4 matched sessions; (b) per trigger predicate,
//! the recurrence of its redirect class on matched sessions that loaded an entry of the
//! trigger (with-arm) against matched sessions that loaded none (before-arm); (c) the
//! verdict line. From the lake alone when there is one — every machine's history, at
//! whatever lag the last export left — else from this ledger's journal (`"source":
//! "ledger"`). Both read ADR 001's as-of rule: each row as its origin ledger held it at the
//! window's end. Built-in entries are reported but never decide the verdict.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};
use clap::Args;
use rematch_core::counters::{class_recurs, redirect_class_of, DAY_MS, KEEP_MIN_MATCHED};
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

fn utc(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()
}

/// The as-of rows of one table, decoded; rows that do not decode are skipped (stderr).
fn decode<T: DeserializeOwned>(rows: Vec<JournalRow>) -> Vec<(String, T)> {
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
    Ok(Data { entries, counters, sessions })
}

#[derive(Default)]
struct Arm {
    sessions: usize,
    recurred: usize,
    redirects: usize,
}

impl Arm {
    fn rate(&self) -> Option<f64> {
        (self.sessions > 0).then(|| self.recurred as f64 / self.sessions as f64)
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

/// The report over `data` for the window `[start, end]`.
pub fn compute(data: &Data) -> (Value, Vec<String>) {
    // (a) kept entries and their matched sessions in the window, by id on the entry's
    // machine or by content hash on any machine.
    let mut kept = Vec::new();
    for (m, e) in data.entries.iter().filter(|(_, e)| e.status == Status::Kept) {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let (mut recurrence, mut harm) = (0u32, 0u32);
        for (cm, c) in &data.counters {
            let ours = (cm == m && c.row.entry_id == e.id)
                || (e.content_sha256.is_some() && c.content_sha256 == e.content_sha256);
            if ours && c.row.matched > 0 && seen.insert(c.row.session_id.as_str()) {
                recurrence += c.row.recurrence;
                harm += c.row.harm;
            }
        }
        kept.push(json!({
            "machine": m, "id": e.id, "name": e.name, "content_sha256": e.content_sha256, "builtin": is_builtin(e),
            "matched": seen.len(), "recurrence": recurrence, "harm": harm,
            "reached": seen.len() as u32 >= KEEP_MIN_MATCHED,
        }));
    }

    // (b) triggers: entries grouped by predicate hash.
    let mut groups: BTreeMap<String, Vec<&(String, Entry)>> = BTreeMap::new();
    for x in data.entries.iter().filter(|(_, e)| e.status != Status::Draft) {
        groups.entry(predicate_hash(&x.1.predicate)).or_default().push(x);
    }
    let mut triggers = Vec::new();
    let mut fell_on: Vec<String> = Vec::new();
    let mut trigger_lines = Vec::new();
    for (hash, mut es) in groups {
        es.sort_by_key(|(_, e)| (e.created_at, e.id.clone()));
        let class: Option<RedirectClass> = es.iter().find_map(|(_, e)| e.redirect_class.filter(|c| *c != RedirectClass::None));
        let pred = &es.iter().max_by_key(|(_, e)| e.updated_at).expect("non-empty group").1.predicate;
        let ids: BTreeSet<(&str, &str)> = es.iter().map(|(m, e)| (m.as_str(), e.id.as_str())).collect();
        let shas: BTreeSet<&str> = es.iter().filter_map(|(_, e)| e.content_sha256.as_deref()).collect();
        let builtin = es.iter().all(|(_, e)| is_builtin(e));
        let (mut with, mut before) = (Arm::default(), Arm::default());
        for (m, s) in &data.sessions {
            if !count_match(pred, &Place::of_session(s), &s.exts, &s.stems) {
                continue;
            }
            let loaded = s.loaded_entry_shas.iter().any(|h| shas.contains(h.as_str()))
                || s.loaded_entries.iter().any(|id| ids.contains(&(m.as_str(), id.as_str())));
            let n = s.redirects.iter().filter(|r| class_recurs(class, redirect_class_of(r.class, &r.prelabel))).count();
            let arm = if loaded { &mut with } else { &mut before };
            arm.sessions += 1;
            arm.recurred += (n > 0) as usize;
            arm.redirects += n;
        }
        let delta = with.rate().zip(before.rate()).map(|(w, b)| w - b);
        let covered = with.sessions > 0;
        let fell = delta.is_some_and(|d| d < 0.0);
        let label = names(&es);
        let class_s = class.map(class_name).unwrap_or("correction|preference");
        if fell && !builtin {
            fell_on.push(label.clone());
        }
        if with.sessions + before.sessions > 0 {
            trigger_lines.push(format!(
                "trigger {label}{} [{}] class {class_s}: with {} · before {}{}",
                if builtin { " (built-in)" } else { "" },
                &hash[..12],
                with.text(),
                before.text(),
                delta.map(|d| format!(" · Δ {d:+.2}{}", if fell { " fell" } else { "" })).unwrap_or_default(),
            ));
        }
        triggers.push(json!({
            "predicate_hash": hash, "names": label, "predicate": pred, "redirect_class": class.map(class_name),
            "builtin": builtin, "covered": covered,
            "entries": es.iter().map(|(m, e)| json!({"machine": m, "id": e.id, "name": e.name, "status": e.status,
                                                      "content_sha256": e.content_sha256})).collect::<Vec<_>>(),
            "with": with.json(), "before": before.json(), "delta": delta, "fell": fell,
        }));
    }

    // (c) the verdict, built-ins aside.
    let reached: Vec<String> = kept
        .iter()
        .filter(|k| k["reached"] == true && k["builtin"] == false)
        .map(|k| k["name"].as_str().unwrap_or_default().to_string())
        .collect();
    let yes = |b: bool| if b { "yes" } else { "no" };
    let verdict_line = format!(
        "verdict: an entry reached kept over ≥ {KEEP_MIN_MATCHED} matched sessions: {}{}; recurrence fell on a covered trigger: {}{}",
        yes(!reached.is_empty()),
        if reached.is_empty() { String::new() } else { format!(" ({})", reached.join(", ")) },
        yes(!fell_on.is_empty()),
        if fell_on.is_empty() { String::new() } else { format!(" ({})", fell_on.join(", ")) },
    );

    let mut lines = Vec::new();
    if kept.is_empty() {
        lines.push("kept: none".to_string());
    }
    for k in &kept {
        lines.push(format!(
            "kept {}{}: {} matched session(s) in the window ({}), recurrence {}, harm {}",
            k["name"].as_str().unwrap_or_default(),
            if k["builtin"] == true { " (built-in)" } else { "" },
            k["matched"],
            if k["reached"] == true { format!("≥ {KEEP_MIN_MATCHED}") } else { format!("< {KEEP_MIN_MATCHED}") },
            k["recurrence"],
            k["harm"],
        ));
    }
    if trigger_lines.is_empty() {
        lines.push("triggers: no matched session in the window".into());
    }
    lines.extend(trigger_lines);
    lines.push(verdict_line);
    let out = json!({
        "kept": kept,
        "triggers": triggers,
        "verdict": {"kept_over_min": !reached.is_empty(), "kept_entries": reached,
                    "recurrence_fell": !fell_on.is_empty(), "fell_on": fell_on},
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
        assert_eq!((kept["name"].as_str(), kept["matched"].as_u64(), kept["reached"].as_bool()), (Some("run-tests-before-commit"), Some(4), Some(true)), "{out}");
        let t = out["triggers"].as_array().unwrap().iter().find(|t| t["names"] == "run-tests-before-commit").unwrap();
        assert_eq!(t["redirect_class"], "correction");
        assert_eq!(t["with"], json!({"sessions": 4, "recurred": 0, "redirects": 0, "rate": 0.0}), "{t}");
        assert_eq!(t["before"], json!({"sessions": 4, "recurred": 2, "redirects": 2, "rate": 0.5}), "the scope relabel is no recurrence: {t}");
        assert_eq!((t["delta"].as_f64(), t["fell"].as_bool(), t["covered"].as_bool()), (Some(-0.5), Some(true), Some(true)));
        let doc = out["triggers"].as_array().unwrap().iter().find(|t| t["names"] == "docstring-style").unwrap();
        assert_eq!((doc["with"]["sessions"].as_u64(), doc["before"]["sessions"].as_u64()), (Some(0), Some(0)));
        assert_eq!(out["verdict"], json!({"kept_over_min": true, "kept_entries": ["run-tests-before-commit"],
                                           "recurrence_fell": true, "fell_on": ["run-tests-before-commit"]}));
        assert_eq!(
            out["lines"].as_array().unwrap().last().unwrap(),
            "verdict: an entry reached kept over ≥ 4 matched sessions: yes (run-tests-before-commit); recurrence fell on a covered trigger: yes (run-tests-before-commit)"
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
        assert!(short["lines"].as_array().unwrap().last().unwrap().as_str().unwrap().ends_with("matched sessions: no; recurrence fell on a covered trigger: no"), "{short}");
        // --since the day before the old session: it joins the before-arm (3 of 5 recurred).
        let since = utc(t.ctx.now - 91 * DAY_MS)[..10].to_string();
        let long = cmd_report(&t.ctx, ReportArgs { window: "6w".into(), since: Some(since) }).unwrap();
        let tr = long["triggers"].as_array().unwrap().iter().find(|t| t["names"] == "run-tests-before-commit").unwrap().clone();
        assert_eq!((tr["before"]["sessions"].as_u64(), tr["before"]["recurred"].as_u64()), (Some(5), Some(3)), "{long}");
        // A window ending just before the keep: the entry was provisional then, nothing kept.
        t.ctx.now = e.status_since.unwrap() - 1;
        let out = report(&t.ctx);
        assert_eq!(out["kept"], json!([]), "{out}");
        assert_eq!(out["verdict"]["kept_over_min"], false, "{out}");
        assert!(parse_window("6x").is_err() && parse_window("0w").is_err() && parse_day("2026-13-01").is_err());
    }
}
