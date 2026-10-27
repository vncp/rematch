//! Historical importer: omp session JSONL (`~/.omp/agent/sessions/<slug>/<ts>_<uuid>.jsonl`,
//! subagents under `<ts>_<uuid>/<Name>.jsonl`) → [`SessionRecord`] (source `"import"`),
//! receipt-derived [`Outcome`]s (DESIGN §0, SPEC §"What the importer can and cannot recover"),
//! and the extension's trace synthesized after the fact ([`trace_records_of`]).
//!
//! Format facts relied on: the `session` record is located by type (line 1 is a padded
//! `title` slot in live files); entries form a tree through `id`/`parentId` and the active
//! branch is the chain ending at the last entry; files without `parentId` (version 1) are
//! linear; unknown `type`s are carried along and ignored. Redirect and error-cluster seqs of
//! imported sessions address the synthesized trace, so the ledger row, `reflect --history`
//! and `improve` agree on moments; outcome evidence references use the entry's index on the
//! active branch.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::{json, Value};

use crate::family::{family_of, family_of_selector};
use crate::features::{exts_of, head, stems_of_session, strip_selector};
use crate::outcome::{canonical_json, machine_name, new_outcome_at, sha256_hex, NewOutcome};
use crate::redirect::{prelabel, PRELABEL_NONE};
use crate::trace::{
    basename, fold_trace, push_unique, skill_from_read_path, skills_in_text, tool_input_files,
    AgentInfo, TraceRecord, Usage, ERROR_CLUSTER_MIN, ERROR_CLUSTER_WINDOW,
};
use crate::types::{
    EntityRef, Evidence, EvidenceKind, Family, ModelRef, Outcome, OutcomeKind, OutcomeResult,
    Provenance, ResultStatus, SessionRecord, Subject, UnixMs,
};

/// Tools whose arguments name touched files in imported sessions.
pub const FILE_TOOLS: &[&str] = &["read", "write", "edit", "apply_patch"];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionHeader {
    pub id: String,
    pub version: u32,
    pub timestamp: String,
    pub cwd: String,
    /// Path of the parent session file (subagent sessions).
    pub parent_session: Option<String>,
}

/// A parsed omp session: header plus the active branch, root → leaf.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OmpSession {
    pub path: Option<PathBuf>,
    pub header: SessionHeader,
    pub entries: Vec<Value>,
}

impl OmpSession {
    /// Parent session id for subagent sessions (from `parentSession`'s `<ts>_<uuid>.jsonl`).
    pub fn parent_id(&self) -> Option<String> {
        self.header
            .parent_session
            .as_deref()
            .and_then(|p| session_id_from_filename(Path::new(p)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFile {
    pub path: PathBuf,
    /// Set for subagent files stored under the parent's `<ts>_<uuid>/` directory.
    pub parent_id: Option<String>,
}

/// `.../<ts>_<uuid>.jsonl` → `<uuid>`.
pub fn session_id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let (_, id) = stem.rsplit_once('_')?;
    (!id.is_empty()).then(|| id.to_string())
}

/// Every `*.jsonl` under `dir` (recursively), sorted by path without extension so a parent
/// session precedes its subagents. Subagent files carry the parent id when their directory
/// `<ts>_<uuid>` has a sibling `<ts>_<uuid>.jsonl`.
pub fn scan_sessions_dir(dir: &Path) -> Result<Vec<SessionFile>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in
            std::fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))?
        {
            let entry = entry?;
            let ft = entry.file_type()?;
            let p = entry.path();
            if ft.is_dir() {
                walk(&p, out)?;
            } else if ft.is_file() && p.extension().is_some_and(|e| e == "jsonl") {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    walk(dir, &mut paths)?;
    paths.sort_by_cached_key(|p| p.with_extension("").to_string_lossy().into_owned());
    Ok(paths
        .into_iter()
        .map(|path| {
            let parent_id = path.parent().and_then(|d| {
                let sibling = d.with_extension("jsonl");
                (d != dir && sibling.is_file())
                    .then(|| session_id_from_filename(&sibling))
                    .flatten()
            });
            SessionFile { path, parent_id }
        })
        .collect())
}

pub fn read_session(path: &Path) -> Result<OmpSession> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read session {}", path.display()))?;
    parse_session(&text, Some(path))
}

/// Parses session JSONL text. Fails only when no entry and no header parse.
pub fn parse_session(text: &str, path: Option<&Path>) -> Result<OmpSession> {
    let mut header: Option<SessionHeader> = None;
    let mut body: Vec<Value> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !v.is_object() {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            Some("session") if header.is_none() => {
                header = Some(SessionHeader {
                    id: str_of(&v, "id"),
                    version: v.get("version").and_then(Value::as_u64).unwrap_or(1) as u32,
                    timestamp: str_of(&v, "timestamp"),
                    cwd: str_of(&v, "cwd"),
                    parent_session: v
                        .get("parentSession")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
            Some("title") | Some("session") => {}
            _ => body.push(v),
        }
    }
    if header.is_none() && body.is_empty() {
        bail!(
            "no session records{}",
            path.map(|p| format!(" in {}", p.display()))
                .unwrap_or_default()
        );
    }
    let mut header = header.unwrap_or_default();
    if header.id.is_empty() {
        header.id = path.and_then(session_id_from_filename).unwrap_or_default();
    }
    Ok(OmpSession {
        path: path.map(Path::to_path_buf),
        header,
        entries: active_branch(body),
    })
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Chain ending at the last entry with an `id`, root first. Linear when no entry has a
/// `parentId` key (version 1 files).
fn active_branch(body: Vec<Value>) -> Vec<Value> {
    if !body.iter().any(|v| v.get("parentId").is_some()) {
        return body;
    }
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, v) in body.iter().enumerate() {
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            index.insert(id, i);
        }
    }
    let Some(leaf) = body
        .iter()
        .rposition(|v| v.get("id").and_then(Value::as_str).is_some())
    else {
        return body;
    };
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = Some(leaf);
    while let Some(i) = cur {
        if !seen.insert(i) {
            break;
        }
        chain.push(i);
        cur = body[i]
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|p| index.get(p).copied());
    }
    chain.reverse();
    let keep: HashSet<usize> = chain.iter().copied().collect();
    body.into_iter()
        .enumerate()
        .filter(|(i, _)| keep.contains(i))
        .map(|(_, v)| v)
        .collect()
}

pub(crate) fn iso_ms(s: &str) -> Option<UnixMs> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis())
}

pub(crate) fn entry_ms(e: &Value) -> Option<UnixMs> {
    e.get("timestamp")
        .and_then(Value::as_str)
        .and_then(iso_ms)
        .or_else(|| e.pointer("/message/timestamp").and_then(Value::as_i64))
}

/// A user-role message the harness injected rather than the human typed: task
/// notifications, system notices/reminders, and hook messages, all of which open with a
/// known XML frame. Attachment envelopes (`[Image #1]`) are typed by the user and stay.
pub fn is_injected_frame(text: &str) -> bool {
    const FRAMES: &[&str] = &[
        "<task-notification",
        "<system-notice",
        "<system-reminder",
        "<hook-message",
        "<session-agents",
        "<ttsr",
        "<task-result",
        "<channel-message",
        "<teammate-message",
        "Another Claude session sent a message",
    ];
    let t = text.trim_start();
    FRAMES.iter().any(|f| t.starts_with(f))
}

/// Text blocks of a message `content` (string or block array), joined by newlines.
pub fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn message(e: &Value) -> Option<(&str, &Value)> {
    if e.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let m = e.get("message")?;
    Some((m.get("role").and_then(Value::as_str)?, m))
}

pub(crate) fn tool_calls(m: &Value) -> impl Iterator<Item = &Value> {
    m.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("toolCall"))
}

