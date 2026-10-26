//! Claude Code history importer: `~/.claude/projects/<slug>/<uuid>.jsonl` (main sessions) and
//! `<slug>/<uuid>/subagents/agent-<id>.jsonl` (subagents) → the DESIGN §3 trace the Claude Code
//! adapter would have written ([`trace_records_of`]) and the ledger row folded from it
//! ([`session_record`], source `"import"`). Session ids are prefixed: `cc:<uuid>` for a main
//! session, `cc:<uuid>:agent-<id>` for a subagent.
//!
//! Format facts relied on (read from Claude Code 2.1.x transcripts): one JSON object per line
//! with `type`; `user`/`assistant` entries carry `message` (Anthropic Messages shape),
//! `timestamp` (RFC 3339), `cwd`, `sessionId`, `version`, `isSidechain`; one API response is
//! written as several `assistant` entries sharing `message.id` (one per content block, usage
//! repeated); tool results are `user` entries whose content holds `tool_result` blocks
//! (`tool_use_id`, `content`, `is_error`); typed prompts are `user` entries with string or
//! `text`-block content and no `isMeta` (newer versions add `origin.kind: "human"` and
//! `promptSource`: `typed`/`queued`/`system`); injected frames (`isMeta`, task notifications,
//! peer messages, compaction summaries) are not prompts; `[Request interrupted by user…]` marks
//! an aborted run; a typed slash command is echoed as `<command-name>/x</command-name> …
//! <command-args>…</command-args>`. Every other `type` (attachments, snapshots, titles, …) is
//! ignored. Entries are taken in file order (compaction keeps the pre-compaction entries).
//!
//! The live adapter (`rematch trace-event --harness claude-code`) shares [`tool_result_record`],
//! [`tool_name`], [`model_ref`] and [`usage_of`], so a hook-written trace and a synthesized one
//! describe the same session the same way.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::features::{head, strip_selector};
use crate::import::{
    args_hash, content_text, is_injected_frame, repo_of, truncate_tool_input, TRACE_CONTENT_HEAD_CHARS,
    TRACE_DELIVERABLE_CHARS, TRACE_INPUT_TEXT_CHARS, TRACE_TOOL_INPUT_BYTES,
};
use crate::redirect::prelabel;
use crate::trace::{basename, flag_reasons, fold_trace, skill_from_read_path, skills_in_text, AgentInfo, TraceRecord, Usage};
use crate::types::{ModelRef, SessionRecord, UnixMs};

/// Session id prefix of Claude Code sessions in the ledger and `traces/`.
pub const ID_PREFIX: &str = "cc:";
/// Provider of every model Claude Code reports.
pub const PROVIDER: &str = "anthropic";
/// Prefix of the user entry Claude Code writes when the user aborts a run.
pub const INTERRUPT_MARKER: &str = "[Request interrupted by user";

/// `cc:<uuid>`.
pub fn session_id(uuid: &str) -> String {
    format!("{ID_PREFIX}{uuid}")
}

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn config_dir() -> Option<PathBuf> {
    let non_empty = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    non_empty("CLAUDE_CONFIG_DIR").or_else(|| non_empty("HOME").map(|h| h.join(".claude")))
}

/// `<config dir>/projects`.
pub fn default_sessions_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("projects"))
}

/// A parsed Claude Code transcript: the `user`/`assistant` entries of one agent, in file order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeSession {
    /// `cc:<uuid>` or `cc:<uuid>:agent-<id>`.
    pub id: String,
    /// Claude Code's `sessionId` (the parent's for a subagent).
    pub session_uuid: String,
    /// Set for subagent transcripts.
    pub agent_id: Option<String>,
    pub cwd: String,
    /// Claude Code version that wrote the first entry.
    pub version: String,
    pub path: Option<PathBuf>,
    pub entries: Vec<Value>,
}

impl ClaudeSession {
    pub fn is_subagent(&self) -> bool {
        self.agent_id.is_some()
    }

    /// `cc:<uuid>` of the main session a subagent ran under.
    pub fn parent_id(&self) -> Option<String> {
        self.is_subagent().then(|| session_id(&self.session_uuid))
    }
}

/// Transcript files under `dir` (recursively): `<slug>/<uuid>.jsonl` and
/// `<slug>/<uuid>/subagents/*.jsonl`, sorted so a main session precedes its subagents.
/// `tool-results/` and `memory/` directories are skipped.
pub fn scan_sessions_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let p = entry.path();
            if ft.is_dir() {
                if !matches!(entry.file_name().to_str(), Some("tool-results" | "memory")) {
                    walk(&p, out)?;
                }
            } else if ft.is_file() && p.extension().is_some_and(|e| e == "jsonl") {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    walk(dir, &mut paths)?;
    paths.sort_by_cached_key(|p| p.with_extension("").to_string_lossy().into_owned());
    Ok(paths)
}

