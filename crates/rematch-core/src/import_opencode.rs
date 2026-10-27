//! OpenCode history importer and the record mapping its live adapter shares (DESIGN §18).
//!
//! OpenCode 1.x keeps every session of a machine in one SQLite store, `opencode.db` under its
//! data dir (`$XDG_DATA_HOME/opencode`, default `~/.local/share/opencode`; `$OPENCODE_DB`
//! relocates it). Format facts relied on (OpenCode 1.19.4, `packages/core/src/session/sql.ts`,
//! and stores written by the pinned binary):
//!
//! * `session(id, parent_id, directory, version, title, time_created, time_updated, …)`:
//!   `parent_id` is set for subagent (task tool) sessions; `version` is the OpenCode version
//!   that created the session.
//! * `message(id, session_id, time_created, data)`: `data` is the v1 message info without
//!   `id`/`sessionID` — `role: "user"` (`agent`, `model {providerID, modelID}`) or
//!   `role: "assistant"` (`modelID`, `providerID`, `tokens {input, output, reasoning,
//!   cache {read, write}, total?}`, `cost`, `finish`, `error {name, data}`, `time {created,
//!   completed?}`, `summary` for a compaction summary). Ids sort by creation.
//! * `part(id, message_id, session_id, data)`: `type: "text"` (`text`, `synthetic`,
//!   `ignored`), `"tool"` (`tool`, `callID`, `state {status: pending|running|completed|error,
//!   input, output | error, metadata, time {start, end}}`), `"step-start"`, `"step-finish"`,
//!   `"compaction"`, `"subtask"`, `"file"`, `"reasoning"`, … (unknown types are ignored).
//!
//! Ledger ids: `oc:<session id>` for a main session, `oc:<parent>:agent-<child>` for a
//! subagent session, and `oc:<session id>:r<n>` for the n-th live segment of a session the
//! adapter resumed after its earlier segment was finalized (live traces only; the importer
//! reads a whole session as `oc:<id>`).
//!
//! The live adapter (`rematch trace-event --harness opencode`) maps OpenCode's plugin events
//! through [`tool_result_record`], [`user_prompt`] and [`turn_record`], so a live trace and
//! a synthesized one describe the same session the same way.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

use crate::features::{head, strip_selector};
use crate::import::{args_hash, repo_of, truncate_tool_input, TRACE_CONTENT_HEAD_CHARS, TRACE_DELIVERABLE_CHARS, TRACE_TOOL_INPUT_BYTES};
use crate::import_claude::input_record;
use crate::trace::{basename, flag_reasons, fold_trace, skill_from_read_path, AgentInfo, TraceRecord, Usage};
use crate::types::{ModelRef, SessionRecord, UnixMs};

/// Session id prefix of OpenCode sessions in the ledger and `traces/`.
pub const ID_PREFIX: &str = "oc:";
/// The adapter's command templates start with this tag, so their expansion (which reaches
/// OpenCode as an ordinary user message) is never taken for a typed prompt.
pub const COMMAND_TAG: &str = "<rematch-command";

/// `oc:<id>`.
pub fn session_id(native: &str) -> String {
    format!("{ID_PREFIX}{native}")
}

/// `oc:<parent>:agent-<child>`.
pub fn subagent_id(parent: &str, child: &str) -> String {
    format!("{ID_PREFIX}{parent}:agent-{child}")
}

/// Segment `n` of a live session: `oc:<id>` for the first, `oc:<id>:r<n>` after.
pub fn segment_id(native: &str, n: u32) -> String {
    if n <= 1 {
        session_id(native)
    } else {
        format!("{ID_PREFIX}{native}:r{n}")
    }
}

/// The OpenCode session id behind a ledger id: the child of `oc:<p>:agent-<c>`, the session
/// of a segment `oc:<id>:r<n>`. `None` without the prefix.
pub fn native_id(ledger_id: &str) -> Option<&str> {
    let rest = ledger_id.strip_prefix(ID_PREFIX)?;
    let rest = rest.split_once(":agent-").map_or(rest, |(_, child)| child);
    let rest = match rest.rsplit_once(":r") {
        Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => rest,
    };
    (!rest.is_empty()).then_some(rest)
}

/// OpenCode's data dir: `$XDG_DATA_HOME/opencode`, else `<home>/.local/share/opencode`
/// (`xdg-basedir`, which OpenCode uses on every platform; home is `os.homedir()`: `$HOME`,
/// else `%USERPROFILE%` on Windows).
pub fn data_dir() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
    xdg.or_else(|| crate::paths::home_dir().map(|h| h.join(".local").join("share"))).map(|d| d.join("opencode"))
}

