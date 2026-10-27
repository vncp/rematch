//! Readers over the lake (ADR 001 §"Readers, switched in L4", DESIGN §19). A reader asks
//! here for the rows it needs and falls back to today's path — the ledger, or the harness
//! transcripts for a session's records — when there is no lake, the binary was built
//! without the `lake` feature, or the lake cannot be read. Readers of journal tables also
//! fall back while the lake lags this ledger's journal (an export lag never hides a local
//! write); `rematch report` is the exception: it reads the lake alone and shows the lag.
//! Every reader names the [`Source`] it used in its envelope.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use rematch_core::ledger::JournalRow;
use rematch_core::trace::TraceRecord;
use rematch_core::types::SessionRecord;
use serde::Serialize;

use crate::app::AppCtx;

/// Where a reader's rows came from (`"source"` in its envelope).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Lake,
    Ledger,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Lake => "lake",
            Source::Ledger => "ledger",
        }
    }
}

/// Sources of a reader that reads one session at a time: reported as `"source"` (`lake`
/// when any session came from the lake) and `"sources"` (the counts).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub lake: usize,
    pub ledger: usize,
}

impl Tally {
    pub fn add(&mut self, s: Source) {
        match s {
            Source::Lake => self.lake += 1,
            Source::Ledger => self.ledger += 1,
        }
    }

    pub fn source(&self) -> Source {
        if self.lake > 0 {
            Source::Lake
        } else {
            Source::Ledger
        }
    }

    pub fn sources(&self) -> serde_json::Value {
        serde_json::json!({"lake": self.lake, "ledger": self.ledger})
    }
}

/// One key lookup from ADR 001's key table (`list:stems`, `temporal2:entry_id`, …). It only
/// narrows what the lake returns: callers apply their own filter to the rows either way,
/// so a lookup must select a superset of the rows the caller keeps.
#[derive(Debug, Clone, Default)]
pub struct Lookup {
    pub key: &'static str,
    pub values: Vec<String>,
    /// Inclusive ms UTC on the key's time column.
    pub window: Option<(i64, i64)>,
}

/// `REMATCH_ROOT/lake` when it holds a manifest and the binary can read Parquet.
pub fn lake(ctx: &AppCtx) -> Option<PathBuf> {
    imp::lake(ctx)
}

/// Journal rows this ledger wrote that the lake does not hold yet.
pub fn lag(ctx: &AppCtx) -> Result<i64> {
    imp::lag(ctx)
}

/// The lake when it holds every journal row this ledger wrote, else `None` (with the
/// reason on stderr when there is a lake).
pub fn current_lake(ctx: &AppCtx) -> Option<PathBuf> {
    let lake = lake(ctx)?;
    match lag(ctx) {
        Ok(0) => Some(lake),
        Ok(n) => {
            eprintln!("rematch: the lake is {n} journal row(s) behind the ledger; reading the ledger (`rematch lake export`)");
            None
        }
        Err(e) => {
            eprintln!("rematch: lake manifest unreadable, reading the ledger: {e:#}");
            None
        }
    }
}

/// Rows of one journal table from the lake, narrowed by `lookup` (indexed when `lake index`
/// built the key, else a full scan of the table's files).
pub fn lake_rows(ctx: &AppCtx, table: &str, lookup: Option<&Lookup>) -> Result<Vec<JournalRow>> {
    imp::lake_rows(ctx, table, lookup)
}

