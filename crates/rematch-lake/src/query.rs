//! `lake query` (ADR 001 L3): a key lookup over one lake table. With an index, the keys
//! go to Bellwether in a Parquet key file (`locate`, `latest` or `asof`) — a text key's
//! words as one `locate --has` each, their planned row groups intersected —, the planned
//! row groups are read with the parquet reader, and the residual predicate the plan
//! returns is evaluated here, row by row; rematch never re-plans. Without an index (or
//! without the binary, or for a lookup no Bellwether key serves) every file of the table
//! is read and the same predicate is computed locally: the rows are the same, and the
//! envelope says `indexed: false` and why.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use arrow::array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, Int64Builder, ListArray, StringArray, StringBuilder,
    TimestampMillisecondArray,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rematch_core::words;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::index::{self, KeyDef, KeyKind, KeyType, TableDef};
use crate::lake_dir;

/// A bitemporal (or, with `valid` alone, temporal) instant, ms UTC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AsOf {
    pub valid: Option<i64>,
    pub known: Option<i64>,
}

/// One lookup. `key` is a key of the table's row in the ADR key table, by its query name
/// (`exact:path`, `text:tool_cmd`, `list:facets`, `temporal2:entry_id`, …) or full spec.
#[derive(Clone, Debug, Default)]
pub struct QuerySpec {
    pub table: String,
    pub key: String,
    /// Key values; list keys: elements; text keys: words, every one required (each value
    /// is split at word boundaries); a composite key's value joins its columns with a tab
    /// (or a `:` before the last column).
    pub values: Vec<String>,
    /// Inclusive window, ms UTC, on the table's time column ([`TableDef::window`]).
    pub window: Option<(i64, i64)>,
    /// temporal2: `asof --valid/--known`; temporal: `asof --at` (`valid`, else `known`).
    pub asof: Option<AsOf>,
    /// The newest version per key (temporal2: known by `asof.known` when given).
    pub latest: bool,
    /// List keys: every value (`contains_all`) instead of any (`contains_any`).
    pub all: bool,
    pub limit: Option<usize>,
}

/// The rows a lookup returned and how they were found.
#[derive(Clone, Debug)]
pub struct QueryOutput {
    /// Planned by a Bellwether index (else every file was read).
    pub indexed: bool,
    pub rows: Vec<Value>,
    /// Bellwether's lookup and plan summary (residual without its byte values).
    pub plan: Option<Value>,
    /// Why the table was scanned instead.
    pub reason: Option<String>,
    /// The index was rebuilt first because the table had files it did not track.
    pub reindexed: bool,
}

impl QueryOutput {
    pub fn to_json(&self, spec: &QuerySpec) -> Value {
        json!({
            "table": spec.table,
            "key": spec.key,
            "values": spec.values,
            "indexed": self.indexed,
            "reason": self.reason,
            "reindexed": self.reindexed,
            "plan": self.plan,
            "count": self.rows.len(),
            "rows": self.rows,
        })
    }
}

// ---------------- values and their canonical encoding (Bellwether ADR 016) ----------------

/// One cell as read from a lake file or a path's partition placeholder.
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    /// `Timestamp(ms, UTC)`.
    Ts(i64),
    /// `Date32` (a `{day:date}` placeholder), days since the epoch.
    Date(i32),
    List(Vec<String>),
}

fn int_be(v: i64, out: &mut Vec<u8>) {
    out.extend_from_slice(&((v as u64) ^ (1 << 63)).to_be_bytes());
}

/// Canonical bytes of one UTF-8 string: `0x06`, `0x00` escaped as `0x00 0xFF`, `0x00 0x00`.
pub fn canon_str(s: &str, out: &mut Vec<u8>) {
    out.push(0x06);
    for &b in s.as_bytes() {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

/// Canonical bytes of a ms timestamp column value.
pub fn canon_ts(ms: i64) -> Vec<u8> {
    let mut v = vec![0x08, 0x04, 0x02];
    int_be(ms, &mut v);
    v
}

impl Val {
    /// Appends the canonical encoding; `false` for a null (and a list, which has none).
    fn canon_into(&self, out: &mut Vec<u8>) -> bool {
        match self {
            Val::Null | Val::List(_) => return false,
            Val::Bool(b) => out.extend_from_slice(&[0x01, u8::from(*b)]),
            Val::I64(v) => {
                out.push(0x02);
                int_be(*v, out);
            }
            Val::F64(f) => {
                let bits = if f.is_nan() { f64::NAN.to_bits() } else { f.to_bits() };
                let enc = if bits >> 63 == 1 { !bits } else { bits ^ (1 << 63) };
                out.push(0x04);
                out.extend_from_slice(&enc.to_be_bytes());
            }
            Val::Str(s) => canon_str(s, out),
            Val::Ts(ms) => {
                out.extend_from_slice(&[0x08, 0x04, 0x02]);
                int_be(*ms, out);
            }
            Val::Date(d) => {
                out.extend_from_slice(&[0x08, 0x01, 0x05]);
                int_be(i64::from(*d), out);
            }
        }
        true
    }

    fn canon(&self) -> Option<Vec<u8>> {
        let mut v = Vec::new();
        self.canon_into(&mut v).then_some(v)
    }

    fn to_json(&self) -> Value {
        match self {
            Val::Null => Value::Null,
            Val::Bool(b) => json!(b),
            Val::I64(v) | Val::Ts(v) => json!(v),
            Val::F64(f) => json!(f),
            Val::Str(s) => json!(s),
            Val::Date(d) => json!(chrono::DateTime::from_timestamp(i64::from(*d) * 86_400, 0)
                .map(|t| t.format("%Y-%m-%d").to_string())
                .unwrap_or_default()),
            Val::List(l) => json!(l),
        }
    }
}

// ---------------- rows ----------------

struct Frame {
    names: Vec<String>,
    index: HashMap<String, usize>,
}

/// One row of one file: where it is (for a stable order) and its cells.
pub struct Row {
    path: Rc<PathBuf>,
    n: usize,
    frame: Rc<Frame>,
    vals: Vec<Val>,
}

impl std::fmt::Debug for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Row").field("path", &self.path).field("n", &self.n).field("vals", &self.vals).finish()
    }
}

