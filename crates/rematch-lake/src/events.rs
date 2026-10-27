//! `events/{harness}/{day}/{session_id}.parquet`: one row per trace record (DESIGN §3) of a
//! closed session, flattened; every record field without a column of its own is kept as
//! JSON in `extra`. One column is derived, not a trace field: `tool_cmd`, a bash call's
//! command (the `exact:` and `text:` keys' column). The file name is the session id through
//! [`rematch_core::paths::encode_file_stem`] (`cc:<uuid>` → `cc%3A<uuid>`), so one lake is
//! valid on every platform; a file exported before that codec keeps its raw-id name
//! ([`legacy_events_path`]) and is still found, counted and imported as the same session.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use arrow::array::{Array, BooleanArray, Int64Array, ListArray, StringArray, TimestampMillisecondArray};
use arrow::record_batch::RecordBatch;
use serde_json::{Map, Value};

use crate::columns::{b, i, js, l, opt, req, s, Cell, Col, Ty};
use crate::tables::path_component;

/// Columns of an events file, in order; the trace field each is read from is in
/// [`FIELDS`] (`tool_input` ← `input`, `usage_json` ← `usage`); `tool_cmd`, last, is
/// derived ([`command_of`]).
pub const COLUMNS: &[Col] = &[
    req("seq", Ty::Int64),
    req("t", Ty::TsMs),
    req("ev", Ty::Utf8),
    opt("tool", Ty::Utf8),
    opt("call_id", Ty::Utf8),
    opt("tool_input", Ty::Utf8),
    opt("args_hash", Ty::Utf8),
    opt("result_bytes", Ty::Int64),
    opt("is_error", Ty::Bool),
    opt("path", Ty::Utf8),
    opt("skill_read", Ty::Utf8),
    opt("text", Ty::Utf8),
    opt("prelabel", Ty::Utf8),
    opt("text_head", Ty::Utf8),
    opt("usage_json", Ty::Utf8),
    opt("stop_reason", Ty::Utf8),
    opt("reasons", Ty::ListUtf8),
    opt("flagged", Ty::Bool),
    opt("extra", Ty::Utf8),
    opt("tool_cmd", Ty::Utf8),
];

/// Trace field behind each column of [`COLUMNS`] except `extra`.
const FIELDS: [&str; 18] = [
    "seq", "t", "ev", "tool", "call_id", "input", "args_hash", "result_bytes", "is_error", "path",
    "skill_read", "text", "prelabel", "text_head", "usage", "stop_reason", "reasons", "flagged",
];

/// A parsed trace: its records and what the lake needs to place it.
pub struct Trace {
    pub records: Vec<Value>,
    /// A `session_shutdown` record is present (the session is closed).
    pub closed: bool,
    /// `t` of `session_start` (else of the first record), ms UTC.
    pub start: i64,
}

/// Reads a trace file; lines that are not JSON objects are skipped (a torn last write).
pub fn read_trace(path: &Path) -> Result<Trace> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read trace {}", path.display()))?;
    let records: Vec<Value> = text
        .lines()
        .filter(|x| !x.trim().is_empty())
        .filter_map(|x| serde_json::from_str::<Value>(x).ok())
        .filter(Value::is_object)
        .collect();
    let ev = |r: &Value| r.get("ev").and_then(Value::as_str).map(str::to_string);
    let closed = records.iter().any(|r| ev(r).as_deref() == Some("session_shutdown"));
    let t = |r: &Value| r.get("t").and_then(Value::as_i64);
    let start = records
        .iter()
        .find(|r| ev(r).as_deref() == Some("session_start"))
        .and_then(t)
        .or_else(|| records.iter().find_map(t))
        .unwrap_or(0);
    Ok(Trace { records, closed, start })
}

/// The harness of a session id (DESIGN §1: `cc:<uuid>` Claude Code, `codex:<id>` Codex,
/// `pi:<id>` Pi, `oc:<id>` OpenCode).
pub fn harness_of(session_id: &str) -> &'static str {
    if session_id.starts_with("cc:") {
        "claude-code"
    } else if session_id.starts_with("codex:") {
        "codex"
    } else if session_id.starts_with("pi:") {
        "pi"
    } else if session_id.starts_with("oc:") {
        "opencode"
    } else {
        "omp"
    }
}

