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

pub const SCHEMA_VERSION: i64 = 1;

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

pub struct Ledger {
    conn: Connection,
    blobs: PathBuf,
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
        let mut l = Ledger { conn, blobs };
        l.migrate()?;
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
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql).with_context(|| format!("migration {v}"))?;
            tx.execute("INSERT INTO schema_version(version, created_at) VALUES (?1, ?2)", params![v, now_ms()])?;
            tx.commit()?;
        }
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
        tx.execute(
            "INSERT INTO sessions(id,cwd,repo,branch,started_at,ended_at,family,provider,model,turns,tokens_in,tokens_out,
               tool_calls,tool_errors,skill_reads,files_touched,exts,stems,redirects,flagged,counted,source,json,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)
             ON CONFLICT(id) DO UPDATE SET cwd=excluded.cwd, repo=excluded.repo, branch=excluded.branch,
               started_at=excluded.started_at, ended_at=excluded.ended_at, family=excluded.family,
               provider=excluded.provider, model=excluded.model, turns=excluded.turns,
               tokens_in=excluded.tokens_in, tokens_out=excluded.tokens_out, tool_calls=excluded.tool_calls,
               tool_errors=excluded.tool_errors, skill_reads=excluded.skill_reads,
               files_touched=excluded.files_touched, exts=excluded.exts, stems=excluded.stems,
               redirects=excluded.redirects, flagged=excluded.flagged,
               counted=MAX(sessions.counted, excluded.counted), source=excluded.source,
               json=json_set(excluded.json,
                 '$.source_path', COALESCE(json_extract(excluded.json, '$.source_path'), json_extract(sessions.json, '$.source_path')),
                 '$.history_reflected_at', COALESCE(json_extract(excluded.json, '$.history_reflected_at'), json_extract(sessions.json, '$.history_reflected_at')))",
            params![
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
                now,
            ],
        )?;
        let seqs: Vec<u32> = s.redirects.iter().map(|r| r.seq).collect();
        {
            let mut existing = tx.prepare("SELECT seq FROM redirects WHERE session_id=?1")?;
            let old: Vec<u32> = existing
                .query_map([&s.id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for seq in old.into_iter().filter(|q| !seqs.contains(q)) {
                tx.execute("DELETE FROM redirects WHERE session_id=?1 AND seq=?2", params![s.id, seq])?;
            }
        }
        for r in &s.redirects {
            // A classifier label already in the ledger survives a re-import without one.
            tx.execute(
                "INSERT INTO redirects(session_id,seq,prelabel,class,confidence,text_head,deliverable_head,t,created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(session_id,seq) DO UPDATE SET prelabel=excluded.prelabel,
                   class=COALESCE(excluded.class, redirects.class),
                   confidence=COALESCE(excluded.confidence, redirects.confidence),
                   text_head=excluded.text_head, deliverable_head=excluded.deliverable_head, t=excluded.t",
                params![
                    s.id,
                    r.seq,
                    r.prelabel,
                    r.class.map(|c| enum_str(&c)),
                    r.confidence,
                    r.text_head,
                    r.deliverable_head,
                    r.t,
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
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
        let rows = st.query_map([&s.id], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<f64>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;
        let mut redirects = Vec::new();
        for row in rows {
            let (seq, prelabel, class, confidence, text_head, deliverable_head, t) = row?;
            redirects.push(crate::types::Redirect {
                session_id: s.id.clone(),
                seq,
                prelabel,
                class: class.map(|c| parse_enum(&c)).transpose()?,
                confidence,
                text_head,
                deliverable_head,
                t,
            });
        }
        s.redirects = redirects;
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
        self.conn
            .execute("UPDATE sessions SET counted=?2 WHERE id=?1", params![id, counted])?;
        Ok(())
    }

    /// Marks an imported session as dealt with by `reflect --history` (kept across upserts).
    pub fn set_history_reflected(&self, id: &str, at: UnixMs) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET json=json_set(json, '$.history_reflected_at', ?2) WHERE id=?1",
            params![id, at],
        )?;
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
        let n = self.conn.execute(
            "UPDATE redirects SET class=?3, confidence=?4 WHERE session_id=?1 AND seq=?2",
            params![session_id, seq, enum_str(&class), confidence],
        )?;
        Ok(n > 0)
    }

    // ---------------- entries ----------------

    pub fn upsert_entry(&self, e: &Entry) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO entries(id,json,created_at,updated_at) VALUES (?1,?2,?3,?4)
             ON CONFLICT(id) DO UPDATE SET json=excluded.json, updated_at=excluded.updated_at",
            params![e.id, json(e)?, e.created_at, e.updated_at],
        )?;
        tx.execute(
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
        tx.commit()?;
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
        self.conn.execute(
            "INSERT INTO counters(entry_id,session_id,family,matched,recurrence,harm,tokens,turns,ts,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(entry_id,session_id) DO UPDATE SET family=excluded.family, matched=excluded.matched,
               recurrence=excluded.recurrence, harm=excluded.harm, tokens=excluded.tokens,
               turns=excluded.turns, ts=excluded.ts",
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
                now_ms()
            ],
        )?;
        Ok(())
    }

    pub fn counter_rows(&self, entry_id: &str) -> Result<Vec<CounterRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT entry_id,session_id,family,matched,recurrence,harm,tokens,turns,ts FROM counters
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
            "SELECT entry_id,session_id,family,matched,recurrence,harm,tokens,turns,ts FROM counters
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
        self.conn.execute(
            "INSERT INTO proposals(id,entry_id,session_id,blob,lint_ok,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
            params![row.id, row.entry_id, row.session_id, row.blob, row.lint_ok, row.created_at],
        )?;
        Ok(row)
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
        self.conn.execute(
            "INSERT INTO receipts(id,entry_id,certificate_hash,activation,created_at) VALUES (?1,?2,?3,?4,?5)",
            params![row.id, r.entry, r.certificate, enum_str(&r.activation), row.created_at],
        )?;
        Ok(row)
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
        tx.execute(
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
                now_ms()
            ],
        )?;
        tx.execute("DELETE FROM outcome_facets WHERE outcome_id=?1", [&o.id])?;
        for f in &o.facets {
            tx.execute(
                "INSERT INTO outcome_facets(outcome_id,domain,key,value) VALUES (?1,?2,?3,?4)",
                params![o.id, o.subject.domain, f.key, f.value],
            )?;
        }
        tx.commit()?;
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
        assert_eq!(n, 1, "migration must not re-run");
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
            predicate: Predicate { repo_glob: "/w/repo".into(), file_exts: vec!["rs".into()], stems: vec![] },
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
            sibling_of: None,
            status_since: None,
            status_note: None,
            pending_next_run: false,
            fork_path: None,
            fork_repo: None,
            fork_commit: None,
            families_excluded: vec![],
            links: vec![],
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