impl Row {
    fn get(&self, col: &str) -> &Val {
        self.frame.index.get(col).map(|&i| &self.vals[i]).unwrap_or(&Val::Null)
    }

    /// The canonical key tuple of `cols`; `None` when the leading column is null (not
    /// indexed, ADR 014); a later null is the `0x00` marker.
    fn tuple(&self, cols: &[String]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        for (i, c) in cols.iter().enumerate() {
            if !self.get(c).canon_into(&mut out) {
                if i == 0 {
                    return None;
                }
                out.push(0x00);
            }
        }
        Some(out)
    }

    fn to_json(&self) -> Value {
        let mut m = Map::with_capacity(self.vals.len());
        for (name, v) in self.frame.names.iter().zip(&self.vals) {
            let j = match (name.as_str(), v) {
                ("json", Val::Str(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!(s)),
                _ => v.to_json(),
            };
            m.insert(name.clone(), j);
        }
        Value::Object(m)
    }
}

fn cmp_rows(a: &Row, b: &Row) -> Ordering {
    a.path.cmp(&b.path).then(a.n.cmp(&b.n))
}

fn column_vals(arr: &ArrayRef, name: &str, path: &Path) -> Result<Vec<Val>> {
    let n = arr.len();
    let mut out = Vec::with_capacity(n);
    macro_rules! each {
        ($ty:ty, $f:expr) => {{
            let a = arr.as_any().downcast_ref::<$ty>().ok_or_else(|| anyhow!("{}: column {name}", path.display()))?;
            for i in 0..n {
                out.push(if a.is_null(i) { Val::Null } else { $f(a, i) });
            }
        }};
    }
    match arr.data_type() {
        DataType::Utf8 => each!(StringArray, |a: &StringArray, i| Val::Str(a.value(i).to_string())),
        DataType::Int64 => each!(Int64Array, |a: &Int64Array, i| Val::I64(a.value(i))),
        DataType::Float64 => each!(Float64Array, |a: &Float64Array, i| Val::F64(a.value(i))),
        DataType::Boolean => each!(BooleanArray, |a: &BooleanArray, i| Val::Bool(a.value(i))),
        DataType::Timestamp(TimeUnit::Millisecond, _) => {
            each!(TimestampMillisecondArray, |a: &TimestampMillisecondArray, i| Val::Ts(a.value(i)))
        }
        DataType::List(f) if f.data_type() == &DataType::Utf8 => each!(ListArray, |a: &ListArray, i| {
            let items = a.value(i);
            let s = items.as_any().downcast_ref::<StringArray>().expect("List<Utf8> items");
            Val::List((0..s.len()).filter(|&j| !s.is_null(j)).map(|j| s.value(j).to_string()).collect())
        }),
        other => bail!("{}: column {name} has type {other}, which lake query does not read", path.display()),
    }
    Ok(out)
}

fn day_val(s: &str) -> Option<Val> {
    let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?;
    Some(Val::Date(i32::try_from((d - epoch).num_days()).ok()?))
}

/// The partition columns a file's path gives under its table's template.
fn path_values(t: &TableDef, rel: &Path) -> Vec<(&'static str, Val)> {
    let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    let run = |day: &str, file: &str| -> Vec<(&'static str, Val)> {
        let stem = file.strip_suffix(".parquet").unwrap_or(file);
        let mut v = vec![("day", day_val(day).unwrap_or(Val::Null))];
        if let Some((m, s)) = stem.rsplit_once('-') {
            v.push(("machine", Val::Str(m.to_string())));
            v.push(("run_seq", s.parse().map(Val::I64).unwrap_or(Val::Null)));
        }
        v
    };
    match (t.name, parts.as_slice()) {
        ("events", [harness, day, file]) => vec![
            ("harness", Val::Str((*harness).to_string())),
            ("day", day_val(day).unwrap_or(Val::Null)),
            ("session_id", Val::Str(file.strip_suffix(".parquet").unwrap_or(file).to_string())),
        ],
        ("outcomes", [domain, day, file]) => {
            let mut v = vec![("domain", Val::Str((*domain).to_string()))];
            v.extend(run(day, file));
            v
        }
        (_, [day, file]) => run(day, file),
        _ => Vec::new(),
    }
}

/// Reads `row_groups` (every one when `None`) of one table file into rows.
fn read_file(t: &TableDef, table_dir: &Path, path: &Path, row_groups: Option<&[usize]>, out: &mut Vec<Row>) -> Result<()> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let b = ParquetRecordBatchReaderBuilder::try_new(f).with_context(|| format!("read {}", path.display()))?;
    let meta = b.metadata().clone();
    let all: Vec<usize> = (0..meta.num_row_groups()).collect();
    let mut rgs: Vec<usize> = row_groups.map(<[usize]>::to_vec).unwrap_or(all);
    rgs.sort_unstable();
    rgs.dedup();
    if let Some(bad) = rgs.iter().find(|&&g| g >= meta.num_row_groups()) {
        bail!("{}: planned row group {bad} is past the file's {}", path.display(), meta.num_row_groups());
    }
    // Absolute row numbers of the selected row groups, in reading order.
    let mut starts = Vec::with_capacity(rgs.len());
    let mut acc = 0usize;
    let mut next = 0usize;
    for g in 0..meta.num_row_groups() {
        if rgs.get(next) == Some(&g) {
            starts.push((acc, meta.row_group(g).num_rows() as usize));
            next += 1;
        }
        acc += meta.row_group(g).num_rows() as usize;
    }
    let reader = b.with_row_groups(rgs).build()?;
    let rel = path.strip_prefix(table_dir).unwrap_or(path);
    let parts = path_values(t, rel);
    let shared_path = Rc::new(path.to_path_buf());
    let mut frame: Option<Rc<Frame>> = None;
    // An events file exported before `tool_cmd` (not yet upgraded by `lake index`): the
    // column is derived here, so a scan finds what an upgraded file stores.
    let mut derive_cmd = false;
    let mut cursor = (0usize, 0usize); // (index into starts, offset within that row group)
    for batch in reader {
        let batch = batch.with_context(|| format!("decode {}", path.display()))?;
        let schema = batch.schema();
        let fr = frame
            .get_or_insert_with(|| {
                let mut names: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
                for (p, _) in &parts {
                    if !names.iter().any(|n| n == p) {
                        names.push((*p).to_string());
                    }
                }
                if t.name == "events" && !names.iter().any(|n| n == "tool_cmd") {
                    derive_cmd = true;
                    names.push("tool_cmd".into());
                }
                let index = names.iter().enumerate().map(|(i, n)| (n.clone(), i)).collect();
                Rc::new(Frame { names, index })
            })
            .clone();
        let cols: Vec<Vec<Val>> = batch
            .columns()
            .iter()
            .zip(schema.fields())
            .map(|(a, f)| column_vals(a, f.name(), path))
            .collect::<Result<_>>()?;
        for r in 0..batch.num_rows() {
            while cursor.0 < starts.len() && cursor.1 >= starts[cursor.0].1 {
                cursor = (cursor.0 + 1, 0);
            }
            let n = starts.get(cursor.0).map(|s| s.0 + cursor.1).unwrap_or(usize::MAX);
            cursor.1 += 1;
            let mut vals: Vec<Val> = cols.iter().map(|c| c[r].clone()).collect();
            for (p, v) in &parts {
                if fr.index[*p] >= cols.len() {
                    vals.push(v.clone());
                }
            }
            if derive_cmd {
                let at = |c: &str| fr.index.get(c).and_then(|&i| vals.get(i));
                let tool = match at("tool") {
                    Some(Val::Str(s)) => Some(s.as_str()),
                    _ => None,
                };
                let input = match at("tool_input") {
                    Some(Val::Str(s)) => serde_json::from_str::<Value>(s).ok(),
                    _ => None,
                };
                let cmd = crate::events::command_of(tool, input.as_ref());
                vals.push(cmd.map_or(Val::Null, Val::Str));
            }
            out.push(Row { path: shared_path.clone(), n, frame: fr.clone(), vals });
        }
    }
    Ok(())
}

