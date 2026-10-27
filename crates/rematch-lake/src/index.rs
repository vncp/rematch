//! `lake index` (ADR 001 L3): one Bellwether index root per table under
//! `REMATCH_ROOT/lake-index/<table>/`, built by the external `bellwether` binary with
//! exactly the ADR's key table. The version that built them is pinned in
//! `lake/manifest.json`; another version is refused until `lake index --repin` rebuilds
//! every index under it (the lake files are never touched).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::{lake_dir, Manifest};

/// What a key serves, which decides the Bellwether lookup `lake query` runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    /// `exact:` point lookups (`locate`), optionally in a window.
    Exact,
    /// `text:` words (`locate --has`, one locate per token, no window): rows whose
    /// column has every token.
    Text,
    /// `list:` membership (`locate --any|--all`, no window).
    List,
    /// `range:` prunes another key's window; no lookup of its own.
    Range,
    /// `temporal:` point lookups, `latest`, `asof --at`.
    Temporal,
    /// `temporal2:` sidecar: `latest --known`, `asof --valid --known` only.
    Temporal2,
}

/// Type of a key column in the key file (and in the dataset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyType {
    Utf8,
    Int64,
}

/// One declared key of a table (ADR 001 §"Tables and keys").
#[derive(Debug)]
pub struct KeyDef {
    /// The `bellwether index --key` argument; also the spec's identity (or the sidecar's
    /// name) under which the index manifest records its completeness.
    pub spec: &'static str,
    /// The name `lake query --key` takes.
    pub name: &'static str,
    pub kind: KeyKind,
    /// Key-file columns in order (dataset columns); a text key's column.
    pub columns: &'static [&'static str],
    pub types: &'static [KeyType],
    /// The time column: temporal `ts`, temporal2 `valid`, the range column.
    pub time: Option<&'static str>,
    /// temporal2's known-time column.
    pub known: Option<&'static str>,
}

/// One lake table: its root under `lake/`, the path template Bellwether reads its
/// partition columns from, the column a `lake query` window applies to, and its keys.
#[derive(Debug)]
pub struct TableDef {
    pub name: &'static str,
    pub template: &'static str,
    pub window: &'static str,
    pub keys: &'static [KeyDef],
}

const U: &[KeyType] = &[KeyType::Utf8];

const fn exact(spec: &'static str, name: &'static str, columns: &'static [&'static str]) -> KeyDef {
    KeyDef { spec, name, kind: KeyKind::Exact, columns, types: U, time: None, known: None }
}