/// Git toplevel by walking up from `cwd` looking for `.git` (dir or file). No process.
pub fn repo_of(cwd: &Path) -> Option<PathBuf> {
    if cwd.as_os_str().is_empty() {
        return None;
    }
    cwd.ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Derives the ledger row of an imported session. Redirects, takeovers, error clusters and
/// the flag come from folding [`trace_records_of`], so their seqs address the synthesized
/// trace and an imported session is flagged by exactly the extension's rule.
pub fn session_record(s: &OmpSession) -> SessionRecord {
    let mut r = SessionRecord {
        id: s.header.id.clone(),
        cwd: s.header.cwd.clone(),
        source: "import".to_string(),
        source_path: s.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        ..Default::default()
    };
    r.repo = repo_of(Path::new(&s.header.cwd)).map(|p| p.to_string_lossy().into_owned());
    (r.repo_remote, r.subpath) = crate::identity::session_place(r.repo.as_deref(), &s.header.cwd);
    r.branch = basename(r.repo.as_deref().unwrap_or(&s.header.cwd));
    r.started_at = start_ms(s);
    r.ended_at = s.entries.iter().rev().find_map(entry_ms);

    let mut skills_seen = HashSet::new();
    let mut files_seen = HashSet::new();
    let mut user_texts: Vec<String> = Vec::new();
    let mut fallback_model: Option<String> = None;

    for e in &s.entries {
        if e.get("type").and_then(Value::as_str) == Some("model_change") && fallback_model.is_none()
        {
            fallback_model = e.get("model").and_then(Value::as_str).map(str::to_string);
        }
        let Some((role, m)) = message(e) else {
            continue;
        };
        match role {
            "user" => {
                let text = content_text(m.get("content"));
                for name in skills_in_text(&text) {
                    push_unique(&mut r.skill_reads, &mut skills_seen, name);
                }
                user_texts.push(text);
            }
            "assistant" => {
                r.turns += 1;
                if let Some(u) = m.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    r.tokens_in += n("input") + n("cacheRead") + n("cacheWrite");
                    r.tokens_out += n("output");
                }
                if r.model.is_none() {
                    if let Some(m) = model_ref(m) {
                        r.family = Some(family_of(&m.provider, &m.id));
                        r.model = Some(m);
                    }
                }
                for call in tool_calls(m) {
                    r.tool_calls += 1;
                    let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
                    let args = call.get("arguments").unwrap_or(&Value::Null);
                    if name == "read" {
                        if let Some(skill) = args
                            .get("path")
                            .and_then(Value::as_str)
                            .and_then(skill_from_read_path)
                        {
                            push_unique(&mut r.skill_reads, &mut skills_seen, skill);
                        }
                    }
                    if FILE_TOOLS.contains(&name) {
                        for f in tool_input_files(name, args) {
                            push_unique(&mut r.files_touched, &mut files_seen, f);
                        }
                    }
                }
            }
            "toolResult" if m.get("isError").and_then(Value::as_bool).unwrap_or(false) => {
                r.tool_errors += 1;
            }
            _ => {}
        }
    }

    if r.model.is_none() {
        if let Some(sel) = fallback_model {
            r.family = Some(family_of_selector(&sel));
            if let Some((p, id)) = sel.split_once('/') {
                r.model = Some(ModelRef {
                    provider: p.to_string(),
                    id: id.to_string(),
                });
            }
        }
    }
    r.exts = exts_of(&r.files_touched);
    r.stems = stems_of_session(&user_texts, &r.files_touched);
    apply_trace_signals(&mut r, &fold_trace(&trace_records_of(s), &s.header.id));
    r
}

/// Copies the trace-derived signals of `folded` (redirects, takeovers, error clusters, flag)
/// onto an imported row, keeping the importer's own counts and features. A classifier label
/// already on the row survives when its redirect has the same seq and time in `folded` (rows
/// imported before seqs addressed the trace re-fold without inheriting a stranger's label).
/// Used by [`session_record`], by `count` when it folds a trace synthesized for an imported
/// session (the import row is never replaced by a trace-sourced one), and by `improve`.
pub fn apply_trace_signals(r: &mut SessionRecord, folded: &SessionRecord) {
    let mut redirects = folded.redirects.clone();
    for red in redirects.iter_mut().filter(|x| x.class.is_none()) {
        if let Some(old) = r.redirects.iter().find(|o| o.seq == red.seq && o.t == red.t) {
            red.class = old.class;
            red.confidence = old.confidence;
        }
    }
    r.redirects = redirects;
    r.takeovers = folded.takeovers;
    r.error_cluster_seqs = folded.error_cluster_seqs.clone();
    r.error_clusters = folded.error_clusters;
    r.flagged = folded.flagged;
}

pub fn import_session(path: &Path) -> Result<SessionRecord> {
    Ok(session_record(&read_session(path)?))
}

pub(crate) fn start_ms(s: &OmpSession) -> UnixMs {
    iso_ms(&s.header.timestamp)
        .or_else(|| s.entries.first().and_then(entry_ms))
        .unwrap_or(0)
}

/// `{provider, id}` of an assistant message.
pub(crate) fn model_ref(m: &Value) -> Option<ModelRef> {
    Some(ModelRef {
        provider: m.get("provider")?.as_str()?.to_string(),
        id: m.get("model")?.as_str()?.to_string(),
    })
}

/// `provider/id[:thinking]` selector → `{provider, id}` (the extension's `parseSelector`).
fn selector_ref(sel: &str) -> Option<ModelRef> {
    static THINKING_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i):(?:off|minimal|low|medium|high|xhigh|max)$").expect("static pattern")
    });
    let clean = THINKING_RE.replace(sel, "");
    if clean.is_empty() {
        return None;
    }
    Some(match clean.split_once('/') {
        Some((p, id)) => ModelRef {
            provider: p.to_string(),
            id: id.to_string(),
        },
        None => ModelRef {
            provider: String::new(),
            id: clean.into_owned(),
        },
    })
}

// ---- the extension's trace, synthesized (DESIGN §3) ----

/// Caps of the live extension (`extension/index.ts` `LIMITS`). Text caps count chars (the
/// extension counts UTF-16 units); the tool-input cap counts bytes of JSON.
pub const TRACE_INPUT_TEXT_CHARS: usize = 16 * 1024;
pub const TRACE_TOOL_INPUT_BYTES: usize = 8 * 1024;
pub const TRACE_CONTENT_HEAD_CHARS: usize = 1024;
pub const TRACE_DELIVERABLE_CHARS: usize = 2 * 1024;

/// sha256 of the canonical JSON (keys sorted) of a tool input (the extension's `argsHash`).
pub fn args_hash(input: &Value) -> String {
    sha256_hex(canonical_json(input).as_bytes())
}

fn json_bytes(v: &Value) -> usize {
    v.to_string().len()
}

fn trunc_str(s: &str, limit: usize) -> String {
    match s.char_indices().nth(limit) {
        None => s.to_string(),
        Some((i, _)) => format!("{}…[+{}]", &s[..i], s[i..].chars().count()),
    }
}

fn map_strings(v: &Value, limit: usize, max_items: usize) -> Value {
    match v {
        Value::String(s) => Value::String(trunc_str(s, limit)),
        Value::Array(a) => {
            let mut kept: Vec<Value> =
                a.iter().take(max_items).map(|x| map_strings(x, limit, max_items)).collect();
            if a.len() > max_items {
                kept.push(Value::String(format!("…[+{}]", a.len() - max_items)));
            }
            Value::Array(kept)
        }
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), map_strings(x, limit, max_items)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Tool input for the trace (the extension's `truncateToolInput`): ≤ `max` bytes of JSON,
/// keys kept, long strings cut with `…[+N]` (N = chars removed). Shrinks the per-string
/// limit, then long arrays, then collapses values.
pub fn truncate_tool_input(input: &Value, max: usize) -> Value {
    if json_bytes(input) <= max {
        return input.clone();
    }
    let mut limit = 2048;
    while limit >= 16 {
        let out = map_strings(input, limit, usize::MAX);
        if json_bytes(&out) <= max {
            return out;
        }
        limit /= 2;
    }
    let mut items = 32;
    while items >= 1 {
        let out = map_strings(input, 16, items);
        if json_bytes(&out) <= max {
            return out;
        }
        items /= 2;
    }
    if let Value::Object(o) = input {
        let collapsed: serde_json::Map<String, Value> = o
            .iter()
            .map(|(k, x)| {
                let v = match x {
                    Value::String(s) => trunc_str(s, 16),
                    other => format!("…[+{}]", json_bytes(other)),
                };
                (k.clone(), Value::String(v))
            })
            .collect();
        let collapsed = Value::Object(collapsed);
        if json_bytes(&collapsed) <= max {
            return collapsed;
        }
    }
    Value::String(format!("…[+{}]", json_bytes(input)))
}

/// Bytes of a tool result's content: text blocks as UTF-8, other blocks by their `data`
/// string length (the extension's `contentBytes`).
pub(crate) fn content_bytes(content: Option<&Value>) -> u64 {
    match content {
        Some(Value::String(s)) => s.len() as u64,
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|b| {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        return t.len() as u64;
                    }
                }
                b.get("data").and_then(Value::as_str).map_or(0, |d| d.len() as u64)
            })
            .sum(),
        _ => 0,
    }
}