/// The store OpenCode itself opens: `$OPENCODE_DB` when absolute, else
/// `<data dir>/$OPENCODE_DB`, else `<data dir>/opencode.db` (the store of the npm/`latest`
/// release channel; dev channels use `opencode-<channel>.db`, passed with `--sessions`).
pub fn default_store() -> Option<PathBuf> {
    let custom = std::env::var("OPENCODE_DB").ok().filter(|v| !v.is_empty() && v != ":memory:");
    match custom {
        Some(p) if Path::new(&p).is_absolute() => Some(PathBuf::from(p)),
        Some(p) => data_dir().map(|d| d.join(p)),
        None => data_dir().map(|d| d.join("opencode.db")),
    }
}

/// `--sessions <path>`: the store file itself, or a data dir holding `opencode.db`.
pub fn store_of(p: &Path) -> PathBuf {
    if p.is_dir() {
        p.join("opencode.db")
    } else {
        p.to_path_buf()
    }
}

/// One session of a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub db: PathBuf,
    pub id: String,
}

impl fmt::Display for SessionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.db.display(), self.id)
    }
}

/// One message with its parts, in store order.
#[derive(Debug, Clone, PartialEq)]
pub struct OcMessage {
    pub id: String,
    pub created: UnixMs,
    /// The message info (`data`, with `id` added).
    pub info: Value,
    /// The parts (`data`, with `id` added), in id order.
    pub parts: Vec<Value>,
}

impl OcMessage {
    pub fn role(&self) -> &str {
        self.info.get("role").and_then(Value::as_str).unwrap_or_default()
    }
}

/// A session read from a store.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenCodeSession {
    /// Ledger id: `oc:<id>` or `oc:<parent>:agent-<id>`.
    pub id: String,
    /// OpenCode's session id.
    pub native: String,
    pub parent: Option<String>,
    pub directory: String,
    /// OpenCode version that created the session.
    pub version: String,
    pub title: String,
    pub created: UnixMs,
    pub updated: UnixMs,
    /// The store it was read from (the ledger row's `source_path`).
    pub db: PathBuf,
    pub messages: Vec<OcMessage>,
}

impl OpenCodeSession {
    pub fn is_subagent(&self) -> bool {
        self.parent.is_some()
    }
}

