//! The seven append tables (ADR 001 §"The lake"): one row per journal row, flat typed
//! columns for the keys in the ADR's key table, and the full record as written in `json`.
//! Partition columns (`day`; `domain` for outcomes) live in the path, never in the file.

use rematch_core::ledger::{subject_id, JournalRow};
use rematch_core::predicate::predicate_hash;
use rematch_core::types::{Predicate, Subject};
use serde_json::Value;

use crate::columns::{b, f, i, js, l, opt, req, s, Cell, Col, Ty};

/// Journal tables in the order the ADR lists them.
pub const TABLES: [&str; 7] = ["sessions", "redirects", "entries", "counters", "proposals", "receipts", "outcomes"];

/// Leading columns of every append table: the journal row's identity and clock.
const COMMON: [Col; 5] = [
    req("seq", Ty::Int64),
    req("known", Ty::TsMs),
    req("machine", Ty::Utf8),
    req("op", Ty::Utf8),
    req("row_key", Ty::Utf8),
];

/// Trailing column of every append table: the full record as the ledger wrote it.
const JSON: Col = req("json", Ty::Utf8);

const SESSIONS: &[Col] = &[
    opt("session_id", Ty::Utf8),
    opt("repo_remote", Ty::Utf8),
    opt("subpath", Ty::Utf8),
    opt("domain", Ty::Utf8),
    opt("started_at", Ty::TsMs),
    opt("ended_at", Ty::TsMs),
    opt("family", Ty::Utf8),
    opt("loaded_entry_ids", Ty::ListUtf8),
    opt("loaded_entry_shas", Ty::ListUtf8),
    opt("visible_entries", Ty::ListUtf8),
    opt("skill_reads", Ty::ListUtf8),
    opt("exts", Ty::ListUtf8),
    opt("stems", Ty::ListUtf8),
    opt("redirects_json", Ty::Utf8),
    opt("flagged", Ty::Bool),
    opt("counted", Ty::Bool),
];

const REDIRECTS: &[Col] = &[
    opt("session_id", Ty::Utf8),
    // The redirect's trace seq; `seq` is the journal seq.
    opt("redirect_seq", Ty::Int64),
    opt("prelabel", Ty::Utf8),
    opt("class", Ty::Utf8),
    opt("confidence", Ty::Float64),
    opt("text_head", Ty::Utf8),
    opt("t", Ty::TsMs),
];

const ENTRIES: &[Col] = &[
    opt("entry_id", Ty::Utf8),
    opt("status", Ty::Utf8),
    opt("version", Ty::Int64),
    opt("content_sha256", Ty::Utf8),
    opt("predicate_hash", Ty::Utf8),
    opt("name", Ty::Utf8),
    opt("branch", Ty::Utf8),
    opt("valid", Ty::TsMs),
];

const COUNTERS: &[Col] = &[
    opt("entry_id", Ty::Utf8),
    opt("session_id", Ty::Utf8),
    opt("content_sha256", Ty::Utf8),
    opt("predicate_hash", Ty::Utf8),
    opt("family", Ty::Utf8),
    opt("matched", Ty::Int64),
    opt("recurrence", Ty::Int64),
    opt("harm", Ty::Int64),
    opt("tokens", Ty::Int64),
    opt("turns", Ty::Int64),
    opt("ts", Ty::TsMs),
];

const PROPOSALS: &[Col] = &[
    opt("id", Ty::Utf8),
    opt("entry_id", Ty::Utf8),
    opt("session_id", Ty::Utf8),
    opt("blob", Ty::Utf8),
    opt("lint_ok", Ty::Bool),
    opt("created_at", Ty::TsMs),
];

const RECEIPTS: &[Col] = &[
    opt("id", Ty::Utf8),
    opt("entry_id", Ty::Utf8),
    opt("certificate_hash", Ty::Utf8),
    opt("activation", Ty::Utf8),
    opt("created_at", Ty::TsMs),
];

const OUTCOMES: &[Col] = &[
    opt("id", Ty::Utf8),
    opt("session_id", Ty::Utf8),
    opt("subject_id", Ty::Utf8),
    opt("signature", Ty::Utf8),
    opt("claim", Ty::Utf8),
    opt("facets", Ty::ListUtf8),
    opt("valid", Ty::TsMs),
    opt("result_status", Ty::Utf8),
];

fn specific(table: &str) -> Option<&'static [Col]> {
    Some(match table {
        "sessions" => SESSIONS,
        "redirects" => REDIRECTS,
        "entries" => ENTRIES,
        "counters" => COUNTERS,
        "proposals" => PROPOSALS,
        "receipts" => RECEIPTS,
        "outcomes" => OUTCOMES,
        _ => return None,
    })
}

/// Full column list of a journal table (`None` for an unknown table).
pub fn columns(table: &str) -> Option<Vec<Col>> {
    let mid = specific(table)?;
    let mut v = Vec::with_capacity(COMMON.len() + mid.len() + 1);
    v.extend_from_slice(&COMMON);
    v.extend_from_slice(mid);
    v.push(JSON);
    Some(v)
}