fn scan_rows(root: &Path, t: &TableDef) -> Result<Vec<Row>> {
    let lake = lake_dir(root);
    let dir = index::table_dir(&lake, t.name);
    let (mut files, hidden) = index::table_files(&lake, t.name)?;
    files.extend(hidden);
    files.sort();
    let mut rows = Vec::new();
    for f in &files {
        read_file(t, &dir, f, None, &mut rows)?;
    }
    Ok(rows)
}

/// Every row of a table (all machines, all versions), in file then row order.
pub fn scan(root: &Path, table: &str) -> Result<Vec<Value>> {
    let t = index::table(table)?;
    Ok(scan_rows(root, t)?.iter().map(Row::to_json).collect())
}

// ---------------- the residual ----------------

/// Bellwether's residual predicate (`bellwether_types::Predicate`), the kinds rematch
/// evaluates; values are canonical encodings.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Pred {
    In { columns: Vec<String>, values: Vec<Vec<u8>> },
    Between { column: String, lo: Vec<u8>, hi: Vec<u8> },
    LatestPerKey { columns: Vec<String>, ts_column: String },
    NotOlderThan { columns: Vec<String>, ts_column: String, bounds: Vec<(Vec<u8>, Vec<u8>)> },
    And { all: Vec<Pred> },
    /// A text key's residual: rows whose `column` matches at least one of `values` under
    /// `operator`, both case-folded (`rematch_core::words`); a null never matches.
    Text { column: String, operator: TextOp, values: Vec<String> },
    ContainsAny { column: String, values: Vec<Vec<u8>> },
    ContainsAll { column: String, values: Vec<Vec<Vec<u8>>> },
}

/// A text match (`bellwether_types::TextOp`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextOp {
    /// One of the value's words equals the argument.
    HasToken,
    /// The value contains the argument.
    Contains,
    /// One of the value's words starts with the argument.
    HasPrefix,
}

const KNOWN_OPS: [&str; 8] =
    ["in", "between", "latest_per_key", "not_older_than", "and", "text", "contains_any", "contains_all"];

/// Parses a residual, refusing any kind rematch does not evaluate by name (applying
/// none would return a superset of the rows asked for).
pub fn parse_residual(v: &Value) -> Result<Pred> {
    fn check(v: &Value) -> Result<()> {
        let op = v.get("op").and_then(Value::as_str).ok_or_else(|| anyhow!("residual without an op: {v}"))?;
        if !KNOWN_OPS.contains(&op) {
            bail!("the plan's residual kind `{op}` is not evaluated by rematch; refusing rather than returning unfiltered rows");
        }
        for p in v.get("all").and_then(Value::as_array).into_iter().flatten() {
            check(p)?;
        }
        Ok(())
    }
    check(v)?;
    Pred::deserialize(v).context("parse the plan's residual")
}