/// The transcript of a prefixed id under a projects dir: `*/<uuid>.jsonl`, or
/// `*/<uuid>/subagents/agent-<id>.jsonl` for `cc:<uuid>:agent-<id>`.
pub fn find_session_file(dir: &Path, id: &str) -> Option<PathBuf> {
    let rest = id.strip_prefix(ID_PREFIX)?;
    let (uuid, agent) = match rest.split_once(":agent-") {
        Some((u, a)) => (u, Some(a)),
        None => (rest, None),
    };
    for slug in std::fs::read_dir(dir).ok()?.flatten() {
        let base = slug.path();
        let p = match agent {
            None => base.join(format!("{uuid}.jsonl")),
            Some(a) => base.join(uuid).join("subagents").join(format!("agent-{a}.jsonl")),
        };
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub fn read_session(path: &Path) -> Result<ClaudeSession> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read session {}", path.display()))?;
    parse_session(&text, Some(path))
}

/// Keeps what the trace needs of a `user`/`assistant` entry: no thinking blocks, no
/// `toolUseResult` (the tool-specific copy of the result), no attachments.
fn slim(e: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for k in ["type", "timestamp", "isMeta", "origin", "promptSource", "isCompactSummary"] {
        if let Some(v) = e.get(k) {
            out.insert(k.to_string(), v.clone());
        }
    }
    if let Some(m) = e.get("message").and_then(Value::as_object) {
        let mut msg = serde_json::Map::new();
        for k in ["role", "id", "model", "stop_reason", "usage"] {
            if let Some(v) = m.get(k) {
                msg.insert(k.to_string(), v.clone());
            }
        }
        if let Some(c) = m.get("content") {
            let content = match c {
                Value::Array(blocks) => Value::Array(
                    blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) != Some("thinking"))
                        .cloned()
                        .collect(),
                ),
                other => other.clone(),
            };
            msg.insert("content".into(), content);
        }
        out.insert("message".into(), Value::Object(msg));
    }
    Value::Object(out)
}

/// Parses transcript text. A file under `…/subagents/` (or whose first message entry is a
/// sidechain) is a subagent transcript; a main transcript drops inline sidechain entries.
/// Fails only when no message entry and no session id parse.
pub fn parse_session(text: &str, path: Option<&Path>) -> Result<ClaudeSession> {
    let in_subagents_dir =
        path.and_then(Path::parent).and_then(Path::file_name).is_some_and(|n| n == "subagents");
    let mut s = ClaudeSession { path: path.map(Path::to_path_buf), ..Default::default() };
    let mut sub: Option<bool> = in_subagents_dir.then_some(true);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if s.session_uuid.is_empty() {
            if let Some(id) = v.get("sessionId").and_then(Value::as_str) {
                s.session_uuid = id.to_string();
            }
        }
        if !matches!(v.get("type").and_then(Value::as_str), Some("user" | "assistant")) {
            continue;
        }
        let sidechain = v.get("isSidechain").and_then(Value::as_bool).unwrap_or(false);
        let is_sub = *sub.get_or_insert(sidechain);
        if !is_sub && sidechain {
            continue;
        }
        if s.cwd.is_empty() {
            if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                s.cwd = c.to_string();
            }
        }
        if s.version.is_empty() {
            if let Some(c) = v.get("version").and_then(Value::as_str) {
                s.version = c.to_string();
            }
        }
        if is_sub && s.agent_id.is_none() {
            s.agent_id = v.get("agentId").and_then(Value::as_str).map(str::to_string);
        }
        s.entries.push(slim(&v));
    }
    let stem = path.and_then(Path::file_stem).and_then(|n| n.to_str()).unwrap_or_default();
    if sub == Some(true) && s.agent_id.is_none() {
        s.agent_id = Some(stem.strip_prefix("agent-").unwrap_or(stem).to_string());
    }
    if s.session_uuid.is_empty() {
        // A main transcript is named after its session.
        if sub != Some(true) && !stem.is_empty() {
            s.session_uuid = stem.to_string();
        } else if in_subagents_dir {
            s.session_uuid = path
                .and_then(Path::parent)
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
        }
    }
    if s.session_uuid.is_empty() && s.entries.is_empty() {
        bail!("no Claude Code session entries");
    }
    s.id = match &s.agent_id {
        Some(a) => format!("{}:agent-{a}", session_id(&s.session_uuid)),
        None => session_id(&s.session_uuid),
    };
    Ok(s)
}

fn entry_ms(e: &Value) -> Option<UnixMs> {
    e.get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
}

// ---- shared with the live adapter ----

/// Tool name in the trace: Claude Code's name lowercased (`Read` → `read`, `Bash` → `bash`),
/// so file/skill derivations and hook rules written for omp's lowercase tools apply.
pub fn tool_name(raw: &str) -> String {
    raw.to_ascii_lowercase()
}

/// `{provider: "anthropic", id}`; `None` for Claude Code's `<synthetic>` placeholder.
pub fn model_ref(id: &str) -> Option<ModelRef> {
    let id = id.trim();
    (!id.is_empty() && !id.starts_with('<')).then(|| ModelRef { provider: PROVIDER.into(), id: id.to_string() })
}

/// Anthropic `usage` (`input_tokens`, `output_tokens`, `cache_read_input_tokens`,
/// `cache_creation_input_tokens`) as the trace's [`Usage`].
pub fn usage_of(u: &Value) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let (input, output, cache_read, cache_write) =
        (n("input_tokens"), n("output_tokens"), n("cache_read_input_tokens"), n("cache_creation_input_tokens"));
    Usage { input, output, cache_read, cache_write, total_tokens: input + output + cache_read + cache_write, cost: Value::Null }
}

/// Text of a tool result: a string; text blocks of a block array; for a hook's
/// `tool_response` object, `stdout`/`stderr`, else `content`/`file.content`, else the JSON.
pub fn result_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(_) => content_text(Some(v)),
        Value::Object(o) => {
            let streams: Vec<&str> =
                ["stdout", "stderr"].iter().filter_map(|k| o.get(*k).and_then(Value::as_str)).filter(|s| !s.is_empty()).collect();
            if o.contains_key("stdout") || o.contains_key("stderr") {
                return streams.join("\n");
            }
            if let Some(c) = o.get("content") {
                return result_text(c);
            }
            if let Some(c) = v.pointer("/file/content").and_then(Value::as_str) {
                return c.to_string();
            }
            v.to_string()
        }
        other => other.to_string(),
    }
}

