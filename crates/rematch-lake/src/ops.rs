//! `lake export | import | rebuild | doctor` (ADR 001 §"Commands").

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rematch_core::ledger::{now_ms, JournalRow, Ledger, SessionFilter};
use rematch_core::trace::TraceRecord;
use rematch_core::types::SessionRecord;
use serde_json::{json, Value};

use crate::events::{self, day_of};
use crate::io::{copy_once, read_journal_rows, walk, write_once};
use crate::tables::{self, path_component, TABLES};
use crate::Manifest;

/// Bounds of one export run. `reflect --pending` and a plain `lake export` take one chunk
/// of the journal and a bounded number of sessions; `--all` drains both.
#[derive(Debug, Clone, Copy)]
pub struct ExportOpts {
    pub all: bool,
    /// Journal rows per run (one file per table per run).
    pub max_rows: usize,
    /// Closed sessions whose events one run writes.
    pub max_sessions: usize,
}

impl Default for ExportOpts {
    fn default() -> Self {
        ExportOpts { all: false, max_rows: 50_000, max_sessions: 64 }
    }
}

/// Trace records of a ledger session that has no trace file: the binary synthesizes them
/// from the harness transcript (the converter `reflect --history` uses). `Ok(None)` when
/// the transcript is gone; `Err` when it is unreadable.
pub trait EventSource {
    fn records(&self, session: &SessionRecord) -> Result<Option<Vec<TraceRecord>>>;
}

fn rel(lake: &Path, p: &Path) -> String {
    p.strip_prefix(lake).unwrap_or(p).to_string_lossy().into_owned()
}

fn today() -> String {
    day_of(now_ms())
}

/// `{machine}-{seq}` of an append-table file name, `seq` digits only.
pub fn parse_run_name(file: &str) -> Option<(&str, i64)> {
    let stem = file.strip_suffix(".parquet")?;
    let (machine, seq) = stem.rsplit_once('-')?;
    if machine.is_empty() || machine.starts_with('.') || seq.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((machine, seq.parse().ok()?))
}

fn is_day(s: &str) -> bool {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() && s.len() == 10
}

fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Where a lake file sits in the layout, or why it does not.
enum Placement {
    /// `{table}/[{domain}/]{day}/{machine}-{seq}.parquet`.
    Run { table: &'static str, machine: String, seq: i64 },
    /// `events/{harness}/{day}/{session_id}.parquet`.
    Events { harness: String, file: String },
    Blob,
    Manifest,
    Orphan(&'static str),
}

fn place(lake: &Path, p: &Path) -> Placement {
    let Ok(r) = p.strip_prefix(lake) else { return Placement::Orphan("outside the lake") };
    let parts: Vec<&str> = r.iter().filter_map(|c| c.to_str()).collect();
    let Some(file) = parts.last().copied() else { return Placement::Orphan("empty path") };
    if file.starts_with('.') {
        return Placement::Orphan("temp or hidden file (an interrupted write)");
    }
    match parts.as_slice() {
        ["manifest.json"] => Placement::Manifest,
        ["blobs", name] if is_sha(name) => Placement::Blob,
        ["events", harness, day, f] if is_day(day) && f.ends_with(".parquet") && f.len() > 8 => {
            Placement::Events { harness: harness.to_string(), file: f.to_string() }
        }
        [t, rest @ ..] => {
            let Some(table) = TABLES.iter().copied().find(|x| x == t) else {
                return Placement::Orphan("not a lake table root");
            };
            let ok_shape = match (table, rest) {
                ("outcomes", [_domain, day, _]) => is_day(day),
                ("outcomes", _) => false,
                (_, [day, _]) => is_day(day),
                _ => false,
            };
            if !ok_shape {
                return Placement::Orphan("not at {table}/[{domain}/]{day}/{machine}-{seq}.parquet");
            }
            match parse_run_name(file) {
                Some((machine, seq)) => Placement::Run { table, machine: machine.to_string(), seq },
                None => Placement::Orphan("file name is not {machine}-{seq}.parquet"),
            }
        }
        _ => Placement::Orphan("unknown"),
    }
}

/// Resolve `--from`: a lake directory, or a REMATCH_ROOT holding one.
fn resolve_lake(from: &Path) -> Result<PathBuf> {
    if Manifest::path(from).is_file() {
        return Ok(from.to_path_buf());
    }
    let inner = crate::lake_dir(from);
    if Manifest::path(&inner).is_file() {
        return Ok(inner);
    }
    bail!("{} is not a lake (no manifest.json, nor lake/manifest.json)", from.display())
}

// ------------------------------------------------------------------------------------
// export

/// Journal → per-run Parquet files past this machine's watermark, then events for closed
/// sessions not yet exported: a trace file when there is one, else the records `source`
/// gives for a ledger session. Idempotent: a second run with nothing new writes nothing.
pub fn export(ledger: &Ledger, lake: &Path, traces: &Path, source: &dyn EventSource, opts: ExportOpts) -> Result<Value> {
    let mut m = Manifest::load(lake)?;
    let me = ledger.machine_id().to_string();
    let from = m.watermarks.get(&me).copied().unwrap_or(0);
    let rows = ledger.journal_since(from)?;
    let chunk = opts.max_rows.max(1);
    let mut files: Vec<String> = Vec::new();
    let mut exported = 0usize;
    let mut blobs = 0usize;
    let mut done = 0usize;
    for part in rows.chunks(chunk) {
        let day = today();
        let mut groups: BTreeMap<(&str, Option<String>), Vec<&JournalRow>> = BTreeMap::new();
        for r in part.iter().filter(|r| r.machine == me) {
            let Some(t) = TABLES.iter().copied().find(|t| *t == r.table) else { continue };
            groups.entry((t, tables::partition(r))).or_default().push(r);
        }
        for ((table, partition), rs) in &groups {
            let cols = tables::columns(table).expect("known table");
            let cells: Vec<_> = rs.iter().map(|r| tables::cells(r)).collect();
            let batch = crate::columns::batch(&cols, &cells)?;
            let mut p = lake.join(table);
            if let Some(d) = partition {
                p.push(d);
            }
            p.push(&day);
            p.push(format!("{}-{}.parquet", path_component(&me), rs[0].seq));
            if write_once(&p, &batch)? {
                files.push(rel(lake, &p));
            }
            exported += rs.len();
            blobs += copy_blobs(ledger.blobs_dir(), lake, rs)?;
        }
        done += part.len();
        if let Some(last) = part.last() {
            m.watermarks.insert(me.clone(), last.seq);
            m.save(lake)?;
        }
        if !opts.all {
            break;
        }
    }
    let ev = export_events(&mut m, ledger, lake, traces, source, opts)?;
    if ev.changed || !Manifest::path(lake).exists() {
        m.save(lake)?;
    }
    let watermark = m.watermarks.get(&me).copied().unwrap_or(0);
    let mut lines = vec![format!(
        "lake export: {exported} journal row(s) → {} file(s); {} session(s) → events; watermark {me}={watermark}",
        files.len(),
        ev.written.len()
    )];
    if !ev.missing.is_empty() {
        lines.push(format!(
            "lake export: {} session(s) with no trace or harness transcript, recorded as missing (retried by `lake export --all`)",
            ev.missing.len()
        ));
    }
    let remaining_rows = rows.len() - done;
    if remaining_rows > 0 || ev.remaining > 0 {
        lines.push(format!(
            "lake export: {remaining_rows} journal row(s) and {} session(s) left for the next run (or `lake export --all`)",
            ev.remaining
        ));
    }
    Ok(json!({
        "lake": lake,
        "machine": me,
        "from_seq": from,
        "watermark": watermark,
        "rows": exported,
        "files": files,
        "events": ev.written,
        "events_missing": ev.missing,
        "blobs": blobs,
        "remaining_rows": remaining_rows,
        "remaining_sessions": ev.remaining,
        "lines": lines,
    }))
}

/// Proposal replies and receipt certificates the rows point at, copied to `lake/blobs/`.
fn copy_blobs(src: &Path, lake: &Path, rows: &[&JournalRow]) -> Result<usize> {
    let mut n = 0;
    for r in rows {
        let sha = match r.table.as_str() {
            "proposals" => r.json.get("blob"),
            "receipts" => r.json.get("receipt").and_then(|x| x.get("certificate")),
            _ => None,
        };
        let Some(sha) = sha.and_then(Value::as_str).filter(|s| is_sha(s)) else { continue };
        let from = src.join(sha);
        if from.is_file() && copy_once(&from, &lake.join("blobs").join(sha))? {
            n += 1;
        }
    }
    Ok(n)
}

/// What one run did with closed sessions.
struct EventsRun {
    /// Lake-relative paths of the events files written.
    written: Vec<String>,
    /// Sessions newly recorded as missing.
    missing: Vec<String>,
    /// Sessions left for the next run by the per-run cap.
    remaining: usize,
    /// The manifest gained an exported or missing session.
    changed: bool,
}

/// Where a session's events come from.
enum Origin<'a> {
    Trace(PathBuf),
    Ledger(&'a SessionRecord),
}

fn trace_files(traces: &Path) -> Result<Vec<(String, PathBuf)>> {
    match std::fs::read_dir(traces) {
        Ok(rd) => Ok(rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .filter_map(|p| Some((p.file_stem()?.to_str()?.to_string(), p)))
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", traces.display())),
    }
}

/// Closed sessions not yet under `events/`, in session-id order: every closed trace file,
/// then every ledger session without one (an imported session's records come from
/// `source`). A session recorded missing is retried only with `--all`. The cap counts
/// sessions a run settles (written, or recorded missing); an open trace never counts.
fn export_events(
    m: &mut Manifest,
    ledger: &Ledger,
    lake: &Path,
    traces: &Path,
    source: &dyn EventSource,
    opts: ExportOpts,
) -> Result<EventsRun> {
    let mut todo: BTreeMap<String, Origin> = BTreeMap::new();
    let mut traced = BTreeSet::new();
    for (sid, p) in trace_files(traces)? {
        traced.insert(sid.clone());
        if !m.events_exported.contains_key(&sid) {
            todo.insert(sid, Origin::Trace(p));
        }
    }
    let sessions = ledger.list_sessions(&SessionFilter::default())?;
    for s in &sessions {
        if traced.contains(&s.id) || m.events_exported.contains_key(&s.id) || (!opts.all && m.events_missing.contains(&s.id)) {
            continue;
        }
        todo.insert(s.id.clone(), Origin::Ledger(s));
    }
    let mut run = EventsRun { written: Vec::new(), missing: Vec::new(), remaining: 0, changed: false };
    let mut settled = 0usize;
    for (sid, origin) in todo {
        if !opts.all && settled >= opts.max_sessions {
            run.remaining += 1;
            continue;
        }
        let trace = match &origin {
            Origin::Trace(path) => match events::read_trace(path) {
                Ok(t) if t.closed && !t.records.is_empty() => t,
                Ok(_) => continue,
                Err(e) => {
                    eprintln!("rematch lake export: {e:#}");
                    continue;
                }
            },
            Origin::Ledger(s) => match source.records(s) {
                Ok(Some(rs)) if !rs.is_empty() => events::Trace {
                    records: rs.iter().map(serde_json::to_value).collect::<Result<_, _>>()?,
                    closed: true,
                    start: s.started_at,
                },
                Ok(_) => {
                    settled += 1;
                    if m.events_missing.insert(sid.clone()) {
                        run.missing.push(sid);
                        run.changed = true;
                    }
                    continue;
                }
                Err(e) => {
                    eprintln!("rematch lake export: session {sid}: {e:#}");
                    continue;
                }
            },
        };
        let p = events::events_path(lake, &sid, trace.start);
        if write_once(&p, &events::batch(&trace)?)? {
            run.written.push(rel(lake, &p));
        }
        settled += 1;
        m.events_missing.remove(&sid);
        m.events_exported.insert(sid, true);
        run.changed = true;
    }
    Ok(run)
}

// ------------------------------------------------------------------------------------
// import

/// Copies another lake's partition files that are absent here — by `(table, machine, seq)`
/// for append tables, `(harness, session_id)` for events — and its blobs, then merges the
/// watermarks per machine. Never touches `lake-index/` (it is outside every lake).
pub fn import(lake: &Path, from: &Path) -> Result<Value> {
    let src = resolve_lake(from)?;
    if src.canonicalize().ok() == lake.canonicalize().ok() {
        bail!("--from {} is this lake", from.display());
    }
    let theirs = Manifest::load(&src)?;
    let mut have_runs: BTreeSet<(&'static str, String, i64)> = BTreeSet::new();
    let mut have_events: BTreeSet<(String, String)> = BTreeSet::new();
    for p in walk(lake)? {
        match place(lake, &p) {
            Placement::Run { table, machine, seq } => {
                have_runs.insert((table, machine, seq));
            }
            Placement::Events { harness, file } => {
                have_events.insert((harness, file));
            }
            _ => {}
        }
    }
    let mut copied: BTreeMap<&str, usize> = BTreeMap::new();
    let mut skipped = 0usize;
    for p in walk(&src)? {
        let fresh = match place(&src, &p) {
            Placement::Run { table, machine, seq } => have_runs.insert((table, machine, seq)).then_some(table),
            Placement::Events { harness, file } => have_events.insert((harness, file)).then_some("events"),
            Placement::Blob => Some("blobs"),
            Placement::Manifest | Placement::Orphan(_) => None,
        };
        let Some(kind) = fresh else {
            skipped += 1;
            continue;
        };
        let dst = lake.join(p.strip_prefix(&src).expect("walked under src"));
        if copy_once(&p, &dst)? {
            *copied.entry(kind).or_default() += 1;
        }
    }
    let mut m = Manifest::load(lake)?;
    for (machine, seq) in &theirs.watermarks {
        let w = m.watermarks.entry(machine.clone()).or_insert(0);
        *w = (*w).max(*seq);
    }
    m.save(lake)?;
    let total: usize = copied.values().sum();
    Ok(json!({
        "lake": lake,
        "from": src,
        "copied": copied,
        "skipped": skipped,
        "watermarks": m.watermarks,
        "lines": [format!("lake import: {total} file(s) from {}; watermarks {}", src.display(),
            m.watermarks.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "))],
    }))
}

// ------------------------------------------------------------------------------------
// rebuild

/// Every journal row in a lake's append tables, per machine, in `seq` order (a `seq`
/// seen twice keeps its first row).
pub fn journal_rows(lake: &Path) -> Result<BTreeMap<String, Vec<JournalRow>>> {
    let mut per: BTreeMap<String, BTreeMap<i64, JournalRow>> = BTreeMap::new();
    for p in walk(lake)? {
        if let Placement::Run { table, .. } = place(lake, &p) {
            for r in read_journal_rows(&p, table)? {
                per.entry(r.machine.clone()).or_default().entry(r.seq).or_insert(r);
            }
        }
    }
    Ok(per.into_iter().map(|(k, v)| (k, v.into_values().collect())).collect())
}

fn ledger_rows(l: &Ledger) -> Result<i64> {
    Ok(l.conn().query_row(
        "SELECT (SELECT COUNT(*) FROM journal) + (SELECT COUNT(*) FROM entries) + (SELECT COUNT(*) FROM sessions)",
        [],
        |r| r.get(0),
    )?)
}

fn sidecars(p: &Path) -> [PathBuf; 3] {
    let s = p.to_string_lossy();
    [p.to_path_buf(), PathBuf::from(format!("{s}-wal")), PathBuf::from(format!("{s}-shm"))]
}

fn move_db(from: &Path, to: &Path) -> Result<()> {
    for (a, b) in sidecars(from).iter().zip(sidecars(to)) {
        if a.exists() {
            std::fs::rename(a, &b).with_context(|| format!("rename {} → {}", a.display(), b.display()))?;
        }
    }
    Ok(())
}

/// A fresh ledger at `ledger_file` from the journal partitions of `lake`: one machine's
/// rows (this machine's unless `machine`) replayed through [`Ledger::apply_journal_row`]
/// in `seq` order into a new file, which then takes the place of the old one. Refuses a
/// non-empty ledger unless `force` (the old file is then kept beside it).
pub fn rebuild(ledger_file: &Path, lake: &Path, machine: Option<&str>, force: bool) -> Result<Value> {
    let src = resolve_lake(lake)?;
    if ledger_file.exists() {
        let n = ledger_rows(&Ledger::open(ledger_file)?)?;
        if n > 0 && !force {
            bail!(
                "the ledger at {} is not empty ({n} rows); pass --force to replace it (the current file is kept as {}.pre-rebuild-<ms>)",
                ledger_file.display(),
                ledger_file.display()
            );
        }
    }
    let per = journal_rows(&src)?;
    let tmp = PathBuf::from(format!("{}.rebuild-{}", ledger_file.display(), std::process::id()));
    for p in sidecars(&tmp) {
        let _ = std::fs::remove_file(p);
    }
    let fresh = Ledger::open(&tmp)?;
    let me = machine.map(str::to_string).unwrap_or_else(|| fresh.machine_id().to_string());
    let Some(rows) = per.get(&me).filter(|r| !r.is_empty()) else {
        drop(fresh);
        for p in sidecars(&tmp) {
            let _ = std::fs::remove_file(p);
        }
        let have: Vec<&String> = per.keys().collect();
        bail!("the lake at {} has no journal rows of machine {me:?} (machines: {have:?}); pass --machine", src.display());
    };
    let mut applied: BTreeMap<String, usize> = BTreeMap::new();
    let res = (|| -> Result<()> {
        for r in rows {
            fresh
                .apply_journal_row(r)
                .with_context(|| format!("apply journal row {} ({} {})", r.seq, r.table, r.row_key))?;
            *applied.entry(r.table.clone()).or_default() += 1;
        }
        Ok(())
    })();
    let blobs_dir = fresh.blobs_dir().to_path_buf();
    drop(fresh);
    if let Err(e) = res {
        for p in sidecars(&tmp) {
            let _ = std::fs::remove_file(p);
        }
        return Err(e);
    }
    let mut blobs = 0usize;
    for p in walk(&src.join("blobs"))? {
        if let Some(name) = p.file_name().and_then(|n| n.to_str()).filter(|n| is_sha(n)) {
            if copy_once(&p, &blobs_dir.join(name))? {
                blobs += 1;
            }
        }
    }
    let backup = if ledger_file.exists() {
        let b = PathBuf::from(format!("{}.pre-rebuild-{}", ledger_file.display(), now_ms()));
        move_db(ledger_file, &b)?;
        Some(b)
    } else {
        None
    };
    move_db(&tmp, ledger_file)?;
    let last = rows.last().map(|r| r.seq).unwrap_or(0);
    Ok(json!({
        "ledger": ledger_file,
        "from": src,
        "machine": me,
        "rows": rows.len(),
        "last_seq": last,
        "applied": applied,
        "blobs": blobs,
        "backup": backup,
        "lines": [format!("lake rebuild: {} journal row(s) of {me} (seq ≤ {last}) → {}{}", rows.len(), ledger_file.display(),
            backup.as_ref().map(|b| format!("; previous ledger kept at {}", b.display())).unwrap_or_default())],
    }))
}

// ------------------------------------------------------------------------------------
// doctor

/// Watermarks, export lag, files per table, events progress (ledger sessions exported,
/// missing a source, pending), orphan files, and the journal's pre-journal snapshot range.
pub fn doctor(ledger: &Ledger, lake: &Path) -> Result<Value> {
    let m = Manifest::load(lake)?;
    let me = ledger.machine_id().to_string();
    let max_seq = ledger.journal_max_seq()?;
    let wm = m.watermarks.get(&me).copied().unwrap_or(0);
    let lag = (max_seq - wm).max(0);
    let mut files: BTreeMap<&str, usize> = TABLES.iter().map(|t| (*t, 0)).collect();
    let mut events = 0usize;
    let mut blobs = 0usize;
    let mut orphans = Vec::new();
    for p in walk(lake)? {
        match place(lake, &p) {
            Placement::Run { table, machine, seq } => {
                *files.entry(table).or_default() += 1;
                if m.watermarks.get(&machine).is_none_or(|w| seq > *w) {
                    orphans.push(json!({"path": rel(lake, &p), "reason": "past the manifest watermark of its machine"}));
                }
            }
            Placement::Events { .. } => events += 1,
            Placement::Blob => blobs += 1,
            Placement::Manifest => {}
            Placement::Orphan(reason) => orphans.push(json!({"path": rel(lake, &p), "reason": reason})),
        }
    }
    let snapshot: (i64, Option<i64>, Option<i64>, Option<i64>, Option<i64>) = ledger.conn().query_row(
        "SELECT COUNT(*), MIN(seq), MAX(seq), MIN(known), MAX(known) FROM journal WHERE op='snapshot'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let snapshot_json = if snapshot.0 > 0 {
        json!({
            "rows": snapshot.0, "first_seq": snapshot.1, "last_seq": snapshot.2,
            "first_known": snapshot.3, "last_known": snapshot.4,
            "note": "rows up to last_seq are the ledger's state when the journal was added (op=snapshot), one row per record: transitions before then were never recorded",
        })
    } else {
        Value::Null
    };
    let session_ids: Vec<String> = ledger
        .conn()
        .prepare("SELECT id FROM sessions")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let pending = session_ids
        .iter()
        .filter(|id| !m.events_exported.contains_key(*id) && !m.events_missing.contains(*id))
        .count();
    let mut lines = vec![
        format!("lake: {} (lake_version {}, bellwether pinned: {})", lake.display(), m.lake_version,
            m.bellwether_version_pinned.as_deref().unwrap_or("none")),
        format!("machine {me}: journal seq {max_seq}, exported {wm}, lag {lag}"),
    ];
    for (k, v) in m.watermarks.iter().filter(|(k, _)| **k != me) {
        lines.push(format!("machine {k}: watermark {v} (imported)"));
    }
    lines.push(format!(
        "files: {}; events {events}; blobs {blobs}",
        files.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ")
    ));
    lines.push(format!(
        "events: exported {}, missing {}, pending {pending}",
        m.events_exported.len(),
        m.events_missing.len()
    ));
    if snapshot.0 > 0 {
        lines.push(format!(
            "pre-journal snapshot: seq {}..{} ({} rows): history before the journal is a snapshot, not transitions",
            snapshot.1.unwrap_or(0),
            snapshot.2.unwrap_or(0),
            snapshot.0
        ));
    }
    for o in &orphans {
        lines.push(format!("orphan: {} ({})", o["path"].as_str().unwrap_or("?"), o["reason"].as_str().unwrap_or("?")));
    }
    Ok(json!({
        "lake": lake,
        "exists": lake.is_dir(),
        "feature": "lake",
        "lake_version": m.lake_version,
        "bellwether_version_pinned": m.bellwether_version_pinned,
        "machine": me,
        "journal_max_seq": max_seq,
        "watermark": wm,
        "lag": lag,
        "watermarks": m.watermarks,
        "files": files,
        "events": events,
        "events_exported": m.events_exported.len(),
        "events_missing": m.events_missing.len(),
        "events_pending": pending,
        "blobs": blobs,
        "orphans": orphans,
        "snapshot": snapshot_json,
        "lines": lines,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_names_take_the_last_dash() {
        assert_eq!(parse_run_name("my-host-12.parquet"), Some(("my-host", 12)));
        assert_eq!(parse_run_name("host-x.parquet"), None);
        assert_eq!(parse_run_name("-3.parquet"), None);
        assert_eq!(parse_run_name("host-3.json"), None);
    }

    #[test]
    fn placement_follows_the_layout() {
        let lake = Path::new("/l");
        let run = |s: &str| matches!(place(lake, &lake.join(s)), Placement::Run { .. });
        assert!(run("entries/2026-10-26/h-1.parquet"));
        assert!(run("outcomes/quant/2026-10-26/h-1.parquet"));
        assert!(!run("outcomes/2026-10-26/h-1.parquet"));
        assert!(!run("entries/2026-13-26/h-1.parquet"));
        assert!(!run("entries/2026-10-26/.h-1.parquet.tmp-1"));
        assert!(matches!(place(lake, &lake.join("events/omp/2026-10-26/s.parquet")), Placement::Events { .. }));
        assert!(matches!(place(lake, &lake.join("lake-index/x")), Placement::Orphan(_)));
        assert!(matches!(place(lake, &lake.join(format!("blobs/{}", "a".repeat(64)))), Placement::Blob));
    }
}