/// UTC date `YYYY-MM-DD` of a ms timestamp.
pub fn day_of(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .format("%Y-%m-%d")
        .to_string()
}

/// `lake/events/{harness}/{day}/{session_id}.parquet` of a session starting at `start_ms`
/// (a trace's `session_start.t`; a ledger session's `started_at`); the name is
/// [`rematch_core::paths::encode_file_stem`] of the id on every platform.
pub fn events_path(lake: &Path, session_id: &str, start_ms: i64) -> PathBuf {
    lake.join("events")
        .join(harness_of(session_id))
        .join(day_of(start_ms))
        .join(format!("{}.parquet", rematch_core::paths::encode_file_stem(session_id)))
}

/// Where an export before the file-name codec wrote the session (`path_component` of the raw
/// id: `cc:<uuid>.parquet`); `None` when that is the [`events_path`] name or a name this
/// platform cannot hold (Windows: a `:` would address an NTFS stream of another file).
pub fn legacy_events_path(lake: &Path, session_id: &str, start_ms: i64) -> Option<PathBuf> {
    let name = path_component(session_id);
    if name == rematch_core::paths::encode_file_stem(session_id) {
        return None;
    }
    if cfg!(windows) && name.contains(['<', '>', ':', '"', '|', '?', '*']) {
        return None;
    }
    Some(lake.join("events").join(harness_of(session_id)).join(day_of(start_ms)).join(format!("{name}.parquet")))
}

/// The session's events file present in `lake`: [`events_path`], else [`legacy_events_path`].
pub fn find_events_file(lake: &Path, session_id: &str, start_ms: i64) -> Option<PathBuf> {
    let p = events_path(lake, session_id, start_ms);
    if p.is_file() {
        return Some(p);
    }
    legacy_events_path(lake, session_id, start_ms).filter(|p| p.is_file())
}

/// The session id an events file name (`<stem>.parquet`) holds: the encoded stem decoded, a
/// legacy raw-id name as is ([`rematch_core::paths::decode_file_stem`]).
pub fn session_of_file(file: &str) -> String {
    rematch_core::paths::decode_file_stem(file.strip_suffix(".parquet").unwrap_or(file)).into_owned()
}

/// The derived column: the command of a `bash` call (`input.command`), else `None`.
pub fn command_of(tool: Option<&str>, input: Option<&Value>) -> Option<String> {
    if tool != Some("bash") {
        return None;
    }
    input?.get("command")?.as_str().map(str::to_string)
}

fn cell_of(col: &Col, v: Option<&Value>) -> Cell {
    match col.name {
        "tool_input" | "usage_json" => js(v),
        _ => match col.ty {
            Ty::Int64 | Ty::TsMs => i(v),
            Ty::Bool => b(v),
            Ty::ListUtf8 => l(v),
            _ => s(v),
        },
    }
}

/// One record → cells aligned with [`COLUMNS`]. A field whose value does not fit its
/// column's type (and is not null) moves to `extra` instead of being dropped.
pub fn cells(record: &Value) -> Vec<Cell> {
    let cmd = command_of(record.get("tool").and_then(Value::as_str), record.get("input"));
    let mut rest: Map<String, Value> = record.as_object().cloned().unwrap_or_default();
    let mut out = Vec::with_capacity(COLUMNS.len());
    for (col, field) in COLUMNS.iter().zip(FIELDS) {
        let v = rest.remove(field);
        let c = cell_of(col, v.as_ref());
        if matches!(c, Cell::Null) {
            if let Some(x) = v.filter(|x| !x.is_null()) {
                rest.insert(field.to_string(), x);
            }
        }
        out.push(c);
    }
    out.push(if rest.is_empty() { Cell::Null } else { Cell::S(Value::Object(rest).to_string()) });
    out.push(cmd.map_or(Cell::Null, Cell::S));
    out
}

pub fn batch(trace: &Trace) -> Result<RecordBatch> {
    let rows: Vec<Vec<Cell>> = trace.records.iter().map(cells).collect();
    crate::columns::batch(COLUMNS, &rows)
}