const fn list(spec: &'static str, columns: &'static [&'static str]) -> KeyDef {
    KeyDef { spec, name: spec, kind: KeyKind::List, columns, types: U, time: None, known: None }
}

/// Append tables' template. The run's first journal seq is `{run_seq}`, not `{seq}`: a
/// file stores the column `seq` with one value per row, and Bellwether refuses a path
/// placeholder whose stored column holds more than the path's value (DESIGN §19.2).
const RUN: &str = "{day:date}/{machine}-{run_seq:int}.parquet";

pub const TABLE_DEFS: &[TableDef] = &[
    TableDef {
        name: "events",
        template: "{harness}/{day:date}/{session_id}.parquet",
        window: "t",
        keys: &[
            exact("exact:tool_cmd,counts", "exact:tool_cmd", &["tool_cmd"]),
            KeyDef {
                spec: "text:tool_cmd,tokenizer=words",
                name: "text:tool_cmd",
                kind: KeyKind::Text,
                columns: &["tool_cmd"],
                types: U,
                time: None,
                known: None,
            },
            exact("exact:path", "exact:path", &["path"]),
            exact("exact:skill_read", "exact:skill_read", &["skill_read"]),
        ],
    },
    TableDef {
        name: "sessions",
        template: RUN,
        window: "started_at",
        keys: &[
            exact("exact:repo_remote,counts", "exact:repo_remote", &["repo_remote"]),
            exact("exact:domain", "exact:domain", &["domain"]),
            list("list:loaded_entry_shas", &["loaded_entry_shas"]),
            list("list:exts", &["exts"]),
            list("list:stems", &["stems"]),
            KeyDef {
                spec: "range:started_at",
                name: "range:started_at",
                kind: KeyKind::Range,
                columns: &[],
                types: &[],
                time: Some("started_at"),
                known: None,
            },
        ],
    },
    TableDef {
        name: "redirects",
        template: RUN,
        window: "known",
        keys: &[
            KeyDef {
                spec: "temporal:session_id,redirect_seq@known",
                name: "temporal:session_id,redirect_seq",
                kind: KeyKind::Temporal,
                columns: &["session_id", "redirect_seq"],
                types: &[KeyType::Utf8, KeyType::Int64],
                time: Some("known"),
                known: None,
            },
            exact("exact:class", "exact:class", &["class"]),
        ],
    },
    TableDef {
        name: "entries",
        template: RUN,
        window: "valid",
        keys: &[
            KeyDef {
                spec: "temporal2:entry_id@valid,known",
                name: "temporal2:entry_id",
                kind: KeyKind::Temporal2,
                columns: &["entry_id"],
                types: U,
                time: Some("valid"),
                known: Some("known"),
            },
            exact("exact:content_sha256", "exact:content_sha256", &["content_sha256"]),
        ],
    },
    TableDef {
        name: "counters",
        template: RUN,
        window: "ts",
        keys: &[KeyDef {
            spec: "temporal:content_sha256@ts",
            name: "temporal:content_sha256",
            kind: KeyKind::Temporal,
            columns: &["content_sha256"],
            types: U,
            time: Some("ts"),
            known: None,
        }],
    },
    TableDef { name: "proposals", template: RUN, window: "created_at", keys: &[exact("exact:entry_id", "exact:entry_id", &["entry_id"])] },
    TableDef { name: "receipts", template: RUN, window: "created_at", keys: &[exact("exact:entry_id", "exact:entry_id", &["entry_id"])] },
    TableDef {
        name: "outcomes",
        template: "{domain}/{day:date}/{machine}-{run_seq:int}.parquet",
        window: "valid",
        keys: &[
            exact("exact:signature", "exact:signature", &["signature"]),
            KeyDef {
                spec: "temporal2:subject_id@valid,known",
                name: "temporal2:subject_id",
                kind: KeyKind::Temporal2,
                columns: &["subject_id"],
                types: U,
                time: Some("valid"),
                known: Some("known"),
            },
            list("list:facets", &["facets"]),
        ],
    },
];

/// The table named `name`.
pub fn table(name: &str) -> Result<&'static TableDef> {
    TABLE_DEFS.iter().find(|t| t.name == name).ok_or_else(|| {
        anyhow!(
            "unknown lake table {name:?}; tables: {}",
            TABLE_DEFS.iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
        )
    })
}

impl TableDef {
    /// The key `name` names: its query name or its full spec.
    pub fn key(&self, name: &str) -> Result<&'static KeyDef> {
        let keys: &'static [KeyDef] = self.keys;
        keys.iter().find(|k| k.name == name || k.spec == name).ok_or_else(|| {
            anyhow!(
                "table {} has no key {name:?}; its keys: {}",
                self.name,
                keys.iter().map(|k| k.name).collect::<Vec<_>>().join(", ")
            )
        })
    }
}

/// `REMATCH_ROOT/lake-index`.
pub fn index_root(root: &Path) -> PathBuf {
    root.join("lake-index")
}

/// The Bellwether binary: `explicit`, else `$REMATCH_BELLWETHER`, else `bellwether` on
/// `PATH`; `None` when there is none.
pub fn resolve_bin(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Some(v) = std::env::var_os("REMATCH_BELLWETHER").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join("bellwether")).find(|p| p.is_file())
}