/// Builds the trace the extension would have written, record by record, seq = index.
struct TraceSynth<'a> {
    out: Vec<TraceRecord>,
    /// Assistant message whose `turn_end` is due once its tool results are written, and the
    /// latest time seen for that turn.
    turn: Option<(&'a Value, UnixMs)>,
    turns_since_agent_end: u32,
    last_stop: Option<&'a str>,
    last_model: Option<ModelRef>,
    last_input: Option<String>,
    last_deliverable: Option<String>,
    errors: VecDeque<bool>,
    reasons: Vec<String>,
}

impl<'a> TraceSynth<'a> {
    fn seq(&self) -> u32 {
        self.out.len() as u32
    }

    fn reason(&mut self, r: String) {
        if !self.reasons.contains(&r) {
            self.reasons.push(r);
        }
    }

    fn flush_turn(&mut self) {
        let Some((m, t)) = self.turn.take() else {
            return;
        };
        let model = model_ref(m);
        if let (Some(to), Some(from)) = (&model, &self.last_model) {
            if to != from {
                self.out.push(TraceRecord::ModelChange {
                    seq: self.seq(),
                    t,
                    from: Some(from.clone()),
                    to: Some(to.clone()),
                    reason: Some("turn".into()),
                });
            }
        }
        if model.is_some() {
            self.last_model = model.clone();
        }
        let usage = m
            .get("usage")
            .and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok())
            .unwrap_or_default();
        let text = content_text(m.get("content"));
        self.out.push(TraceRecord::TurnEnd {
            seq: self.seq(),
            t,
            model,
            usage: Some(usage),
            stop_reason: m.get("stopReason").and_then(Value::as_str).map(str::to_string),
            text_head: head(&text, TRACE_DELIVERABLE_CHARS),
        });
        if !text.trim().is_empty() {
            self.last_deliverable = Some(text);
        }
        self.turns_since_agent_end += 1;
    }

    fn agent_end(&mut self, t: UnixMs) {
        if self.turns_since_agent_end == 0 {
            return;
        }
        self.out.push(TraceRecord::AgentEnd {
            seq: self.seq(),
            t,
            turns: self.turns_since_agent_end,
            is_terminal: true,
            yielded: self.last_stop == Some("stop"),
        });
        self.turns_since_agent_end = 0;
    }
}

/// The extension's trace (DESIGN §3) synthesized from an omp session's active branch:
///
/// * `session_start` — header id/cwd, repo via [`repo_of`], model of the first assistant
///   message (else the first `model_change` selector), `mode: "import"`, agent `main` (`sub`
///   for subagent files), `omp_version` from the first entry's `runtime.version`, no loaded
///   entries, hooks or staging dirs.
/// * user message (attribution not `agent`) → `input` with the live prelabel rules: idle
///   unless the previous assistant stopped `aborted` (→ `interrupt`), `reprompt` against the
///   previous input; slash/`!`/`%` lines other than `/rematch mark` and `/skill:` are not
///   inputs, as live. It is preceded by `agent_end` for the run it ends.
/// * `toolResult` → `tool_result`, paired with its `toolCall` by id for the input.
/// * assistant message → `turn_end` after its tool results, preceded by `model_change` when
///   provider/model differs from the previous turn.
/// * a final `agent_end` and `session_shutdown` with the extension's flag reasons.
///
/// `seq` is the record index; `t` the entry timestamp (the previous one when missing).
pub fn trace_records_of(session: &OmpSession) -> Vec<TraceRecord> {
    let header = &session.header;
    let start_t = start_ms(session);
    let repo = repo_of(Path::new(&header.cwd)).map(|p| p.to_string_lossy().into_owned());
    let (repo_remote, subpath) = crate::identity::session_place(repo.as_deref(), &header.cwd);
    let model = session
        .entries
        .iter()
        .filter_map(message)
        .find(|(role, _)| *role == "assistant")
        .and_then(|(_, m)| model_ref(m))
        .or_else(|| {
            session
                .entries
                .iter()
                .find(|e| e.get("type").and_then(Value::as_str) == Some("model_change"))
                .and_then(|e| e.get("model").and_then(Value::as_str))
                .and_then(selector_ref)
        });
    let agent = match &header.parent_session {
        Some(_) => AgentInfo {
            kind: "sub".into(),
            name: session
                .path
                .as_deref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        },
        None => AgentInfo {
            kind: "main".into(),
            name: "main".into(),
        },
    };
    let mut w = TraceSynth {
        out: Vec::new(),
        turn: None,
        turns_since_agent_end: 0,
        last_stop: None,
        last_model: model.clone(),
        last_input: None,
        last_deliverable: None,
        errors: VecDeque::with_capacity(ERROR_CLUSTER_WINDOW),
        reasons: Vec::new(),
    };
    w.out.push(TraceRecord::SessionStart {
        seq: 0,
        t: start_t,
        session_id: header.id.clone(),
        cwd: header.cwd.clone(),
        branch_hint: basename(repo.as_deref().unwrap_or(&header.cwd)),
        repo,
        model,
        mode: "import".into(),
        agent: Some(agent),
        omp_version: session
            .entries
            .iter()
            .find_map(|e| e.pointer("/runtime/version").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string(),
        loaded_entries: Vec::new(),
        hooks_loaded: Vec::new(),
        staging_dirs: Vec::new(),
        repo_remote,
        subpath,
        head_sha: None,
        visible_entries: Vec::new(),
    });

    let mut calls: HashMap<&str, &Value> = HashMap::new();
    let mut last_t = start_t;
    for e in &session.entries {
        let t = entry_ms(e).unwrap_or(last_t);
        last_t = t;
        let Some((role, m)) = message(e) else {
            continue;
        };
        match role {
            "user" => {
                // Subagent tasks are written by the parent agent, not typed by a human.
                if m.get("attribution").and_then(Value::as_str) == Some("agent") {
                    continue;
                }
                w.flush_turn();
                w.agent_end(t);
                let text = content_text(m.get("content"));
                // Frames the harness injects as user-role messages (task notifications, system
                // notices/reminders, hook messages) are not typed by a human; the live
                // extension never sees them as `input`, so they must not become redirects.
                if is_injected_frame(&text) {
                    continue;
                }
                let idle = w.last_stop != Some("aborted");
                w.last_stop = None;
                let label = prelabel(&text, w.last_input.as_deref(), idle);
                let trimmed = text.trim_start();
                let command = trimmed.starts_with(['/', '!', '%'])
                    && !label.starts_with("mark:")
                    && !trimmed.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("/skill:"));
                if command {
                    continue;
                }
                let mut skills = Vec::new();
                let mut seen = HashSet::new();
                for name in skills_in_text(&text) {
                    push_unique(&mut skills, &mut seen, name);
                }
                w.out.push(TraceRecord::Input {
                    seq: w.seq(),
                    t,
                    text: head(&text, TRACE_INPUT_TEXT_CHARS),
                    source: String::new(),
                    idle,
                    after_deliverable: w
                        .last_deliverable
                        .as_deref()
                        .map(|d| head(d, TRACE_DELIVERABLE_CHARS)),
                    prelabel: label.to_string(),
                    skills_invoked: skills,
                });
                if label != PRELABEL_NONE {
                    w.reason(if label.starts_with("mark:") {
                        label.to_string()
                    } else {
                        format!("redirect:{label}")
                    });
                }
                w.last_input = Some(text);
            }
            "assistant" => {
                w.flush_turn();
                for c in tool_calls(m) {
                    if let Some(id) = c.get("id").and_then(Value::as_str) {
                        calls.insert(id, c);
                    }
                }
                w.last_stop = m.get("stopReason").and_then(Value::as_str);
                w.turn = Some((m, t));
            }
            "toolResult" => {
                let call_id = m.get("toolCallId").and_then(Value::as_str).unwrap_or_default();
                let call = calls.get(call_id).copied();
                let tool = m
                    .get("toolName")
                    .and_then(Value::as_str)
                    .or_else(|| call.and_then(|c| c.get("name")).and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string();
                let input = call.and_then(|c| c.get("arguments")).unwrap_or(&Value::Null);
                let raw_path = input
                    .get("path")
                    .or_else(|| input.get("file_path"))
                    .and_then(Value::as_str);
                let is_error = m.get("isError").and_then(Value::as_bool).unwrap_or(false);
                w.out.push(TraceRecord::ToolResult {
                    seq: w.seq(),
                    t,
                    skill_read: (tool == "read")
                        .then(|| raw_path.and_then(skill_from_read_path))
                        .flatten(),
                    tool,
                    call_id: call_id.to_string(),
                    input: truncate_tool_input(input, TRACE_TOOL_INPUT_BYTES),
                    args_hash: args_hash(input),
                    result_bytes: content_bytes(m.get("content")),
                    is_error,
                    error_class: is_error.then(|| "error".to_string()),
                    content_head: head(&content_text(m.get("content")), TRACE_CONTENT_HEAD_CHARS),
                    path: raw_path.map(|p| strip_selector(p).to_string()),
                });
                if w.errors.len() == ERROR_CLUSTER_WINDOW {
                    w.errors.pop_front();
                }
                w.errors.push_back(is_error);
                if w.errors.iter().filter(|e| **e).count() >= ERROR_CLUSTER_MIN {
                    w.reason("tool_error_cluster".into());
                }
                if let Some((_, turn_t)) = &mut w.turn {
                    *turn_t = (*turn_t).max(t);
                }
            }
            _ => {}
        }
    }
    w.flush_turn();
    w.agent_end(last_t);
    let reasons = std::mem::take(&mut w.reasons);
    w.out.push(TraceRecord::SessionShutdown {
        seq: w.seq(),
        t: last_t,
        flagged: !reasons.is_empty(),
        reasons,
        files_changed: Vec::new(),
    });
    w.out
}

// ---- outcomes from receipts (deterministic, no model) ----

/// Bash commands that run a test suite; capture 1 is the runner as written.
pub const TEST_COMMAND_PATTERN: &str = r"(?m)(?:^|[;&|(]|\bthen\b|\bdo\b)\s*(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*(cargo\s+test|python3?\s+-m\s+pytest|pytest|bun\s+test|npm\s+(?:run\s+)?test|go\s+test)\b";

static TEST_COMMAND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(TEST_COMMAND_PATTERN).expect("static pattern"));
static CARGO_RESULT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed").expect("static pattern")
});
static COUNT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(\d+) (passed|failed|pass|fail|errors?)\b").expect("static pattern")
});
static GO_OK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^ok\s+\S+").expect("static pattern"));
static FAIL_MARKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^(?:FAIL\b|--- FAIL\b)|error: could not compile|error\[E\d{4}\]|^FAILED\b")
        .expect("static pattern")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestVerdict {
    Pass,
    Fail,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestSummary {
    pub passed: u32,
    pub failed: u32,
    pub verdict: TestVerdict,
}

/// Runner of a test command, normalized to `cargo test|pytest|bun test|npm test|go test`.
pub fn test_runner(command: &str) -> Option<&'static str> {
    let raw = TEST_COMMAND_RE.captures(command)?.get(1)?.as_str();
    let first = raw.split_whitespace().next().unwrap_or_default();
    Some(match first {
        "cargo" => "cargo test",
        "bun" => "bun test",
        "npm" => "npm test",
        "go" => "go test",
        _ => "pytest",
    })
}