/// Opens a store read-only (OpenCode may be writing it: WAL, 5 s busy timeout) after checking
/// it has the 1.x `session`/`message`/`part` tables.
pub fn open_store(db: &Path) -> Result<Connection> {
    if !db.is_file() {
        bail!("OpenCode store {} does not exist", db.display());
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("open OpenCode store {}", db.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let columns = |table: &str| -> Result<Vec<String>> {
        let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(names)
    };
    for (table, need) in [
        ("session", &["id", "parent_id", "directory", "version", "title", "time_created", "time_updated"][..]),
        ("message", &["id", "session_id", "time_created", "data"][..]),
        ("part", &["id", "message_id", "session_id", "data"][..]),
    ] {
        let have = columns(table)?;
        if let Some(missing) = need.iter().find(|c| !have.iter().any(|h| h == *c)) {
            bail!("{} is not an OpenCode 1.x store: table {table} has no column {missing}", db.display());
        }
    }
    Ok(conn)
}

/// Every session of a store, oldest first (a parent precedes its subagents).
pub fn scan_store(db: &Path) -> Result<Vec<SessionRef>> {
    let conn = open_store(db)?;
    let mut st = conn.prepare("SELECT id FROM session ORDER BY time_created, id")?;
    let ids = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids.into_iter().map(|id| SessionRef { db: db.to_path_buf(), id }).collect())
}

fn with_id(data: &str, id: &str) -> Value {
    let mut v: Value = serde_json::from_str(data).unwrap_or(Value::Null);
    if let Value::Object(o) = &mut v {
        o.insert("id".into(), Value::String(id.to_string()));
    }
    v
}

pub fn read_session(r: &SessionRef) -> Result<OpenCodeSession> {
    let conn = open_store(&r.db)?;
    read_session_from(&conn, &r.db, &r.id)
}

/// [`read_session`] on an open store.
pub fn read_session_from(conn: &Connection, db: &Path, id: &str) -> Result<OpenCodeSession> {
    let row = conn
        .query_row(
            "SELECT id, parent_id, directory, version, title, time_created, time_updated FROM session WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("session {id} not in {}", db.display()))?;
    let (native, parent, directory, version, title, created, updated) = row;
    let parent = parent.filter(|p| !p.is_empty());
    let mut parts: HashMap<String, Vec<Value>> = HashMap::new();
    {
        let mut st = conn.prepare("SELECT id, message_id, data FROM part WHERE session_id = ?1 ORDER BY message_id, id")?;
        let rows = st.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        for row in rows {
            let (pid, mid, data) = row?;
            parts.entry(mid).or_default().push(with_id(&data, &pid));
        }
    }
    let mut messages = Vec::new();
    let mut st = conn.prepare("SELECT id, time_created, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")?;
    let rows = st.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?;
    for row in rows {
        let (mid, t, data) = row?;
        let parts = parts.remove(&mid).unwrap_or_default();
        messages.push(OcMessage { info: with_id(&data, &mid), id: mid, created: t, parts });
    }
    Ok(OpenCodeSession {
        id: match &parent {
            Some(p) => subagent_id(p, &native),
            None => session_id(&native),
        },
        native,
        parent,
        directory,
        version,
        title,
        created,
        updated,
        db: db.to_path_buf(),
        messages,
    })
}

/// The session of a ledger id: in the recorded store (`source_path`) when it is a file,
/// else in [`default_store`]. `None` when neither holds it.
pub fn locate(source_path: Option<&Path>, ledger_id: &str) -> Option<SessionRef> {
    let native = native_id(ledger_id)?;
    let db = source_path.filter(|p| p.is_file()).map(Path::to_path_buf).or_else(|| default_store().filter(|p| p.is_file()))?;
    let conn = open_store(&db).ok()?;
    let found: Option<String> = conn.query_row("SELECT id FROM session WHERE id = ?1", [native], |r| r.get(0)).optional().ok()?;
    found.map(|id| SessionRef { db, id })
}

// ---- shared with the live adapter ----

/// Tool name in the trace (OpenCode's ids are lowercase already: `bash`, `read`, `edit`,
/// `write`, `skill`, `task`, `apply_patch`, MCP tools `<server>_<tool>`).
pub fn tool_name(raw: &str) -> String {
    raw.to_ascii_lowercase()
}

fn file_path_of(input: &Value) -> Option<&str> {
    ["filePath", "file_path", "path"].iter().find_map(|k| input.get(*k).and_then(Value::as_str))
}

/// A tool part's result failed: `state.status == "error"` (a thrown tool, a refused
/// permission, a plugin's `tool.execute.before` rejection), or a `bash` call whose
/// `metadata.exit` is a non-zero exit code (OpenCode completes those normally).
pub fn tool_failed(part: &Value) -> bool {
    let state = &part["state"];
    match state.get("status").and_then(Value::as_str) {
        Some("error") => true,
        Some("completed") => {
            part.get("tool").and_then(Value::as_str) == Some("bash") && state.pointer("/metadata/exit").and_then(Value::as_i64).is_some_and(|c| c != 0)
        }
        _ => false,
    }
}

/// The `tool_result` of a tool part in a final state (`completed` or `error`); `None` for
/// `pending`/`running` parts and non-tool parts. `t` = `state.time.end`. A skill counts as
/// read only when the call succeeded: the `skill` tool's `name`, or a `read` of
/// `…/skills/<name>/SKILL.md`.
pub fn tool_result_record(seq: u32, part: &Value) -> Option<TraceRecord> {
    if part.get("type").and_then(Value::as_str) != Some("tool") {
        return None;
    }
    let state = part.get("state")?;
    let status = state.get("status").and_then(Value::as_str)?;
    if status != "completed" && status != "error" {
        return None;
    }
    let tool = tool_name(part.get("tool").and_then(Value::as_str).unwrap_or_default());
    let input = state.get("input").cloned().unwrap_or(Value::Null);
    let content = if status == "error" { state.get("error") } else { state.get("output") };
    let text = match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    };
    let is_error = tool_failed(part);
    let skill_read = if is_error {
        None
    } else {
        match tool.as_str() {
            "skill" => input.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            "read" => file_path_of(&input).and_then(skill_from_read_path),
            _ => None,
        }
    };
    let t = state.pointer("/time/end").and_then(Value::as_i64).or_else(|| state.pointer("/time/start").and_then(Value::as_i64)).unwrap_or(0);
    Some(TraceRecord::ToolResult {
        seq,
        t,
        call_id: part.get("callID").and_then(Value::as_str).unwrap_or_default().to_string(),
        input: truncate_tool_input(&input, TRACE_TOOL_INPUT_BYTES),
        args_hash: args_hash(&input),
        result_bytes: text.len() as u64,
        is_error,
        error_class: is_error.then(|| "error".to_string()),
        content_head: head(&text, TRACE_CONTENT_HEAD_CHARS),
        skill_read,
        path: file_path_of(&input).map(|p| strip_selector(p).to_string()),
        tool,
    })
}

/// What a user message is to the trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserText {
    /// Words the human sent (the non-synthetic, non-ignored text parts).
    Prompt(String),
    /// The expansion of one of the adapter's commands (`<rematch-command name=… args=…>`),
    /// rendered `/rematch-<name> <args>`: only `/rematch-mark` becomes an input.
    Command(String),
    /// Nothing typed: only synthetic parts (file reads, agent mentions, compaction
    /// continuations), files, or a subtask.
    Ignored,
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}=\"");
    let i = tag.find(&key)? + key.len();
    let j = tag[i..].find('"')? + i;
    Some(&tag[i..j])
}