/// `bellwether --version` without the program name (`0.1.0`).
pub fn version(bin: &Path) -> Result<String> {
    let out = Command::new(bin).arg("--version").output().with_context(|| format!("run {} --version", bin.display()))?;
    if !out.status.success() {
        bail!("{} --version: {}", bin.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    Ok(s.strip_prefix("bellwether").map(str::trim).unwrap_or(s).to_string())
}

/// Runs `bellwether <args>` and parses the JSON line it prints (`--json` is in `args`).
pub(crate) fn run_bw(bin: &Path, args: &[String]) -> Result<Value> {
    let out = Command::new(bin).args(args).output().with_context(|| format!("run {}", bin.display()))?;
    if !out.status.success() {
        bail!(
            "bellwether {} failed (exit {}): {}",
            args.first().map(String::as_str).unwrap_or(""),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
    serde_json::from_str(line).with_context(|| format!("bellwether {}: output is not JSON: {line}", args[0]))
}

/// Whether Bellwether lists a file: it skips every path component under the root that
/// starts with `.` or `_` (hidden files, `_SUCCESS` markers).
fn bellwether_lists(rel: &Path) -> bool {
    rel.iter().all(|c| c.to_str().is_some_and(|n| !n.starts_with('.') && !n.starts_with('_')))
}

/// An exporter temp file (`.<name>.tmp-<pid>`), never part of the lake.
fn is_temp(rel: &Path) -> bool {
    rel.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.') && n.contains(".tmp-"))
}

/// A table's root, canonical when it exists (Bellwether records canonical paths, so
/// listings compare with a plan's and an index manifest's).
pub(crate) fn table_dir(lake: &Path, table: &str) -> PathBuf {
    let d = lake.join(table);
    rematch_core::paths::canonicalize(&d).unwrap_or(d)
}

/// The table's Parquet files: those Bellwether lists, and those it skips (a `_`- or
/// `.`-named directory or file; exporter temp files are neither).
pub(crate) fn table_files(lake: &Path, table: &str) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let dir = table_dir(lake, table);
    let (mut seen, mut hidden) = (Vec::new(), Vec::new());
    for p in crate::io::walk(&dir)? {
        let rel = p.strip_prefix(&dir).unwrap_or(&p);
        if is_temp(rel) || p.extension().and_then(|e| e.to_str()) != Some("parquet") {
            continue;
        }
        if bellwether_lists(rel) {
            seen.push(p);
        } else {
            hidden.push(p);
        }
    }
    Ok((seen, hidden))
}

/// How an index root stands against its table's files: `None` without an index, else
/// the files it does not track (new or changed in size) and the tracked files gone.
pub(crate) fn staleness(root: &Path, table: &str) -> Result<Option<(Vec<String>, Vec<String>)>> {
    let mp = index_root(root).join(table).join("manifest.json");
    let text = match std::fs::read_to_string(&mp) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", mp.display())),
    };
    let m: Value = serde_json::from_str(&text).with_context(|| format!("parse {}", mp.display()))?;
    let tracked: BTreeMap<String, u64> = m["rowgroup_space"]["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| Some((f["fingerprint"]["path"].as_str()?.to_string(), f["fingerprint"]["size"].as_u64()?)))
        .collect();
    let (seen, _) = table_files(&lake_dir(root), table)?;
    let mut untracked = Vec::new();
    let mut present = BTreeSet::new();
    for p in &seen {
        let ps = p.to_string_lossy().into_owned();
        let size = std::fs::metadata(p).map(|m| m.len()).ok();
        if tracked.get(&ps).copied() != size {
            untracked.push(ps.clone());
        }
        present.insert(ps);
    }
    let gone = tracked.keys().filter(|p| !present.contains(*p)).cloned().collect();
    Ok(Some((untracked, gone)))
}

/// The version check every index build and indexed query makes.
pub(crate) fn pin_mismatch(pinned: Option<&str>, version: &str) -> Option<String> {
    pinned.filter(|p| *p != version).map(|p| {
        format!(
            "the lake's indexes were built by bellwether {p} and this binary is {version}; \
             run `rematch lake index --repin` to rebuild every index under {version}"
        )
    })
}

/// Indexes one table root with its keys (incremental: Bellwether reads only new or
/// changed files). Returns Bellwether's result, or `None` when the table has no files.
/// First, `events` files exported before `tool_cmd` are upgraded (`events::upgrade`;
/// `upgraded` in the result), and an index recording a key this table no longer declares
/// (`expr:tool_cmd` before `exact:`/`text:tool_cmd`) is removed and built afresh
/// (`rebuilt: true`): Bellwether keeps every spec an index was built with.
pub(crate) fn index_table(root: &Path, bin: &Path, t: &TableDef) -> Result<Option<Value>> {
    let lake = lake_dir(root);
    let (seen, _) = table_files(&lake, t.name)?;
    if seen.is_empty() {
        return Ok(None);
    }
    let mut upgraded = 0usize;
    if t.name == "events" {
        for p in &seen {
            upgraded += usize::from(crate::events::upgrade(p)?);
        }
    }
    let idx = index_root(root).join(t.name);
    let rebuilt = built_specs(&idx)?.is_some_and(|s| s.iter().any(|spec| !t.keys.iter().any(|k| k.spec == spec)));
    if rebuilt {
        std::fs::remove_dir_all(&idx).with_context(|| format!("remove {}", idx.display()))?;
    }
    std::fs::create_dir_all(index_root(root)).with_context(|| format!("create {}", index_root(root).display()))?;
    let mut args: Vec<String> = vec![
        "index".into(),
        "--json".into(),
        lake.join(t.name).to_string_lossy().into_owned(),
        "--index".into(),
        idx.to_string_lossy().into_owned(),
        "--path-template".into(),
        t.template.into(),
    ];
    for k in t.keys {
        args.push("--key".into());
        args.push(k.spec.into());
    }
    let mut r = run_bw(bin, &args).with_context(|| format!("index table {}", t.name))?;
    r["upgraded"] = json!(upgraded);
    r["rebuilt"] = json!(rebuilt);
    Ok(Some(r))
}

/// The key specs an index root's manifest records as built (`None` without an index).
pub(crate) fn built_specs(idx: &Path) -> Result<Option<Vec<String>>> {
    let mp = idx.join("manifest.json");
    let text = match std::fs::read_to_string(&mp) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", mp.display())),
    };
    let m: Value = serde_json::from_str(&text).with_context(|| format!("parse {}", mp.display()))?;
    Ok(Some(m["key_spec_completeness"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default()))
}

/// `rematch lake index [<table>…] [--repin]`. Callers hold the lake lock (the manifest is
/// rewritten when the pin changes).
pub fn index(root: &Path, tables: &[String], bin: Option<&Path>, repin: bool) -> Result<Value> {
    let bin = resolve_bin(bin).ok_or_else(|| {
        anyhow!("no bellwether binary: pass --bellwether <path>, set REMATCH_BELLWETHER, or put bellwether on PATH")
    })?;
    let version = version(&bin)?;
    let lake = lake_dir(root);
    let mut m = Manifest::load(&lake)?;
    let mismatch = pin_mismatch(m.bellwether_version_pinned.as_deref(), &version);
    if let (Some(why), false) = (&mismatch, repin) {
        bail!("{why}");
    }
    let repinned = mismatch.is_some();
    let chosen: Vec<&'static TableDef> = if tables.is_empty() || repinned {
        TABLE_DEFS.iter().collect()
    } else {
        tables.iter().map(|n| table(n)).collect::<Result<_>>()?
    };
    if repinned {
        // Every index was built by another version: rebuilt from scratch, never upgraded.
        for t in TABLE_DEFS {
            let d = index_root(root).join(t.name);
            if d.exists() {
                std::fs::remove_dir_all(&d).with_context(|| format!("remove {}", d.display()))?;
            }
        }
    }
    let mut out = Vec::new();
    let mut lines = Vec::new();
    for t in chosen {
        let (_, hidden) = table_files(&lake, t.name)?;
        let hidden: Vec<String> = hidden.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        match index_table(root, &bin, t)? {
            None => {
                lines.push(format!("{}: no files", t.name));
                out.push(json!({"table": t.name, "indexed": false, "reason": "no files"}));
            }
            Some(r) => {
                lines.push(format!(
                    "{}: sequence {}, files new {} changed {} removed {} unchanged {}; segments written {}{}",
                    t.name,
                    r["sequence"],
                    r["files_new"],
                    r["files_changed"],
                    r["files_removed"],
                    r["files_unchanged"],
                    r["segments_written"],
                    if hidden.is_empty() {
                        String::new()
                    } else {
                        format!("; {} file(s) Bellwether skips (a `_`/`.` path component): queries scan this table", hidden.len())
                    }
                ));
                if r["upgraded"].as_u64().unwrap_or(0) > 0 {
                    lines.push(format!("{}: {} file(s) exported before tool_cmd upgraded in place", t.name, r["upgraded"]));
                }
                if r["rebuilt"] == true {
                    lines.push(format!("{}: the index recorded keys this table no longer declares; rebuilt", t.name));
                }
                out.push(json!({
                    "table": t.name,
                    "indexed": true,
                    "index": index_root(root).join(t.name),
                    "keys": t.keys.iter().map(|k| k.spec).collect::<Vec<_>>(),
                    "path_template": t.template,
                    "sequence": r["sequence"],
                    "files_new": r["files_new"],
                    "files_changed": r["files_changed"],
                    "files_removed": r["files_removed"],
                    "files_unchanged": r["files_unchanged"],
                    "segments_written": r["segments_written"],
                    "unindexed_files": hidden,
                    "upgraded": r["upgraded"],
                    "rebuilt": r["rebuilt"],
                }));
            }
        }
    }
    if m.bellwether_version_pinned.as_deref() != Some(version.as_str()) {
        // Reloaded under the caller's lock so nothing written since the first load is lost.
        m = Manifest::load(&lake)?;
        m.bellwether_version_pinned = Some(version.clone());
        m.save(&lake)?;
        lines.insert(0, format!("pinned bellwether {version} in {}", Manifest::path(&lake).display()));
    }
    Ok(json!({
        "bellwether": bin,
        "version": version,
        "pinned": version,
        "repinned": repinned,
        "tables": out,
        "lines": lines,
    }))
}

/// The index half of `lake doctor`: the pinned version against the binary's, each
/// table's index (present, files it does not track, key completeness), and the sessions
/// that loaded entries, were counted, and have no counters row (the attribution anti-join).
pub fn doctor(root: &Path, bin: Option<&Path>) -> Result<Value> {
    let lake = lake_dir(root);
    let m = Manifest::load(&lake)?;
    let bin = resolve_bin(bin);
    let version = bin.as_deref().map(version).transpose().ok().flatten();
    let refused = version.as_deref().and_then(|v| pin_mismatch(m.bellwether_version_pinned.as_deref(), v));
    let mut lines = vec![format!(
        "bellwether: {} (version {}), pinned {}",
        bin.as_deref().map(|b| b.display().to_string()).unwrap_or_else(|| "not found".into()),
        version.as_deref().unwrap_or("unknown"),
        m.bellwether_version_pinned.as_deref().unwrap_or("none")
    )];
    if let Some(r) = &refused {
        lines.push(format!("index refused: {r}"));
    }
    let mut tables = Vec::new();
    for t in TABLE_DEFS {
        let idx = index_root(root).join(t.name);
        let (_, hidden) = table_files(&lake, t.name)?;
        let Some((untracked, gone)) = staleness(root, t.name)? else {
            lines.push(format!("index {}: none", t.name));
            tables.push(json!({"table": t.name, "indexed": false}));
            continue;
        };
        let mtext = std::fs::read_to_string(idx.join("manifest.json"))?;
        let mj: Value = serde_json::from_str(&mtext)?;
        let complete = &mj["key_spec_completeness"];
        let keys: Vec<Value> =
            t.keys.iter().map(|k| json!({"key": k.spec, "complete": complete.get(k.spec).is_some()})).collect();
        let incomplete: Vec<&str> = t.keys.iter().filter(|k| complete.get(k.spec).is_none()).map(|k| k.spec).collect();
        lines.push(format!(
            "index {}: {} untracked, {} gone, {} skipped file(s); keys {}",
            t.name,
            untracked.len(),
            gone.len(),
            hidden.len(),
            if incomplete.is_empty() { "all complete".to_string() } else { format!("incomplete: {}", incomplete.join(" ")) }
        ));
        tables.push(json!({
            "table": t.name, "indexed": true, "untracked": untracked, "gone": gone,
            "skipped": hidden.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            "keys": keys,
        }));
    }
    let uncounted = sessions_without_counters(root)?;
    lines.push(format!("sessions counted with loaded entries and no counters row: {}", uncounted.len()));
    for s in &uncounted {
        lines.push(format!("  no counters row: {s}"));
    }
    Ok(json!({
        "bellwether": bin,
        "bellwether_version": version,
        "bellwether_version_pinned": m.bellwether_version_pinned,
        "index_refused": refused,
        "indexes": tables,
        "sessions_without_counters": uncounted,
        "lines": lines,
    }))
}

/// Session ids whose newest `sessions` row is counted and lists loaded entries, with no
/// `counters` row for that session in the lake (every machine's files).
pub fn sessions_without_counters(root: &Path) -> Result<Vec<String>> {
    let mut newest: BTreeMap<(String, String), (i64, Value)> = BTreeMap::new();
    for r in crate::query::scan(root, "sessions")? {
        let (Some(sid), Some(seq)) = (r["session_id"].as_str(), r["seq"].as_i64()) else { continue };
        let machine = r["machine"].as_str().unwrap_or("").to_string();
        let k = (machine, sid.to_string());
        if newest.get(&k).is_none_or(|(s, _)| seq > *s) {
            newest.insert(k, (seq, r));
        }
    }
    let counted: BTreeSet<String> = crate::query::scan(root, "counters")?
        .iter()
        .filter_map(|r| r["session_id"].as_str().map(str::to_string))
        .collect();
    let mut out: Vec<String> = newest
        .into_iter()
        .filter(|(_, (_, r))| {
            r["counted"].as_bool() == Some(true)
                && r["loaded_entry_ids"].as_array().is_some_and(|a| !a.is_empty())
        })
        .map(|((_, sid), _)| sid)
        .filter(|sid| !counted.contains(sid))
        .collect();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_is_listed_once_and_keys_resolve_by_name_or_spec() {
        let names: BTreeSet<&str> = TABLE_DEFS.iter().map(|t| t.name).collect();
        assert_eq!(names.len(), TABLE_DEFS.len());
        assert!(names.contains("events") && crate::tables::TABLES.iter().all(|t| names.contains(t)));
        let ev = table("events").unwrap();
        assert_eq!(ev.key("exact:tool_cmd").unwrap().columns, &["tool_cmd"]);
        assert_eq!(ev.key("text:tool_cmd,tokenizer=words").unwrap().kind, KeyKind::Text);
        assert!(ev.key("expr:tool_cmd").is_err(), "the virtual column gave way to the stored one");
        assert!(table("entries").unwrap().key("exact:path").is_err());
        assert!(table("nope").is_err());
        for t in TABLE_DEFS {
            for k in t.keys {
                assert_eq!(k.columns.len(), k.types.len(), "{}", k.spec);
            }
        }
    }

    #[test]
    fn bellwether_skips_hidden_components_and_temp_files_are_not_lake_files() {
        assert!(bellwether_lists(Path::new("2026-10-26/m-1.parquet")));
        assert!(!bellwether_lists(Path::new("_/2026-10-26/m-1.parquet")));
        assert!(!bellwether_lists(Path::new("omp/2026-10-26/.x.parquet")));
        assert!(is_temp(Path::new("2026-10-26/.m-1.parquet.tmp-12")));
        assert!(!is_temp(Path::new("2026-10-26/m-1.parquet")));
    }

    #[test]
    fn a_different_version_is_refused_by_name() {
        assert!(pin_mismatch(None, "0.1.0").is_none());
        assert!(pin_mismatch(Some("0.1.0"), "0.1.0").is_none());
        let why = pin_mismatch(Some("0.1.0"), "0.2.0").unwrap();
        assert!(why.contains("0.1.0") && why.contains("0.2.0") && why.contains("--repin"), "{why}");
    }
}