/// Applies `p` to `rows`, keeping their order. `LatestPerKey` keeps every row at the
/// greatest time of its key (ties are kept, as Bellwether plans every row group at the
/// winning time); a row whose time is null is never the latest.
pub fn eval(p: &Pred, rows: Vec<Row>) -> Result<Vec<Row>> {
    Ok(match p {
        Pred::And { all } => {
            let mut rows = rows;
            for q in all {
                rows = eval(q, rows)?;
            }
            rows
        }
        Pred::In { columns, values } => {
            let set: HashSet<&[u8]> = values.iter().map(Vec::as_slice).collect();
            rows.into_iter().filter(|r| r.tuple(columns).is_some_and(|k| set.contains(k.as_slice()))).collect()
        }
        Pred::Between { column, lo, hi } => rows
            .into_iter()
            .filter(|r| r.get(column).canon().is_some_and(|v| v.as_slice() >= lo.as_slice() && v.as_slice() <= hi.as_slice()))
            .collect(),
        Pred::NotOlderThan { columns, ts_column, bounds } => {
            let b: HashMap<&[u8], &[u8]> = bounds.iter().map(|(k, t)| (k.as_slice(), t.as_slice())).collect();
            rows.into_iter()
                .filter(|r| match r.tuple(columns).and_then(|k| b.get(k.as_slice()).copied()) {
                    None => true,
                    Some(bound) => r.get(ts_column).canon().is_some_and(|t| t.as_slice() >= bound),
                })
                .collect()
        }
        Pred::LatestPerKey { columns, ts_column } => {
            // (key tuple, time) of each row; `None` when either is null (never the latest).
            let keyed: Vec<Option<(Vec<u8>, Vec<u8>)>> =
                rows.iter().map(|r| r.tuple(columns).zip(r.get(ts_column).canon())).collect();
            let mut max: HashMap<&[u8], &[u8]> = HashMap::new();
            for (k, t) in keyed.iter().flatten() {
                let e = max.entry(k.as_slice()).or_insert(t.as_slice());
                if t.as_slice() > *e {
                    *e = t.as_slice();
                }
            }
            let keep: Vec<bool> = keyed
                .iter()
                .map(|kt| kt.as_ref().is_some_and(|(k, t)| max.get(k.as_slice()) == Some(&t.as_slice())))
                .collect();
            rows.into_iter().zip(keep).filter_map(|(r, k)| k.then_some(r)).collect()
        }
        Pred::Text { column, operator, values } => rows
            .into_iter()
            .filter(|r| {
                let Val::Str(s) = r.get(column) else { return false };
                values.iter().any(|v| match operator {
                    TextOp::HasToken => words::has_token(s, v),
                    TextOp::Contains => words::contains(s, v),
                    TextOp::HasPrefix => words::has_prefix(s, v),
                })
            })
            .collect(),
        Pred::ContainsAny { column, values } => {
            let set: HashSet<&[u8]> = values.iter().map(Vec::as_slice).collect();
            rows.into_iter().filter(|r| list_canon(r, column).iter().any(|e| set.contains(e.as_slice()))).collect()
        }
        Pred::ContainsAll { column, values } => rows
            .into_iter()
            .filter(|r| {
                let have: HashSet<Vec<u8>> = list_canon(r, column).into_iter().collect();
                !values.is_empty() && values.iter().all(|g| g.iter().any(|v| have.contains(v)))
            })
            .collect(),
    })
}