/// Classifies a user message by its parts (OpenCode marks injected text `synthetic`).
pub fn user_prompt(parts: &[Value]) -> UserText {
    let texts: Vec<&str> = parts
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
        .filter(|p| p.get("synthetic").and_then(Value::as_bool) != Some(true) && p.get("ignored").and_then(Value::as_bool) != Some(true))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect();
    let text = texts.join("\n");
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return UserText::Ignored;
    }
    if trimmed.starts_with(COMMAND_TAG) {
        let tag = trimmed.lines().next().unwrap_or_default();
        let name = attr(tag, "name").unwrap_or_default();
        let args = attr(tag, "args").unwrap_or_default().trim();
        return UserText::Command(if args.is_empty() { format!("/rematch-{name}") } else { format!("/rematch-{name} {args}") });
    }
    UserText::Prompt(text)
}

/// `{provider, id}` of an assistant message (`providerID`, `modelID`) or of a user
/// message's `model {providerID, modelID}`; `None` without a model id. The provider is
/// `""` when OpenCode recorded none.
pub fn model_of(info: &Value) -> Option<ModelRef> {
    let (provider, id) = match info.get("model") {
        Some(m) if m.is_object() => (m.get("providerID"), m.get("modelID")),
        _ => (info.get("providerID"), info.get("modelID")),
    };
    let id = id.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())?;
    Some(ModelRef { provider: provider.and_then(Value::as_str).unwrap_or_default().to_string(), id: id.to_string() })
}

/// An assistant message's `tokens` and `cost` as the trace's [`Usage`]: `output` excludes
/// reasoning tokens (OpenCode counts them apart); `total_tokens` is OpenCode's `total`, else
/// the sum of every count. `None` without `tokens`.
pub fn usage_of(info: &Value) -> Option<Usage> {
    let t = info.get("tokens").filter(|t| t.is_object())?;
    let n = |p: &str| t.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    let (input, output, reasoning, cache_read, cache_write) = (n("/input"), n("/output"), n("/reasoning"), n("/cache/read"), n("/cache/write"));
    Some(Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: t.get("total").and_then(Value::as_u64).unwrap_or(input + output + reasoning + cache_read + cache_write),
        cost: info.get("cost").filter(|c| c.is_number()).cloned().unwrap_or(Value::Null),
    })
}

/// `stop_reason` of an assistant message: `aborted` (the user stopped the run), `error`
/// (another error), else OpenCode's `finish` (`stop`, `tool-calls`, `length`, …).
pub fn stop_reason(info: &Value) -> Option<String> {
    match info.pointer("/error/name").and_then(Value::as_str) {
        Some("MessageAbortedError") => Some("aborted".into()),
        Some(_) => Some("error".into()),
        None => info.get("finish").and_then(Value::as_str).map(str::to_string),
    }
}

/// The run ended on its own: the last turn stopped with a final answer, not a tool call,
/// an abort or an error.
pub fn yielded(stop: Option<&str>) -> bool {
    !matches!(stop, None | Some("tool-calls" | "aborted" | "error" | "unknown"))
}

/// The run is over: its last turn yielded, or was aborted or failed (the loop stopped). A
/// run whose last turn called a tool, or that has no finished turn, is still going.
pub fn run_ended(stop: Option<&str>) -> bool {
    yielded(stop) || matches!(stop, Some("aborted" | "error"))
}