/// Pass/fail from test output. cargo `test result:` lines win; otherwise `N passed|failed|
/// pass|fail|error(s)` counts (pytest, jest, bun) plus go `ok`/`FAIL` lines. Failure
/// markers or failed > 0 ⇒ Fail; passed > 0 ⇒ Pass; else Fail if the tool reported an error,
/// otherwise Unknown.
pub fn parse_test_output(text: &str, is_error: bool) -> TestSummary {
    let (mut passed, mut failed, mut fail_marker) = (0u32, 0u32, FAIL_MARKER_RE.is_match(text));
    let cargo: Vec<_> = CARGO_RESULT_RE.captures_iter(text).collect();
    if !cargo.is_empty() {
        for c in cargo {
            passed += c[2].parse::<u32>().unwrap_or(0);
            failed += c[3].parse::<u32>().unwrap_or(0);
            fail_marker |= &c[1] == "FAILED";
        }
    } else {
        for c in COUNT_RE.captures_iter(text) {
            let n = c[1].parse::<u32>().unwrap_or(0);
            match &c[2] {
                "passed" | "pass" => passed += n,
                _ => failed += n,
            }
        }
        passed += GO_OK_RE.find_iter(text).count() as u32;
    }
    let verdict = if failed > 0 || fail_marker {
        TestVerdict::Fail
    } else if passed > 0 {
        TestVerdict::Pass
    } else if is_error {
        TestVerdict::Fail
    } else {
        TestVerdict::Unknown
    };
    TestSummary {
        passed,
        failed,
        verdict,
    }
}

/// Verdict words → result status.
pub fn status_of(v: &Value) -> Option<ResultStatus> {
    if let Some(b) = v.as_bool() {
        return Some(if b {
            ResultStatus::Kept
        } else {
            ResultStatus::Killed
        });
    }
    let s = v.as_str()?.trim().to_ascii_lowercase();
    Some(match s.as_str() {
        "pass" | "passed" | "ok" | "success" | "succeeded" | "green" | "kept" | "keep"
        | "accepted" => ResultStatus::Kept,
        "fail" | "failed" | "failure" | "error" | "red" | "killed" | "kill" | "rejected" => {
            ResultStatus::Killed
        }
        "shipped" | "merged" | "deployed" => ResultStatus::Shipped,
        "reverted" | "rolled_back" | "rollback" => ResultStatus::Reverted,
        "inconclusive" | "unknown" | "pending" | "skipped" => ResultStatus::Inconclusive,
        _ => return None,
    })
}

fn verdict_word(s: ResultStatus) -> &'static str {
    match s {
        ResultStatus::Kept => "pass",
        ResultStatus::Killed => "fail",
        ResultStatus::Shipped => "shipped",
        ResultStatus::Reverted => "reverted",
        ResultStatus::Inconclusive => "inconclusive",
    }
}

/// JSON objects in tool output: the whole text, else the `{`…`}` span, else each line.
fn json_objects(text: &str) -> Vec<serde_json::Map<String, Value>> {
    let t = text.trim();
    let whole = serde_json::from_str::<Value>(t).ok().or_else(|| {
        let (s, e) = (t.find('{')?, t.rfind('}')?);
        (s < e)
            .then(|| serde_json::from_str::<Value>(&t[s..=e]).ok())
            .flatten()
    });
    if let Some(Value::Object(o)) = whole {
        return vec![o];
    }
    t.lines()
        .filter_map(|l| match serde_json::from_str::<Value>(l.trim()) {
            Ok(Value::Object(o)) => Some(o),
            _ => None,
        })
        .collect()
}

/// A receipt: has `receipt` or `verdict`, or a `status` that is a recognised verdict word.
fn is_receipt(o: &serde_json::Map<String, Value>) -> bool {
    o.contains_key("receipt")
        || o.contains_key("verdict")
        || o.get("status")
            .and_then(status_of)
            .is_some_and(|s| s != ResultStatus::Inconclusive)
}

struct Ctx<'a> {
    session: &'a OmpSession,
    family: Family,
    repo: String,
    branch: String,
    machine: String,
    omp_version: Option<String>,
}