fn list_canon(r: &Row, column: &str) -> Vec<Vec<u8>> {
    match r.get(column) {
        Val::List(items) => items
            .iter()
            .map(|s| {
                let mut v = Vec::new();
                canon_str(s, &mut v);
                v
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The residual with its byte values replaced by their counts (for the envelope); a text
/// residual's values are words and stay.
fn residual_summary(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| k.as_str() != "lo" && k.as_str() != "hi")
                .map(|(k, x)| {
                    let y = match (k.as_str(), x) {
                        ("values", Value::Array(_)) if m.get("op").and_then(Value::as_str) == Some("text") => x.clone(),
                        ("values" | "bounds", Value::Array(a)) => json!(a.len()),
                        _ => residual_summary(x),
                    };
                    (k.clone(), y)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(residual_summary).collect()),
        x => x.clone(),
    }
}

// ---------------- the lookup ----------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lookup {
    Locate,
    Latest,
    Asof,
}

/// The values as key tuples aligned with the key's columns.
fn tuples(k: &KeyDef, values: &[String]) -> Result<Vec<Vec<Val>>> {
    values
        .iter()
        .map(|v| {
            let parts: Vec<&str> = match k.columns.len() {
                1 => vec![v.as_str()],
                2 => match v.split_once('\t').or_else(|| v.rsplit_once(':')) {
                    Some((a, b)) => vec![a, b],
                    None => bail!("{}: value {v:?} needs both columns ({}), joined by a tab or ':'", k.name, k.columns.join(", ")),
                },
                n => bail!("{}: {n}-column keys are not supported", k.name),
            };
            parts
                .iter()
                .zip(k.types)
                .map(|(p, t)| match t {
                    KeyType::Utf8 => Ok(Val::Str((*p).to_string())),
                    KeyType::Int64 => p.parse().map(Val::I64).map_err(|_| anyhow!("{}: {p:?} is not an integer", k.name)),
                })
                .collect()
        })
        .collect()
}

fn encode_tuple(t: &[Val]) -> Vec<u8> {
    let mut out = Vec::new();
    for v in t {
        v.canon_into(&mut out);
    }
    out
}

/// What the request asks, before any index: the lookup, and the predicate a full scan
/// applies (the same rows a plan plus its residual returns).
struct Request {
    t: &'static TableDef,
    k: &'static KeyDef,
    lookup: Lookup,
    tuples: Vec<Vec<Val>>,
    /// A text key's words, each required (`rematch_core::words::tokens` of the values).
    tokens: Vec<String>,
    local: Vec<Pred>,
    window: Option<Pred>,
    /// Why Bellwether cannot plan this lookup (served by a scan).
    unplannable: Option<String>,
}

fn request(spec: &QuerySpec) -> Result<Request> {
    let t = index::table(&spec.table)?;
    let k = t.key(&spec.key)?;
    if spec.window.is_some() && (spec.latest || spec.asof.is_some()) {
        bail!("a window does not combine with --latest or --valid/--known: ask for the versions in the window, or the version as of an instant");
    }
    if spec.latest && spec.asof.is_some_and(|a| a.valid.is_some()) {
        bail!("--latest takes --known only (the newest version known by then); --valid asks for a version as of an instant");
    }
    let lookup = match (spec.latest, spec.asof.is_some()) {
        (true, _) => Lookup::Latest,
        (false, true) => Lookup::Asof,
        _ => Lookup::Locate,
    };
    if lookup != Lookup::Locate && !matches!(k.kind, KeyKind::Temporal | KeyKind::Temporal2) {
        bail!("{} is not a temporal key: --latest and --valid/--known need one ({})", k.name, temporal_keys(t));
    }
    if spec.all && k.kind != KeyKind::List {
        bail!("--all is for list keys (contains_all); {} is not one", k.name);
    }
    if k.kind == KeyKind::Range {
        if !spec.values.is_empty() {
            bail!("{} takes no values: it prunes a window (--from/--to)", k.name);
        }
        if spec.window.is_none() {
            bail!("{} needs a window (--from/--to)", k.name);
        }
    } else if spec.values.is_empty() {
        bail!("{} needs at least one value", k.name);
    }
    let tokens = if k.kind == KeyKind::Text {
        let t = words::tokens(&spec.values);
        if t.is_empty() {
            bail!("{}: {:?} holds no word (values split at Unicode word boundaries)", k.name, spec.values);
        }
        t
    } else {
        Vec::new()
    };
    let tuples = if matches!(k.kind, KeyKind::Range | KeyKind::Text) { Vec::new() } else { tuples(k, &spec.values)? };
    let cols = || k.columns.iter().map(|c| c.to_string()).collect::<Vec<_>>();
    let canon_strs = || {
        spec.values
            .iter()
            .map(|v| {
                let mut b = Vec::new();
                canon_str(v, &mut b);
                b
            })
            .collect::<Vec<_>>()
    };
    let mut local = Vec::new();
    match k.kind {
        KeyKind::Exact | KeyKind::Temporal | KeyKind::Temporal2 => {
            local.push(Pred::In { columns: cols(), values: tuples.iter().map(|t| encode_tuple(t)).collect() })
        }
        KeyKind::Text => local.extend(tokens.iter().map(|w| Pred::Text {
            column: k.columns[0].into(),
            operator: TextOp::HasToken,
            values: vec![w.clone()],
        })),
        KeyKind::List if spec.all => {
            local.push(Pred::ContainsAll { column: k.columns[0].into(), values: canon_strs().into_iter().map(|v| vec![v]).collect() })
        }
        KeyKind::List => local.push(Pred::ContainsAny { column: k.columns[0].into(), values: canon_strs() }),
        KeyKind::Range => {}
    }
    let max = canon_ts(i64::MAX);
    let min = canon_ts(i64::MIN);
    let upto = |c: &str, at: Option<i64>| Pred::Between { column: c.into(), lo: min.clone(), hi: at.map(canon_ts).unwrap_or_else(|| max.clone()) };
    let latest = |c: &str| Pred::LatestPerKey { columns: cols(), ts_column: c.into() };
    let asof = spec.asof.unwrap_or_default();
    match (k.kind, lookup) {
        (KeyKind::Temporal, Lookup::Latest) => local.push(latest(k.time.unwrap_or_default())),
        (KeyKind::Temporal, Lookup::Asof) => {
            let ts = k.time.unwrap_or_default();
            local.push(upto(ts, Some(temporal_at(asof)?)));
            local.push(latest(ts));
        }
        (KeyKind::Temporal2, Lookup::Latest | Lookup::Asof) => {
            let (valid, known) = (k.time.unwrap_or_default(), k.known.unwrap_or_default());
            local.push(upto(valid, asof.valid));
            local.push(upto(known, asof.known));
            local.push(latest(valid));
            local.push(latest(known));
        }
        _ => {}
    }
    let window = spec.window.map(|(lo, hi)| Pred::Between { column: t.window.into(), lo: canon_ts(lo), hi: canon_ts(hi) });
    let unplannable = match (k.kind, lookup) {
        (KeyKind::Range, _) => Some(format!("{} prunes another key's window; a window alone has no Bellwether lookup (locate needs keys)", k.name)),
        (KeyKind::Temporal2, Lookup::Locate) => Some(format!(
            "{} is a temporal2 sidecar, which serves only --latest and --valid/--known; every version is read by a scan",
            k.name
        )),
        _ => None,
    };
    Ok(Request { t, k, lookup, tuples, tokens, local, window, unplannable })
}

fn temporal_keys(t: &TableDef) -> String {
    let v: Vec<&str> = t.keys.iter().filter(|k| matches!(k.kind, KeyKind::Temporal | KeyKind::Temporal2)).map(|k| k.name).collect();
    if v.is_empty() {
        format!("table {} has none", t.name)
    } else {
        v.join(", ")
    }
}

/// A temporal (one-time) key's `asof --at`: `valid`, else `known`; both must agree.
fn temporal_at(a: AsOf) -> Result<i64> {
    match (a.valid, a.known) {
        (Some(v), Some(k)) if v != k => bail!("a temporal key has one time: give --valid (or --known) alone"),
        (Some(v), _) | (None, Some(v)) => Ok(v),
        (None, None) => bail!("--valid or --known needs an instant"),
    }
}

static KEYFILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Writes the key file Bellwether reads: one column per key column, named by it, typed
/// as the dataset's.
fn write_keys(dir: &Path, k: &KeyDef, spec: &QuerySpec, tuples: &[Vec<Val>]) -> Result<PathBuf> {
    let mut fields = Vec::new();
    let mut arrays: Vec<ArrayRef> = Vec::new();
    if k.kind == KeyKind::List {
        fields.push(Field::new(k.columns[0], DataType::Utf8, false));
        let mut b = StringBuilder::new();
        for v in &spec.values {
            b.append_value(v);
        }
        arrays.push(Arc::new(b.finish()));
    } else {
        for (i, (c, ty)) in k.columns.iter().zip(k.types).enumerate() {
            match ty {
                KeyType::Utf8 => {
                    fields.push(Field::new(*c, DataType::Utf8, false));
                    let mut b = StringBuilder::new();
                    for t in tuples {
                        match &t[i] {
                            Val::Str(s) => b.append_value(s),
                            other => bail!("{}: {other:?} is not a string", k.name),
                        }
                    }
                    arrays.push(Arc::new(b.finish()));
                }
                KeyType::Int64 => {
                    fields.push(Field::new(*c, DataType::Int64, false));
                    let mut b = Int64Builder::new();
                    for t in tuples {
                        match &t[i] {
                            Val::I64(n) => b.append_value(*n),
                            other => bail!("{}: {other:?} is not an integer", k.name),
                        }
                    }
                    arrays.push(Arc::new(b.finish()));
                }
            }
        }
    }
    let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).context("build the key file")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!(
        ".keys-{}-{}.parquet",
        std::process::id(),
        KEYFILE_SEQ.fetch_add(1, AtomicOrdering::Relaxed)
    ));
    let f = File::create(&path).with_context(|| format!("create {}", path.display()))?;
    let mut w = parquet::arrow::ArrowWriter::try_new(f, batch.schema(), None)?;
    w.write(&batch)?;
    w.close()?;
    Ok(path)
}

fn finish(mut rows: Vec<Row>, limit: Option<usize>) -> Vec<Value> {
    rows.sort_by(cmp_rows);
    if let Some(n) = limit {
        rows.truncate(n);
    }
    rows.iter().map(Row::to_json).collect()
}