/// Bytes of a tool result: text as UTF-8, image/document blocks by their base64 `data`.
fn result_bytes(v: &Value) -> u64 {
    match v {
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b.get("text").and_then(Value::as_str) {
                Some(t) => t.len() as u64,
                None => b.pointer("/source/data").and_then(Value::as_str).map_or(0, |d| d.len() as u64),
            })
            .sum(),
        other => result_text(other).len() as u64,
    }
}

/// Skill a call loads: the `Skill` tool's `skill` (`plugin:name` → `name`), or a `Read` of
/// `…/skills/<name>/SKILL.md`.
pub fn skill_of_call(tool: &str, input: &Value) -> Option<String> {
    match tool {
        "skill" => input
            .get("skill")
            .and_then(Value::as_str)
            .map(|s| s.rsplit(':').next().unwrap_or(s).trim().to_string())
            .filter(|s| !s.is_empty()),
        "read" => file_path_of(input).and_then(skill_from_read_path),
        _ => None,
    }
}

fn file_path_of(input: &Value) -> Option<&str> {
    ["file_path", "path", "notebook_path"].iter().find_map(|k| input.get(*k).and_then(Value::as_str))
}

/// One `tool_result` record. `raw_tool` is Claude Code's tool name; `content` the result
/// (transcript `content`, hook `tool_response`, or the hook `error` string).
pub fn tool_result_record(seq: u32, t: UnixMs, raw_tool: &str, call_id: &str, input: &Value, content: &Value, is_error: bool) -> TraceRecord {
    let tool = tool_name(raw_tool);
    TraceRecord::ToolResult {
        seq,
        t,
        skill_read: skill_of_call(&tool, input),
        call_id: call_id.to_string(),
        input: truncate_tool_input(input, TRACE_TOOL_INPUT_BYTES),
        args_hash: args_hash(input),
        result_bytes: result_bytes(content),
        is_error,
        error_class: is_error.then(|| "error".to_string()),
        content_head: head(&result_text(content), TRACE_CONTENT_HEAD_CHARS),
        path: file_path_of(input).map(|p| strip_selector(p).to_string()),
        tool,
    }
}

/// What a typed `user` text entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    /// A prompt the user typed (slash commands rendered as `/name args`).
    Prompt(String),
    /// The user aborted the running turn.
    Interrupt,
    /// Injected or local-only content.
    Ignored,
}

/// `<command-name>/x</command-name> … <command-args>a b</command-args>` → `/x a b`.
fn command_echo(text: &str) -> Option<String> {
    let between = |open: &str, close: &str| -> Option<String> {
        let i = text.find(open)? + open.len();
        let j = text[i..].find(close)? + i;
        Some(text[i..j].trim().to_string())
    };
    let name = between("<command-name>", "</command-name>")?;
    let args = between("<command-args>", "</command-args>").unwrap_or_default();
    let name = if name.starts_with('/') { name } else { format!("/{name}") };
    Some(if args.is_empty() { name } else { format!("{name} {args}") })
}

/// Classifies a typed-text `user` entry (no `tool_result` blocks).
pub fn prompt_kind(e: &Value) -> PromptKind {
    if e.get("isMeta").and_then(Value::as_bool) == Some(true)
        || e.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        || e.pointer("/origin/kind").and_then(Value::as_str).is_some_and(|k| k != "human")
    {
        return PromptKind::Ignored;
    }
    let text = content_text(e.pointer("/message/content"));
    let trimmed = text.trim_start();
    if trimmed.starts_with(INTERRUPT_MARKER) {
        return PromptKind::Interrupt;
    }
    if let Some(cmd) = command_echo(trimmed) {
        return PromptKind::Prompt(cmd);
    }
    if trimmed.is_empty()
        || is_injected_frame(trimmed)
        || ["<local-command-", "<bash-input", "<bash-stdout", "<bash-stderr", "<user-memory-input"].iter().any(|p| trimmed.starts_with(p))
    {
        return PromptKind::Ignored;
    }
    PromptKind::Prompt(text)
}

/// Slash/`!`/`%` lines other than `/rematch mark` and `/skill:` are not user turns (the omp
/// extension's rule, applied to Claude Code prompts as typed).
pub fn is_command(text: &str, label: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with(['/', '!', '%'])
        && !label.starts_with("mark:")
        && !trimmed.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("/skill:"))
}

/// The plugin's mark command, `/rematch-mark <class>` (namespaced `/rematch:rematch-mark`),
/// spelled as omp's `/rematch mark <class>`, so the shared mark pre-label applies.
pub fn canonical_prompt(text: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = text.trim_start();
    for cmd in ["/rematch:rematch-mark", "/rematch-mark"] {
        let rest = trimmed.get(cmd.len()..);
        if trimmed.get(..cmd.len()).is_some_and(|p| p.eq_ignore_ascii_case(cmd)) && rest.is_some_and(|r| r.is_empty() || r.starts_with(char::is_whitespace)) {
            return format!("/rematch mark{}", rest.unwrap_or_default()).into();
        }
    }
    text.into()
}