impl Ctx<'_> {
    fn outcome(&self, seq: u32, ts: UnixMs, tag: &str, n: NewOutcome) -> Outcome {
        let id = format!(
            "imp-{}",
            &sha256_hex(format!("{}:{seq}:{tag}", self.session.header.id).as_bytes())[..24]
        );
        new_outcome_at(n, id, ts, self.machine.clone())
    }

    fn base(&self, kind: OutcomeKind, domain: String, claim: String) -> NewOutcome {
        let mut tool_versions = BTreeMap::new();
        if let Some(v) = &self.omp_version {
            tool_versions.insert("omp".to_string(), v.clone());
        }
        NewOutcome {
            session_id: self.session.header.id.clone(),
            model_family: self.family.clone(),
            repo: self.repo.clone(),
            branch: self.branch.clone(),
            kind,
            subject: Subject {
                domain,
                entities: vec![],
            },
            claim,
            params: BTreeMap::new(),
            facets: vec![],
            evidence: vec![],
            result: OutcomeResult {
                status: ResultStatus::Inconclusive,
                metrics: BTreeMap::new(),
                kill_criterion: None,
            },
            links: vec![],
            provenance: Provenance {
                tool_versions,
                ..Default::default()
            },
        }
    }

    fn repo_domain(&self) -> String {
        let b: String = self
            .branch
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        format!("repo.{}", if b.is_empty() { "unknown" } else { &b })
    }
}

fn parse_kind(v: Option<&Value>) -> Option<OutcomeKind> {
    serde_json::from_value(v?.clone()).ok()
}