fn scanned(root: &Path, r: &Request, spec: &QuerySpec, reason: String) -> Result<QueryOutput> {
    let mut rows = scan_rows(root, r.t)?;
    for p in r.local.iter().chain(&r.window) {
        rows = eval(p, rows)?;
    }
    Ok(QueryOutput { indexed: false, rows: finish(rows, spec.limit), plan: None, reason: Some(reason), reindexed: false })
}

/// Runs one lookup over `REMATCH_ROOT`'s lake. `bellwether`: the binary (else
/// `$REMATCH_BELLWETHER`, else `bellwether` on `PATH`). Falls back to a full scan, with
/// the reason, when there is no binary, no index for the table, a pinned version the
/// binary does not match, files Bellwether does not list, or a lookup no key serves.
pub fn run(root: &Path, spec: &QuerySpec, bellwether: Option<&Path>) -> Result<QueryOutput> {
    let r = request(spec)?;
    if let Some(why) = &r.unplannable {
        return scanned(root, &r, spec, why.clone());
    }
    let Some(bin) = index::resolve_bin(bellwether) else {
        return scanned(root, &r, spec, "no bellwether binary (--bellwether, REMATCH_BELLWETHER, PATH)".into());
    };
    let idx = index::index_root(root).join(r.t.name);
    if !idx.join("manifest.json").is_file() {
        return scanned(root, &r, spec, format!("table {} has no index (run `rematch lake index {}`)", r.t.name, r.t.name));
    }
    let version = index::version(&bin)?;
    let pinned = crate::Manifest::load(&lake_dir(root))?.bellwether_version_pinned;
    if let Some(why) = index::pin_mismatch(pinned.as_deref(), &version) {
        return scanned(root, &r, spec, why);
    }
    let lake = lake_dir(root);
    let (_, hidden) = index::table_files(&lake, r.t.name)?;
    if !hidden.is_empty() {
        return scanned(
            root,
            &r,
            spec,
            format!("{} file(s) of {} sit under a `_`/`.` path component, which Bellwether never lists", hidden.len(), r.t.name),
        );
    }
    // Re-index when the table has files the index does not track, or the index was built
    // before this key was declared.
    let stale = index::staleness(root, r.t.name)?.is_some_and(|(untracked, gone)| !untracked.is_empty() || !gone.is_empty());
    let missing = index::built_specs(&idx)?.is_some_and(|s| !s.iter().any(|x| x == r.k.spec));
    let reindexed = stale || missing;
    if reindexed {
        index::index_table(root, &bin, r.t)?;
    }
    let outs: Vec<Value> = if r.k.kind == KeyKind::Text {
        // One `locate --has` per word; a row must have every one.
        r.tokens.iter().map(|w| index::run_bw(&bin, &text_args(&lake, &idx, r.t, r.k, w))).collect::<Result<_>>()?
    } else {
        let keyfile = write_keys(&index::index_root(root), r.k, spec, &r.tuples)?;
        let res = lookup(&bin, &lake, &idx, &r, spec, &keyfile);
        let _ = std::fs::remove_file(&keyfile);
        vec![res?]
    };
    let mut residuals = Vec::with_capacity(outs.len());
    let mut planned: Option<BTreeMap<String, BTreeSet<usize>>> = None;
    for out in &outs {
        let plan = &out["evidence"]["record"]["body"]["plan"];
        residuals.push(parse_residual(&plan["residual"])?);
        let entries = plan_entries(plan, r.lookup)?;
        planned = Some(match planned {
            None => entries,
            // Row groups planned for every word (a row group holds a row with all of them
            // only if each lookup planned it).
            Some(prev) => prev
                .into_iter()
                .filter_map(|(p, g)| {
                    let both: BTreeSet<usize> = g.intersection(entries.get(&p)?).copied().collect();
                    (!both.is_empty()).then_some((p, both))
                })
                .collect(),
        });
    }
    let planned = planned.unwrap_or_default();
    let table_dir = index::table_dir(&lake, r.t.name);
    let mut rows = Vec::new();
    for (p, rgs) in &planned {
        read_file(r.t, &table_dir, Path::new(p), Some(&rgs.iter().copied().collect::<Vec<_>>()), &mut rows)?;
    }
    for residual in &residuals {
        rows = eval(residual, rows)?;
    }
    if let Some(w) = &r.window {
        // Idempotent when Bellwether already planned the window; required when the key
        // (text, list) takes none.
        rows = eval(w, rows)?;
    }
    let one = |k: &str| outs.first().map(|o| o[k].clone()).unwrap_or(Value::Null);
    let summaries: Vec<Value> =
        outs.iter().map(|o| residual_summary(&o["evidence"]["record"]["body"]["plan"]["residual"])).collect();
    let summary = if outs.len() == 1 {
        json!({"lookup": one("lookup"), "files_total": one("files_total"), "row_groups_total": one("row_groups_total"),
               "planned": one("planned"), "residual": summaries[0]})
    } else {
        json!({
            "lookup": outs.iter().map(|o| o["lookup"].clone()).collect::<Vec<_>>(),
            "files_total": one("files_total"),
            "row_groups_total": one("row_groups_total"),
            "planned": {"files": planned.len(), "row_groups": planned.values().map(BTreeSet::len).sum::<usize>()},
            "residual": {"op": "and", "all": summaries},
        })
    };
    Ok(QueryOutput { indexed: true, rows: finish(rows, spec.limit), plan: Some(summary), reason: None, reindexed })
}

/// A plan's files and their row groups.
fn plan_entries(plan: &Value, lookup: Lookup) -> Result<BTreeMap<String, BTreeSet<usize>>> {
    plan["entries"]
        .as_array()
        .ok_or_else(|| anyhow!("bellwether {lookup:?}: plan without entries"))?
        .iter()
        .map(|e| -> Result<(String, BTreeSet<usize>)> {
            let p = e["file"]["path"].as_str().ok_or_else(|| anyhow!("plan entry without a path: {e}"))?;
            let rgs = e["row_groups"]
                .as_array()
                .ok_or_else(|| anyhow!("plan entry without row groups: {e}"))?
                .iter()
                .map(|g| g.as_u64().map(|g| g as usize).ok_or_else(|| anyhow!("bad row group {g}")))
                .collect::<Result<_>>()?;
            Ok((p.to_string(), rgs))
        })
        .collect()
}