/// The `input` record for a prompt ([`canonical_prompt`]), or `None` when it is a command.
pub fn input_record(seq: u32, t: UnixMs, text: &str, idle: bool, prev: Option<&str>, deliverable: Option<&str>, source: &str) -> Option<TraceRecord> {
    let text = &*canonical_prompt(text);
    let label = prelabel(text, prev, idle);
    if is_command(text, label) {
        return None;
    }
    let mut skills: Vec<String> = Vec::new();
    for name in skills_in_text(text) {
        if !skills.contains(&name) {
            skills.push(name);
        }
    }
    Some(TraceRecord::Input {
        seq,
        t,
        text: head(text, TRACE_INPUT_TEXT_CHARS),
        source: source.to_string(),
        idle,
        after_deliverable: deliverable.map(|d| head(d, TRACE_DELIVERABLE_CHARS)),
        prelabel: label.to_string(),
        skills_invoked: skills,
    })
}

/// One API response: the `assistant` entries sharing a `message.id`, in file order.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    /// Timestamp of the response's last entry.
    pub t: UnixMs,
    pub model: Option<ModelRef>,
    pub usage: Option<Usage>,
    pub stop_reason: Option<String>,
    pub text: String,
}

/// The API responses of a transcript whose last entry is later than `after` (ms).
pub fn responses_after(s: &ClaudeSession, after: UnixMs) -> Vec<Response> {
    let mut out: Vec<(String, Response)> = Vec::new();
    for e in s.entries.iter().filter(|e| e.get("type").and_then(Value::as_str) == Some("assistant")) {
        let Some(m) = e.get("message") else { continue };
        let t = entry_ms(e).unwrap_or(0);
        let id = m.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        if out.last().is_none_or(|(last, _)| *last != id || id.is_empty()) {
            let model = m.get("model").and_then(Value::as_str).and_then(model_ref);
            out.push((id, Response { t, model, usage: None, stop_reason: None, text: String::new() }));
        }
        let r = &mut out.last_mut().expect("just pushed").1;
        r.t = r.t.max(t);
        if let Some(u) = m.get("usage").filter(|u| u.is_object()) {
            r.usage = Some(usage_of(u));
        }
        if let Some(stop) = m.get("stop_reason").and_then(Value::as_str) {
            r.stop_reason = Some(stop.to_string());
        }
        for b in m.get("content").and_then(Value::as_array).into_iter().flatten() {
            if b.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(x) = b.get("text").and_then(Value::as_str) {
                    if !r.text.is_empty() {
                        r.text.push('\n');
                    }
                    r.text.push_str(x);
                }
            }
        }
    }
    out.into_iter().map(|(_, r)| r).filter(|r| r.t > after).collect()
}

/// The last `max_bytes` of a transcript, parsed (a cut first line is dropped). Live hooks
/// read only the tail: transcripts reach hundreds of MB.
pub fn read_session_tail(path: &Path, max_bytes: u64) -> Result<ClaudeSession> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let text = if start > 0 { text.split_once('\n').map_or("", |(_, rest)| rest) } else { &text };
    parse_session(text, Some(path))
}

// ---- the synthesized trace ----

struct Turn {
    id: String,
    model: Option<ModelRef>,
    usage: Option<Usage>,
    stop: Option<String>,
    text: Vec<String>,
    t: UnixMs,
}

struct Synth {
    out: Vec<TraceRecord>,
    turn: Option<Turn>,
    turns_since_agent_end: u32,
    last_stop: Option<String>,
    last_model: Option<ModelRef>,
    last_input: Option<String>,
    last_deliverable: Option<String>,
    aborted: bool,
}

impl Synth {
    fn seq(&self) -> u32 {
        self.out.len() as u32
    }

    fn flush_turn(&mut self) {
        let Some(turn) = self.turn.take() else { return };
        if let (Some(to), Some(from)) = (&turn.model, &self.last_model) {
            if to != from {
                self.out.push(TraceRecord::ModelChange {
                    seq: self.seq(),
                    t: turn.t,
                    from: Some(from.clone()),
                    to: Some(to.clone()),
                    reason: Some("turn".into()),
                });
            }
        }
        if turn.model.is_some() {
            self.last_model = turn.model.clone();
        }
        let text = turn.text.join("\n");
        self.out.push(TraceRecord::TurnEnd {
            seq: self.seq(),
            t: turn.t,
            model: turn.model,
            usage: Some(turn.usage.unwrap_or_default()),
            stop_reason: turn.stop.clone(),
            text_head: head(&text, TRACE_DELIVERABLE_CHARS),
        });
        if !text.trim().is_empty() {
            self.last_deliverable = Some(text);
        }
        self.last_stop = turn.stop;
        self.turns_since_agent_end += 1;
    }

    fn agent_end(&mut self, t: UnixMs, aborted: bool) {
        if self.turns_since_agent_end == 0 {
            return;
        }
        self.out.push(TraceRecord::AgentEnd {
            seq: self.seq(),
            t,
            turns: self.turns_since_agent_end,
            is_terminal: true,
            yielded: !aborted && self.last_stop.as_deref() == Some("end_turn"),
        });
        self.turns_since_agent_end = 0;
    }
}