/// Rows of one journal table from the ledger's own journal (`keys`: only these row keys).
pub fn ledger_rows(ctx: &AppCtx, table: &str, keys: Option<&[String]>) -> Result<Vec<JournalRow>> {
    let conn = ctx.ledger.conn();
    let mut st = conn.prepare(
        "SELECT seq,known,machine,row_key,op,json FROM journal WHERE table_name=?1 AND (?2 IS NULL OR row_key=?2) ORDER BY seq",
    )?;
    let wanted: Vec<Option<String>> = match keys {
        None => vec![None],
        Some(keys) => keys.iter().cloned().map(Some).collect(),
    };
    let mut out = Vec::new();
    for key in wanted {
        let raw = st
            .query_map((table, key), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (seq, known, machine, row_key, op, text) in raw {
            out.push(JournalRow { seq, known, machine, table: table.to_string(), row_key, op, json: serde_json::from_str(&text)? });
        }
    }
    Ok(out)
}

/// Rows from `src`: the lake narrowed by `lookup`, or the ledger's journal (`keys`).
pub fn rows(ctx: &AppCtx, src: Source, table: &str, lookup: Option<&Lookup>, keys: Option<&[String]>) -> Result<Vec<JournalRow>> {
    match src {
        Source::Lake => lake_rows(ctx, table, lookup),
        Source::Ledger => ledger_rows(ctx, table, keys),
    }
}

/// ADR 001 §"Bitemporal semantics": per `(machine, row_key)` the row with the greatest
/// `known ≤ k` (ties: the later journal `seq`) — what the origin ledger held at `k`.
/// Ordered by `(machine, row_key)`.
pub fn as_of(rows: Vec<JournalRow>, k: i64) -> Vec<JournalRow> {
    let mut best: BTreeMap<(String, String), JournalRow> = BTreeMap::new();
    for r in rows.into_iter().filter(|r| r.known <= k) {
        let key = (r.machine.clone(), r.row_key.clone());
        match best.get(&key) {
            Some(b) if (b.known, b.seq) >= (r.known, r.seq) => {}
            _ => {
                best.insert(key, r);
            }
        }
    }
    best.into_values().collect()
}

/// The records of a closed session from `events/<harness>/<day>/<session>.parquet`, by path
/// (no index); `None` when there is no lake, no such file, or it cannot be read.
pub fn lake_events(ctx: &AppCtx, s: &SessionRecord) -> Option<Vec<TraceRecord>> {
    imp::lake_events(ctx, s)
}

/// Ids of the sessions whose events hold a bash command with every word of `words`
/// (`events` `text:tool_cmd`: indexed when `lake index` built it, else a scan), every
/// machine's sessions. Errors without the `lake` feature.
pub fn lake_sessions_that_ran(ctx: &AppCtx, words: &[String]) -> Result<std::collections::BTreeSet<String>> {
    imp::lake_sessions_that_ran(ctx, words)
}

#[cfg(feature = "lake")]
mod imp {
    use std::path::PathBuf;

    use anyhow::{anyhow, Result};
    use rematch_core::ledger::JournalRow;
    use rematch_core::trace::TraceRecord;
    use rematch_core::types::SessionRecord;
    use serde_json::Value;

    use super::Lookup;
    use crate::app::AppCtx;

    pub fn lake(ctx: &AppCtx) -> Option<PathBuf> {
        let lake = rematch_lake::lake_dir(&ctx.paths.root);
        rematch_lake::Manifest::path(&lake).is_file().then_some(lake)
    }

    pub fn lag(ctx: &AppCtx) -> Result<i64> {
        let lake = rematch_lake::lake_dir(&ctx.paths.root);
        let m = rematch_lake::Manifest::load(&lake)?;
        let have = m.watermarks.get(ctx.ledger.machine_id()).copied().unwrap_or(0);
        Ok((ctx.ledger.journal_max_seq()? - have).max(0))
    }

    fn journal_row(table: &str, v: Value) -> Result<JournalRow> {
        let int = |k: &str| v.get(k).and_then(Value::as_i64).ok_or_else(|| anyhow!("lake {table} row without {k}"));
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| anyhow!("lake {table} row without {k}"));
        let json = match v.get("json") {
            Some(Value::String(s)) => serde_json::from_str(s)?,
            Some(j @ Value::Object(_)) => j.clone(),
            _ => return Err(anyhow!("lake {table} row without json")),
        };
        Ok(JournalRow {
            seq: int("seq")?,
            known: int("known")?,
            machine: text("machine")?,
            table: table.to_string(),
            row_key: text("row_key")?,
            op: text("op")?,
            json,
        })
    }

    pub fn lake_rows(ctx: &AppCtx, table: &str, lookup: Option<&Lookup>) -> Result<Vec<JournalRow>> {
        let root = &ctx.paths.root;
        let raw = match lookup {
            None => rematch_lake::query::scan(root, table)?,
            Some(l) => {
                let spec = rematch_lake::query::QuerySpec {
                    table: table.to_string(),
                    key: l.key.to_string(),
                    values: l.values.clone(),
                    window: l.window,
                    asof: None,
                    latest: false,
                    all: false,
                    limit: None,
                };
                rematch_lake::query::run(root, &spec, None)?.rows
            }
        };
        raw.into_iter().map(|v| journal_row(table, v)).collect()
    }

    pub fn lake_sessions_that_ran(ctx: &AppCtx, words: &[String]) -> Result<std::collections::BTreeSet<String>> {
        let spec = rematch_lake::query::QuerySpec {
            table: "events".into(),
            key: "text:tool_cmd".into(),
            values: words.to_vec(),
            ..Default::default()
        };
        let out = rematch_lake::query::run(&ctx.paths.root, &spec, None)?;
        Ok(out.rows.iter().filter_map(|r| r["session_id"].as_str().map(str::to_string)).collect())
    }

    pub fn lake_events(ctx: &AppCtx, s: &SessionRecord) -> Option<Vec<TraceRecord>> {
        let lake = rematch_lake::lake_dir(&ctx.paths.root);
        let path = rematch_lake::events::find_events_file(&lake, &s.id, s.started_at)?;
        match rematch_lake::events::read_records(&path) {
            Ok(values) => Some(values.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect()),
            Err(e) => {
                eprintln!("rematch: {}: {e:#}; reading the session from its trace or transcript", path.display());
                None
            }
        }
    }
}

#[cfg(not(feature = "lake"))]
mod imp {
    use std::path::PathBuf;

    use anyhow::{bail, Result};
    use rematch_core::ledger::JournalRow;
    use rematch_core::trace::TraceRecord;
    use rematch_core::types::SessionRecord;

    use super::Lookup;
    use crate::app::AppCtx;

    pub fn lake(_ctx: &AppCtx) -> Option<PathBuf> {
        None
    }

    pub fn lag(_ctx: &AppCtx) -> Result<i64> {
        Ok(0)
    }

    pub fn lake_rows(_ctx: &AppCtx, _table: &str, _lookup: Option<&Lookup>) -> Result<Vec<JournalRow>> {
        bail!("built without the lake feature")
    }

    pub fn lake_sessions_that_ran(_ctx: &AppCtx, _words: &[String]) -> Result<std::collections::BTreeSet<String>> {
        bail!("built without the lake feature")
    }

    pub fn lake_events(_ctx: &AppCtx, _s: &SessionRecord) -> Option<Vec<TraceRecord>> {
        None
    }
}

/// Test helper: `lake export --all` into the context's root.
#[cfg(all(test, feature = "lake"))]
pub(crate) fn export_all(ctx: &AppCtx) {
    crate::lake_cmd::cmd_lake(ctx, crate::lake_cmd::LakeCmd::Export(crate::lake_cmd::ExportArgs { all: true })).unwrap();
}