/// `locate --has <column>=<word>`: a text key's lookup of one word (it takes no window).
fn text_args(lake: &Path, idx: &Path, t: &TableDef, k: &KeyDef, word: &str) -> Vec<String> {
    vec![
        "locate".into(),
        "--json".into(),
        lake.join(t.name).to_string_lossy().into_owned(),
        "--index".into(),
        idx.to_string_lossy().into_owned(),
        "--has".into(),
        format!("{}={word}", k.columns[0]),
    ]
}

fn lookup(bin: &Path, lake: &Path, idx: &Path, r: &Request, spec: &QuerySpec, keyfile: &Path) -> Result<Value> {
    let verb = match r.lookup {
        Lookup::Locate => "locate",
        Lookup::Latest => "latest",
        Lookup::Asof => "asof",
    };
    let mut args: Vec<String> = vec![
        verb.into(),
        "--json".into(),
        lake.join(r.t.name).to_string_lossy().into_owned(),
        "--index".into(),
        idx.to_string_lossy().into_owned(),
        "--keys".into(),
        keyfile.to_string_lossy().into_owned(),
    ];
    let asof = spec.asof.unwrap_or_default();
    match (r.k.kind, r.lookup) {
        (KeyKind::List, _) => args.push(if spec.all { "--all" } else { "--any" }.into()),
        (KeyKind::Exact | KeyKind::Temporal, Lookup::Locate) => {
            if let Some((lo, hi)) = spec.window {
                args.extend(["--from".into(), lo.to_string(), "--to".into(), hi.to_string(), "--ts".into(), r.t.window.into()]);
            }
        }
        (KeyKind::Temporal, Lookup::Asof) => args.extend(["--at".into(), temporal_at(asof)?.to_string()]),
        (KeyKind::Temporal2, Lookup::Latest) => {
            args.extend(["--known".into(), asof.known.unwrap_or(i64::MAX).to_string()])
        }
        (KeyKind::Temporal2, Lookup::Asof) => {
            if let Some(v) = asof.valid {
                args.extend(["--valid".into(), v.to_string()]);
            }
            if let Some(k) = asof.known {
                args.extend(["--known".into(), k.to_string()]);
            }
        }
        _ => {}
    }
    index::run_bw(bin, &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(names: &[&str]) -> Rc<Frame> {
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        let index = names.iter().enumerate().map(|(i, n)| (n.clone(), i)).collect();
        Rc::new(Frame { names, index })
    }

    fn rows(f: &Rc<Frame>, vals: Vec<Vec<Val>>) -> Vec<Row> {
        let p = Rc::new(PathBuf::from("/x.parquet"));
        vals.into_iter().enumerate().map(|(n, vals)| Row { path: p.clone(), n, frame: f.clone(), vals }).collect()
    }

    fn s(x: &str) -> Val {
        Val::Str(x.into())
    }

    fn cs(x: &str) -> Vec<u8> {
        let mut v = Vec::new();
        canon_str(x, &mut v);
        v
    }

    fn ns(r: &[Row]) -> Vec<usize> {
        r.iter().map(|r| r.n).collect()
    }

    #[test]
    fn canonical_bytes_match_bellwethers_vectors() {
        assert_eq!(s("a").canon().unwrap(), [0x06, 0x61, 0, 0]);
        assert_eq!(s("a\0b").canon().unwrap(), [0x06, 0x61, 0, 0xff, 0x62, 0, 0]);
        assert_eq!(Val::I64(0).canon().unwrap(), [0x02, 0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(Val::I64(-1).canon().unwrap(), [0x02, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(Val::Bool(true).canon().unwrap(), [0x01, 0x01]);
        // A plan's bound seen from the real binary: Timestamp(ms) 3005.
        assert_eq!(Val::Ts(3005).canon().unwrap(), [8, 4, 2, 128, 0, 0, 0, 0, 0, 11, 189]);
        assert!(Val::Ts(-1).canon() < Val::Ts(0).canon() && Val::F64(-2.0).canon() < Val::F64(1.0).canon());
        assert_eq!(Val::Date(1).canon().unwrap()[..3], [0x08, 0x01, 0x05]);
    }

    #[test]
    fn in_and_between_filter_by_canonical_tuples() {
        let f = frame(&["sid", "seq", "t"]);
        let rs = rows(&f, vec![
            vec![s("a"), Val::I64(1), Val::Ts(10)],
            vec![s("a"), Val::I64(2), Val::Ts(20)],
            vec![Val::Null, Val::I64(1), Val::Ts(30)],
            vec![s("b"), Val::Null, Val::Null],
        ]);
        let key = [s("a"), Val::I64(2)];
        let p = Pred::In { columns: vec!["sid".into(), "seq".into()], values: vec![encode_tuple(&key), {
            let mut v = cs("b");
            v.push(0);
            v
        }] };
        assert_eq!(ns(&eval(&p, rs).unwrap()), [1, 3], "a trailing null is the 0x00 marker; a leading null never matches");
        let rs = rows(&f, vec![vec![s("a"), Val::I64(1), Val::Ts(10)], vec![s("a"), Val::I64(1), Val::Ts(20)], vec![s("a"), Val::I64(1), Val::Null]]);
        let p = Pred::Between { column: "t".into(), lo: canon_ts(10), hi: canon_ts(15) };
        assert_eq!(ns(&eval(&p, rs).unwrap()), [0], "inclusive bounds; a null is outside every window");
    }

    #[test]
    fn not_older_than_and_latest_per_key_keep_order_and_ties() {
        let f = frame(&["id", "t"]);
        let rs = rows(&f, vec![
            vec![s("a"), Val::Ts(1)],
            vec![s("a"), Val::Ts(3)],
            vec![s("b"), Val::Ts(1)],
            vec![s("a"), Val::Ts(3)],
            vec![s("c"), Val::Ts(9)],
            vec![s("a"), Val::Null],
        ]);
        let p = Pred::NotOlderThan { columns: vec!["id".into()], ts_column: "t".into(), bounds: vec![(cs("a"), canon_ts(2))] };
        let kept = eval(&p, rs).unwrap();
        assert_eq!(ns(&kept), [1, 2, 3, 4], "keys without a bound are kept; a null time is older than any bound");
        let p = Pred::LatestPerKey { columns: vec!["id".into()], ts_column: "t".into() };
        assert_eq!(ns(&eval(&p, kept).unwrap()), [1, 2, 3, 4], "per key the greatest time, ties kept");
    }

    #[test]
    fn list_membership_any_and_all() {
        let f = frame(&["facets"]);
        let l = |x: &[&str]| Val::List(x.iter().map(|s| s.to_string()).collect());
        let rs = || rows(&f, vec![vec![l(&["venue", "tenor"])], vec![l(&["venue"])], vec![l(&[])], vec![Val::Null]]);
        let any = Pred::ContainsAny { column: "facets".into(), values: vec![cs("tenor"), cs("venue")] };
        assert_eq!(ns(&eval(&any, rs()).unwrap()), [0, 1]);
        let all = Pred::ContainsAll { column: "facets".into(), values: vec![vec![cs("tenor")], vec![cs("venue")]] };
        assert_eq!(ns(&eval(&all, rs()).unwrap()), [0]);
    }

    #[test]
    fn text_residuals_fold_and_split_like_bellwether() {
        let f = frame(&["tool_cmd"]);
        let rs = || {
            rows(&f, vec![
                vec![s("cd x && cargo test --workspace")],
                vec![s("cargo build")],
                vec![s("CARGO TEST")],
                vec![Val::Null],
            ])
        };
        let has = |w: &str| Pred::Text { column: "tool_cmd".into(), operator: TextOp::HasToken, values: vec![w.into()] };
        assert_eq!(ns(&eval(&Pred::And { all: vec![has("cargo"), has("test")] }, rs()).unwrap()), [0, 2]);
        let any = Pred::Text { column: "tool_cmd".into(), operator: TextOp::HasToken, values: vec!["build".into(), "workspace".into()] };
        assert_eq!(ns(&eval(&any, rs()).unwrap()), [0, 1], "one of the values");
        let pre = Pred::Text { column: "tool_cmd".into(), operator: TextOp::HasPrefix, values: vec!["work".into()] };
        assert_eq!(ns(&eval(&pre, rs()).unwrap()), [0]);
        let sub = Pred::Text { column: "tool_cmd".into(), operator: TextOp::Contains, values: vec!["o te".into()] };
        assert_eq!(ns(&eval(&sub, rs()).unwrap()), [0, 2]);
        // The request: every word of every value, one residual each.
        let r = request(&QuerySpec { table: "events".into(), key: "text:tool_cmd".into(), values: vec!["Cargo test".into(), "cargo".into()], ..Default::default() }).unwrap();
        assert_eq!(r.tokens, ["cargo", "test"]);
        assert_eq!(r.local, vec![has("cargo"), has("test")]);
        assert!(request(&QuerySpec { table: "events".into(), key: "text:tool_cmd".into(), values: vec!["&&".into()], ..Default::default() }).is_err());
    }

    #[test]
    fn residual_json_parses_as_bellwether_prints_it_and_unknown_kinds_are_refused() {
        let v = json!({"op":"and","all":[
            {"op":"in","columns":["entry_id"],"values":[[6,97,0,0]]},
            {"op":"between","column":"valid","lo":[8,4,2,128,0,0,0,0,0,0,1],"hi":[8,4,2,128,0,0,0,0,0,0,2]},
            {"op":"not_older_than","columns":["entry_id"],"ts_column":"valid","bounds":[[[6,97,0,0],[8,4,2,128,0,0,0,0,0,0,1]]]},
            {"op":"latest_per_key","columns":["entry_id"],"ts_column":"valid"},
            {"op":"contains_all","column":"facets","values":[[[6,97,0,0]]]},
            {"op":"contains_any","column":"facets","values":[[6,97,0,0]]},
            {"op":"text","column":"tool_cmd","operator":"has_token","values":["CARGO"]}]});
        let Pred::And { all } = parse_residual(&v).unwrap() else { panic!() };
        assert_eq!(all.len(), 7);
        assert_eq!(all[6], Pred::Text { column: "tool_cmd".into(), operator: TextOp::HasToken, values: vec!["CARGO".into()] });
        let bad = json!({"op":"and","all":[{"op":"semi_join","columns":["a"],"keys_from":{}}]});
        let e = parse_residual(&bad).unwrap_err().to_string();
        assert!(e.contains("`semi_join`"), "{e}");
        assert_eq!(residual_summary(&v)["all"][0]["values"], 1);
    }

    #[test]
    fn requests_refuse_what_no_key_serves() {
        let q = |table: &str, key: &str| QuerySpec { table: table.into(), key: key.into(), values: vec!["x".into()], ..Default::default() };
        assert!(request(&QuerySpec { latest: true, ..q("events", "exact:path") }).is_err());
        assert!(request(&QuerySpec { all: true, ..q("events", "exact:path") }).is_err());
        assert!(request(&QuerySpec { window: Some((0, 1)), latest: true, ..q("counters", "temporal:content_sha256") }).is_err());
        assert!(request(&q("sessions", "range:started_at")).is_err());
        let r = request(&QuerySpec { values: vec![], window: Some((0, 1)), ..q("sessions", "range:started_at") }).unwrap();
        assert!(r.unplannable.is_some());
        assert!(request(&q("entries", "temporal2:entry_id")).unwrap().unplannable.is_some());
        let r = request(&QuerySpec { values: vec!["cc:1\t3".into(), "cc:2:4".into()], ..q("redirects", "temporal:session_id,redirect_seq") }).unwrap();
        assert_eq!(r.tuples, vec![vec![s("cc:1"), Val::I64(3)], vec![s("cc:2"), Val::I64(4)]]);
    }

    #[test]
    fn partition_values_follow_the_templates() {
        let t = index::table("events").unwrap();
        let v = path_values(t, Path::new("omp/2026-10-26/cc:abc.parquet"));
        assert_eq!(v[2], ("session_id", s("cc:abc")));
        assert_eq!(v[1].1.to_json(), json!("2026-10-26"));
        let v = path_values(index::table("outcomes").unwrap(), Path::new("quant.b/2026-10-26/my-host-12.parquet"));
        assert_eq!(v, vec![("domain", s("quant.b")), ("day", day_val("2026-10-26").unwrap()), ("machine", s("my-host")), ("run_seq", Val::I64(12))]);
    }
}