/// Rewrites an events file exported before `tool_cmd` existed with the column appended
/// (the same records, as a fresh export writes them now); `false` when it has the column.
/// Bellwether takes one schema per root, so `lake index` upgrades such files before it
/// declares keys on the column — the one rewrite a lake file gets (DESIGN §19.1).
pub fn upgrade(path: &Path) -> Result<bool> {
    let f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let schema = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(f)
        .with_context(|| format!("read {}", path.display()))?
        .schema()
        .clone();
    if schema.field_with_name("tool_cmd").is_ok() {
        return Ok(false);
    }
    let records = read_records(path)?;
    let rows: Vec<Vec<Cell>> = records.iter().map(cells).collect();
    crate::io::replace(path, &crate::columns::batch(COLUMNS, &rows)?)?;
    Ok(true)
}

fn col<'a, T: 'static>(b: &'a RecordBatch, name: &str, path: &Path) -> Result<&'a T> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<T>())
        .ok_or_else(|| anyhow!("{}: column {name} missing or mistyped", path.display()))
}

/// The value of row `r` of `col` as the trace field it came from; `None` when null.
fn value_of(b: &RecordBatch, c: &Col, r: usize, path: &Path) -> Result<Option<Value>> {
    let a = b.column_by_name(c.name).ok_or_else(|| anyhow!("{}: column {} missing", path.display(), c.name))?;
    if a.is_null(r) {
        return Ok(None);
    }
    Ok(Some(match c.ty {
        Ty::Int64 => Value::from(col::<Int64Array>(b, c.name, path)?.value(r)),
        Ty::TsMs => Value::from(col::<TimestampMillisecondArray>(b, c.name, path)?.value(r)),
        Ty::Bool => Value::from(col::<BooleanArray>(b, c.name, path)?.value(r)),
        Ty::ListUtf8 => {
            let items = col::<ListArray>(b, c.name, path)?.value(r);
            let items = items.as_any().downcast_ref::<StringArray>().ok_or_else(|| anyhow!("{}: {} items", path.display(), c.name))?;
            Value::Array((0..items.len()).map(|k| Value::from(items.value(k))).collect())
        }
        _ => {
            let text = col::<StringArray>(b, c.name, path)?.value(r);
            match c.name {
                "tool_input" | "usage_json" | "extra" => {
                    serde_json::from_str(text).with_context(|| format!("{}: {} is not JSON", path.display(), c.name))?
                }
                _ => Value::from(text),
            }
        }
    }))
}