/// Outcomes recoverable without a model: tool results carrying a JSON receipt
/// (`receipt`/`verdict`/`status`), and bash test runs (`cargo test|pytest|bun test|npm test|
/// go test`) with pass/fail parsed from the output. Ids are derived from session id, entry
/// index and kind, so re-imports are idempotent. `record` is the session's
/// [`session_record`] (family, repo and branch of the outcomes).
pub fn extract_outcomes(session: &OmpSession, record: &SessionRecord) -> Vec<Outcome> {
    let ctx = Ctx {
        session,
        family: record
            .family
            .clone()
            .unwrap_or_else(|| Family("unknown/unknown".into())),
        repo: record.repo.clone().unwrap_or_else(|| record.cwd.clone()),
        branch: record.branch.clone(),
        machine: machine_name(),
        omp_version: session.entries.iter().find_map(|e| {
            e.pointer("/runtime/version")
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
    };
    let mut calls: HashMap<String, (String, Value)> = HashMap::new();
    let mut out = Vec::new();
    for (seq, e) in session.entries.iter().enumerate() {
        let seq = seq as u32;
        let Some((role, m)) = message(e) else {
            continue;
        };
        if role == "assistant" {
            for c in tool_calls(m) {
                if let Some(id) = c.get("id").and_then(Value::as_str) {
                    let name = c
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    calls.insert(
                        id.to_string(),
                        (name, c.get("arguments").cloned().unwrap_or(Value::Null)),
                    );
                }
            }
            continue;
        }
        if role != "toolResult" {
            continue;
        }
        let ts = entry_ms(e).unwrap_or(0);
        let text = content_text(m.get("content"));
        let is_error = m.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let call = m
            .get("toolCallId")
            .and_then(Value::as_str)
            .and_then(|id| calls.get(id));
        let tool = m
            .get("toolName")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| call.map(|c| c.0.clone()))
            .unwrap_or_default();
        let command = call
            .and_then(|c| c.1.get("command"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let reference = format!("{}:{seq}", session.header.id);

        for (i, o) in json_objects(&text)
            .into_iter()
            .filter(is_receipt)
            .enumerate()
        {
            let status = o
                .get("verdict")
                .and_then(status_of)
                .or_else(|| o.get("status").and_then(status_of))
                .unwrap_or(ResultStatus::Inconclusive);
            let domain = o
                .get("domain")
                .or_else(|| o.get("subject").and_then(|s| s.get("domain")))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| ctx.repo_domain());
            let claim = ["claim", "summary", "message"]
                .iter()
                .find_map(|k| o.get(*k).and_then(Value::as_str))
                .map(str::to_string)
                .unwrap_or_else(|| format!("{tool} receipt: {}", verdict_word(status)));
            let mut n = ctx.base(
                parse_kind(o.get("kind")).unwrap_or(OutcomeKind::Artifact),
                domain,
                claim,
            );
            n.params = match o.get("params") {
                Some(Value::Object(p)) => p.clone().into_iter().collect(),
                _ => {
                    let mut p = BTreeMap::new();
                    p.insert("tool".to_string(), json!(tool));
                    if !command.is_empty() {
                        p.insert("command".to_string(), json!(command));
                    }
                    if let Some(r) = o.get("receipt").filter(|r| r.is_string()) {
                        p.insert("receipt".to_string(), r.clone());
                    }
                    p
                }
            };
            let metrics_src = match o.get("metrics") {
                Some(Value::Object(mm)) => mm.clone(),
                _ => o.clone(),
            };
            n.result.metrics = metrics_src
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_f64()?)))
                .collect();
            n.result.status = status;
            n.result.kill_criterion = o
                .get("kill_criterion")
                .and_then(Value::as_str)
                .map(str::to_string);
            n.evidence.push(Evidence {
                kind: EvidenceKind::Receipt,
                reference: o
                    .get("receipt")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| reference.clone()),
                verdict: verdict_word(status).to_string(),
            });
            out.push(ctx.outcome(seq, ts, &format!("receipt{i}"), n));
        }

        if tool == "bash" {
            if let Some(runner) = test_runner(command) {
                let sum = parse_test_output(&text, is_error);
                let (status, verdict) = match sum.verdict {
                    TestVerdict::Pass => (ResultStatus::Kept, "pass"),
                    TestVerdict::Fail => (ResultStatus::Killed, "fail"),
                    TestVerdict::Unknown => (ResultStatus::Inconclusive, "unknown"),
                };
                let claim = format!(
                    "`{runner}` {verdict}: {} passed, {} failed",
                    sum.passed, sum.failed
                );
                let mut n = ctx.base(OutcomeKind::Change, ctx.repo_domain(), claim);
                n.subject.entities.push(EntityRef {
                    ty: "test_suite".into(),
                    id: runner.into(),
                });
                n.params.insert("runner".into(), json!(runner));
                n.params.insert("command".into(), json!(command));
                n.result.status = status;
                n.result.metrics.insert("passed".into(), sum.passed as f64);
                n.result.metrics.insert("failed".into(), sum.failed as f64);
                n.evidence.push(Evidence {
                    kind: EvidenceKind::Test,
                    reference: reference.clone(),
                    verdict: verdict.into(),
                });
                out.push(ctx.outcome(seq, ts, "test", n));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RedirectClass;

    fn session_jsonl(lines: &[Value]) -> String {
        lines
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn msg(id: &str, parent: Option<&str>, ts: &str, message: Value) -> Value {
        json!({"type": "message", "id": id, "parentId": parent, "timestamp": ts, "runtime": {"version": "18.12.6"}, "message": message})
    }

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": text}], "attribution": "user"})
    }

    fn assistant(text: &str, calls: Vec<Value>, stop: &str) -> Value {
        let mut content = vec![json!({"type": "thinking", "thinking": "hmm"})];
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
        content.extend(calls);
        json!({"role": "assistant", "content": content, "provider": "anthropic", "model": "claude-opus-5-5",
               "usage": {"input": 10, "output": 4, "cacheRead": 100, "cacheWrite": 1}, "stopReason": stop})
    }

    fn call(id: &str, name: &str, args: Value) -> Value {
        json!({"type": "toolCall", "id": id, "name": name, "arguments": args})
    }

    fn result(id: &str, name: &str, text: &str, is_error: bool) -> Value {
        json!({"role": "toolResult", "toolCallId": id, "toolName": name, "content": [{"type": "text", "text": text}], "isError": is_error})
    }

    /// Title slot first, a dead branch (b1) and an unknown entry type on the active path.
    fn fixture() -> String {
        session_jsonl(&[
            json!({"type": "title", "v": 1, "title": "x", "pad": "   "}),
            json!({"type": "session", "version": 3, "id": "SID", "timestamp": "2026-10-26T00:00:00.000Z", "cwd": "/nonexistent/proj"}),
            json!({"type": "model_change", "id": "m0", "parentId": null, "timestamp": "2026-10-26T00:00:00.100Z", "model": "anthropic/claude-opus-5-5"}),
            msg("u1", Some("m0"), "2026-10-26T00:00:01.000Z", user("Fix the order book spread /skill:quant-options")),
            msg("a1", Some("u1"), "2026-10-26T00:00:02.000Z", assistant("", vec![
                call("c1", "read", json!({"path": "skill://bellwether-dev:1-40"})),
                call("c2", "read", json!({"path": "/h/.omp/agent/skills/qo-proof/SKILL.md:raw"})),
                call("c3", "edit", json!({"input": "[src/book.rs#1A2B]\nPUT 1.=1:\n+x\n[src/lib.rs#FFFF]\nCUT 2.=3"})),
                call("c4", "bash", json!({"command": "cd crates && cargo test -p core 2>&1 | tail"})),
                call("c5", "grep", json!({"path": "src", "pattern": "x"})),
            ], "toolUse")),
            msg("r1", Some("a1"), "2026-10-26T00:00:03.000Z", result("c1", "read", "skill text", false)),
            msg("r2", Some("r1"), "2026-10-26T00:00:03.100Z", result("c2", "read", "skill text", false)),
            msg("r3", Some("r2"), "2026-10-26T00:00:03.200Z", result("c3", "edit", "applied", false)),
            msg("r4", Some("r3"), "2026-10-26T00:00:03.300Z",
                result("c4", "bash", "running 3 tests\ntest result: FAILED. 2 passed; 1 failed; 0 ignored\n\nWall time: 1s", false)),
            msg("r5", Some("r4"), "2026-10-26T00:00:03.400Z",
                result("c5", "grep", "{\"receipt\":\"rcpt-9\",\"verdict\":\"pass\",\"sharpe\":1.5,\"claim\":\"Parity holds\"}", false)),
            msg("a2", Some("r5"), "2026-10-26T00:00:04.000Z", assistant("I fixed the spread.", vec![], "stop")),
            // Dead branch: forked from a2, superseded by the branch below.
            msg("b1", Some("a2"), "2026-10-26T00:00:05.000Z", user("dead branch text")),
            json!({"type": "brand_new_kind", "id": "k1", "parentId": "a2", "timestamp": "2026-10-26T00:00:05.500Z"}),
            msg("u2", Some("k1"), "2026-10-26T00:00:06.000Z", user("No, the spread uses the wrong side")),
            msg("a3", Some("u2"), "2026-10-26T00:00:07.000Z", assistant("Partial", vec![call("c6", "write", json!({"path": "notes/plan.md"}))], "aborted")),
            msg("r6", Some("a3"), "2026-10-26T00:00:07.100Z", result("c6", "write", "aborted", true)),
            msg("u3", Some("r6"), "2026-10-26T00:00:08.000Z", user("wait, keep the old file")),
            "garbage line".into(),
        ])
    }

    #[test]
    fn locates_header_and_active_branch() {
        let s = parse_session(&fixture(), Some(Path::new("/x/2026_SID.jsonl"))).unwrap();
        assert_eq!(s.header.id, "SID");
        assert_eq!(s.header.version, 3);
        let ids: Vec<&str> = s.entries.iter().filter_map(|e| e["id"].as_str()).collect();
        assert!(!ids.contains(&"b1"), "dead branch dropped");
        assert_eq!(ids.first(), Some(&"m0"));
        assert_eq!(ids.last(), Some(&"u3"));
        assert!(ids.contains(&"k1"), "unknown types stay on the path");
        assert_eq!(s.entries.len(), 14);
    }

    #[test]
    fn record_from_active_branch() {
        let s = parse_session(&fixture(), None).unwrap();
        let r = session_record(&s);
        assert_eq!(
            (r.id.as_str(), r.source.as_str(), r.branch.as_str()),
            ("SID", "import", "proj")
        );
        assert_eq!(r.repo, None);
        assert_eq!(r.started_at, iso_ms("2026-10-26T00:00:00.000Z").unwrap());
        assert_eq!(r.ended_at, iso_ms("2026-10-26T00:00:08.000Z"));
        assert_eq!(r.family.as_ref().unwrap().0, "anthropic/claude-opus");
        assert_eq!(r.turns, 3);
        assert_eq!((r.tokens_in, r.tokens_out), (333, 12));
        assert_eq!((r.tool_calls, r.tool_errors), (6, 1));
        assert_eq!(
            r.skill_reads,
            vec!["quant-options", "bellwether-dev", "qo-proof"]
        );
        assert_eq!(
            r.files_touched,
            vec![
                "/h/.omp/agent/skills/qo-proof/SKILL.md",
                "src/book.rs",
                "src/lib.rs",
                "notes/plan.md"
            ],
            "read/edit/write only; grep's search root and skill:// URLs excluded"
        );
        assert_eq!(r.exts, vec!["md", "rs"]);
        assert!(r.stems.contains(&"spread".to_string()) && r.stems.contains(&"book".to_string()));
        let labels: Vec<(u32, &str)> = r
            .redirects
            .iter()
            .map(|x| (x.seq, x.prelabel.as_str()))
            .collect();
        // Seqs address the synthesized trace: u2 = negation (record 10); u3 follows an
        // aborted assistant = interrupt (record 14).
        assert_eq!(labels, vec![(10, "negation"), (14, "interrupt")]);
        assert_eq!(r.source_path, None);
        assert_eq!(r.redirects[0].deliverable_head, "I fixed the spread.");
        assert_eq!(r.redirects[1].deliverable_head, "Partial");
        assert_eq!(r.takeovers, 1);
        assert_eq!(r.error_clusters, 0);
        assert!(r.flagged);
    }

    #[test]
    fn trace_synthesized_like_the_extension() {
        let s = parse_session(&fixture(), Some(Path::new("/s/2026_SID.jsonl"))).unwrap();
        let recs = trace_records_of(&s);
        let evs: Vec<&str> = recs.iter().map(TraceRecord::ev).collect();
        assert_eq!(
            evs,
            vec![
                "session_start", "input", "tool_result", "tool_result", "tool_result", "tool_result",
                "tool_result", "turn_end", "turn_end", "agent_end", "input", "tool_result", "turn_end",
                "agent_end", "input", "session_shutdown",
            ]
        );
        assert!(recs.iter().enumerate().all(|(i, r)| r.seq() == i as u32), "seq = record index");
        match &recs[0] {
            TraceRecord::SessionStart { session_id, cwd, repo, branch_hint, model, mode, agent, omp_version, loaded_entries, .. } => {
                assert_eq!((session_id.as_str(), cwd.as_str(), repo, branch_hint.as_str()), ("SID", "/nonexistent/proj", &None, "proj"));
                assert_eq!(model.as_ref().map(|m| m.id.as_str()), Some("claude-opus-5-5"));
                assert_eq!(mode, "import");
                assert_eq!(agent.as_ref().map(|a| a.kind.as_str()), Some("main"));
                assert_eq!(omp_version, "18.12.6");
                assert!(loaded_entries.is_empty());
            }
            other => panic!("{other:?}"),
        }
        match &recs[1] {
            TraceRecord::Input { text, idle, after_deliverable, prelabel, skills_invoked, .. } => {
                assert!(text.starts_with("Fix the order book"));
                assert!(*idle);
                assert_eq!(after_deliverable, &None);
                assert_eq!(prelabel, "none");
                assert_eq!(skills_invoked, &vec!["quant-options".to_string()]);
            }
            other => panic!("{other:?}"),
        }
        match &recs[2] {
            TraceRecord::ToolResult { tool, call_id, input, args_hash: h, result_bytes, is_error, error_class, content_head, skill_read, path, t, .. } => {
                assert_eq!((tool.as_str(), call_id.as_str()), ("read", "c1"));
                assert_eq!(input, &json!({"path": "skill://bellwether-dev:1-40"}));
                assert_eq!(h, &args_hash(&json!({"path": "skill://bellwether-dev:1-40"})));
                assert_eq!((*result_bytes, *is_error, error_class), (10, false, &None));
                assert_eq!(content_head, "skill text");
                assert_eq!(skill_read.as_deref(), Some("bellwether-dev"));
                assert_eq!(path.as_deref(), Some("skill://bellwether-dev"));
                assert_eq!(*t, iso_ms("2026-10-26T00:00:03.000Z").unwrap());
            }
            other => panic!("{other:?}"),
        }
        match &recs[7] {
            TraceRecord::TurnEnd { model, usage, stop_reason, text_head, t, .. } => {
                assert_eq!(model.as_ref().map(|m| m.provider.as_str()), Some("anthropic"));
                assert_eq!(usage.as_ref().map(|u| (u.input, u.output, u.cache_read)), Some((10, 4, 100)));
                assert_eq!(stop_reason.as_deref(), Some("toolUse"));
                assert_eq!(text_head, "");
                assert_eq!(*t, iso_ms("2026-10-26T00:00:03.400Z").unwrap(), "turn ends after its tool results");
            }
            other => panic!("{other:?}"),
        }
        match &recs[10] {
            TraceRecord::Input { prelabel, after_deliverable, .. } => {
                assert_eq!(prelabel, "negation");
                assert_eq!(after_deliverable.as_deref(), Some("I fixed the spread."));
            }
            other => panic!("{other:?}"),
        }
        match &recs[11] {
            TraceRecord::ToolResult { is_error, error_class, path, .. } => {
                assert!(*is_error);
                assert_eq!(error_class.as_deref(), Some("error"));
                assert_eq!(path.as_deref(), Some("notes/plan.md"));
            }
            other => panic!("{other:?}"),
        }
        match &recs[14] {
            TraceRecord::Input { prelabel, idle, .. } => assert_eq!((prelabel.as_str(), *idle), ("interrupt", false)),
            other => panic!("{other:?}"),
        }
        match recs.last().unwrap() {
            TraceRecord::SessionShutdown { reasons, flagged, t, .. } => {
                assert_eq!(reasons, &vec!["redirect:negation".to_string(), "redirect:interrupt".to_string()]);
                assert!(*flagged);
                assert_eq!(*t, iso_ms("2026-10-26T00:00:08.000Z").unwrap());
            }
            other => panic!("{other:?}"),
        }
        // The fold of the synthesized trace is the import row's signal source.
        let folded = fold_trace(&recs, "SID");
        assert_eq!(folded.source, "import");
        let r = session_record(&s);
        assert_eq!((folded.redirects.clone(), folded.flagged), (r.redirects.clone(), r.flagged));
        assert_eq!(r.source_path.as_deref(), Some("/s/2026_SID.jsonl"));
        // Round-trips through the trace file format.
        let text: String = recs.iter().map(|r| format!("{}\n", serde_json::to_string(r).unwrap())).collect();
        assert_eq!(crate::trace::parse_trace(&text), recs);
    }

    #[test]
    fn synthesized_trace_flags_error_clusters_model_changes_and_skips_commands() {
        let mut lines = vec![
            json!({"type": "session", "version": 3, "id": "E", "timestamp": "2026-10-26T00:00:00Z", "cwd": "/p"}),
            msg("u0", None, "2026-10-26T00:00:01Z", user("/compact keep it short")),
            msg("u1", Some("u0"), "2026-10-26T00:00:02Z", user("Run the migration")),
        ];
        let mut parent = "u1".to_string();
        for i in 0..4 {
            let a = format!("a{i}");
            let mut m = assistant("", vec![call(&format!("c{i}"), "bash", json!({"command": format!("migrate {i}")}))], "toolUse");
            if i == 3 {
                m["provider"] = json!("openai");
                m["model"] = json!("gpt-6-luna");
            }
            lines.push(msg(&a, Some(&parent), "2026-10-26T00:00:03Z", m));
            let r = format!("r{i}");
            lines.push(msg(&r, Some(&a), "2026-10-26T00:00:04Z", result(&format!("c{i}"), "bash", "boom", i != 1)));
            parent = r;
        }
        let s = parse_session(&session_jsonl(&lines), None).unwrap();
        let recs = trace_records_of(&s);
        let inputs = recs.iter().filter(|r| r.ev() == "input").count();
        assert_eq!(inputs, 1, "slash commands are not inputs");
        let changes: Vec<&TraceRecord> = recs.iter().filter(|r| r.ev() == "model_change").collect();
        assert_eq!(changes.len(), 1);
        match changes[0] {
            TraceRecord::ModelChange { from, to, reason, .. } => {
                assert_eq!(from.as_ref().unwrap().id, "claude-opus-5-5");
                assert_eq!(to.as_ref().unwrap().id, "gpt-6-luna");
                assert_eq!(reason.as_deref(), Some("turn"));
            }
            _ => unreachable!(),
        }
        match recs.last().unwrap() {
            TraceRecord::SessionShutdown { reasons, flagged, .. } => {
                assert_eq!(reasons, &vec!["tool_error_cluster".to_string()]);
                assert!(*flagged);
            }
            other => panic!("{other:?}"),
        }
        let r = session_record(&s);
        assert_eq!(r.error_clusters, 1);
        let tipping = recs.iter().filter(|r| r.ev() == "tool_result").nth(3).unwrap().seq();
        assert_eq!(r.error_cluster_seqs, vec![tipping], "cluster seq addresses the trace");
        assert!(r.flagged);
    }

    #[test]
    fn tool_input_bounds_and_canonical_hash() {
        let v = json!({"b": [1, {"y": 2, "x": "é"}], "a": null});
        assert_eq!(canonical_json(&v), r#"{"a":null,"b":[1,{"x":"é","y":2}]}"#);
        assert_eq!(args_hash(&v), args_hash(&json!({"a": null, "b": [1, {"x": "é", "y": 2}]})));
        let small = json!({"path": "src/a.rs"});
        assert_eq!(truncate_tool_input(&small, TRACE_TOOL_INPUT_BYTES), small);
        let big = json!({"path": "src/a.rs", "input": "x".repeat(20_000)});
        let t = truncate_tool_input(&big, TRACE_TOOL_INPUT_BYTES);
        assert!(t.to_string().len() <= TRACE_TOOL_INPUT_BYTES);
        assert_eq!(t["path"], "src/a.rs");
        assert_eq!(t["input"].as_str().unwrap(), format!("{}…[+{}]", "x".repeat(2048), 20_000 - 2048));
        let many = json!({"paths": (0..5000).map(|i| format!("f{i}")).collect::<Vec<_>>()});
        let t = truncate_tool_input(&many, TRACE_TOOL_INPUT_BYTES);
        assert!(t.to_string().len() <= TRACE_TOOL_INPUT_BYTES);
        assert_eq!(t["paths"].as_array().unwrap().last().unwrap(), &json!(format!("…[+{}]", 5000 - 32)));
    }

    #[test]
    fn linear_v1_and_headerless_files() {
        let v1 = session_jsonl(&[
            json!({"type": "session", "version": 1, "id": "V1", "timestamp": "2025-01-01T00:00:00Z", "cwd": "/p"}),
            json!({"type": "message", "timestamp": "2025-01-01T00:00:01Z", "message": user("/rematch mark correction")}),
            json!({"type": "message", "timestamp": "2025-01-01T00:00:02Z", "message": {"role": "assistant", "content": "plain string", "provider": "openai", "model": "gpt-6-luna"}}),
            json!({"type": "message", "timestamp": "2025-01-01T00:00:03Z", "message": {"role": "user", "content": "plain user string"}}),
        ]);
        let r = session_record(&parse_session(&v1, None).unwrap());
        assert_eq!(
            (r.turns, r.family.unwrap().0.as_str()),
            (1, "openai/gpt-luna")
        );
        assert_eq!(r.redirects.len(), 1);
        assert_eq!(r.redirects[0].class, Some(RedirectClass::Correction));

        let headerless = json!({"type": "message", "message": user("hi")}).to_string();
        let s = parse_session(
            &headerless,
            Some(Path::new("/s/2026-10-26T00-00-00-000Z_abc-123.jsonl")),
        )
        .unwrap();
        assert_eq!(s.header.id, "abc-123");
        assert!(parse_session("not json\n\n", None).is_err());
    }

    #[test]
    fn subagent_tasks_are_not_redirects_and_model_falls_back() {
        let text = session_jsonl(&[
            json!({"type": "session", "version": 3, "id": "SUB", "timestamp": "2026-10-26T00:00:00Z", "cwd": "/p",
                   "parentSession": "/s/-p/2026-10-26T00-00-00-000Z_PARENT.jsonl"}),
            json!({"type": "model_change", "id": "m", "parentId": null, "model": "github-copilot/gemini-4-argon"}),
            msg(
                "u",
                Some("m"),
                "2026-10-26T00:00:01Z",
                json!({"role": "user", "content": "No, revert it", "attribution": "agent"}),
            ),
        ]);
        let s = parse_session(&text, None).unwrap();
        assert_eq!(s.parent_id().as_deref(), Some("PARENT"));
        let r = session_record(&s);
        assert!(r.redirects.is_empty());
        assert_eq!(r.family.unwrap().0, "github-copilot/gemini-argon");
        assert!(r.stems.contains(&"revert".to_string()));
    }

    #[test]
    fn outcomes_from_receipts_and_tests_only() {
        let s = parse_session(&fixture(), None).unwrap();
        let outs = extract_outcomes(&s, &session_record(&s));
        assert_eq!(outs.len(), 2);
        let test = outs
            .iter()
            .find(|o| o.evidence[0].kind == EvidenceKind::Test)
            .unwrap();
        assert_eq!(test.result.status, ResultStatus::Killed);
        assert_eq!(test.claim, "`cargo test` fail: 2 passed, 1 failed");
        assert_eq!(test.params["runner"], json!("cargo test"));
        assert_eq!(test.subject.domain, "repo.proj");
        assert_eq!(test.evidence[0].reference, "SID:6");
        assert_eq!(
            test.provenance.tool_versions.get("omp").map(String::as_str),
            Some("18.12.6")
        );
        assert!(test.provenance.self_reported.is_empty());
        let receipt = outs
            .iter()
            .find(|o| o.evidence[0].kind == EvidenceKind::Receipt)
            .unwrap();
        assert_eq!(receipt.result.status, ResultStatus::Kept);
        assert_eq!(receipt.claim, "Parity holds");
        assert_eq!(receipt.evidence[0].reference, "rcpt-9");
        assert_eq!(receipt.result.metrics.get("sharpe"), Some(&1.5));
        assert_eq!(receipt.kind, OutcomeKind::Artifact);
        // Deterministic ids and signatures across re-imports.
        let again = extract_outcomes(&s, &session_record(&s));
        assert_eq!(
            outs.iter()
                .map(|o| (&o.id, &o.signature))
                .collect::<Vec<_>>(),
            again
                .iter()
                .map(|o| (&o.id, &o.signature))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn receipt_detection_rules() {
        assert!(is_receipt(&json_objects("{\"verdict\":\"weird\"}")[0]));
        assert!(is_receipt(
            &json_objects("noise\n{\"status\":\"passed\"}\nWall time: 1s")[0]
        ));
        assert!(!is_receipt(&json_objects("{\"status\":\"pending\"}")[0]));
        assert!(!is_receipt(&json_objects("{\"status\":200}")[0]));
        assert!(json_objects("[1,2]").is_empty());
        assert_eq!(json_objects("{\"a\":1}\n{\"b\":2}").len(), 2, "JSON lines");
    }

    #[test]
    fn test_commands_and_output() {
        assert_eq!(test_runner("cargo test -p x"), Some("cargo test"));
        assert_eq!(
            test_runner("cd a && RUST_LOG=1 cargo  test"),
            Some("cargo test")
        );
        assert_eq!(test_runner("python3 -m pytest -q"), Some("pytest"));
        assert_eq!(test_runner("npm run test"), Some("npm test"));
        assert_eq!(test_runner("go test ./..."), Some("go test"));
        assert_eq!(test_runner("bun test"), Some("bun test"));
        assert_eq!(test_runner("grep -c 'cargo test' notes.md"), None);
        assert_eq!(test_runner("cargo testify"), None);

        let p = |t: &str, e: bool| parse_test_output(t, e);
        assert_eq!(
            p(
                "test result: ok. 5 passed; 0 failed;\ntest result: ok. 2 passed; 0 failed;",
                false
            ),
            TestSummary {
                passed: 7,
                failed: 0,
                verdict: TestVerdict::Pass
            }
        );
        assert_eq!(
            p("test result: ok. 0 passed; 0 failed;", false).verdict,
            TestVerdict::Unknown
        );
        assert_eq!(
            p(
                "error[E0277]: trait bound\nerror: could not compile `x`",
                false
            )
            .verdict,
            TestVerdict::Fail
        );
        assert_eq!(
            p("===== 2 failed, 10 passed in 0.3s =====", false),
            TestSummary {
                passed: 10,
                failed: 2,
                verdict: TestVerdict::Fail
            }
        );
        assert_eq!(
            p(" 12 pass\n 0 fail\n", false),
            TestSummary {
                passed: 12,
                failed: 0,
                verdict: TestVerdict::Pass
            }
        );
        assert_eq!(
            p("ok  \tpkg/a\t0.1s\nok  \tpkg/b\t0.2s", false).verdict,
            TestVerdict::Pass
        );
        assert_eq!(
            p("--- FAIL: TestX\nFAIL\tpkg/a", true).verdict,
            TestVerdict::Fail
        );
        assert_eq!(p("Tests: 1 failed, 4 passed, 5 total", false).failed, 1);
        assert_eq!(p("no recognizable output", true).verdict, TestVerdict::Fail);
        assert_eq!(
            p("no recognizable output", false).verdict,
            TestVerdict::Unknown
        );
    }

    #[test]
    fn scan_marks_subagent_parents() {
        let dir = tempfile::tempdir().unwrap();
        let slug = dir.path().join("-repos-x");
        std::fs::create_dir_all(slug.join("2026-10-26T01-00-00-000Z_P1")).unwrap();
        std::fs::write(slug.join("2026-10-26T01-00-00-000Z_P1.jsonl"), "{}").unwrap();
        std::fs::write(slug.join("2026-10-26T01-00-00-000Z_P1/Worker.jsonl"), "{}").unwrap();
        std::fs::write(slug.join("2026-10-26T01-00-00-000Z_P1/0.read.log"), "").unwrap();
        std::fs::create_dir_all(slug.join("orphan")).unwrap();
        std::fs::write(slug.join("orphan/x.jsonl"), "{}").unwrap();
        let files = scan_sessions_dir(dir.path()).unwrap();
        let got: Vec<(String, Option<String>)> = files
            .iter()
            .map(|f| {
                (
                    f.path
                        .strip_prefix(&slug)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    f.parent_id.clone(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("2026-10-26T01-00-00-000Z_P1.jsonl".to_string(), None),
                (
                    "2026-10-26T01-00-00-000Z_P1/Worker.jsonl".to_string(),
                    Some("P1".to_string())
                ),
                ("orphan/x.jsonl".to_string(), None),
            ]
        );
    }

    #[test]
    fn repo_detection_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("r/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("r/a/b")).unwrap();
        assert_eq!(
            repo_of(&dir.path().join("r/a/b")),
            Some(dir.path().join("r"))
        );
        assert_eq!(repo_of(Path::new("")), None);
    }
}

#[cfg(test)]
mod injected_frames {
    use super::is_injected_frame;

    #[test]
    fn harness_frames_are_not_user_turns() {
        assert!(is_injected_frame("<task-notification>\n<task-id>x</task-id>"));
        assert!(is_injected_frame("  <system-notice>User interjection"));
        assert!(is_injected_frame("<system-reminder>\n3 todo items"));
        assert!(!is_injected_frame("[Image #1] look at this"));
        assert!(!is_injected_frame("No, revert that"));
        assert!(!is_injected_frame("<p>html pasted by me</p>"));
    }
}