/// The assistant message's visible text (non-synthetic text parts).
pub fn assistant_text(parts: &[Value]) -> String {
    parts
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text") && p.get("synthetic").and_then(Value::as_bool) != Some(true))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `turn_end` of a finished assistant message (`time.completed`, or an error): `None`
/// while it is still streaming. `t` = `time.completed` (else `time.created`).
pub fn turn_record(seq: u32, info: &Value, text: &str) -> Option<TraceRecord> {
    let completed = info.pointer("/time/completed").and_then(Value::as_i64);
    if completed.is_none() && info.get("error").is_none() {
        return None;
    }
    let t = completed.or_else(|| info.pointer("/time/created").and_then(Value::as_i64)).unwrap_or(0);
    let summary = info.get("summary").and_then(Value::as_bool) == Some(true);
    Some(TraceRecord::TurnEnd {
        seq,
        t,
        model: model_of(info),
        usage: usage_of(info),
        stop_reason: stop_reason(info),
        // A compaction summary is not a deliverable.
        text_head: if summary { String::new() } else { head(text, TRACE_DELIVERABLE_CHARS) },
    })
}

/// The `input` record of a user message, or `None` when it is not one (synthetic only, or a
/// command other than `/rematch-mark`, dropped as in omp).
pub fn prompt_record(seq: u32, t: UnixMs, text: &UserText, idle: bool, prev: Option<&str>, deliverable: Option<&str>) -> Option<TraceRecord> {
    match text {
        UserText::Prompt(s) => input_record(seq, t, s, idle, prev, deliverable, ""),
        UserText::Command(s) => input_record(seq, t, s, idle, prev, deliverable, "command"),
        UserText::Ignored => None,
    }
}

// ---- trace synthesis ----

struct Synth {
    out: Vec<TraceRecord>,
    turns_since_agent_end: u32,
    /// An `input` opened a run that has no `agent_end` yet.
    open: bool,
    last_stop: Option<String>,
    last_model: Option<ModelRef>,
    last_input: Option<String>,
    last_deliverable: Option<String>,
}

impl Synth {
    fn seq(&self) -> u32 {
        self.out.len() as u32
    }

    fn agent_end(&mut self, t: UnixMs) {
        if !self.open && self.turns_since_agent_end == 0 {
            return;
        }
        let seq = self.seq();
        let yielded = yielded(self.last_stop.as_deref());
        self.out.push(TraceRecord::AgentEnd { seq, t, turns: self.turns_since_agent_end, is_terminal: true, yielded });
        self.turns_since_agent_end = 0;
        self.open = false;
    }
}

/// The DESIGN §3 trace the live adapter would have written for a stored session:
///
/// * `session_start`: `cwd` = the session's directory, repo by walking up to `.git`, model of
///   the first assistant message, `mode: "import"`, agent `main` / `sub` (a session with a
///   parent), `omp_version: "opencode <version>"`, no loaded entries (the store does not
///   record what the plugin loaded).
/// * a user message whose [`user_prompt`] is a prompt (or `/rematch-mark`) → `input`, after
///   `agent_end` for the run it ends. It is a takeover (`interrupt`) when that run had not
///   ended (its last turn stopped on a tool call), idle after a final answer, an abort or
///   an error. Subagent sessions have no inputs (the parent agent wrote their prompt).
/// * each tool part in a final state → `tool_result` ([`tool_result_record`]), then the
///   message's `turn_end` ([`turn_record`]), preceded by `model_change` when the model
///   differs from the previous turn.
/// * a final `agent_end` and `session_shutdown` flagged by [`flag_reasons`].
pub fn trace_records_of(s: &OpenCodeSession) -> Vec<TraceRecord> {
    let repo = repo_of(Path::new(&s.directory)).map(|p| p.to_string_lossy().into_owned());
    let (repo_remote, subpath) = crate::identity::session_place(repo.as_deref(), &s.directory);
    let assistants = || s.messages.iter().filter(|m| m.role() == "assistant");
    let model = assistants().find_map(|m| model_of(&m.info));
    let agent = match &s.parent {
        Some(_) => AgentInfo {
            kind: "sub".into(),
            name: assistants().find_map(|m| m.info.get("agent").and_then(Value::as_str)).unwrap_or("sub").to_string(),
        },
        None => AgentInfo { kind: "main".into(), name: "main".into() },
    };
    let mut w = Synth {
        out: Vec::new(),
        turns_since_agent_end: 0,
        open: false,
        last_stop: None,
        last_model: model.clone(),
        last_input: None,
        last_deliverable: None,
    };
    w.out.push(TraceRecord::SessionStart {
        seq: 0,
        t: s.created,
        session_id: s.id.clone(),
        cwd: s.directory.clone(),
        branch_hint: basename(repo.as_deref().unwrap_or(&s.directory)),
        repo,
        model,
        mode: "import".into(),
        agent: Some(agent),
        omp_version: if s.version.is_empty() { "opencode".into() } else { format!("opencode {}", s.version) },
        loaded_entries: Vec::new(),
        hooks_loaded: Vec::new(),
        staging_dirs: Vec::new(),
        repo_remote,
        subpath,
        head_sha: None,
        visible_entries: Vec::new(),
    });
    let mut last_t = s.created;
    for m in &s.messages {
        let t = m.created.max(last_t);
        last_t = t;
        match m.role() {
            "user" => {
                if s.is_subagent() {
                    continue;
                }
                let text = user_prompt(&m.parts);
                if text == UserText::Ignored {
                    continue;
                }
                // A run that has not ended (no turn yet, or its last turn called a tool) is
                // still going: this prompt takes it over.
                let idle = !w.open || run_ended(w.last_stop.as_deref());
                w.agent_end(t);
                let seq = w.seq();
                if let Some(rec) = prompt_record(seq, t, &text, idle, w.last_input.as_deref(), w.last_deliverable.as_deref()) {
                    if let TraceRecord::Input { text, .. } = &rec {
                        w.last_input = Some(text.clone());
                    }
                    w.out.push(rec);
                    w.open = true;
                    w.last_stop = None;
                }
            }
            "assistant" => {
                for p in &m.parts {
                    let seq = w.seq();
                    if let Some(rec) = tool_result_record(seq, p) {
                        last_t = last_t.max(rec.t());
                        w.out.push(rec);
                    }
                }
                let text = assistant_text(&m.parts);
                let seq = w.seq();
                let Some(TraceRecord::TurnEnd { t, model, usage, stop_reason, text_head, .. }) = turn_record(seq, &m.info, &text) else { continue };
                let t = t.max(last_t);
                last_t = t;
                if let (Some(to), Some(from)) = (&model, &w.last_model) {
                    if to != from {
                        let seq = w.seq();
                        w.out.push(TraceRecord::ModelChange { seq, t, from: Some(from.clone()), to: Some(to.clone()), reason: Some("turn".into()) });
                    }
                }
                if model.is_some() {
                    w.last_model = model.clone();
                }
                if !text_head.trim().is_empty() {
                    w.last_deliverable = Some(text_head.clone());
                }
                w.last_stop = stop_reason.clone();
                let seq = w.seq();
                w.out.push(TraceRecord::TurnEnd { seq, t, model, usage, stop_reason, text_head });
                w.turns_since_agent_end += 1;
            }
            _ => {}
        }
    }
    w.agent_end(last_t);
    let reasons = flag_reasons(&w.out);
    let seq = w.seq();
    w.out.push(TraceRecord::SessionShutdown { seq, t: last_t, flagged: !reasons.is_empty(), reasons, files_changed: Vec::new() });
    w.out
}

/// The ledger row of a stored session: [`fold_trace`] of [`trace_records_of`], source
/// `"import"`, the store recorded as `source_path` ([`locate`] reads the session back).
pub fn session_record(s: &OpenCodeSession) -> SessionRecord {
    let mut r = fold_trace(&trace_records_of(s), &s.id);
    r.source = "import".to_string();
    r.source_path = Some(s.db.to_string_lossy().into_owned());
    r
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// The 1.19.4 columns the importer reads (the real tables have more).
    pub(crate) const SCHEMA: &str = "
        CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL DEFAULT 'global', parent_id TEXT, slug TEXT NOT NULL DEFAULT 's',
          directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);";

    /// Builds a store: `sessions` = (id, parent, dir, created); `messages` = (session, id, t, info, parts).
    pub(crate) fn store(path: &Path, sessions: &[(&str, Option<&str>, &str, i64)], messages: &[(&str, &str, i64, Value, Vec<Value>)]) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        for (id, parent, dir, t) in sessions {
            c.execute(
                "INSERT INTO session (id, parent_id, directory, title, version, time_created, time_updated) VALUES (?1, ?2, ?3, 't', '1.19.4', ?4, ?4)",
                rusqlite::params![id, parent, dir, t],
            )
            .unwrap();
        }
        for (sid, mid, t, info, parts) in messages {
            c.execute("INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4)", rusqlite::params![mid, sid, t, info.to_string()]).unwrap();
            for (i, p) in parts.iter().enumerate() {
                c.execute(
                    "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                    rusqlite::params![format!("prt_{mid}_{i:03}"), mid, sid, t, p.to_string()],
                )
                .unwrap();
            }
        }
    }

    pub(crate) fn user(text: &str) -> (Value, Vec<Value>) {
        (json!({"role": "user", "agent": "build", "model": {"providerID": "mock", "modelID": "scripted"}}), vec![json!({"type": "text", "text": text})])
    }

    pub(crate) fn assistant(t: i64, finish: &str) -> Value {
        json!({"role": "assistant", "agent": "build", "modelID": "scripted", "providerID": "mock", "finish": finish, "cost": 0.5,
               "tokens": {"input": 11, "output": 7, "reasoning": 2, "cache": {"read": 3, "write": 1}, "total": 24},
               "time": {"created": t, "completed": t + 5}})
    }

    pub(crate) fn tool(name: &str, call: &str, status: &str, input: Value, out: &str, exit: Option<i64>) -> Value {
        let mut state = json!({"status": status, "input": input, "time": {"start": 1, "end": 2}});
        if status == "error" {
            state["error"] = json!(out);
        } else {
            state["output"] = json!(out);
            state["metadata"] = json!({"exit": exit});
        }
        json!({"type": "tool", "tool": name, "callID": call, "state": state})
    }

    fn evs(r: &[TraceRecord]) -> Vec<&str> {
        r.iter().map(TraceRecord::ev).collect()
    }

    #[test]
    fn ids_round_trip() {
        assert_eq!(session_id("ses_a"), "oc:ses_a");
        assert_eq!(segment_id("ses_a", 1), "oc:ses_a");
        assert_eq!(segment_id("ses_a", 3), "oc:ses_a:r3");
        assert_eq!(native_id("oc:ses_a:r3"), Some("ses_a"));
        assert_eq!(native_id(&subagent_id("ses_p", "ses_c")), Some("ses_c"));
        assert_eq!(native_id("oc:ses_arx"), Some("ses_arx"));
        assert_eq!(native_id("cc:x"), None);
    }

    #[test]
    fn user_text_kinds() {
        let p = |v: Value| user_prompt(&[v]);
        assert_eq!(p(json!({"type": "text", "text": "fix it"})), UserText::Prompt("fix it".into()));
        assert_eq!(p(json!({"type": "text", "text": "Called the Read tool", "synthetic": true})), UserText::Ignored);
        assert_eq!(p(json!({"type": "file", "url": "file:///x"})), UserText::Ignored);
        assert_eq!(
            p(json!({"type": "text", "text": "<rematch-command name=\"mark\" args=\"correction\"/>\nmarked"})),
            UserText::Command("/rematch-mark correction".into())
        );
        let r = prompt_record(0, 0, &UserText::Command("/rematch-mark correction".into()), true, None, None).unwrap();
        assert!(matches!(r, TraceRecord::Input { ref prelabel, .. } if prelabel == "mark:correction"));
        assert!(prompt_record(0, 0, &UserText::Command("/rematch-status".into()), true, None, None).is_none());
    }

    #[test]
    fn tool_parts_map_success_failure_and_skills() {
        let ok = tool("skill", "c1", "completed", json!({"name": "probe-skill"}), "<skill_content…>", None);
        let Some(TraceRecord::ToolResult { tool: name, skill_read, is_error, call_id, .. }) = tool_result_record(0, &ok) else { panic!() };
        assert_eq!((name.as_str(), skill_read.as_deref(), is_error, call_id.as_str()), ("skill", Some("probe-skill"), false, "c1"));
        let exit1 = tool("bash", "c2", "completed", json!({"command": "ls /nope"}), "ls: no such file", Some(1));
        assert!(matches!(tool_result_record(0, &exit1), Some(TraceRecord::ToolResult { is_error: true, .. })));
        let thrown = tool("read", "c3", "error", json!({"filePath": "/x/skills/foo/SKILL.md"}), "File not found", None);
        let Some(TraceRecord::ToolResult { is_error, skill_read, path, content_head, error_class, .. }) = tool_result_record(0, &thrown) else { panic!() };
        assert!(is_error);
        assert_eq!(skill_read, None, "a failed read loads no skill");
        assert_eq!(path.as_deref(), Some("/x/skills/foo/SKILL.md"));
        assert_eq!((content_head.as_str(), error_class.as_deref()), ("File not found", Some("error")));
        let read = tool("read", "c4", "completed", json!({"filePath": "/x/skills/foo/SKILL.md"}), "body", None);
        assert!(matches!(tool_result_record(0, &read), Some(TraceRecord::ToolResult { skill_read: Some(ref s), .. }) if s == "foo"));
        let running = json!({"type": "tool", "tool": "bash", "callID": "c5", "state": {"status": "running", "input": {}}});
        assert!(tool_result_record(0, &running).is_none());
    }

    #[test]
    fn turn_usage_and_stop() {
        let info = assistant(10, "stop");
        let Some(TraceRecord::TurnEnd { t, model, usage, stop_reason, text_head, .. }) = turn_record(0, &info, "done") else { panic!() };
        assert_eq!(t, 15);
        assert_eq!(model, Some(ModelRef { provider: "mock".into(), id: "scripted".into() }));
        let u = usage.unwrap();
        assert_eq!((u.input, u.output, u.cache_read, u.cache_write, u.total_tokens), (11, 7, 3, 1, 24));
        assert_eq!(u.cost, json!(0.5));
        assert_eq!((stop_reason.as_deref(), text_head.as_str()), (Some("stop"), "done"));
        let streaming = json!({"role": "assistant", "modelID": "m", "time": {"created": 1}});
        assert!(turn_record(0, &streaming, "").is_none());
        let aborted = json!({"role": "assistant", "modelID": "m", "time": {"created": 1}, "error": {"name": "MessageAbortedError", "data": {}}});
        assert!(matches!(turn_record(0, &aborted, ""), Some(TraceRecord::TurnEnd { stop_reason: Some(ref s), .. }) if s == "aborted"));
        assert_eq!(model_of(&json!({"modelID": "x"})), Some(ModelRef { provider: String::new(), id: "x".into() }));
    }

    #[test]
    fn imports_sessions_from_a_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode.db");
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_s = cwd.to_string_lossy().into_owned();
        let (u1, p1) = user("do the probe");
        let (u2, p2) = user("no, that is wrong, use the other file");
        let (uo, po) = user("other session prompt");
        let child_user = (json!({"role": "user", "agent": "general"}), vec![json!({"type": "text", "text": "child work please"})]);
        let synthetic = (json!({"role": "user"}), vec![json!({"type": "text", "text": "Continue if you have next steps", "synthetic": true})]);
        store(
            &db,
            &[("ses_a", None, &cwd_s, 100), ("ses_b", None, &cwd_s, 101), ("ses_c", Some("ses_a"), &cwd_s, 150)],
            &[
                ("ses_a", "msg_a1", 110, u1, p1),
                ("ses_b", "msg_b1", 111, uo, po),
                (
                    "ses_a",
                    "msg_a2",
                    120,
                    assistant(120, "tool-calls"),
                    vec![
                        json!({"type": "step-start"}),
                        tool("skill", "c1", "completed", json!({"name": "probe-skill"}), "skill body", None),
                        tool("bash", "c2", "completed", json!({"command": "ls /nope"}), "no such file", Some(1)),
                        tool("read", "c3", "error", json!({"filePath": "/nope"}), "File not found: /nope", None),
                        tool("bash", "c4", "error", json!({"command": "rm -rf /"}), "rematch: blocked", None),
                    ],
                ),
                ("ses_c", "msg_c1", 151, child_user.0, child_user.1),
                ("ses_c", "msg_c2", 152, assistant(152, "stop"), vec![json!({"type": "text", "text": "child done"})]),
                ("ses_a", "msg_a3", 130, assistant(130, "stop"), vec![json!({"type": "text", "text": "All done."})]),
                ("ses_a", "msg_a4", 140, synthetic.0, synthetic.1),
                ("ses_a", "msg_a5", 160, u2, p2),
                ("ses_a", "msg_a6", 170, assistant(170, "stop"), vec![json!({"type": "text", "text": "fixed"})]),
            ],
        );
        let refs = scan_store(&db).unwrap();
        assert_eq!(refs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["ses_a", "ses_b", "ses_c"]);
        let a = read_session(&refs[0]).unwrap();
        assert_eq!((a.id.as_str(), a.is_subagent(), a.messages.len()), ("oc:ses_a", false, 6));
        let recs = trace_records_of(&a);
        assert_eq!(
            evs(&recs),
            [
                "session_start", "input", "tool_result", "tool_result", "tool_result", "tool_result", "turn_end", "turn_end", "agent_end",
                "input", "turn_end", "agent_end", "session_shutdown"
            ]
        );
        let TraceRecord::SessionStart { omp_version, mode, model, cwd, .. } = &recs[0] else { panic!() };
        assert_eq!((omp_version.as_str(), mode.as_str(), cwd.as_str()), ("opencode 1.19.4", "import", cwd_s.as_str()));
        assert_eq!(model.as_ref().map(|m| m.id.as_str()), Some("scripted"));
        let errors = recs.iter().filter(|r| matches!(r, TraceRecord::ToolResult { is_error: true, .. })).count();
        assert_eq!(errors, 3);
        // The synthetic continuation is no prompt; the correction after a final answer is idle.
        let TraceRecord::Input { prelabel, idle, after_deliverable, .. } = &recs[9] else { panic!() };
        assert_eq!((prelabel.as_str(), *idle, after_deliverable.as_deref()), ("negation", true, Some("All done.")));
        let rec = session_record(&a);
        assert_eq!(rec.source, "import");
        assert_eq!(rec.source_path.as_deref(), Some(db.to_string_lossy().as_ref()));
        assert!(rec.skill_reads.contains(&"probe-skill".to_string()));
        assert!(rec.flagged, "a negation and an error cluster flag the session");
        // Concurrent sessions stay apart.
        let b = read_session(&refs[1]).unwrap();
        assert_eq!(b.messages.len(), 1);
        assert!(trace_records_of(&b).iter().all(|r| !matches!(r, TraceRecord::ToolResult { .. })));
        // The subagent: namespaced under its parent, no inputs.
        let c = read_session(&refs[2]).unwrap();
        assert_eq!((c.id.as_str(), c.is_subagent()), ("oc:ses_a:agent-ses_c", true));
        let crecs = trace_records_of(&c);
        assert!(crecs.iter().all(|r| !matches!(r, TraceRecord::Input { .. })));
        assert!(matches!(&crecs[0], TraceRecord::SessionStart { agent: Some(a), .. } if a.kind == "sub" && a.name == "build"));
        // Read back by ledger id, segments included.
        assert_eq!(locate(Some(&db), "oc:ses_a:r2").map(|r| r.id), Some("ses_a".to_string()));
        assert_eq!(locate(Some(&db), "oc:ses_a:agent-ses_c").map(|r| r.id), Some("ses_c".to_string()));
        assert_eq!(locate(Some(&db), "oc:ses_zzz"), None);
    }

    #[test]
    fn takeover_while_the_run_is_going() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode.db");
        let (u1, p1) = user("build the thing");
        let (u2, p2) = user("stop, do it differently instead please");
        store(
            &db,
            &[("ses_t", None, "/nowhere-oc", 1)],
            &[
                ("ses_t", "msg_1", 10, u1, p1),
                ("ses_t", "msg_2", 20, assistant(20, "tool-calls"), vec![tool("bash", "c1", "completed", json!({"command": "make"}), "ok", Some(0))]),
                ("ses_t", "msg_3", 30, u2, p2),
            ],
        );
        let s = read_session(&SessionRef { db: db.clone(), id: "ses_t".into() }).unwrap();
        let recs = trace_records_of(&s);
        let inputs: Vec<(&str, bool)> = recs
            .iter()
            .filter_map(|r| match r {
                TraceRecord::Input { prelabel, idle, .. } => Some((prelabel.as_str(), *idle)),
                _ => None,
            })
            .collect();
        assert_eq!(inputs, [("none", true), ("interrupt", false)]);
        assert!(recs.iter().any(|r| matches!(r, TraceRecord::AgentEnd { yielded: false, .. })));
    }

    #[test]
    fn refuses_a_foreign_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        Connection::open(&db).unwrap().execute_batch("CREATE TABLE session (id TEXT)").unwrap();
        assert!(scan_store(&db).unwrap_err().to_string().contains("not an OpenCode 1.x store"));
        assert!(scan_store(&dir.path().join("missing.db")).is_err());
    }
}