/// The trace records of an events file, the inverse of [`cells`]: columns back under their
/// trace field names (`tool_input` → `input`, `usage_json` → `usage`, both parsed), `extra`
/// merged in, null columns omitted. Each deserializes as a DESIGN §3 trace record.
pub fn read_records(path: &Path) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for b in crate::io::read_batches(path)? {
        for r in 0..b.num_rows() {
            let mut rec = Map::new();
            for (c, field) in COLUMNS.iter().zip(FIELDS) {
                if let Some(v) = value_of(&b, c, r, path)? {
                    rec.insert(field.to_string(), v);
                }
            }
            if let Some(Value::Object(extra)) = value_of(&b, &COLUMNS[FIELDS.len()], r, path)? {
                rec.extend(extra);
            }
            out.push(Value::Object(rec));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_flatten_with_the_rest_in_extra() {
        let r = json!({"seq":2,"t":5,"ev":"tool_result","tool":"bash","call_id":"c1","input":{"command":"cargo test"},
                       "args_hash":"h","result_bytes":10,"is_error":false,"error_class":null,"content_head":"ok","skill_read":null,"path":null});
        let c = cells(&r);
        assert_eq!(c.len(), COLUMNS.len());
        assert_eq!(c[0], Cell::I(2));
        assert_eq!(c[5], Cell::S(r#"{"command":"cargo test"}"#.into()));
        assert_eq!(c[18], Cell::S(r#"{"content_head":"ok","error_class":null}"#.into()), "other fields verbatim, nulls included");
        let mistyped = cells(&json!({"seq":1,"t":1,"ev":"x","is_error":"yes"}));
        assert_eq!(mistyped[8], Cell::Null);
        assert_eq!(mistyped[18], Cell::S(r#"{"is_error":"yes"}"#.into()));
        assert_eq!(c[19], Cell::S("cargo test".into()), "tool_cmd: the bash command");
        assert_eq!(mistyped[19], Cell::Null);
        let other = cells(&json!({"seq":1,"t":1,"ev":"tool_result","tool":"python","input":{"command":"x"}}));
        assert_eq!(other[19], Cell::Null, "only bash calls have a tool_cmd");
    }

    #[test]
    fn read_records_inverts_cells() {
        use rematch_core::trace::TraceRecord;
        let originals: Vec<TraceRecord> = [
            json!({"seq":0,"t":3000,"ev":"session_start","session_id":"k-0","cwd":"/r","repo":"/r",
                   "model":{"provider":"anthropic","id":"claude-opus-5-5"},"loaded_entries":["e1"]}),
            json!({"seq":1,"t":3001,"ev":"input","text":"Edit the ledger","idle":true,"prelabel":"none"}),
            json!({"seq":2,"t":3002,"ev":"tool_result","tool":"bash","call_id":"c1","input":{"command":"cargo test"},
                   "args_hash":"h","result_bytes":12,"is_error":false,"error_class":null,"content_head":"ok","skill_read":null,"path":null}),
            json!({"seq":3,"t":3003,"ev":"turn_end","usage":{"input":10,"output":5},"stop_reason":"stop","text_head":"Done."}),
            json!({"seq":4,"t":3004,"ev":"session_shutdown","reasons":["redirect:negation"],"flagged":true}),
        ]
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect();
        let trace = Trace {
            records: originals.iter().map(|r| serde_json::to_value(r).unwrap()).collect(),
            closed: true,
            start: 3000,
        };
        let dir = tempfile::tempdir().unwrap();
        let p = events_path(dir.path(), "k-0", trace.start);
        assert!(p.ends_with("events/omp/1970-01-01/k-0.parquet"), "{}", p.display());
        crate::io::write_once(&p, &batch(&trace).unwrap()).unwrap();
        let back: Vec<TraceRecord> = read_records(&p).unwrap().into_iter().map(|v| serde_json::from_value(v).unwrap()).collect();
        assert_eq!(back, originals);
        assert!(!upgrade(&p).unwrap(), "a current file is left alone");

        // A file exported before tool_cmd: upgraded in place to what an export writes now.
        let old = dir.path().join("old.parquet");
        let cols = &COLUMNS[..COLUMNS.len() - 1];
        let rows: Vec<Vec<Cell>> = trace.records.iter().map(|r| cells(r)[..cols.len()].to_vec()).collect();
        crate::io::write_once(&old, &crate::columns::batch(cols, &rows).unwrap()).unwrap();
        assert!(upgrade(&old).unwrap());
        assert_eq!(std::fs::read(&old).unwrap(), std::fs::read(&p).unwrap(), "byte-identical to a fresh export");
        assert!(!upgrade(&old).unwrap());
    }

    #[test]
    fn harness_and_day() {
        assert_eq!(harness_of("cc:1"), "claude-code");
        assert_eq!(harness_of("codex:1"), "codex");
        assert_eq!(harness_of("pi:1:r2"), "pi");
        assert_eq!(harness_of("oc:ses_1"), "opencode");
        assert_eq!(harness_of("019a"), "omp");
        assert_eq!(day_of(0), "1970-01-01");
        assert_eq!(day_of(1_761_436_800_000), "2025-10-26");
    }

    #[test]
    fn events_file_names_are_portable_and_legacy_names_still_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let lake = dir.path();
        let p = events_path(lake, "cc:abc", 0);
        assert!(p.ends_with("events/claude-code/1970-01-01/cc%3Aabc.parquet"), "{}", p.display());
        assert_eq!(session_of_file("cc%3Aabc.parquet"), "cc:abc");
        assert_eq!(session_of_file("cc:abc.parquet"), "cc:abc", "a legacy raw-id name");
        assert_eq!(session_of_file("a%2541.parquet"), "a%41", "decoded once");
        assert_eq!(legacy_events_path(lake, "k-0", 0), None, "an omp id never changed name");
        assert_eq!(find_events_file(lake, "cc:abc", 0), None);
        let legacy = legacy_events_path(lake, "cc:abc", 0);
        if cfg!(windows) {
            assert_eq!(legacy, None);
        } else {
            let legacy = legacy.unwrap();
            assert!(legacy.ends_with("events/claude-code/1970-01-01/cc:abc.parquet"));
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, "x").unwrap();
            assert_eq!(find_events_file(lake, "cc:abc", 0), Some(legacy.clone()));
            std::fs::write(&p, "y").unwrap();
            assert_eq!(find_events_file(lake, "cc:abc", 0), Some(p.clone()), "the current name wins");
        }
    }
}