/// One journal row as cells aligned with [`columns`]`(row.table)`.
pub fn cells(row: &JournalRow) -> Vec<Cell> {
    let j = &row.json;
    let g = |k: &str| j.get(k);
    let mut v = vec![
        Cell::I(row.seq),
        Cell::I(row.known),
        Cell::S(row.machine.clone()),
        Cell::S(row.op.clone()),
        Cell::S(row.row_key.clone()),
    ];
    match row.table.as_str() {
        "sessions" => v.extend([
            s(g("id")),
            s(g("repo_remote")),
            s(g("subpath")),
            s(g("domain")),
            i(g("started_at")),
            i(g("ended_at")),
            s(g("family")),
            l(g("loaded_entries")),
            l(g("loaded_entry_shas")),
            l(g("visible_entries")),
            l(g("skill_reads")),
            l(g("exts")),
            l(g("stems")),
            js(g("redirects")),
            b(g("flagged")),
            b(g("counted")),
        ]),
        "redirects" => v.extend([
            s(g("session_id")),
            i(g("seq")),
            s(g("prelabel")),
            s(g("class")),
            f(g("confidence")),
            s(g("text_head")),
            i(g("t")),
        ]),
        "entries" => v.extend([
            s(g("id")),
            s(g("status")),
            i(g("version")),
            s(g("content_sha256")),
            entry_predicate_hash(j),
            s(g("name")),
            s(g("branch")),
            match i(g("status_since")) {
                Cell::Null => i(g("created_at")),
                c => c,
            },
        ]),
        "counters" => v.extend([
            s(g("entry_id")),
            s(g("session_id")),
            s(g("content_sha256")),
            s(g("predicate_hash")),
            s(g("family")),
            i(g("matched")),
            i(g("recurrence")),
            i(g("harm")),
            i(g("tokens")),
            i(g("turns")),
            i(g("ts")),
        ]),
        "proposals" => v.extend([
            s(g("id")),
            s(g("entry_id")),
            s(g("session_id")),
            s(g("blob")),
            b(g("lint_ok")),
            i(g("created_at")),
        ]),
        "receipts" => {
            let r = g("receipt");
            v.extend([
                s(g("id")),
                s(r.and_then(|r| r.get("entry"))),
                s(r.and_then(|r| r.get("certificate"))),
                s(r.and_then(|r| r.get("activation"))),
                i(g("created_at")),
            ])
        }
        "outcomes" => v.extend([
            s(g("id")),
            s(g("session_id")),
            outcome_subject_id(j),
            s(g("signature")),
            s(g("claim")),
            Cell::L(facets(j)),
            i(g("ts")),
            s(g("result").and_then(|r| r.get("status"))),
        ]),
        _ => {}
    }
    v.push(Cell::S(j.to_string()));
    v
}

fn entry_predicate_hash(j: &Value) -> Cell {
    match j.get("predicate").and_then(|p| serde_json::from_value::<Predicate>(p.clone()).ok()) {
        Some(p) => Cell::S(predicate_hash(&p)),
        None => Cell::Null,
    }
}

fn outcome_subject_id(j: &Value) -> Cell {
    match j.get("subject").and_then(|x| serde_json::from_value::<Subject>(x.clone()).ok()) {
        Some(sub) => Cell::S(subject_id(&sub)),
        None => Cell::Null,
    }
}

/// `domain=<each dotted prefix>`, then `key=value` and bare `key` per facet, first-seen
/// order without repeats (ADR 001: `transfer suggest`'s overlap is one `contains_all`).
pub fn facets(j: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |x: String| {
        if !out.contains(&x) {
            out.push(x);
        }
    };
    let domain = j.get("subject").and_then(|x| x.get("domain")).and_then(Value::as_str).unwrap_or("");
    if !domain.is_empty() {
        let mut prefix = String::new();
        for part in domain.split('.') {
            if !prefix.is_empty() {
                prefix.push('.');
            }
            prefix.push_str(part);
            push(format!("domain={prefix}"));
        }
    }
    for fct in j.get("facets").and_then(Value::as_array).into_iter().flatten() {
        let (Some(k), Some(v)) = (fct.get("key").and_then(Value::as_str), fct.get("value").and_then(Value::as_str)) else {
            continue;
        };
        push(format!("{k}={v}"));
        push(k.to_string());
    }
    out
}

/// Extra partition directory of a row above `{day}`: the outcome's domain, else none.
pub fn partition(row: &JournalRow) -> Option<String> {
    (row.table == "outcomes").then(|| {
        let d = row.json.get("subject").and_then(|x| x.get("domain")).and_then(Value::as_str).unwrap_or("");
        path_component(d)
    })
}

/// A string safe as one path component (no separators, never empty or a dot name).
pub fn path_component(s: &str) -> String {
    let c: String = s.chars().map(|ch| if matches!(ch, '/' | '\\' | '\0') { '_' } else { ch }).collect();
    if c.is_empty() || c.starts_with('.') {
        format!("_{c}")
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_row_aligns_with_its_columns() {
        for t in TABLES {
            let row = JournalRow {
                seq: 1,
                known: 2,
                machine: "m".into(),
                table: t.into(),
                row_key: "k".into(),
                op: "upsert".into(),
                json: json!({}),
            };
            assert_eq!(cells(&row).len(), columns(t).unwrap().len(), "{t}");
        }
    }

    #[test]
    fn facets_hold_domain_prefixes_pairs_and_bare_keys() {
        let j = json!({"subject":{"domain":"quant.options"},"facets":[{"key":"venue","value":"cboe"},{"key":"tenor","value":"1w"}]});
        assert_eq!(
            facets(&j),
            vec!["domain=quant", "domain=quant.options", "venue=cboe", "venue", "tenor=1w", "tenor"]
        );
    }

    #[test]
    fn path_components_never_escape() {
        assert_eq!(path_component("a/b"), "a_b");
        assert_eq!(path_component(""), "_");
        assert_eq!(path_component(".."), "_..");
    }
}