/// The adapter's trace (DESIGN §3) synthesized from a Claude Code transcript:
///
/// * `session_start` — prefixed id, cwd, repo via [`repo_of`], model of the first assistant
///   message, `mode: "import"`, agent `main` (`sub`, named `agent-<id>`, for subagents),
///   `omp_version` = `claude-code <version>`, no loaded entries, hooks or staging dirs.
/// * a typed prompt ([`prompt_kind`]) → `input` with the live prelabel rules (idle unless the
///   run before it was aborted or the prompt was queued while Claude worked → `interrupt`;
///   `reprompt` against the previous prompt; commands other than marks/`/skill:` dropped),
///   preceded by `agent_end` for the run it ends. Subagent transcripts have no prompts (their
///   user text is written by the parent agent).
/// * `tool_result` block → `tool_result` ([`tool_result_record`]), paired with its `tool_use`.
/// * one API response (entries sharing `message.id`) → `turn_end`, preceded by `model_change`
///   when the model differs from the previous turn.
/// * a final `agent_end` and `session_shutdown` flagged by [`flag_reasons`].
///
/// `seq` is the record index; `t` the entry timestamp (the previous one when missing).
pub fn trace_records_of(s: &ClaudeSession) -> Vec<TraceRecord> {
    let start_t = s.entries.iter().find_map(entry_ms).unwrap_or(0);
    let repo = repo_of(Path::new(&s.cwd)).map(|p| p.to_string_lossy().into_owned());
    let model = s
        .entries
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("assistant"))
        .find_map(|e| e.pointer("/message/model").and_then(Value::as_str).and_then(model_ref));
    let agent = match &s.agent_id {
        Some(a) => AgentInfo { kind: "sub".into(), name: format!("agent-{a}") },
        None => AgentInfo { kind: "main".into(), name: "main".into() },
    };
    let mut w = Synth {
        out: Vec::new(),
        turn: None,
        turns_since_agent_end: 0,
        last_stop: None,
        last_model: model.clone(),
        last_input: None,
        last_deliverable: None,
        aborted: false,
    };
    w.out.push(TraceRecord::SessionStart {
        seq: 0,
        t: start_t,
        session_id: s.id.clone(),
        cwd: s.cwd.clone(),
        branch_hint: basename(repo.as_deref().unwrap_or(&s.cwd)),
        repo,
        model,
        mode: "import".into(),
        agent: Some(agent),
        omp_version: if s.version.is_empty() { String::new() } else { format!("claude-code {}", s.version) },
        loaded_entries: Vec::new(),
        hooks_loaded: Vec::new(),
        staging_dirs: Vec::new(),
    });

    let mut calls: HashMap<String, (String, Value)> = HashMap::new();
    let mut last_t = start_t;
    for e in &s.entries {
        let t = entry_ms(e).unwrap_or(last_t);
        last_t = t;
        let Some(m) = e.get("message") else { continue };
        match e.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let id = m.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                if w.turn.as_ref().is_none_or(|turn| turn.id != id || id.is_empty()) {
                    w.flush_turn();
                    w.turn = Some(Turn {
                        id,
                        model: m.get("model").and_then(Value::as_str).and_then(model_ref),
                        usage: None,
                        stop: None,
                        text: Vec::new(),
                        t,
                    });
                }
                let turn = w.turn.as_mut().expect("turn just set");
                turn.t = turn.t.max(t);
                if let Some(u) = m.get("usage").filter(|u| u.is_object()) {
                    turn.usage = Some(usage_of(u));
                }
                if let Some(stop) = m.get("stop_reason").and_then(Value::as_str) {
                    turn.stop = Some(stop.to_string());
                }
                for b in m.get("content").and_then(Value::as_array).into_iter().flatten() {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(x) = b.get("text").and_then(Value::as_str) {
                                turn.text.push(x.to_string());
                            }
                        }
                        Some("tool_use") => {
                            if let Some(cid) = b.get("id").and_then(Value::as_str) {
                                let name = b.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                                calls.insert(cid.to_string(), (name, b.get("input").cloned().unwrap_or(Value::Null)));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("user") => {
                let results: Vec<&Value> = m
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                    .collect();
                if !results.is_empty() {
                    for b in results {
                        let call_id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or_default();
                        let (name, input) = calls.get(call_id).cloned().unwrap_or_default();
                        let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        let content = b.get("content").cloned().unwrap_or(Value::Null);
                        let seq = w.seq();
                        w.out.push(tool_result_record(seq, t, &name, call_id, &input, &content, is_error));
                        if let Some(turn) = &mut w.turn {
                            turn.t = turn.t.max(t);
                        }
                    }
                    continue;
                }
                if s.is_subagent() {
                    continue;
                }
                match prompt_kind(e) {
                    PromptKind::Ignored => {}
                    PromptKind::Interrupt => {
                        w.flush_turn();
                        w.agent_end(t, true);
                        w.aborted = true;
                    }
                    PromptKind::Prompt(text) => {
                        w.flush_turn();
                        w.agent_end(t, false);
                        let queued = e.get("promptSource").and_then(Value::as_str) == Some("queued");
                        let idle = !w.aborted && !queued;
                        w.aborted = false;
                        let source = e.get("promptSource").and_then(Value::as_str).unwrap_or_default();
                        let seq = w.seq();
                        if let Some(rec) =
                            input_record(seq, t, &text, idle, w.last_input.as_deref(), w.last_deliverable.as_deref(), source)
                        {
                            w.out.push(rec);
                            w.last_input = Some(text);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    w.flush_turn();
    w.agent_end(last_t, false);
    let reasons = flag_reasons(&w.out);
    let seq = w.seq();
    w.out.push(TraceRecord::SessionShutdown { seq, t: last_t, flagged: !reasons.is_empty(), reasons });
    w.out
}

/// The ledger row of an imported transcript: [`fold_trace`] of [`trace_records_of`] (so
/// redirect and error-cluster seqs address the synthesized trace), source `"import"`, the
/// transcript path recorded for `reflect --history` and `improve`.
pub fn session_record(s: &ClaudeSession) -> SessionRecord {
    let mut r = fold_trace(&trace_records_of(s), &s.id);
    r.source = "import".to_string();
    r.source_path = s.path.as_ref().map(|p| p.to_string_lossy().into_owned());
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SID: &str = "11111111-2222-3333-4444-555555555555";

    fn line(v: Value) -> String {
        format!("{v}\n")
    }

    fn base(kind: &str, ts: &str) -> Value {
        json!({"type": kind, "timestamp": ts, "sessionId": SID, "cwd": "/tmp/nowhere-cc", "version": "2.1.285",
               "isSidechain": false, "uuid": ts, "userType": "external"})
    }

    fn user_text(ts: &str, text: &str) -> Value {
        let mut v = base("user", ts);
        v["message"] = json!({"role": "user", "content": text});
        v["origin"] = json!({"kind": "human"});
        v["promptSource"] = json!("typed");
        v
    }

    fn assistant(ts: &str, id: &str, block: Value, stop: Option<&str>) -> Value {
        let mut v = base("assistant", ts);
        v["message"] = json!({"model": "claude-fable-5", "id": id, "type": "message", "role": "assistant",
            "content": [block], "stop_reason": stop,
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 1}});
        v
    }

    fn tool_result(ts: &str, call: &str, content: &str, is_error: bool) -> Value {
        let mut v = base("user", ts);
        v["message"] = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": call, "content": content, "is_error": is_error}]});
        v["toolUseResult"] = json!({"stdout": "huge copy", "stderr": ""});
        v
    }

    /// A handwritten transcript covering every mapping rule.
    fn fixture() -> String {
        let mut s = String::new();
        s += &line(json!({"type": "permission-mode", "permissionMode": "default", "sessionId": SID}));
        s += &line(user_text("2026-10-01T10:00:00Z", "Fix the flaky parser test in crates/parse"));
        let mut meta = base("user", "2026-10-01T10:00:00.500Z");
        meta["isMeta"] = json!(true);
        meta["message"] = json!({"role": "user", "content": [{"type": "text", "text": "Base directory for this skill: /x/skills/tdd\n\n# TDD"}]});
        s += &line(meta);
        s += &line(assistant("2026-10-01T10:00:01Z", "msg_1", json!({"type": "thinking", "thinking": "hmm"}), None));
        s += &line(assistant("2026-10-01T10:00:02Z", "msg_1", json!({"type": "text", "text": "Looking."}), None));
        s += &line(assistant(
            "2026-10-01T10:00:03Z",
            "msg_1",
            json!({"type": "tool_use", "id": "toolu_skill", "name": "Skill", "input": {"skill": "superpowers:systematic-debugging"}}),
            Some("tool_use"),
        ));
        s += &line(tool_result("2026-10-01T10:00:04Z", "toolu_skill", "Launching skill: superpowers:systematic-debugging", false));
        s += &line(assistant(
            "2026-10-01T10:00:05Z",
            "msg_2",
            json!({"type": "tool_use", "id": "toolu_read", "name": "Read", "input": {"file_path": "/home/u/.claude/skills/parse-debug/SKILL.md"}}),
            Some("tool_use"),
        ));
        s += &line(tool_result("2026-10-01T10:00:06Z", "toolu_read", "1\t# parse-debug", false));
        for (i, ts) in ["2026-10-01T10:00:07Z", "2026-10-01T10:00:09Z", "2026-10-01T10:00:11Z"].iter().enumerate() {
            let call = format!("toolu_bash{i}");
            s += &line(assistant(
                ts,
                &format!("msg_b{i}"),
                json!({"type": "tool_use", "id": call, "name": "Bash", "input": {"command": "cargo test -p parse"}}),
                Some("tool_use"),
            ));
            s += &line(tool_result(ts, &call, "Exit code 101\ntest result: FAILED. 3 passed; 1 failed", true));
        }
        s += &line(assistant("2026-10-01T10:00:13Z", "msg_3", json!({"type": "text", "text": "Fixed by retrying the test."}), Some("end_turn")));
        let mut notif = base("user", "2026-10-01T10:00:14Z");
        notif["origin"] = json!({"kind": "task-notification"});
        notif["message"] = json!({"role": "user", "content": "<task-notification>\n<task-id>b1</task-id>"});
        s += &line(notif);
        let mut side = assistant("2026-10-01T10:00:14.5Z", "msg_side", json!({"type": "text", "text": "sidechain"}), Some("end_turn"));
        side["isSidechain"] = json!(true);
        s += &line(side);
        s += &line(user_text("2026-10-01T10:00:15Z", "No, that's wrong — retrying hides the bug. Find the race."));
        s += &line(assistant("2026-10-01T10:00:16Z", "msg_4", json!({"type": "text", "text": "Investigating the race."}), Some("tool_use")));
        let mut abort = base("user", "2026-10-01T10:00:17Z");
        abort["message"] = json!({"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]});
        s += &line(abort);
        s += &line(user_text("2026-10-01T10:00:18Z", "look at the lock in pool.rs"));
        let mut cmd = base("user", "2026-10-01T10:00:19Z");
        cmd["message"] = json!({"role": "user", "content": "<command-name>/rematch-mark</command-name>\n<command-message>rematch-mark</command-message>\n<command-args>correction</command-args>"});
        s += &line(cmd);
        let mut login = base("user", "2026-10-01T10:00:20Z");
        login["message"] = json!({"role": "user", "content": "<command-name>/login</command-name>\n<command-args></command-args>"});
        s += &line(login);
        let mut model2 = assistant("2026-10-01T10:00:21Z", "msg_5", json!({"type": "text", "text": "Lock fixed."}), Some("end_turn"));
        model2["message"]["model"] = json!("claude-opus-5-5");
        s += &line(model2);
        s
    }

    #[test]
    fn parses_session_identity() {
        let s = parse_session(&fixture(), Some(Path::new("/p/-tmp/11111111-2222-3333-4444-555555555555.jsonl"))).unwrap();
        assert_eq!(s.id, format!("cc:{SID}"));
        assert!(!s.is_subagent());
        assert_eq!(s.cwd, "/tmp/nowhere-cc");
        assert_eq!(s.version, "2.1.285");
        // The sidechain entry of a main transcript is dropped; non-message types are skipped.
        assert!(s.entries.iter().all(|e| e.pointer("/message/id") != Some(&json!("msg_side"))));
        assert!(s.entries.iter().all(|e| e.get("toolUseResult").is_none()));
    }

    #[test]
    fn synthesized_trace_golden() {
        let s = parse_session(&fixture(), None).unwrap();
        let recs = trace_records_of(&s);
        let evs: Vec<&str> = recs.iter().map(TraceRecord::ev).collect();
        assert_eq!(
            evs,
            vec![
                "session_start",
                "input",
                "tool_result", // Skill
                "turn_end",    // msg_1 (thinking dropped), flushed when msg_2 starts
                "tool_result", // Read SKILL.md
                "turn_end",    // msg_2
                "tool_result",
                "turn_end",
                "tool_result",
                "turn_end",
                "tool_result",
                "turn_end",
                "turn_end", // msg_3
                "agent_end",
                "input", // negation
                "turn_end", // msg_4
                "agent_end", // aborted
                "input", // interrupt
                "input", // /rematch-mark correction
                "model_change",
                "turn_end",
                "agent_end",
                "session_shutdown",
            ],
            "{recs:#?}"
        );
        for (i, r) in recs.iter().enumerate() {
            assert_eq!(r.seq() as usize, i);
        }
        let TraceRecord::SessionStart { session_id, mode, model, omp_version, agent, .. } = &recs[0] else { panic!() };
        assert_eq!(session_id, &format!("cc:{SID}"));
        assert_eq!(mode, "import");
        assert_eq!(model.as_ref().unwrap().id, "claude-fable-5");
        assert_eq!(omp_version, "claude-code 2.1.285");
        assert_eq!(agent.as_ref().unwrap().kind, "main");
        // Turn order (as omp's importer): a turn's tool results, then its turn_end.
        let TraceRecord::TurnEnd { usage, text_head, stop_reason, .. } = &recs[3] else { panic!() };
        assert_eq!(text_head, "Looking.");
        assert_eq!(stop_reason.as_deref(), Some("tool_use"));
        let u = usage.as_ref().unwrap();
        assert_eq!((u.input, u.output, u.cache_read, u.cache_write, u.total_tokens), (10, 5, 100, 1, 116));
        let TraceRecord::ToolResult { tool, skill_read, .. } = &recs[2] else { panic!() };
        assert_eq!((tool.as_str(), skill_read.as_deref()), ("skill", Some("systematic-debugging")));
        let TraceRecord::ToolResult { tool, skill_read, path, .. } = &recs[4] else { panic!() };
        assert_eq!((tool.as_str(), skill_read.as_deref()), ("read", Some("parse-debug")));
        assert_eq!(path.as_deref(), Some("/home/u/.claude/skills/parse-debug/SKILL.md"));
        let TraceRecord::ToolResult { tool, is_error, content_head, args_hash: h, .. } = &recs[6] else { panic!() };
        assert_eq!(tool, "bash");
        assert!(is_error);
        assert!(content_head.starts_with("Exit code 101"));
        assert_eq!(h, &args_hash(&json!({"command": "cargo test -p parse"})));
        let labels: Vec<(&str, bool)> = recs
            .iter()
            .filter_map(|r| match r {
                TraceRecord::Input { prelabel, idle, .. } => Some((prelabel.as_str(), *idle)),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec![("none", true), ("negation", true), ("interrupt", false), ("mark:correction", true)]);
        let TraceRecord::Input { after_deliverable, .. } = &recs[14] else { panic!() };
        assert_eq!(after_deliverable.as_deref(), Some("Fixed by retrying the test."));
        let TraceRecord::AgentEnd { yielded, turns, .. } = &recs[13] else { panic!() };
        assert!(*yielded);
        assert_eq!(*turns, 6);
        let TraceRecord::AgentEnd { yielded, .. } = &recs[16] else { panic!() };
        assert!(!yielded, "an aborted run did not yield");
        let TraceRecord::SessionShutdown { reasons, flagged, .. } = recs.last().unwrap() else { panic!() };
        assert!(flagged);
        assert_eq!(
            reasons,
            &vec!["tool_error_cluster".to_string(), "redirect:negation".into(), "redirect:interrupt".into(), "mark:correction".into()]
        );
    }

    #[test]
    fn session_record_folds_the_trace() {
        let s = parse_session(&fixture(), Some(Path::new("/p/-tmp/x.jsonl"))).unwrap();
        let r = session_record(&s);
        assert_eq!(r.id, format!("cc:{SID}"));
        assert_eq!(r.source, "import");
        assert_eq!(r.source_path.as_deref(), Some("/p/-tmp/x.jsonl"));
        assert_eq!(r.turns, 8);
        assert_eq!((r.tool_calls, r.tool_errors), (5, 3));
        assert_eq!(r.family.as_ref().unwrap().0, "anthropic/claude-fable");
        assert_eq!(r.skill_reads, vec!["systematic-debugging".to_string(), "parse-debug".into()]);
        assert!(r.flagged);
        assert_eq!(r.error_clusters, 1);
        assert_eq!(r.takeovers, 1);
        assert_eq!(r.redirects.len(), 3);
    }

    #[test]
    fn subagent_transcript() {
        let mut first = user_text("2026-10-01T10:00:00Z", "You are a reviewer. Review pool.rs.");
        first["isSidechain"] = json!(true);
        first["agentId"] = json!("a12");
        first.as_object_mut().unwrap().remove("origin");
        let mut a = assistant(
            "2026-10-01T10:00:01Z",
            "m1",
            json!({"type": "tool_use", "id": "t1", "name": "Grep", "input": {"pattern": "lock", "path": "/r/src"}}),
            Some("tool_use"),
        );
        a["isSidechain"] = json!(true);
        let mut r = tool_result("2026-10-01T10:00:02Z", "t1", "src/pool.rs", false);
        r["isSidechain"] = json!(true);
        let text = [first, a, r].iter().map(|v| line(v.clone())).collect::<String>();
        let p = PathBuf::from(format!("/p/-r/{SID}/subagents/agent-a12.jsonl"));
        let s = parse_session(&text, Some(&p)).unwrap();
        assert!(s.is_subagent());
        assert_eq!(s.id, format!("cc:{SID}:agent-a12"));
        assert_eq!(s.parent_id(), Some(format!("cc:{SID}")));
        let recs = trace_records_of(&s);
        assert!(recs.iter().all(|r| r.ev() != "input"), "subagent prompts come from the parent");
        let TraceRecord::SessionStart { agent, .. } = &recs[0] else { panic!() };
        assert_eq!(agent.as_ref().unwrap(), &AgentInfo { kind: "sub".into(), name: "agent-a12".into() });
    }

    #[test]
    fn scan_and_find() {
        let t = tempfile::tempdir().unwrap();
        let slug = t.path().join("-r");
        std::fs::create_dir_all(slug.join(SID).join("subagents")).unwrap();
        std::fs::create_dir_all(slug.join(SID).join("tool-results")).unwrap();
        std::fs::create_dir_all(slug.join("memory")).unwrap();
        std::fs::write(slug.join(format!("{SID}.jsonl")), fixture()).unwrap();
        std::fs::write(slug.join(SID).join("subagents").join("agent-a12.jsonl"), "").unwrap();
        std::fs::write(slug.join(SID).join("tool-results").join("x.jsonl"), "").unwrap();
        std::fs::write(slug.join("memory").join("m.jsonl"), "").unwrap();
        let files = scan_sessions_dir(t.path()).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with(format!("{SID}.jsonl")));
        assert!(files[1].ends_with("agent-a12.jsonl"));
        assert_eq!(find_session_file(t.path(), &format!("cc:{SID}")), Some(files[0].clone()));
        assert_eq!(find_session_file(t.path(), &format!("cc:{SID}:agent-a12")), Some(files[1].clone()));
        assert_eq!(find_session_file(t.path(), "codex:x"), None);
    }

    #[test]
    fn plugin_mark_command_is_a_mark() {
        assert_eq!(canonical_prompt("/rematch-mark correction"), "/rematch mark correction");
        assert_eq!(canonical_prompt("  /rematch:rematch-mark scope"), "/rematch mark scope");
        assert_eq!(canonical_prompt("/rematch-marker x"), "/rematch-marker x");
        let Some(TraceRecord::Input { prelabel, text, .. }) = input_record(0, 0, "/rematch:rematch-mark preference", true, None, None, "")
        else {
            panic!()
        };
        assert_eq!((prelabel.as_str(), text.as_str()), ("mark:preference", "/rematch mark preference"));
        assert!(input_record(0, 0, "/compact", true, None, None, "").is_none());
    }

    #[test]
    fn result_text_of_hook_responses() {
        assert_eq!(result_text(&json!({"stdout": "ok", "stderr": "warn", "interrupted": false})), "ok\nwarn");
        assert_eq!(result_text(&json!({"type": "text", "file": {"filePath": "/a", "content": "body"}})), "body");
        assert_eq!(result_text(&json!([{"type": "text", "text": "a"}, {"type": "image"}])), "a");
        assert_eq!(result_text(&json!("Exit code 1")), "Exit code 1");
    }

    /// Read-only smoke over the real `~/.claude/projects` (`REMATCH_REAL_HISTORY=1`).
    #[test]
    fn real_history_smoke() {
        if std::env::var("REMATCH_REAL_HISTORY").as_deref() != Ok("1") {
            return;
        }
        let Some(dir) = default_sessions_dir().filter(|d| d.is_dir()) else {
            eprintln!("no Claude Code projects dir");
            return;
        };
        let files = scan_sessions_dir(&dir).unwrap();
        let (mut main, mut subs, mut flagged, mut inputs, mut results) = (0, 0, 0, 0, 0);
        for f in &files {
            let s = read_session(f).unwrap_or_else(|e| panic!("{}: {e:#}", f.display()));
            assert!(s.id.starts_with(ID_PREFIX), "{}", f.display());
            let recs = trace_records_of(&s);
            assert_eq!(recs.first().map(TraceRecord::ev), Some("session_start"));
            assert_eq!(recs.last().map(TraceRecord::ev), Some("session_shutdown"));
            for (i, r) in recs.iter().enumerate() {
                assert_eq!(r.seq() as usize, i);
            }
            inputs += recs.iter().filter(|r| r.ev() == "input").count();
            results += recs.iter().filter(|r| r.ev() == "tool_result").count();
            if s.is_subagent() {
                subs += 1;
            } else {
                main += 1;
            }
            flagged += session_record(&s).flagged as u32;
        }
        eprintln!("{} files: {main} main, {subs} subagent, {flagged} flagged, {inputs} inputs, {results} tool results", files.len());
        assert_eq!(main + subs, files.len());
    }
}
