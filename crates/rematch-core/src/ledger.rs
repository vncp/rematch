//! Ledger (DESIGN §5, §13): SQLite in WAL mode plus a content-addressed blob store
//! beside it (`<ledger dir>/blobs/<sha256>`). Nested data lives in JSON columns; the
//! flat columns exist for filtering and indexes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::predicate::{hex, predicate_hash};
use crate::types::{
    Activation, Counters, Entry, EntryId, Family, FamilyCounters, Outcome, Receipt,
    RedirectClass, SessionRecord, Sha256Hex, Status, Subject, UnixMs,
};

pub const SCHEMA_VERSION: i64 = 3;

/// Ledger tables whose every write is journaled (ADR 001), in backfill order.
pub const JOURNAL_TABLES: &[&str] =
    &["sessions", "redirects", "entries", "counters", "proposals", "receipts", "outcomes"];

/// Ordered migrations; migration `i` brings the schema to version `i + 1`.
const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE sessions(
  id TEXT PRIMARY KEY, cwd TEXT NOT NULL, repo TEXT, branch TEXT NOT NULL,
  started_at INTEGER NOT NULL, ended_at INTEGER, family TEXT, provider TEXT, model TEXT,
  turns INTEGER NOT NULL DEFAULT 0, tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0,
  tool_calls INTEGER NOT NULL DEFAULT 0, tool_errors INTEGER NOT NULL DEFAULT 0,
  skill_reads TEXT NOT NULL DEFAULT '[]', files_touched TEXT NOT NULL DEFAULT '[]',
  exts TEXT NOT NULL DEFAULT '[]', stems TEXT NOT NULL DEFAULT '[]', redirects TEXT NOT NULL DEFAULT '[]',
  flagged INTEGER NOT NULL DEFAULT 0, counted INTEGER NOT NULL DEFAULT 0, source TEXT NOT NULL,
  json TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE INDEX sessions_branch ON sessions(branch, started_at);
CREATE INDEX sessions_counted ON sessions(counted, flagged);

CREATE TABLE redirects(
  id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, seq INTEGER NOT NULL,
  prelabel TEXT NOT NULL, class TEXT, confidence REAL, text_head TEXT NOT NULL,
  deliverable_head TEXT NOT NULL DEFAULT '', t INTEGER NOT NULL, created_at INTEGER NOT NULL,
  UNIQUE(session_id, seq));

CREATE TABLE entries(id TEXT PRIMARY KEY, json TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE entry_status(
  id TEXT PRIMARY KEY REFERENCES entries(id), status TEXT NOT NULL, branch TEXT NOT NULL,
  name TEXT NOT NULL, predicate_hash TEXT NOT NULL, kind TEXT NOT NULL, family TEXT NOT NULL,
  updated_at INTEGER NOT NULL);
CREATE INDEX entry_status_status ON entry_status(status, branch);
CREATE INDEX entry_status_name ON entry_status(name);
CREATE INDEX entry_status_pred ON entry_status(predicate_hash);

CREATE TABLE counters(
  entry_id TEXT NOT NULL, session_id TEXT NOT NULL, family TEXT NOT NULL,
  matched INTEGER NOT NULL, recurrence INTEGER NOT NULL, harm INTEGER NOT NULL,
  tokens INTEGER NOT NULL, turns INTEGER NOT NULL, ts INTEGER NOT NULL, created_at INTEGER NOT NULL,
  PRIMARY KEY(entry_id, session_id));
CREATE INDEX counters_family ON counters(family, ts);

CREATE TABLE proposals(
  id TEXT PRIMARY KEY, entry_id TEXT, session_id TEXT, blob TEXT NOT NULL,
  lint_ok INTEGER NOT NULL, created_at INTEGER NOT NULL);
CREATE INDEX proposals_entry ON proposals(entry_id);
CREATE INDEX proposals_session ON proposals(session_id);

CREATE TABLE receipts(
  id TEXT PRIMARY KEY, entry_id TEXT NOT NULL, certificate_hash TEXT NOT NULL,
  activation TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE INDEX receipts_entry ON receipts(entry_id);

CREATE TABLE admissions(
  predicate_hash TEXT NOT NULL, day TEXT NOT NULL, entry_id TEXT NOT NULL, created_at INTEGER NOT NULL,
  PRIMARY KEY(predicate_hash, day, entry_id));

CREATE TABLE jobs(
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, status TEXT NOT NULL, json TEXT NOT NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX jobs_status ON jobs(status);

CREATE TABLE outcomes(
  id TEXT PRIMARY KEY, session_id TEXT NOT NULL, ts INTEGER NOT NULL, machine TEXT NOT NULL,
  family TEXT NOT NULL, repo TEXT NOT NULL, branch TEXT NOT NULL, kind TEXT NOT NULL,
  domain TEXT NOT NULL, subject_id TEXT NOT NULL, subject_json TEXT NOT NULL, claim TEXT NOT NULL,
  signature TEXT NOT NULL, params_json TEXT NOT NULL, evidence_json TEXT NOT NULL,
  result_json TEXT NOT NULL, links_json TEXT NOT NULL, provenance_json TEXT NOT NULL,
  status TEXT NOT NULL, json TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE INDEX outcomes_exact ON outcomes(signature);
CREATE INDEX outcomes_temporal ON outcomes(subject_id, ts);
CREATE INDEX outcomes_domain ON outcomes(domain);
CREATE TABLE outcome_facets(outcome_id TEXT NOT NULL, domain TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL);
CREATE INDEX outcome_facets_composite ON outcome_facets(domain, key, value);
CREATE INDEX outcome_facets_outcome ON outcome_facets(outcome_id);
CREATE TABLE minhash(outcome_id TEXT NOT NULL, band INTEGER NOT NULL, hash INTEGER NOT NULL, PRIMARY KEY(outcome_id, band));
CREATE INDEX minhash_band ON minhash(band, hash);
"#, r#"
CREATE TABLE journal(
  seq INTEGER PRIMARY KEY AUTOINCREMENT, known INTEGER NOT NULL, machine TEXT NOT NULL,
  table_name TEXT NOT NULL, row_key TEXT NOT NULL, op TEXT NOT NULL, json TEXT NOT NULL);
CREATE INDEX journal_row ON journal(table_name, row_key, seq);
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#, r#"
-- 3: counters.used (NULL = folded before actual use was recorded); see `Ledger::migrate`.
"#];

pub fn now_ms() -> UnixMs {
    chrono::Utc::now().timestamp_millis()
}

/// Serde snake_case name of a unit enum variant (`Status::Kept` → `"kept"`).
pub fn enum_str<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

pub fn parse_enum<T: DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| anyhow!("bad enum value {s:?}: {e}"))
}

pub fn sha256_hex(bytes: &[u8]) -> Sha256Hex {
    hex(&Sha256::digest(bytes))
}

/// Temporal key of an outcome subject: `domain` or `domain#type:id,type:id` (sorted).
pub fn subject_id(s: &Subject) -> String {
    if s.entities.is_empty() {
        return s.domain.clone();
    }
    let mut ents: Vec<String> = s.entities.iter().map(|e| format!("{}:{}", e.ty, e.id)).collect();
    ents.sort();
    ents.dedup();
    format!("{}#{}", s.domain, ents.join(","))
}

#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    pub branch: Option<String>,
    pub counted: Option<bool>,
    pub flagged: Option<bool>,
    /// Sessions whose `skill_reads` contain this skill name.
    pub skill_read: Option<String>,
    pub since: Option<UnixMs>,
}

#[derive(Debug, Clone, Default)]
pub struct EntryFilter {
    pub status: Option<Status>,
    pub branch: Option<String>,
    pub name: Option<String>,
    pub predicate_hash: Option<String>,
}

/// One per (entry, session): what that matched session contributed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterRow {
    pub entry_id: EntryId,
    pub session_id: String,
    pub family: Family,
    pub matched: u32,
    pub recurrence: u32,
    pub harm: u32,
    pub tokens: u64,
    pub turns: u32,
    /// Session time (ended_at or started_at), used for windows.
    pub ts: UnixMs,
    /// 1 when the session actually used the entry (`counters::actual_use`), 0 when it was
    /// only matched; `None` for rows folded before use was recorded (legacy: unknown).
    #[serde(default)]
    pub used: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalRow {
    pub id: String,
    pub entry_id: Option<EntryId>,
    pub session_id: Option<String>,
    /// sha256 of the proposal JSON in the blob store.
    pub blob: Sha256Hex,
    pub lint_ok: bool,
    pub created_at: UnixMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptRow {
    pub id: String,
    pub receipt: Receipt,
    pub created_at: UnixMs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub data: serde_json::Value,
    pub created_at: UnixMs,
    pub updated_at: UnixMs,
}

/// One journal row (ADR 001): a write the ledger performed, with the full record as
/// written. `known` is the ledger clock at that write (ms UTC); `op` is `insert`,
/// `update` or `snapshot` (the migration-2 backfill of rows that predate the journal).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalRow {
    pub seq: i64,
    pub known: UnixMs,
    pub machine: String,
    /// One of [`JOURNAL_TABLES`].
    pub table: String,
    /// sessions/entries/proposals/receipts/outcomes: id; redirects: [`redirect_key`];
    /// counters: [`counter_key`].
    pub row_key: String,
    pub op: String,
    /// sessions: the hydrated `SessionRecord` (flags and redirects as stored); redirects:
    /// `Redirect`; entries: `Entry`; counters: `CounterRow` plus the entry's
    /// `content_sha256` and `predicate_hash` at fold time; proposals: `ProposalRow`;
    /// receipts: `ReceiptRow`; outcomes: `Outcome`.
    pub json: serde_json::Value,
}

/// Journal row key of a redirect: `<session_id>#<seq>`.
pub fn redirect_key(session_id: &str, seq: u32) -> String {
    format!("{session_id}#{seq}")
}

/// Journal row key of a counter row: `<entry_id>#<session_id>`.
pub fn counter_key(entry_id: &str, session_id: &str) -> String {
    format!("{entry_id}#{session_id}")
}

/// The ledger's machine id, written once into `meta` (`$REMATCH_MACHINE` or the host name
/// at the time) and read back ever after.
fn ensure_machine(conn: &Connection) -> Result<String> {
    let got: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key='machine'", [], |r| r.get(0))
        .optional()?;
    if let Some(m) = got {
        return Ok(m);
    }
    let m = crate::outcome::machine_name();
    conn.execute("INSERT OR IGNORE INTO meta(key, value) VALUES ('machine', ?1)", [&m])?;
    Ok(conn.query_row("SELECT value FROM meta WHERE key='machine'", [], |r| r.get(0))?)
}

pub struct Ledger {
    conn: Connection,
    blobs: PathBuf,
    machine: String,
}

fn json<T: Serialize>(v: &T) -> Result<String> {
    Ok(serde_json::to_string(v)?)
}

fn from_json<T: DeserializeOwned>(s: &str) -> Result<T> {
    Ok(serde_json::from_str(s)?)
}

impl Ledger {
    /// Open (creating if needed) and migrate. Blobs live in `<dir of path>/blobs`.
    pub fn open(path: &Path) -> Result<Ledger> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        let blobs = path.parent().unwrap_or(Path::new(".")).join("blobs");
        let mut l = Ledger { conn, blobs, machine: String::new() };
        l.migrate()?;
        l.machine = ensure_machine(&l.conn)?;
        Ok(l)
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version(version INTEGER NOT NULL, created_at INTEGER NOT NULL);",
        )?;
        let current = self.schema_version()?;
        if current > SCHEMA_VERSION {
            return Err(anyhow!(
                "ledger schema version {current} is newer than this binary ({SCHEMA_VERSION})"
            ));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let v = i as i64 + 1;
            if v <= current {
                continue;
            }
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(sql).with_context(|| format!("migration {v}"))?;
            if v == 3 && !self.has_column("counters", "used")? {
                tx.execute_batch("ALTER TABLE counters ADD COLUMN used INTEGER;").context("migration 3")?;
            }
            if v == 2 {
                self.backfill_journal().context("migration 2: journal backfill")?;
            }
            tx.execute("INSERT INTO schema_version(version, created_at) VALUES (?1, ?2)", params![v, now_ms()])?;
            tx.commit()?;
        }
        Ok(())
    }

    fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        let mut st = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(names.iter().any(|n| n == column))
    }

    /// Migration 2: one `snapshot` journal row per existing row of every journaled table,
    /// `known` = the row's `updated_at`, else its `created_at`; tables in
    /// [`JOURNAL_TABLES`] order, rows by `(known, key)`.
    fn backfill_journal(&self) -> Result<()> {
        let machine = ensure_machine(&self.conn)?;
        let keyed = |sql: &str| -> Result<Vec<(String, i64)>> {
            let mut st = self.conn.prepare(sql)?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        };
        let mut rows: Vec<(&str, String, i64, serde_json::Value)> = Vec::new();
        for (id, known) in keyed("SELECT id, created_at FROM sessions ORDER BY created_at, id")? {
            if let Some(s) = self.get_session(&id)? {
                rows.push(("sessions", id, known, serde_json::to_value(&s)?));
            }
        }
        for (sid, seq, known) in {
            let mut st = self
                .conn
                .prepare("SELECT session_id, seq, created_at FROM redirects ORDER BY created_at, session_id, seq")?;
            let v = st
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?, r.get::<_, i64>(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        } {
            if let Some(r) = self.get_redirect(&sid, seq)? {
                rows.push(("redirects", redirect_key(&sid, seq), known, serde_json::to_value(&r)?));
            }
        }
        for (id, known) in keyed("SELECT id, updated_at FROM entries ORDER BY updated_at, id")? {
            if let Some(e) = self.get_entry(&id)? {
                rows.push(("entries", id, known, serde_json::to_value(&e)?));
            }
        }
        let counters = {
            // `used` exists from migration 3 on (a version-1 ledger backfills before it).
            let used = if self.has_column("counters", "used")? { "used" } else { "NULL AS used" };
            let mut st = self.conn.prepare(&format!(
                "SELECT entry_id,session_id,family,matched,recurrence,harm,tokens,turns,ts,created_at,{used} FROM counters
                 ORDER BY created_at, entry_id, session_id"
            ))?;
            let v = st
                .query_map([], |r| Ok((counter_row(r)?, r.get::<_, i64>(9)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        for (c, known) in counters {
            rows.push(("counters", counter_key(&c.entry_id, &c.session_id), known, self.counter_json(&c)?));
        }
        for p in self.list_proposals(None, None)? {
            rows.push(("proposals", p.id.clone(), p.created_at, serde_json::to_value(&p)?));
        }
        for r in self.list_receipts(None)? {
            rows.push(("receipts", r.id.clone(), r.created_at, serde_json::to_value(&r)?));
        }
        for (id, known) in keyed("SELECT id, created_at FROM outcomes ORDER BY created_at, id")? {
            if let Some(o) = self.get_outcome(&id)? {
                rows.push(("outcomes", id, known, serde_json::to_value(&o)?));
            }
        }
        let mut st = self.conn.prepare(
            "INSERT INTO journal(known,machine,table_name,row_key,op,json) VALUES (?1,?2,?3,?4,'snapshot',?5)",
        )?;
        for (table, key, known, j) in rows {
            st.execute(params![known, machine, table, key, j.to_string()])?;
        }
        Ok(())
    }

    // ---------------- journal ----------------

    /// This ledger's machine id (`$REMATCH_MACHINE` or the host name when the ledger got its
    /// journal; stored once in `meta`).
    pub fn machine_id(&self) -> &str {
        &self.machine
    }

    /// Append one journal row, unless the row's latest journaled json is identical (a
    /// re-fold that changed nothing is not a write). Callers hold the write transaction.
    fn journal(&self, table: &str, row_key: &str, op: &str, j: &serde_json::Value) -> Result<()> {
        let text = j.to_string();
        let last: Option<String> = self
            .conn
            .prepare_cached(
                "SELECT json FROM journal WHERE table_name=?1 AND row_key=?2 ORDER BY seq DESC LIMIT 1",
            )?
            .query_row(params![table, row_key], |r| r.get(0))
            .optional()?;
        if last.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        self.conn
            .prepare_cached(
                "INSERT INTO journal(known,machine,table_name,row_key,op,json) VALUES (?1,?2,?3,?4,?5,?6)",
            )?
            .execute(params![now_ms(), self.machine, table, row_key, op, text])?;
        Ok(())
    }

    fn exists(&self, sql: &str, args: impl rusqlite::Params) -> Result<bool> {
        Ok(self.conn.prepare_cached(sql)?.query_row(args, |_| Ok(())).optional()?.is_some())
    }

    /// Journal rows with `seq > after`, in `seq` order.
    pub fn journal_since(&self, after: i64) -> Result<Vec<JournalRow>> {
        let mut st = self.conn.prepare(
            "SELECT seq,known,machine,table_name,row_key,op,json FROM journal WHERE seq>?1 ORDER BY seq",
        )?;
        let raw = st
            .query_map([after], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raw.into_iter()
            .map(|(seq, known, machine, table, row_key, op, j)| {
                Ok(JournalRow { seq, known, machine, table, row_key, op, json: from_json(&j)? })
            })
            .collect()
    }

    /// Highest journal `seq` (0 when empty).
    pub fn journal_max_seq(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM journal", [], |r| r.get(0))?)
    }

    /// Replay one journal row (`lake rebuild`): the record becomes the current state of its
    /// row and the journal row is copied verbatim (same `seq`, ignored when that `seq` is
    /// present); no new journal row is written. Idempotent: the json is the state as
    /// written, so replaying rows in `seq` order converges on the origin ledger's state.
    pub fn apply_journal_row(&self, row: &JournalRow) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let j = row.json.clone();
        match row.table.as_str() {
            "sessions" => {
                let s: SessionRecord = serde_json::from_value(j)?;
                self.write_session_row(&s, row.known, true)?;
                self.write_redirects(&s, row.known, true)?;
            }
            "redirects" => self.write_redirect(&serde_json::from_value(j)?, row.known, true)?,
            "entries" => self.write_entry(&serde_json::from_value(j)?)?,
            "counters" => self.write_counter(&serde_json::from_value(j)?, row.known)?,
            "proposals" => self.write_proposal(&serde_json::from_value(j)?)?,
            "receipts" => self.write_receipt(&serde_json::from_value(j)?)?,
            "outcomes" => self.write_outcome(&serde_json::from_value(j)?, row.known)?,
            other => return Err(anyhow!("journal row {}: unknown table {other:?}", row.seq)),
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO journal(seq,known,machine,table_name,row_key,op,json) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![row.seq, row.known, row.machine, row.table, row.row_key, row.op, row.json.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(version), 0) FROM schema_version", [], |r| r.get(0))?)
    }

    /// Raw connection for ad-hoc read queries (`why`, doctor).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // ---------------- sessions ----------------

    pub fn upsert_session(&self, s: &SessionRecord) -> Result<()> {
        let now = now_ms();
        let tx = self.conn.unchecked_transaction()?;
        let existed = self.exists("SELECT 1 FROM sessions WHERE id=?1", [&s.id])?;
        let old_seqs = self.redirect_seqs(&s.id)?;
        self.write_session_row(s, now, false)?;
        // A classifier label already in the ledger survives a re-import without one.
        self.write_redirects(s, now, false)?;
        let stored = self.get_session(&s.id)?.ok_or_else(|| anyhow!("session {} vanished", s.id))?;
        let op = if existed { "update" } else { "insert" };
        self.journal("sessions", &s.id, op, &serde_json::to_value(&stored)?)?;
        for r in &stored.redirects {
            let op = if old_seqs.contains(&r.seq) { "update" } else { "insert" };
            self.journal("redirects", &redirect_key(&r.session_id, r.seq), op, &serde_json::to_value(r)?)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The session row. `exact` (journal replay) writes the record as is; otherwise
    /// `counted` never goes back to false and `source_path` / `history_reflected_at`
    /// survive a record that lacks them.
    fn write_session_row(&self, s: &SessionRecord, created_at: UnixMs, exact: bool) -> Result<()> {
        let merge = if exact {
            "counted=excluded.counted, source=excluded.source, json=excluded.json"
        } else {
            "counted=MAX(sessions.counted, excluded.counted), source=excluded.source,
               json=json_set(excluded.json,
                 '$.source_path', COALESCE(json_extract(excluded.json, '$.source_path'), json_extract(sessions.json, '$.source_path')),
                 '$.history_reflected_at', COALESCE(json_extract(excluded.json, '$.history_reflected_at'), json_extract(sessions.json, '$.history_reflected_at')))"
        };
        let sql = format!(
            "INSERT INTO sessions(id,cwd,repo,branch,started_at,ended_at,family,provider,model,turns,tokens_in,tokens_out,
               tool_calls,tool_errors,skill_reads,files_touched,exts,stems,redirects,flagged,counted,source,json,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)
             ON CONFLICT(id) DO UPDATE SET cwd=excluded.cwd, repo=excluded.repo, branch=excluded.branch,
               started_at=excluded.started_at, ended_at=excluded.ended_at, family=excluded.family,
               provider=excluded.provider, model=excluded.model, turns=excluded.turns,
               tokens_in=excluded.tokens_in, tokens_out=excluded.tokens_out, tool_calls=excluded.tool_calls,
               tool_errors=excluded.tool_errors, skill_reads=excluded.skill_reads,
               files_touched=excluded.files_touched, exts=excluded.exts, stems=excluded.stems,
               redirects=excluded.redirects, flagged=excluded.flagged, {merge}"
        );
        self.conn.prepare_cached(&sql)?.execute(params![
            s.id,
            s.cwd,
            s.repo,
            s.branch,
            s.started_at,
            s.ended_at,
            s.family.as_ref().map(|f| f.0.clone()),
            s.model.as_ref().map(|m| m.provider.clone()),
            s.model.as_ref().map(|m| m.id.clone()),
            s.turns,
            s.tokens_in as i64,
            s.tokens_out as i64,
            s.tool_calls,
            s.tool_errors,
            json(&s.skill_reads)?,
            json(&s.files_touched)?,
            json(&s.exts)?,
            json(&s.stems)?,
            json(&s.redirects)?,
            s.flagged,
            s.counted,
            s.source,
            json(s)?,
            created_at,
        ])?;
        Ok(())
    }

    fn redirect_seqs(&self, session_id: &str) -> Result<Vec<u32>> {
        let mut st = self.conn.prepare_cached("SELECT seq FROM redirects WHERE session_id=?1")?;
        let v = st.query_map([session_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// The session's redirect rows become `s.redirects`: seqs not in it are deleted.
    fn write_redirects(&self, s: &SessionRecord, created_at: UnixMs, exact: bool) -> Result<()> {
        for seq in self.redirect_seqs(&s.id)? {
            if !s.redirects.iter().any(|r| r.seq == seq) {
                self.conn.execute("DELETE FROM redirects WHERE session_id=?1 AND seq=?2", params![s.id, seq])?;
            }
        }
        for r in &s.redirects {
            self.write_redirect(r, created_at, exact)?;
        }
        Ok(())
    }

    /// One redirect row. `exact` (journal replay) writes the label as is; otherwise a stored
    /// label survives a redirect that carries none.
    fn write_redirect(&self, r: &crate::types::Redirect, created_at: UnixMs, exact: bool) -> Result<()> {
        let labels = if exact {
            "class=excluded.class, confidence=excluded.confidence"
        } else {
            "class=COALESCE(excluded.class, redirects.class), confidence=COALESCE(excluded.confidence, redirects.confidence)"
        };
        let sql = format!(
            "INSERT INTO redirects(session_id,seq,prelabel,class,confidence,text_head,deliverable_head,t,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(session_id,seq) DO UPDATE SET prelabel=excluded.prelabel, {labels},
               text_head=excluded.text_head, deliverable_head=excluded.deliverable_head, t=excluded.t"
        );
        self.conn.prepare_cached(&sql)?.execute(params![
            r.session_id,
            r.seq,
            r.prelabel,
            r.class.map(|c| enum_str(&c)),
            r.confidence,
            r.text_head,
            r.deliverable_head,
            r.t,
            created_at
        ])?;
        Ok(())
    }

    /// One stored redirect, labels included.
    pub fn get_redirect(&self, session_id: &str, seq: u32) -> Result<Option<crate::types::Redirect>> {
        let raw = self
            .conn
            .prepare_cached(
                "SELECT seq,prelabel,class,confidence,text_head,deliverable_head,t FROM redirects
                 WHERE session_id=?1 AND seq=?2",
            )?
            .query_row(params![session_id, seq], redirect_raw)
            .optional()?;
        raw.map(|r| redirect_from_raw(session_id, r)).transpose()
    }

    fn session_from_row(&self, row: &Row<'_>) -> rusqlite::Result<(String, bool, bool)> {
        Ok((row.get("json")?, row.get("flagged")?, row.get("counted")?))
    }

    fn hydrate_session(&self, (j, flagged, counted): (String, bool, bool)) -> Result<SessionRecord> {
        let mut s: SessionRecord = from_json(&j)?;
        s.flagged = flagged;
        s.counted = counted;
        let mut st = self.conn.prepare_cached(
            "SELECT seq,prelabel,class,confidence,text_head,deliverable_head,t FROM redirects
             WHERE session_id=?1 ORDER BY seq",
        )?;
        let raws = st.query_map([&s.id], redirect_raw)?.collect::<rusqlite::Result<Vec<_>>>()?;
        s.redirects = raws.into_iter().map(|r| redirect_from_raw(&s.id, r)).collect::<Result<_>>()?;
        Ok(s)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRecord>> {
        let raw = self
            .conn
            .query_row("SELECT json, flagged, counted FROM sessions WHERE id=?1", [id], |r| {
                self.session_from_row(r)
            })
            .optional()?;
        raw.map(|r| self.hydrate_session(r)).transpose()
    }

    /// Sessions in `started_at` order.
    pub fn list_sessions(&self, f: &SessionFilter) -> Result<Vec<SessionRecord>> {
        let mut sql = String::from("SELECT json, flagged, counted FROM sessions WHERE 1=1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(b) = &f.branch {
            args.push(b.clone().into());
            sql.push_str(&format!(" AND branch=?{}", args.len()));
        }
        if let Some(c) = f.counted {
            args.push((c as i64).into());
            sql.push_str(&format!(" AND counted=?{}", args.len()));
        }
        if let Some(fl) = f.flagged {
            args.push((fl as i64).into());
            sql.push_str(&format!(" AND flagged=?{}", args.len()));
        }
        if let Some(sk) = &f.skill_read {
            args.push(sk.clone().into());
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM json_each(sessions.skill_reads) WHERE value=?{})",
                args.len()
            ));
        }
        if let Some(t) = f.since {
            args.push(t.into());
            sql.push_str(&format!(" AND started_at>=?{}", args.len()));
        }
        sql.push_str(" ORDER BY started_at, id");
        let mut st = self.conn.prepare(&sql)?;
        let raws: Vec<(String, bool, bool)> = st
            .query_map(rusqlite::params_from_iter(args), |r| self.session_from_row(r))?
            .collect::<rusqlite::Result<_>>()?;
        raws.into_iter().map(|r| self.hydrate_session(r)).collect()
    }

    pub fn set_session_counted(&self, id: &str, counted: bool) -> Result<()> {
        self.update_session(id, "UPDATE sessions SET counted=?2 WHERE id=?1", params![id, counted])
    }

    /// Marks an imported session as dealt with by `reflect --history` (kept across upserts).
    pub fn set_history_reflected(&self, id: &str, at: UnixMs) -> Result<()> {
        self.update_session(
            id,
            "UPDATE sessions SET json=json_set(json, '$.history_reflected_at', ?2) WHERE id=?1",
            params![id, at],
        )
    }

    /// One in-place update of a session row, journaled with the hydrated result.
    fn update_session(&self, id: &str, sql: &str, args: impl rusqlite::Params) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        if self.conn.execute(sql, args)? > 0 {
            if let Some(s) = self.get_session(id)? {
                self.journal("sessions", id, "update", &serde_json::to_value(&s)?)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Classifier output for one redirect.
    pub fn set_redirect_class(
        &self,
        session_id: &str,
        seq: u32,
        class: RedirectClass,
        confidence: Option<f64>,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let n = self.conn.execute(
            "UPDATE redirects SET class=?3, confidence=?4 WHERE session_id=?1 AND seq=?2",
            params![session_id, seq, enum_str(&class), confidence],
        )?;
        if let Some(r) = self.get_redirect(session_id, seq)? {
            self.journal("redirects", &redirect_key(session_id, seq), "update", &serde_json::to_value(&r)?)?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    // ---------------- entries ----------------

    pub fn upsert_entry(&self, e: &Entry) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let existed = self.exists("SELECT 1 FROM entries WHERE id=?1", [&e.id])?;
        self.write_entry(e)?;
        self.journal("entries", &e.id, if existed { "update" } else { "insert" }, &serde_json::to_value(e)?)?;
        tx.commit()?;
        Ok(())
    }

    fn write_entry(&self, e: &Entry) -> Result<()> {
        self.conn.execute(
            "INSERT INTO entries(id,json,created_at,updated_at) VALUES (?1,?2,?3,?4)
             ON CONFLICT(id) DO UPDATE SET json=excluded.json, updated_at=excluded.updated_at",
            params![e.id, json(e)?, e.created_at, e.updated_at],
        )?;
        self.conn.execute(
            "INSERT INTO entry_status(id,status,branch,name,predicate_hash,kind,family,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(id) DO UPDATE SET status=excluded.status, branch=excluded.branch, name=excluded.name,
               predicate_hash=excluded.predicate_hash, kind=excluded.kind, family=excluded.family,
               updated_at=excluded.updated_at",
            params![
                e.id,
                enum_str(&e.status),
                e.branch,
                e.name,
                predicate_hash(&e.predicate),
                enum_str(&e.kind),
                e.family.0,
                e.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn get_entry(&self, id: &str) -> Result<Option<Entry>> {
        let j: Option<String> = self
            .conn
            .query_row("SELECT json FROM entries WHERE id=?1", [id], |r| r.get(0))
            .optional()?;
        j.map(|j| from_json(&j)).transpose()
    }

    /// Entries ordered by `updated_at` (oldest first).
    pub fn list_entries(&self, f: &EntryFilter) -> Result<Vec<Entry>> {
        let mut sql = String::from(
            "SELECT e.json FROM entries e JOIN entry_status s ON s.id=e.id WHERE 1=1",
        );
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        let mut push = |col: &str, v: String, sql: &mut String| {
            args.push(v.into());
            sql.push_str(&format!(" AND s.{col}=?{}", args.len()));
        };
        if let Some(st) = f.status {
            push("status", enum_str(&st), &mut sql);
        }
        if let Some(b) = &f.branch {
            push("branch", b.clone(), &mut sql);
        }
        if let Some(n) = &f.name {
            push("name", n.clone(), &mut sql);
        }
        if let Some(h) = &f.predicate_hash {
            push("predicate_hash", h.clone(), &mut sql);
        }
        sql.push_str(" ORDER BY e.updated_at, e.id");
        let mut st = self.conn.prepare(&sql)?;
        let rows: Vec<String> = st
            .query_map(rusqlite::params_from_iter(args), |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        rows.iter().map(|j| from_json(j)).collect()
    }

    /// Resolve a user-supplied reference: exact id, else the most recently updated
    /// entry with that name.
    pub fn find_entry(&self, id_or_name: &str) -> Result<Option<Entry>> {
        if let Some(e) = self.get_entry(id_or_name)? {
            return Ok(Some(e));
        }
        Ok(self
            .list_entries(&EntryFilter { name: Some(id_or_name.to_string()), ..Default::default() })?
            .pop())
    }

    // ---------------- counters ----------------

    /// Idempotent per (entry, session).
    pub fn add_counter(&self, row: &CounterRow) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let existed = self.exists(
            "SELECT 1 FROM counters WHERE entry_id=?1 AND session_id=?2",
            params![row.entry_id, row.session_id],
        )?;
        self.write_counter(row, now_ms())?;
        let op = if existed { "update" } else { "insert" };
        self.journal("counters", &counter_key(&row.entry_id, &row.session_id), op, &self.counter_json(row)?)?;
        tx.commit()?;
        Ok(())
    }

    /// Journal form of a counter row: the row plus the entry's `content_sha256` and
    /// `predicate_hash` at fold time (cross-machine aggregation is by content).
    fn counter_json(&self, row: &CounterRow) -> Result<serde_json::Value> {
        let mut j = serde_json::to_value(row)?;
        let e = self.get_entry(&row.entry_id)?;
        j["content_sha256"] = serde_json::json!(e.as_ref().and_then(|e| e.content_sha256.clone()));
        j["predicate_hash"] = serde_json::json!(e.as_ref().map(|e| predicate_hash(&e.predicate)));
        Ok(j)
    }

    fn write_counter(&self, row: &CounterRow, created_at: UnixMs) -> Result<()> {
        self.conn.execute(
            "INSERT INTO counters(entry_id,session_id,family,matched,recurrence,harm,tokens,turns,ts,created_at,used)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(entry_id,session_id) DO UPDATE SET family=excluded.family, matched=excluded.matched,
               recurrence=excluded.recurrence, harm=excluded.harm, tokens=excluded.tokens,
               turns=excluded.turns, ts=excluded.ts, used=excluded.used",
            params![
                row.entry_id,
                row.session_id,
                row.family.0,
                row.matched,
                row.recurrence,
                row.harm,
                row.tokens as i64,
                row.turns,
                row.ts,
                created_at,
                row.used
            ],
        )?;
        Ok(())
    }

    pub fn counter_rows(&self, entry_id: &str) -> Result<Vec<CounterRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT entry_id,session_id,family,matched,recurrence,harm,tokens,turns,ts,used FROM counters
             WHERE entry_id=?1 ORDER BY ts, session_id",
        )?;
        let rows = st
            .query_map([entry_id], counter_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Aggregate counters for an entry, with the per-family breakdown and harm times.
    pub fn counters(&self, entry_id: &str) -> Result<Counters> {
        Ok(aggregate(&self.counter_rows(entry_id)?))
    }

    /// Every entry that has counter rows, with its aggregate.
    pub fn list_counters(&self) -> Result<Vec<(EntryId, Counters)>> {
        let mut st = self.conn.prepare(
            "SELECT entry_id,session_id,family,matched,recurrence,harm,tokens,turns,ts,used FROM counters
             ORDER BY entry_id, ts, session_id",
        )?;
        let rows = st.query_map([], counter_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out: Vec<(EntryId, Counters)> = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            let id = rows[i].entry_id.clone();
            let j = rows[i..].iter().position(|r| r.entry_id != id).map_or(rows.len(), |p| i + p);
            out.push((id, aggregate(&rows[i..j])));
            i = j;
        }
        Ok(out)
    }

    /// Per-entry counters restricted to one family since `since` (capability floor).
    pub fn counters_for_family(
        &self,
        family: &Family,
        since: UnixMs,
    ) -> Result<Vec<(EntryId, FamilyCounters)>> {
        let mut st = self.conn.prepare(
            "SELECT entry_id, SUM(matched), SUM(recurrence), SUM(harm) FROM counters
             WHERE family=?1 AND ts>=?2 GROUP BY entry_id ORDER BY entry_id",
        )?;
        let rows = st
            .query_map(params![family.0, since], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    FamilyCounters { matched: r.get(1)?, recurrence: r.get(2)?, harm: r.get(3)? },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---------------- proposals / receipts ----------------

    /// Stores the proposal JSON as a blob and records the row; returns the row.
    pub fn insert_proposal(
        &self,
        entry_id: Option<&str>,
        session_id: Option<&str>,
        proposal: &serde_json::Value,
        lint_ok: bool,
    ) -> Result<ProposalRow> {
        let blob = self.put_blob(serde_json::to_string(proposal)?.as_bytes())?;
        let row = ProposalRow {
            id: uuid::Uuid::now_v7().to_string(),
            entry_id: entry_id.map(str::to_string),
            session_id: session_id.map(str::to_string),
            blob,
            lint_ok,
            created_at: now_ms(),
        };
        let tx = self.conn.unchecked_transaction()?;
        self.write_proposal(&row)?;
        self.journal("proposals", &row.id, "insert", &serde_json::to_value(&row)?)?;
        tx.commit()?;
        Ok(row)
    }

    fn write_proposal(&self, row: &ProposalRow) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO proposals(id,entry_id,session_id,blob,lint_ok,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
            params![row.id, row.entry_id, row.session_id, row.blob, row.lint_ok, row.created_at],
        )?;
        Ok(())
    }

    pub fn list_proposals(
        &self,
        entry_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<ProposalRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,entry_id,session_id,blob,lint_ok,created_at FROM proposals
             WHERE (?1 IS NULL OR entry_id=?1) AND (?2 IS NULL OR session_id=?2) ORDER BY created_at, id",
        )?;
        let rows = st
            .query_map(params![entry_id, session_id], |r| {
                Ok(ProposalRow {
                    id: r.get(0)?,
                    entry_id: r.get(1)?,
                    session_id: r.get(2)?,
                    blob: r.get(3)?,
                    lint_ok: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn insert_receipt(&self, r: &Receipt) -> Result<ReceiptRow> {
        let row = ReceiptRow {
            id: uuid::Uuid::now_v7().to_string(),
            receipt: r.clone(),
            created_at: now_ms(),
        };
        let tx = self.conn.unchecked_transaction()?;
        self.write_receipt(&row)?;
        self.journal("receipts", &row.id, "insert", &serde_json::to_value(&row)?)?;
        tx.commit()?;
        Ok(row)
    }

    fn write_receipt(&self, row: &ReceiptRow) -> Result<()> {
        let r = &row.receipt;
        self.conn.execute(
            "INSERT OR REPLACE INTO receipts(id,entry_id,certificate_hash,activation,created_at) VALUES (?1,?2,?3,?4,?5)",
            params![row.id, r.entry, r.certificate, enum_str(&r.activation), row.created_at],
        )?;
        Ok(())
    }

    pub fn list_receipts(&self, entry_id: Option<&str>) -> Result<Vec<ReceiptRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,entry_id,certificate_hash,activation,created_at FROM receipts
             WHERE (?1 IS NULL OR entry_id=?1) ORDER BY created_at, id",
        )?;
        let raw = st
            .query_map(params![entry_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raw.into_iter()
            .map(|(id, entry, certificate, act, created_at)| {
                Ok(ReceiptRow {
                    id,
                    receipt: Receipt { certificate, entry, activation: parse_enum::<Activation>(&act)? },
                    created_at,
                })
            })
            .collect()
    }

    // ---------------- admissions ----------------

    /// Number of admissions for a predicate on a UTC day (`YYYY-MM-DD`).
    pub fn admissions_on(&self, predicate_hash: &str, day: &str) -> Result<u32> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM admissions WHERE predicate_hash=?1 AND day=?2",
            params![predicate_hash, day],
            |r| r.get(0),
        )?)
    }

    pub fn record_admission(&self, predicate_hash: &str, day: &str, entry_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO admissions(predicate_hash,day,entry_id,created_at) VALUES (?1,?2,?3,?4)",
            params![predicate_hash, day, entry_id, now_ms()],
        )?;
        Ok(())
    }

    // ---------------- jobs ----------------

    pub fn put_job(&self, id: &str, kind: &str, status: &str, data: &serde_json::Value) -> Result<()> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO jobs(id,kind,status,json,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?5)
             ON CONFLICT(id) DO UPDATE SET kind=excluded.kind, status=excluded.status, json=excluded.json,
               updated_at=excluded.updated_at",
            params![id, kind, status, data.to_string(), now],
        )?;
        Ok(())
    }

    pub fn get_job(&self, id: &str) -> Result<Option<JobRecord>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT id,kind,status,json,created_at,updated_at FROM jobs WHERE id=?1")?;
        let raw = st.query_row([id], job_raw).optional()?;
        raw.map(job_from_raw).transpose()
    }

    pub fn list_jobs(&self, status: Option<&str>) -> Result<Vec<JobRecord>> {
        let mut st = self.conn.prepare(
            "SELECT id,kind,status,json,created_at,updated_at FROM jobs
             WHERE (?1 IS NULL OR status=?1) ORDER BY created_at, id",
        )?;
        let raws = st.query_map(params![status], job_raw)?.collect::<rusqlite::Result<Vec<_>>>()?;
        raws.into_iter().map(job_from_raw).collect()
    }

    // ---------------- outcomes ----------------

    /// Insert or replace by id (the ledger keeps the latest status per id).
    pub fn insert_outcome(&self, o: &Outcome) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let existed = self.exists("SELECT 1 FROM outcomes WHERE id=?1", [&o.id])?;
        self.write_outcome(o, now_ms())?;
        self.journal("outcomes", &o.id, if existed { "update" } else { "insert" }, &serde_json::to_value(o)?)?;
        tx.commit()?;
        Ok(())
    }

    fn write_outcome(&self, o: &Outcome, created_at: UnixMs) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO outcomes(id,session_id,ts,machine,family,repo,branch,kind,domain,subject_id,
               subject_json,claim,signature,params_json,evidence_json,result_json,links_json,provenance_json,
               status,json,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
            params![
                o.id,
                o.session_id,
                o.ts,
                o.machine,
                o.model_family.0,
                o.repo,
                o.branch,
                enum_str(&o.kind),
                o.subject.domain,
                subject_id(&o.subject),
                json(&o.subject)?,
                o.claim,
                o.signature,
                json(&o.params)?,
                json(&o.evidence)?,
                json(&o.result)?,
                json(&o.links)?,
                json(&o.provenance)?,
                enum_str(&o.result.status),
                json(o)?,
                created_at
            ],
        )?;
        self.conn.execute("DELETE FROM outcome_facets WHERE outcome_id=?1", [&o.id])?;
        for f in &o.facets {
            self.conn.execute(
                "INSERT INTO outcome_facets(outcome_id,domain,key,value) VALUES (?1,?2,?3,?4)",
                params![o.id, o.subject.domain, f.key, f.value],
            )?;
        }
        Ok(())
    }

    fn outcomes_where(&self, clause: &str, args: impl rusqlite::Params) -> Result<Vec<Outcome>> {
        let sql = format!("SELECT json FROM outcomes WHERE {clause}");
        let mut st = self.conn.prepare(&sql)?;
        let rows: Vec<String> =
            st.query_map(args, |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        rows.iter().map(|j| from_json(j)).collect()
    }

    pub fn get_outcome(&self, id: &str) -> Result<Option<Outcome>> {
        Ok(self.outcomes_where("id=?1", [id])?.pop())
    }

    /// `latest`: the newest outcome on a subject.
    pub fn latest_outcome(&self, subject_id: &str) -> Result<Option<Outcome>> {
        Ok(self
            .outcomes_where("subject_id=?1 ORDER BY ts DESC, id DESC LIMIT 1", [subject_id])?
            .pop())
    }

    /// `asof`: what was known about a subject at `ts` (newest with ts ≤ given).
    pub fn outcome_asof(&self, subject_id: &str, ts: UnixMs) -> Result<Option<Outcome>> {
        Ok(self
            .outcomes_where(
                "subject_id=?1 AND ts<=?2 ORDER BY ts DESC, id DESC LIMIT 1",
                params![subject_id, ts],
            )?
            .pop())
    }

    /// `exact`: same-thing gate.
    pub fn outcomes_by_signature(&self, signature: &str) -> Result<Vec<Outcome>> {
        self.outcomes_where("signature=?1 ORDER BY ts, id", [signature])
    }

    /// `composite`: prefix seek on `(domain, key, value)`. `domain_prefix` matches the
    /// domain itself and its dotted descendants (`quant` ⇒ `quant`, `quant.options`).
    pub fn outcomes_by_facet(
        &self,
        domain_prefix: &str,
        key: Option<&str>,
        value: Option<&str>,
    ) -> Result<Vec<Outcome>> {
        let dotted = format!("{domain_prefix}.");
        self.outcomes_where(
            "id IN (SELECT outcome_id FROM outcome_facets
                     WHERE (domain=?1 OR substr(domain,1,length(?2))=?2)
                       AND (?3 IS NULL OR key=?3) AND (?4 IS NULL OR value=?4))
             ORDER BY ts, id",
            params![domain_prefix, dotted, key, value],
        )
    }

    /// Replace the MinHash band hashes of an outcome (bands computed by `outcome`).
    pub fn put_minhash(&self, outcome_id: &str, bands: &[u64]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM minhash WHERE outcome_id=?1", [outcome_id])?;
        for (i, h) in bands.iter().enumerate() {
            tx.execute(
                "INSERT INTO minhash(outcome_id,band,hash) VALUES (?1,?2,?3)",
                params![outcome_id, i as i64, *h as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Close-thing suggestion: outcomes sharing ≥ `min_shared` bands, most shared first.
    pub fn minhash_neighbors(&self, bands: &[u64], min_shared: usize) -> Result<Vec<(String, usize)>> {
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        let mut st = self.conn.prepare_cached("SELECT outcome_id FROM minhash WHERE band=?1 AND hash=?2")?;
        for (i, h) in bands.iter().enumerate() {
            let ids = st
                .query_map(params![i as i64, *h as i64], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for id in ids {
                *counts.entry(id).or_default() += 1;
            }
        }
        let mut out: Vec<(String, usize)> =
            counts.into_iter().filter(|(_, n)| *n >= min_shared.max(1)).collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(out)
    }

    // ---------------- blobs ----------------

    pub fn blobs_dir(&self) -> &Path {
        &self.blobs
    }

    pub fn put_blob(&self, bytes: &[u8]) -> Result<Sha256Hex> {
        let h = sha256_hex(bytes);
        let p = self.blobs.join(&h);
        if !p.exists() {
            std::fs::create_dir_all(&self.blobs)?;
            let tmp = self.blobs.join(format!(".{h}.{}", uuid::Uuid::now_v7()));
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &p)?;
        }
        Ok(h)
    }

    pub fn get_blob(&self, sha: &str) -> Result<Option<Vec<u8>>> {
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(anyhow!("not a sha256: {sha}"));
        }
        match std::fs::read(self.blobs.join(sha)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

type RedirectRaw = (u32, String, Option<String>, Option<f64>, String, String, i64);

fn redirect_raw(r: &Row<'_>) -> rusqlite::Result<RedirectRaw> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
}

fn redirect_from_raw(
    session_id: &str,
    (seq, prelabel, class, confidence, text_head, deliverable_head, t): RedirectRaw,
) -> Result<crate::types::Redirect> {
    Ok(crate::types::Redirect {
        session_id: session_id.to_string(),
        seq,
        prelabel,
        class: class.map(|c| parse_enum(&c)).transpose()?,
        confidence,
        text_head,
        deliverable_head,
        t,
    })
}

fn counter_row(r: &Row<'_>) -> rusqlite::Result<CounterRow> {
    Ok(CounterRow {
        entry_id: r.get(0)?,
        session_id: r.get(1)?,
        family: Family(r.get(2)?),
        matched: r.get(3)?,
        recurrence: r.get(4)?,
        harm: r.get(5)?,
        tokens: r.get::<_, i64>(6)? as u64,
        turns: r.get(7)?,
        ts: r.get(8)?,
        // Absent from pre-migration-3 selects (journal backfill of a version-1 ledger).
        used: r.get::<_, Option<u32>>("used").ok().flatten(),
    })
}

/// Fold counter rows into aggregate counters (raw sums; per-family breakdown).
pub fn aggregate(rows: &[CounterRow]) -> Counters {
    let mut c = Counters::default();
    for r in rows {
        c.matched += r.matched;
        c.recurrence += r.recurrence;
        c.harm += r.harm;
        c.tokens += r.tokens;
        c.turns += r.turns;
        let f = c.per_family.entry(r.family.clone()).or_default();
        f.matched += r.matched;
        f.recurrence += r.recurrence;
        f.harm += r.harm;
        for _ in 0..r.harm {
            c.harm_times.push(r.ts);
        }
        if r.matched > 0 {
            match r.used {
                Some(u) if u > 0 => {
                    c.used += 1;
                    if r.recurrence == 0 && r.harm == 0 {
                        c.used_clean += 1;
                    }
                }
                Some(_) => {}
                None => c.use_unknown += 1,
            }
        }
    }
    c.harm_times.sort_unstable();
    c
}

type JobRaw = (String, String, String, String, i64, i64);

fn job_raw(r: &Row<'_>) -> rusqlite::Result<JobRaw> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
}

fn job_from_raw((id, kind, status, j, created_at, updated_at): JobRaw) -> Result<JobRecord> {
    Ok(JobRecord { id, kind, status, data: from_json(&j)?, created_at, updated_at })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::types::*;
    use std::collections::BTreeMap;

    pub(crate) fn ledger() -> (tempfile::TempDir, Ledger) {
        let t = tempfile::tempdir().unwrap();
        let l = Ledger::open(&t.path().join("ledger.sqlite")).unwrap();
        (t, l)
    }

    pub(crate) fn session(id: &str, branch: &str, t: i64) -> SessionRecord {
        SessionRecord {
            id: id.into(),
            cwd: "/w/repo".into(),
            repo: Some("/w/repo".into()),
            branch: branch.into(),
            started_at: t,
            ended_at: Some(t + 10),
            family: Some(Family("anthropic/claude-opus".into())),
            model: Some(ModelRef { provider: "anthropic".into(), id: "claude-opus-5-5".into() }),
            source: "trace".into(),
            ..Default::default()
        }
    }

    fn redirect(sid: &str, seq: u32, prelabel: &str) -> Redirect {
        Redirect {
            session_id: sid.into(),
            seq,
            prelabel: prelabel.into(),
            class: None,
            confidence: None,
            text_head: "no".into(),
            deliverable_head: String::new(),
            t: 1,
        }
    }

    #[test]
    fn opens_migrates_and_reopens() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("sub/ledger.sqlite");
        let l = Ledger::open(&p).unwrap();
        assert_eq!(l.schema_version().unwrap(), SCHEMA_VERSION);
        let mode: String = l.conn().query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        drop(l);
        let l = Ledger::open(&p).unwrap();
        let n: i64 = l.conn().query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0)).unwrap();
        assert_eq!(n, SCHEMA_VERSION, "migrations must not re-run");
        l.conn()
            .execute("INSERT INTO schema_version VALUES (99, 0)", [])
            .unwrap();
        drop(l);
        assert!(Ledger::open(&p).is_err(), "newer schema refused");
    }

    #[test]
    fn sessions_roundtrip_filters_and_classifier_survives_reimport() {
        let (_t, l) = ledger();
        let mut a = session("a", "repo", 100);
        a.skill_reads = vec!["forge-ux".into()];
        a.flagged = true;
        a.redirects = vec![redirect("a", 3, "negation"), redirect("a", 7, "revert")];
        l.upsert_session(&a).unwrap();
        let mut b = session("b", "other", 200);
        b.counted = true;
        l.upsert_session(&b).unwrap();

        let got = l.get_session("a").unwrap().unwrap();
        assert_eq!(got, a);

        let f = |f: SessionFilter| -> Vec<String> {
            l.list_sessions(&f).unwrap().into_iter().map(|s| s.id).collect()
        };
        assert_eq!(f(SessionFilter::default()), vec!["a", "b"]);
        assert_eq!(f(SessionFilter { branch: Some("other".into()), ..Default::default() }), vec!["b"]);
        assert_eq!(f(SessionFilter { counted: Some(false), ..Default::default() }), vec!["a"]);
        assert_eq!(f(SessionFilter { flagged: Some(true), ..Default::default() }), vec!["a"]);
        assert_eq!(
            f(SessionFilter { skill_read: Some("forge-ux".into()), ..Default::default() }),
            vec!["a"]
        );
        assert_eq!(f(SessionFilter { since: Some(150), ..Default::default() }), vec!["b"]);

        assert!(l.set_redirect_class("a", 3, RedirectClass::Correction, Some(0.9)).unwrap());
        assert!(!l.set_redirect_class("a", 4, RedirectClass::Correction, None).unwrap());
        // re-import without labels and with redirect 7 gone
        let mut a2 = a.clone();
        a2.redirects = vec![redirect("a", 3, "negation")];
        l.upsert_session(&a2).unwrap();
        let got = l.get_session("a").unwrap().unwrap();
        assert_eq!(got.redirects.len(), 1);
        assert_eq!(got.redirects[0].class, Some(RedirectClass::Correction));
        assert_eq!(got.redirects[0].confidence, Some(0.9));

        l.set_session_counted("a", true).unwrap();
        // a re-upsert with counted=false must not un-count
        l.upsert_session(&a2).unwrap();
        assert!(l.get_session("a").unwrap().unwrap().counted);

        // The import's source path and the `reflect --history` marker survive a re-fold that
        // carries neither.
        let mut imported = session("i", "repo", 300);
        imported.source = "import".into();
        imported.source_path = Some("/s/2026_i.jsonl".into());
        l.upsert_session(&imported).unwrap();
        l.set_history_reflected("i", 42).unwrap();
        assert_eq!(l.get_session("i").unwrap().unwrap().history_reflected_at, Some(42));
        let mut refold = imported.clone();
        refold.source_path = None;
        l.upsert_session(&refold).unwrap();
        let got = l.get_session("i").unwrap().unwrap();
        assert_eq!((got.source_path.as_deref(), got.history_reflected_at), (Some("/s/2026_i.jsonl"), Some(42)));
    }

    pub(crate) fn entry(id: &str, name: &str, branch: &str, status: Status) -> Entry {
        Entry {
            id: id.into(),
            kind: Kind::Skill,
            name: name.into(),
            branch: branch.into(),
            scope: Scope::Personal,
            status,
            family: Family("anthropic/claude-opus".into()),
            verified_on: vec![],
            helpful: 0,
            harmful: 0,
            evidence: vec![Moment { session_id: "s".into(), seq: 1 }],
            origin: Origin::Session { session_id: "s".into() },
            predicate: Predicate { repo_glob: "/w/repo".into(), file_exts: vec!["rs".into()], ..Default::default() },
            trigger_line: "Use when editing the ledger.".into(),
            redirect_class: Some(RedirectClass::Correction),
            predecessor: None,
            version: 1,
            path: PathBuf::from("/x"),
            created_at: 1,
            updated_at: 1,
            manifest: None,
            lesson_tag: None,
            pinned: false,
            hook: None,
            approved_scripts: vec![],
            approved_fragment: None,
            approved_agent: None,
            sibling_of: None,
            status_since: None,
            status_note: None,
            pending_next_run: false,
            fork_path: None,
            fork_repo: None,
            fork_commit: None,
            families_excluded: vec![],
            links: vec![],
            content_sha256: None,
            last_tune_at: None,
            tune: None,
            retention: None,
        }
    }

    #[test]
    fn entries_status_index_and_filters() {
        let (_t, l) = ledger();
        let mut e1 = entry("e1", "alpha", "repo", Status::Provisional);
        l.upsert_entry(&e1).unwrap();
        let mut e2 = entry("e2", "beta", "repo", Status::Kept);
        e2.predicate.stems = vec!["x".into()];
        e2.updated_at = 2;
        l.upsert_entry(&e2).unwrap();
        l.upsert_entry(&entry("e3", "alpha", "other", Status::Draft)).unwrap();

        assert_eq!(l.get_entry("e2").unwrap().unwrap(), e2);
        let ids = |f: EntryFilter| -> Vec<String> {
            l.list_entries(&f).unwrap().into_iter().map(|e| e.id).collect()
        };
        assert_eq!(ids(EntryFilter { status: Some(Status::Provisional), ..Default::default() }), vec!["e1"]);
        assert_eq!(ids(EntryFilter { branch: Some("repo".into()), ..Default::default() }), vec!["e1", "e2"]);
        assert_eq!(ids(EntryFilter { name: Some("alpha".into()), ..Default::default() }), vec!["e1", "e3"]);
        let h = predicate_hash(&e1.predicate);
        assert_eq!(ids(EntryFilter { predicate_hash: Some(h), ..Default::default() }), vec!["e1", "e3"]);

        e1.status = Status::Retired;
        e1.updated_at = 5;
        l.upsert_entry(&e1).unwrap();
        assert!(ids(EntryFilter { status: Some(Status::Provisional), ..Default::default() }).is_empty());
        assert_eq!(l.find_entry("alpha").unwrap().unwrap().id, "e1", "most recently updated by name");
        assert_eq!(l.find_entry("e3").unwrap().unwrap().id, "e3");
        assert!(l.find_entry("nope").unwrap().is_none());
    }

    fn crow(entry: &str, session: &str, fam: &str, m: u32, r: u32, h: u32, ts: i64) -> CounterRow {
        CounterRow {
            entry_id: entry.into(),
            session_id: session.into(),
            family: Family(fam.into()),
            matched: m,
            recurrence: r,
            harm: h,
            tokens: 100,
            turns: 2,
            ts,
            used: Some(0),
        }
    }

    #[test]
    fn counters_idempotent_per_session_with_family_breakdown() {
        let (_t, l) = ledger();
        l.add_counter(&crow("e", "s1", "a/x", 1, 1, 0, 10)).unwrap();
        l.add_counter(&crow("e", "s1", "a/x", 1, 1, 0, 10)).unwrap();
        l.add_counter(&crow("e", "s2", "b/y", 1, 0, 1, 20)).unwrap();
        l.add_counter(&crow("f", "s2", "b/y", 1, 0, 0, 20)).unwrap();
        let c = l.counters("e").unwrap();
        assert_eq!((c.matched, c.recurrence, c.harm, c.tokens, c.turns), (2, 1, 1, 200, 4));
        assert_eq!(c.per_family[&Family("a/x".into())], FamilyCounters { matched: 1, recurrence: 1, harm: 0 });
        assert_eq!(c.per_family[&Family("b/y".into())].harm, 1);
        assert_eq!(c.harm_times, vec![20]);
        let all = l.list_counters().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].0, "f");
        let fam = l.counters_for_family(&Family("b/y".into()), 15).unwrap();
        assert_eq!(fam.len(), 2);
        assert!(l.counters_for_family(&Family("b/y".into()), 25).unwrap().is_empty());
        assert_eq!(l.counters("none").unwrap(), Counters::default());
        // Actual use: used rows count, clean used rows count, legacy rows stay unknown.
        l.add_counter(&CounterRow { used: Some(1), ..crow("u", "s1", "a/x", 1, 0, 0, 10) }).unwrap();
        l.add_counter(&CounterRow { used: Some(1), ..crow("u", "s2", "a/x", 1, 0, 1, 11) }).unwrap();
        l.add_counter(&CounterRow { used: None, ..crow("u", "s3", "a/x", 1, 0, 0, 12) }).unwrap();
        let c = l.counters("u").unwrap();
        assert_eq!((c.matched, c.used, c.used_clean, c.use_unknown), (3, 2, 1, 1));
    }

    #[test]
    fn proposals_receipts_admissions_jobs() {
        let (_t, l) = ledger();
        let p = l
            .insert_proposal(Some("e"), Some("s"), &serde_json::json!({"edits": []}), true)
            .unwrap();
        assert_eq!(
            l.get_blob(&p.blob).unwrap().unwrap(),
            br#"{"edits":[]}"#.to_vec()
        );
        l.insert_proposal(None, Some("s2"), &serde_json::json!({}), false).unwrap();
        assert_eq!(l.list_proposals(Some("e"), None).unwrap(), vec![p.clone()]);
        assert_eq!(l.list_proposals(None, None).unwrap().len(), 2);

        let r = Receipt { certificate: "c".repeat(64), entry: "e".into(), activation: Activation::Prompted };
        l.insert_receipt(&r).unwrap();
        let rs = l.list_receipts(Some("e")).unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].receipt, r);
        assert!(l.list_receipts(Some("x")).unwrap().is_empty());

        assert_eq!(l.admissions_on("h", "2026-10-25").unwrap(), 0);
        l.record_admission("h", "2026-10-25", "e").unwrap();
        l.record_admission("h", "2026-10-25", "e").unwrap();
        assert_eq!(l.admissions_on("h", "2026-10-25").unwrap(), 1);
        assert_eq!(l.admissions_on("h", "2026-10-26").unwrap(), 0);

        l.put_job("j1", "run", "running", &serde_json::json!({"pid": 1})).unwrap();
        l.put_job("j1", "run", "done", &serde_json::json!({"pid": 1, "ok": true})).unwrap();
        l.put_job("j2", "reflect", "running", &serde_json::json!({})).unwrap();
        let j = l.get_job("j1").unwrap().unwrap();
        assert_eq!(j.status, "done");
        assert_eq!(j.data["ok"], true);
        assert_eq!(l.list_jobs(Some("running")).unwrap().len(), 1);
        assert_eq!(l.list_jobs(None).unwrap().len(), 2);
        assert!(l.get_job("nope").unwrap().is_none());
    }

    #[test]
    fn blobs_are_content_addressed() {
        let (_t, l) = ledger();
        let h = l.put_blob(b"abc").unwrap();
        assert_eq!(h, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(l.put_blob(b"abc").unwrap(), h);
        assert_eq!(l.get_blob(&h).unwrap().unwrap(), b"abc");
        assert!(l.get_blob(&"0".repeat(64)).unwrap().is_none());
        assert!(l.get_blob("../x").is_err());
    }

    /// A ledger exercising every journaled write path.
    fn busy_ledger(l: &Ledger) {
        let mut a = session("a", "repo", 100);
        a.redirects = vec![redirect("a", 3, "negation"), redirect("a", 7, "revert")];
        a.repo_remote = Some("github.com/org/repo".into());
        a.loaded_versions.insert("e1".into(), 2);
        l.upsert_session(&a).unwrap();
        l.upsert_session(&session("b", "other", 200)).unwrap();
        l.set_redirect_class("a", 3, RedirectClass::Correction, Some(0.9)).unwrap();
        a.redirects = vec![redirect("a", 3, "negation")];
        l.upsert_session(&a).unwrap(); // drops redirect 7, keeps the label of 3
        l.set_session_counted("a", true).unwrap();
        l.set_history_reflected("b", 42).unwrap();
        let mut e1 = entry("e1", "alpha", "repo", Status::Provisional);
        e1.content_sha256 = Some("c".repeat(64));
        l.upsert_entry(&e1).unwrap();
        e1.status = Status::Kept;
        e1.updated_at = 5;
        l.upsert_entry(&e1).unwrap();
        l.upsert_entry(&entry("e2", "beta", "other", Status::Draft)).unwrap();
        l.add_counter(&crow("e1", "a", "a/x", 1, 0, 0, 110)).unwrap();
        l.add_counter(&crow("e1", "b", "b/y", 1, 1, 0, 210)).unwrap();
        l.add_counter(&crow("e1", "b", "b/y", 1, 1, 1, 210)).unwrap(); // refold
        l.insert_proposal(Some("e1"), Some("a"), &serde_json::json!({"edits": []}), true).unwrap();
        l.insert_receipt(&Receipt { certificate: "d".repeat(64), entry: "e1".into(), activation: Activation::Personal }).unwrap();
        let o = outcome("o1", "quant", "k", 100, "sig", ResultStatus::Inconclusive, &[("m", "carry")]);
        l.insert_outcome(&o).unwrap();
        let mut o2 = o.clone();
        o2.result.status = ResultStatus::Kept;
        l.insert_outcome(&o2).unwrap();
    }

    type State = (Vec<Entry>, Vec<SessionRecord>, Vec<(EntryId, Counters)>, Vec<CounterRow>, usize, usize, Vec<Outcome>);

    fn state(l: &Ledger) -> State {
        (
            l.list_entries(&EntryFilter::default()).unwrap(),
            l.list_sessions(&SessionFilter::default()).unwrap(),
            l.list_counters().unwrap(),
            l.counter_rows("e1").unwrap(),
            l.list_proposals(None, None).unwrap().len(),
            l.list_receipts(None).unwrap().len(),
            l.outcomes_by_signature("sig").unwrap(),
        )
    }

    #[test]
    fn every_write_journals_and_a_replay_reproduces_the_ledger() {
        let (_t, l) = ledger();
        assert!(!l.machine_id().is_empty());
        busy_ledger(&l);
        let rows = l.journal_since(0).unwrap();
        assert_eq!(l.journal_max_seq().unwrap(), rows.last().unwrap().seq);
        assert!(rows.windows(2).all(|w| w[0].seq < w[1].seq));
        assert!(rows.iter().all(|r| r.machine == l.machine_id() && r.op != "snapshot"));
        let tables: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.table.as_str()).collect();
        assert_eq!(tables, JOURNAL_TABLES.iter().copied().collect());
        let ops = |table: &str, key: &str| -> Vec<String> {
            rows.iter().filter(|r| r.table == table && r.row_key == key).map(|r| r.op.clone()).collect()
        };
        // a: insert, label (a redirect write), re-upsert, counted
        assert_eq!(ops("sessions", "a"), ["insert", "update", "update"]);
        assert_eq!(ops("sessions", "b"), ["insert", "update"], "history marker");
        assert_eq!(ops("redirects", &redirect_key("a", 3)), ["insert", "update"], "the label; the re-upsert changed nothing");
        assert_eq!(ops("entries", "e1"), ["insert", "update"]);
        assert_eq!(ops("counters", &counter_key("e1", "b")), ["insert", "update"]);
        assert_eq!(ops("outcomes", "o1"), ["insert", "update"]);
        let counted = rows.iter().rfind(|r| r.table == "sessions" && r.row_key == "a").unwrap();
        assert_eq!(counted.json["counted"], true);
        assert_eq!(counted.json["repo_remote"], "github.com/org/repo");
        let c = rows.iter().find(|r| r.table == "counters").unwrap();
        assert_eq!(c.json["content_sha256"], "c".repeat(64));
        assert_eq!(c.json["predicate_hash"], predicate_hash(&entry("e1", "alpha", "repo", Status::Kept).predicate));
        // An identical rewrite is not a write.
        let before = l.journal_max_seq().unwrap();
        l.add_counter(&crow("e1", "a", "a/x", 1, 0, 0, 110)).unwrap();
        l.upsert_entry(&l.get_entry("e2").unwrap().unwrap()).unwrap();
        assert_eq!(l.journal_max_seq().unwrap(), before);
        assert!(!l.set_redirect_class("a", 99, RedirectClass::Scope, None).unwrap());
        assert_eq!(l.journal_max_seq().unwrap(), before);

        // Replay into a fresh ledger: identical state, the journal copied verbatim, nothing new
        // journaled; replaying twice changes nothing.
        let (_t2, fresh) = ledger();
        for r in &rows {
            fresh.apply_journal_row(r).unwrap();
        }
        assert_eq!(state(&fresh), state(&l));
        assert_eq!(fresh.journal_since(0).unwrap(), rows);
        for r in &rows {
            fresh.apply_journal_row(r).unwrap();
        }
        assert_eq!(state(&fresh), state(&l));
        assert_eq!(fresh.journal_since(0).unwrap(), rows);
        assert_eq!(fresh.get_session("a").unwrap().unwrap().redirects[0].class, Some(RedirectClass::Correction));
        assert!(fresh.apply_journal_row(&JournalRow { table: "nope".into(), ..rows[0].clone() }).is_err());
    }

    #[test]
    fn migration_two_backfills_snapshots_of_existing_rows() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("ledger.sqlite");
        let l = Ledger::open(&p).unwrap();
        busy_ledger(&l);
        let want = state(&l);
        // Back to a version-1 ledger: no journal, no meta.
        l.conn()
            .execute_batch("DROP TABLE journal; DROP TABLE meta; DELETE FROM schema_version WHERE version>=2;")
            .unwrap();
        drop(l);
        let l = Ledger::open(&p).unwrap();
        assert_eq!(l.schema_version().unwrap(), SCHEMA_VERSION);
        let rows = l.journal_since(0).unwrap();
        assert!(rows.iter().all(|r| r.op == "snapshot"));
        let count = |t: &str| rows.iter().filter(|r| r.table == t).count();
        assert_eq!(
            (count("sessions"), count("redirects"), count("entries"), count("counters"), count("proposals"), count("receipts"), count("outcomes")),
            (2, 1, 2, 2, 1, 1, 1)
        );
        // Table order, then (known, key) within a table.
        let order: Vec<usize> = rows.iter().map(|r| JOURNAL_TABLES.iter().position(|t| *t == r.table).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] <= w[1]), "{order:?}");
        let e1 = rows.iter().find(|r| r.table == "entries" && r.row_key == "e1").unwrap();
        assert_eq!(e1.known, 5, "entries: known = updated_at");
        let (_t2, fresh) = ledger();
        for r in &rows {
            fresh.apply_journal_row(r).unwrap();
        }
        assert_eq!(state(&fresh), want);
        // The machine id is stored once.
        let m = l.machine_id().to_string();
        drop(l);
        assert_eq!(Ledger::open(&p).unwrap().machine_id(), m);
    }

    fn outcome(id: &str, domain: &str, ent: &str, ts: i64, sig: &str, status: ResultStatus, facets: &[(&str, &str)]) -> Outcome {
        Outcome {
            id: id.into(),
            session_id: "s".into(),
            ts,
            machine: "m".into(),
            model_family: Family("a/x".into()),
            repo: "/w/repo".into(),
            branch: "repo".into(),
            kind: OutcomeKind::Hypothesis,
            subject: Subject {
                domain: domain.into(),
                entities: vec![EntityRef { ty: "strategy".into(), id: ent.into() }],
            },
            claim: "c".into(),
            signature: sig.into(),
            params: BTreeMap::new(),
            facets: facets.iter().map(|(k, v)| Facet { key: k.to_string(), value: v.to_string() }).collect(),
            evidence: vec![],
            result: OutcomeResult { status, metrics: BTreeMap::new(), kill_criterion: None },
            links: vec![],
            provenance: Provenance::default(),
        }
    }

    #[test]
    fn outcomes_latest_asof_signature_facets_minhash() {
        let (_t, l) = ledger();
        let o1 = outcome("o1", "quant.options", "kelly", 100, "sig1", ResultStatus::Inconclusive, &[("mechanism", "carry")]);
        let o2 = outcome("o2", "quant.options", "kelly", 200, "sig1", ResultStatus::Killed, &[("mechanism", "carry"), ("horizon", "1d")]);
        let o3 = outcome("o3", "quantx", "other", 150, "sig3", ResultStatus::Kept, &[("mechanism", "carry")]);
        for o in [&o1, &o2, &o3] {
            l.insert_outcome(o).unwrap();
        }
        let sid = subject_id(&o1.subject);
        assert_eq!(sid, "quant.options#strategy:kelly");
        assert_eq!(l.latest_outcome(&sid).unwrap().unwrap().id, "o2");
        assert_eq!(l.outcome_asof(&sid, 150).unwrap().unwrap().id, "o1");
        assert!(l.outcome_asof(&sid, 50).unwrap().is_none());
        let ids = |v: Vec<Outcome>| v.into_iter().map(|o| o.id).collect::<Vec<_>>();
        assert_eq!(ids(l.outcomes_by_signature("sig1").unwrap()), vec!["o1", "o2"]);
        assert_eq!(ids(l.outcomes_by_facet("quant", Some("mechanism"), Some("carry")).unwrap()), vec!["o1", "o2"]);
        assert_eq!(ids(l.outcomes_by_facet("quant.options", Some("horizon"), None).unwrap()), vec!["o2"]);
        assert_eq!(ids(l.outcomes_by_facet("quantx", None, None).unwrap()), vec!["o3"]);
        // replace by id keeps latest status and facets
        let mut o1b = o1.clone();
        o1b.result.status = ResultStatus::Kept;
        o1b.facets.clear();
        l.insert_outcome(&o1b).unwrap();
        assert_eq!(l.get_outcome("o1").unwrap().unwrap(), o1b);
        assert_eq!(ids(l.outcomes_by_facet("quant", Some("mechanism"), None).unwrap()), vec!["o2"]);

        l.put_minhash("o1", &[1, 2, 3, u64::MAX]).unwrap();
        l.put_minhash("o2", &[1, 9, 3, 4]).unwrap();
        let n = l.minhash_neighbors(&[1, 2, 3, u64::MAX], 2).unwrap();
        assert_eq!(n, vec![("o1".to_string(), 4), ("o2".to_string(), 2)]);
        assert_eq!(l.minhash_neighbors(&[1, 2, 3, u64::MAX], 3).unwrap().len(), 1);
    }
}
