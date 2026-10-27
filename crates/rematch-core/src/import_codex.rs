//! Codex history importer: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl` (and the
//! `codex exec --json` event stream) → the extension's trace (DESIGN §3, §18.2), so
//! `fold_trace`, `count`, `reflect --history` and `improve` treat Codex sessions like omp ones.
//!
//! Format facts relied on (rollouts of Codex Desktop / codex-cli 0.163; DESIGN §18.2):
//! every line is `{"timestamp", "type", "payload"}`; the first `session_meta` carries `id`,
//! `cwd`, `cli_version`, `model_provider`, `source` (`"exec"`, `"vscode"`, or
//! `{"subagent": …}` for reviewer threads) and `parent_thread_id`; `turn_context` and
//! `event_msg/thread_settings_applied` name the model; `event_msg/task_started|task_complete|
//! turn_aborted` delimit turns; `token_usage_record` (else `event_msg/token_count`) carries
//! usage. Two representations of the conversation exist and exactly one is used per file:
//! `event_msg/item_completed` thread items (`UserMessage`, `AgentMessage`,
//! `CommandExecution`, `FileChange`, `McpToolCall`, `ImageView`, `Extension`) when present —
//! the `exec --json` stream's `item.completed` items are the same items in snake_case — else
//! the model-facing `response_item`s (`message`, `function_call`/`custom_tool_call` and their
//! `*_output`). Unknown types are ignored.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::{json, Value};

use crate::features::{head, strip_selector};
use crate::import::{
    args_hash, content_text, is_injected_frame, repo_of, truncate_tool_input, TRACE_CONTENT_HEAD_CHARS,
    TRACE_DELIVERABLE_CHARS, TRACE_INPUT_TEXT_CHARS, TRACE_TOOL_INPUT_BYTES,
};
use crate::redirect::prelabel;
use crate::trace::{
    basename, flag_reasons, fold_trace, push_unique, skill_from_read_path, skills_in_text, AgentInfo, LoadedEntry,
    TraceRecord, Usage,
};
use crate::types::{ModelRef, SessionRecord, UnixMs};

/// Session ids of imported Codex sessions are `codex:<session_meta.id>`.
pub const ID_PREFIX: &str = "codex:";

/// A parsed Codex rollout: the `session_meta` header plus every other line as
/// `(timestamp ms, type, payload)` in file order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodexSession {
    /// `codex:<id>`.
    pub id: String,
    pub cwd: String,
    pub path: Option<PathBuf>,
    /// ISO timestamp of `session_meta.payload.timestamp` (else the line's).
    pub timestamp: String,
    pub cli_version: String,
    /// `session_meta.payload.model_provider` (`openai` when absent).
    pub provider: String,
    /// `session_meta.payload.source`: `"exec"`, `"vscode"`, `"cli"`, or `{"subagent": …}`.
    pub source: Value,
    pub parent_thread_id: Option<String>,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub t: Option<UnixMs>,
    pub kind: String,
    pub payload: Value,
}

impl CodexSession {
    /// Reviewer/subagent threads (`source: {"subagent": …}` or a `parent_thread_id`): their
    /// "user" turns are written by the parent agent, never by a human.
    pub fn is_subagent(&self) -> bool {
        self.source.get("subagent").is_some() || self.parent_thread_id.is_some()
    }

    fn agent(&self) -> AgentInfo {
        if !self.is_subagent() {
            return AgentInfo { kind: "main".into(), name: "main".into() };
        }
        let name = self
            .source
            .pointer("/subagent/other")
            .and_then(Value::as_str)
            .or_else(|| self.source.get("subagent").and_then(Value::as_str))
            .unwrap_or("subagent");
        AgentInfo { kind: "sub".into(), name: name.to_string() }
    }

    /// Absolute `SKILL.md` paths of the skills Codex listed to the model: every `## Skills`
    /// block (`world_state.state.host_skills.body`, developer messages) with its `rN` roots
    /// expanded, in first-seen order.
    pub fn offered_skills(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for l in &self.lines {
            let body = match (l.kind.as_str(), str_at(&l.payload, "role")) {
                ("world_state", _) => {
                    l.payload.pointer("/state/host_skills/body").and_then(Value::as_str).unwrap_or_default().to_string()
                }
                ("response_item", "developer") => content_text_blocks(l.payload.get("content")),
                _ => continue,
            };
            if body.contains("SKILL.md") {
                skill_list_paths(&body, &mut out);
            }
        }
        out
    }
}

static SKILL_ROOT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^- `(r\d+)` = `([^`]+)`").expect("static pattern"));
static SKILL_FILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)\(file: ([^)\n]+SKILL\.md)\)\s*$").expect("static pattern"));

/// `### Skill roots` (`- \`r0\` = \`/abs\``) + `### Available skills` (`- name: … (file:
/// r0/x/SKILL.md)`) → absolute paths, appended to `out` when new.
fn skill_list_paths(body: &str, out: &mut Vec<PathBuf>) {
    let roots: HashMap<&str, &str> =
        SKILL_ROOT_RE.captures_iter(body).map(|c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str())).collect();
    for c in SKILL_FILE_RE.captures_iter(body) {
        let short = &c[1];
        let path = match short.split_once('/') {
            Some((root, rest)) if roots.contains_key(root) => Path::new(roots[root]).join(rest),
            _ => PathBuf::from(short),
        };
        if path.is_absolute() && !out.contains(&path) {
            out.push(path);
        }
    }
}

/// The rematch entry behind an offered skill, resolved now from the filesystem: a link in
/// the shared skills dir leads to the entry dir (staging or kept) holding `rematch.json`. An
/// entry created after `started` was not what Codex offered then (a newer version behind the
/// same name) and is not attributed. Returns the entry (id, version, `content_sha256`) and
/// whether it loads: a hook entry's router `SKILL.md` is offered (visible) too, but Codex never
/// runs the hook, so it is not a loaded entry.
pub(crate) fn rematch_entry_of(skill_md: &Path, started: UnixMs) -> Option<(LoadedEntry, bool)> {
    let dir = skill_md.parent()?;
    let v: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(crate::library::REMATCH_JSON)).ok()?).ok()?;
    let loads = !matches!(v.get("kind").and_then(Value::as_str), Some("hook" | "agent" | "lesson"));
    let created = v.get("created_at").and_then(Value::as_i64).unwrap_or(0);
    let id = v.get("id").and_then(Value::as_str).filter(|id| !id.is_empty() && created <= started)?;
    let entry = LoadedEntry {
        id: id.to_string(),
        version: v.get("version").and_then(Value::as_u64).map(|n| n as u32),
        sha: v.get("content_sha256").and_then(Value::as_str).map(str::to_string),
    };
    Some((entry, loads))
}

/// `$CODEX_HOME/sessions`, else `~/.codex/sessions`.
pub fn default_sessions_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("CODEX_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h).join("sessions"));
    }
    crate::paths::home_dir().map(|h| h.join(".codex").join("sessions"))
}

/// Every `rollout-*.jsonl` under `dir` (recursively), sorted by path (= by start time).
pub fn scan_sessions_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let p = entry.path();
            if ft.is_dir() {
                walk(&p, out)?;
            } else if ft.is_file() && is_rollout(&p) {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn is_rollout(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "jsonl")
        && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-"))
}

/// `rollout-2026-10-18T14-13-24-<uuid>.jsonl` → `<uuid>` (the last 36 chars of the stem).
fn id_from_filename(p: &Path) -> Option<String> {
    let stem = p.file_stem()?.to_str()?;
    let tail = stem.get(stem.len().checked_sub(36)?..)?;
    (tail.len() == 36 && tail.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')).then(|| tail.to_string())
}

/// The rollout of `id` (`codex:<id>` or bare) under `dir`: a `rollout-*<id>.jsonl` file.
pub fn find_session_file(dir: &Path, id: &str) -> Option<PathBuf> {
    let raw = id.strip_prefix(ID_PREFIX).unwrap_or(id);
    if raw.is_empty() {
        return None;
    }
    scan_sessions_dir(dir)
        .ok()?
        .into_iter()
        .find(|p| p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.ends_with(raw)))
}

pub fn read_session(path: &Path) -> Result<CodexSession> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read session {}", path.display()))?;
    parse_session(&text, Some(path))
}

fn iso_ms(s: &str) -> Option<UnixMs> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Parses rollout JSONL (or a `codex exec --json` stream). Fails only when no line parses.
pub fn parse_session(text: &str, path: Option<&Path>) -> Result<CodexSession> {
    let mut s = CodexSession::default();
    let mut meta_seen = false;
    let mut raw_id = String::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(kind) = v.get("type").and_then(Value::as_str) else {
            continue;
        };
        let t = v.get("timestamp").and_then(Value::as_str).and_then(iso_ms);
        // Rollout lines wrap their content in `payload`; exec --json events are flat.
        let payload = v.get("payload").cloned().unwrap_or_else(|| v.clone());
        match kind {
            "session_meta" if !meta_seen => {
                meta_seen = true;
                raw_id = str_at(&payload, "id").to_string();
                s.cwd = str_at(&payload, "cwd").to_string();
                s.timestamp = payload
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .or_else(|| v.get("timestamp").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string();
                s.cli_version = str_at(&payload, "cli_version").to_string();
                s.provider = str_at(&payload, "model_provider").to_string();
                s.source = payload.get("source").cloned().unwrap_or(Value::Null);
                s.parent_thread_id = payload
                    .get("parent_thread_id")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string);
            }
            "session_meta" => {}
            "thread.started" if raw_id.is_empty() => raw_id = str_at(&payload, "thread_id").to_string(),
            _ => s.lines.push(Line { t, kind: kind.to_string(), payload }),
        }
    }
    if !meta_seen && s.lines.is_empty() {
        bail!("no Codex session records{}", path.map(|p| format!(" in {}", p.display())).unwrap_or_default());
    }
    if raw_id.is_empty() {
        raw_id = path.and_then(id_from_filename).unwrap_or_default();
    }
    if s.cwd.is_empty() {
        s.cwd = s
            .lines
            .iter()
            .find(|l| l.kind == "turn_context")
            .map(|l| str_at(&l.payload, "cwd").to_string())
            .unwrap_or_default();
    }
    if s.provider.is_empty() {
        s.provider = "openai".into();
    }
    s.id = if raw_id.is_empty() { String::new() } else { format!("{ID_PREFIX}{raw_id}") };
    s.path = path.map(Path::to_path_buf);
    Ok(s)
}

/// The ledger row of an imported Codex session: the fold of [`trace_records_of`] (so its
/// redirect and cluster seqs address the synthesized trace and the flag follows the
/// extension's rule), with `source: "import"` and the rollout as `source_path`.
pub fn session_record(s: &CodexSession) -> SessionRecord {
    let mut r = fold_trace(&trace_records_of(s), &s.id);
    r.source = "import".into();
    r.source_path = s.path.as_ref().map(|p| p.to_string_lossy().into_owned());
    r
}

// ---- normalization of user text, items and calls ----

static AMBIENT_OPEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^<([A-Za-z][A-Za-z0-9_-]*)[^>]*>").expect("static pattern"));
static SKILL_MENTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[\s\[(])\$([a-z][a-z0-9_-]*)").expect("static pattern"));
static SKILL_IN_COMMAND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:^|[\s'"=/])skills/([^/\s'"]+)/SKILL\.md"#).expect("static pattern"));
static EXIT_CODE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^(?:Process exited with code|Exit code:)\s*(-?\d+)").expect("static pattern")
});

/// What a user-role text is, after removing what the harness wraps around it.
enum UserText {
    /// Typed by the human (the request, with ambient context blocks removed).
    Typed(String),
    /// The human's answer to the agent's question tool (`request_user_input`): idle input.
    Reply(String),
    /// Injected by the harness (heartbeats, system frames, environment context).
    Injected,
}

/// Codex Desktop prefixes requests with ambient blocks (`<in-app-browser-context …>…</…>`,
/// then `## My request:`), automations send `<heartbeat>`, question answers arrive as
/// `<send_user_message_question_reply>[{question, answer}]</…>`; response-item user
/// messages also carry `<environment_context>`, `<user_instructions>` and the AGENTS.md
/// preamble. Only the human's words survive.
fn classify_user_text(raw: &str) -> UserText {
    let mut text = raw.trim();
    if is_injected_frame(text) || text.starts_with("<heartbeat") || text.starts_with("# AGENTS.md instructions") {
        return UserText::Injected;
    }
    if let Some(rest) = text.strip_prefix("<send_user_message_question_reply>") {
        let body = rest.split("</send_user_message_question_reply>").next().unwrap_or_default();
        let answers: Vec<String> = serde_json::from_str::<Vec<Value>>(body.trim())
            .unwrap_or_default()
            .iter()
            .filter_map(|a| a.get("answer").and_then(Value::as_str))
            .filter(|a| !a.trim().is_empty())
            .map(str::to_string)
            .collect();
        return if answers.is_empty() { UserText::Injected } else { UserText::Reply(answers.join("\n")) };
    }
    while let Some(c) = AMBIENT_OPEN_RE.captures(text) {
        let close = format!("</{}>", &c[1]);
        let Some(end) = text.find(&close) else {
            break;
        };
        text = text[end + close.len()..].trim_start();
    }
    let text = text.strip_prefix("## My request:").unwrap_or(text).trim();
    if text.is_empty() {
        UserText::Injected
    } else {
        UserText::Typed(text.to_string())
    }
}

/// Skills a user text invokes: Codex `$name` mentions (codex-rs `skills/src/mentions.rs`,
/// lowercase names only so `$HOME`-style variables do not count) and `/skill:` tokens.
fn skills_invoked(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for name in SKILL_MENTION_RE.captures_iter(text).map(|c| c[1].to_string()).chain(skills_in_text(text)) {
        push_unique(&mut out, &mut seen, name);
    }
    out
}

/// `file:///a%20b` → `/a b`; on Windows `file:///C:/x` → `C:/x` (the drive path); other
/// strings unchanged.
fn file_path(p: &str) -> String {
    let Some(rest) = p.strip_prefix("file://") else {
        return p.to_string();
    };
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let path = String::from_utf8(out).unwrap_or_else(|_| rest.to_string());
    let b = path.as_bytes();
    if cfg!(windows) && b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        return path[1..].to_string();
    }
    path
}

/// A shell argv (`["/bin/zsh","-lc","<script>"]`) or command string → the script text.
fn command_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => {
            let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            match parts.as_slice() {
                [_, flag, script @ ..] if matches!(*flag, "-lc" | "-c") && !script.is_empty() => script.join(" "),
                _ => parts.join(" "),
            }
        }
        _ => String::new(),
    }
}

/// Skill whose `SKILL.md` a shell command reads (`cat|sed … skills/<name>/SKILL.md`).
fn skill_in_command(cmd: &str) -> Option<String> {
    SKILL_IN_COMMAND_RE.captures(cmd).map(|c| c[1].to_string())
}

/// Text of an output that is a string or an array of `{type: input_text|text, text}` blocks.
fn output_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Text of an `AgentMessage` item (`content: [{type: "Text", text}]`, or exec's `text`).
fn item_text(item: &Value) -> String {
    match item.get("text").and_then(Value::as_str) {
        Some(t) => t.to_string(),
        None => output_text(item.get("content")),
    }
}

/// One tool result, normalized.
struct ToolOut {
    tool: String,
    call_id: String,
    input: Value,
    output: String,
    is_error: bool,
    path: Option<String>,
    skill_read: Option<String>,
}

/// Item type without case or underscores: `CommandExecution` / `command_execution` →
/// `commandexecution`.
fn item_kind(item: &Value) -> String {
    str_at(item, "type").chars().filter(|c| *c != '_').flat_map(char::to_lowercase).collect()
}

fn failed(item: &Value) -> bool {
    matches!(str_at(item, "status"), "failed" | "declined" | "rejected")
}

/// A thread item that is a tool call → its result (`None` for messages, reasoning, …).
fn tool_of_item(item: &Value) -> Option<ToolOut> {
    let call_id = str_at(item, "id").to_string();
    match item_kind(item).as_str() {
        "commandexecution" => {
            let cmd = command_text(item.get("command"));
            let mut input = json!({ "command": cmd });
            if let Some(cwd) = item.get("cwd").and_then(Value::as_str) {
                input["cwd"] = json!(file_path(cwd));
            }
            let reads: Vec<&str> = item
                .get("parsed_cmd")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|p| str_at(p, "type") == "read")
                .filter_map(|p| p.get("path").and_then(Value::as_str))
                .collect();
            let exit = item.get("exit_code").and_then(Value::as_i64);
            Some(ToolOut {
                tool: "bash".into(),
                call_id,
                output: item
                    .get("aggregated_output")
                    .or_else(|| item.get("formatted_output"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                is_error: failed(item) || exit.is_some_and(|c| c != 0),
                path: reads.first().map(|p| p.to_string()),
                skill_read: reads.iter().find_map(|p| skill_from_read_path(p)).or_else(|| skill_in_command(&cmd)),
                input,
            })
        }
        "filechange" => {
            // Rollout: `changes: {path: {type, …}}`; exec --json: `changes: [{path, kind}]`.
            let changes: Vec<(String, String)> = match item.get("changes") {
                Some(Value::Object(m)) => m.iter().map(|(p, c)| (file_path(p), str_at(c, "type").to_string())).collect(),
                Some(Value::Array(a)) => {
                    a.iter().map(|c| (file_path(str_at(c, "path")), str_at(c, "kind").to_string())).collect()
                }
                _ => Vec::new(),
            };
            let summary = changes.iter().map(|(p, k)| format!("{k} {p}")).collect::<Vec<_>>().join("\n");
            Some(ToolOut {
                tool: "apply_patch".into(),
                call_id,
                input: json!({
                    "paths": changes.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
                    "changes": changes.iter().map(|(p, k)| json!({"path": p, "kind": k})).collect::<Vec<_>>(),
                }),
                output: summary,
                is_error: failed(item),
                path: changes.first().map(|(p, _)| p.clone()),
                skill_read: None,
            })
        }
        "mcptoolcall" => {
            let result = item.get("result");
            let mut output = output_text(result.and_then(|r| r.get("content")));
            let error = item.pointer("/error/message").and_then(Value::as_str);
            if let Some(e) = error {
                output = if output.is_empty() { e.to_string() } else { format!("{e}\n{output}") };
            }
            let input = item.get("arguments").cloned().unwrap_or(Value::Null);
            Some(ToolOut {
                tool: format!("mcp__{}__{}", str_at(item, "server"), str_at(item, "tool")),
                call_id,
                path: input.get("path").and_then(Value::as_str).map(file_path),
                input,
                output,
                is_error: failed(item)
                    || error.is_some()
                    || result.and_then(|r| r.get("isError")).and_then(Value::as_bool) == Some(true),
                skill_read: None,
            })
        }
        "imageview" => {
            let path = file_path(str_at(item, "path"));
            Some(ToolOut {
                tool: "view_image".into(),
                call_id,
                input: json!({ "path": path }),
                output: String::new(),
                is_error: false,
                path: (!path.is_empty()).then_some(path),
                skill_read: None,
            })
        }
        "websearch" | "extension" => {
            let kind = item.get("kind").and_then(Value::as_str).unwrap_or("web.search");
            Some(ToolOut {
                tool: if kind == "web.search" { "web_search".into() } else { kind.to_string() },
                call_id,
                input: json!({ "query": str_at(item, "query") }),
                output: String::new(),
                is_error: failed(item),
                path: None,
                skill_read: None,
            })
        }
        _ => None,
    }
}

/// Shell-like function tools of the model-facing API and their command argument.
const SHELL_TOOLS: &[&str] = &["exec_command", "shell", "shell_command", "local_shell", "container.exec"];

/// A `function_call` / `custom_tool_call` response item and its output → the result.
fn tool_of_call(call: Option<&Value>, out: &Value) -> ToolOut {
    let name = call.map(|c| str_at(c, "name")).unwrap_or_default();
    let namespace = call.map(|c| str_at(c, "namespace")).unwrap_or_default();
    let custom = call.is_some_and(|c| str_at(c, "type") == "custom_tool_call");
    let args: Value = match call {
        Some(c) if custom => match name {
            "apply_patch" => json!({ "input": str_at(c, "input") }),
            "exec" => json!({ "code": str_at(c, "input") }),
            _ => json!({ "input": str_at(c, "input") }),
        },
        Some(c) => match c.get("arguments") {
            Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({ "raw": s })),
            Some(v) => v.clone(),
            None => Value::Null,
        },
        None => Value::Null,
    };
    let (tool, input) = if SHELL_TOOLS.contains(&name) {
        let cmd = command_text(args.get("cmd").or_else(|| args.get("command")));
        let mut input = json!({ "command": cmd });
        if let Some(w) = args.get("workdir").and_then(Value::as_str) {
            input["cwd"] = json!(w);
        }
        ("bash".to_string(), input)
    } else if namespace.starts_with("mcp__") {
        (format!("{namespace}__{name}"), args)
    } else {
        (name.to_string(), args)
    };
    // Older rollouts serialize shell output as `{"output", "metadata": {"exit_code"}}`.
    let mut text = output_text(out.get("output"));
    let mut exit = None;
    if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(&text) {
        if let Some(code) = o.get("metadata").and_then(|m| m.get("exit_code")).and_then(Value::as_i64) {
            exit = Some(code);
            text = o.get("output").and_then(Value::as_str).unwrap_or_default().to_string();
        }
    }
    let exit = exit.or_else(|| EXIT_CODE_RE.captures(&text).and_then(|c| c[1].parse::<i64>().ok()));
    let first = text.lines().next().unwrap_or_default();
    let is_error = exit.is_some_and(|c| c != 0) || first.starts_with("Script failed") || first.starts_with("aborted");
    let cmd = input.get("command").and_then(Value::as_str).unwrap_or_default();
    let raw_path = input.get("path").or_else(|| input.get("file_path")).and_then(Value::as_str);
    ToolOut {
        skill_read: match tool.as_str() {
            "bash" => skill_in_command(cmd),
            "read" => raw_path.and_then(skill_from_read_path),
            _ => None,
        },
        path: raw_path.map(|p| strip_selector(p).to_string()),
        tool,
        call_id: str_at(out, "call_id").to_string(),
        input,
        output: text,
        is_error,
    }
}

/// OpenAI usage (`input_tokens` includes the cached part) → omp-shaped [`Usage`].
fn usage_of(u: &Value) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cached = n("cached_input_tokens");
    Usage {
        input: n("input_tokens").saturating_sub(cached),
        output: n("output_tokens"),
        cache_read: cached,
        cache_write: n("cache_write_input_tokens"),
        total_tokens: n("total_tokens"),
        cost: Value::Null,
    }
}

fn add_usage(acc: &mut Option<Usage>, u: Usage) {
    let a = acc.get_or_insert_with(Usage::default);
    a.input += u.input;
    a.output += u.output;
    a.cache_read += u.cache_read;
    a.cache_write += u.cache_write;
    a.total_tokens += u.total_tokens;
}

// ---- the extension's trace, synthesized ----

/// An assistant message whose `turn_end` is due once its tool results are written.
struct PendingTurn {
    text: String,
    stop_reason: Option<String>,
    t: UnixMs,
}

struct Synth {
    out: Vec<TraceRecord>,
    turn: Option<PendingTurn>,
    usage: Option<Usage>,
    model: Option<ModelRef>,
    last_model: Option<ModelRef>,
    turns_since_agent_end: u32,
    last_stop: Option<&'static str>,
    /// A task (turn) is running and has already received this many inputs.
    turn_open: bool,
    turn_inputs: u32,
    last_input: Option<String>,
    last_deliverable: Option<String>,
}

impl Synth {
    fn seq(&self) -> u32 {
        self.out.len() as u32
    }

    fn flush_turn(&mut self) {
        let Some(turn) = self.turn.take() else {
            return;
        };
        if let (Some(to), Some(from)) = (&self.model, &self.last_model) {
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
        if self.model.is_some() {
            self.last_model = self.model.clone();
        }
        self.out.push(TraceRecord::TurnEnd {
            seq: self.seq(),
            t: turn.t,
            model: self.model.clone(),
            usage: Some(self.usage.take().unwrap_or_default()),
            stop_reason: turn.stop_reason,
            text_head: head(&turn.text, TRACE_DELIVERABLE_CHARS),
        });
        if !turn.text.trim().is_empty() {
            self.last_deliverable = Some(turn.text);
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

    fn assistant(&mut self, text: String, phase: &str, t: UnixMs) {
        self.flush_turn();
        let stop_reason = match phase {
            "final_answer" => Some("stop".to_string()),
            "commentary" => Some("toolUse".to_string()),
            _ => None,
        };
        self.turn = Some(PendingTurn { text, stop_reason, t });
    }

    fn user(&mut self, raw: &str, t: UnixMs) {
        let (text, reply) = match classify_user_text(raw) {
            UserText::Injected => return,
            UserText::Typed(s) => (s, false),
            UserText::Reply(s) => (s, true),
        };
        self.flush_turn();
        self.agent_end(t);
        // An input after an aborted turn, or a second input steering a running turn, arrives
        // while the agent is busy (the extension's `!ctx.isIdle()`); a question answer is the
        // input the agent waited for.
        let idle = reply || (self.last_stop != Some("aborted") && !(self.turn_open && self.turn_inputs > 0));
        self.last_stop = None;
        self.turn_inputs += 1;
        let label = prelabel(&text, self.last_input.as_deref(), idle);
        let trimmed = text.trim_start();
        let command = trimmed.starts_with(['/', '!', '%'])
            && !label.starts_with("mark:")
            && !trimmed.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("/skill:"));
        if command {
            return;
        }
        self.out.push(TraceRecord::Input {
            seq: self.seq(),
            t,
            text: head(&text, TRACE_INPUT_TEXT_CHARS),
            source: if reply { "question_reply".into() } else { String::new() },
            idle,
            after_deliverable: self.last_deliverable.as_deref().map(|d| head(d, TRACE_DELIVERABLE_CHARS)),
            prelabel: label.to_string(),
            skills_invoked: skills_invoked(&text),
        });
        self.last_input = Some(text);
    }

    fn tool(&mut self, r: ToolOut, t: UnixMs) {
        self.out.push(TraceRecord::ToolResult {
            seq: self.seq(),
            t,
            args_hash: args_hash(&r.input),
            input: truncate_tool_input(&r.input, TRACE_TOOL_INPUT_BYTES),
            result_bytes: r.output.len() as u64,
            is_error: r.is_error,
            error_class: r.is_error.then(|| "error".to_string()),
            content_head: head(&r.output, TRACE_CONTENT_HEAD_CHARS),
            skill_read: r.skill_read,
            path: r.path,
            skill_entry: None,
            tool: r.tool,
            call_id: r.call_id,
        });
        if let Some(turn) = &mut self.turn {
            turn.t = turn.t.max(t);
        }
    }

    fn task_end(&mut self, stop: &'static str, t: UnixMs) {
        self.flush_turn();
        self.last_stop = Some(stop);
        self.agent_end(t);
        self.turn_open = false;
        self.turn_inputs = 0;
    }
}

fn model_ref(provider: &str, id: &str) -> Option<ModelRef> {
    (!id.is_empty()).then(|| ModelRef { provider: provider.to_string(), id: id.to_string() })
}

/// The model a line names: `turn_context.model`, `thread_settings_applied.thread_settings`.
fn model_of_line(l: &Line, provider: &str) -> Option<ModelRef> {
    match (l.kind.as_str(), str_at(&l.payload, "type")) {
        ("turn_context", _) => model_ref(provider, str_at(&l.payload, "model")),
        ("event_msg", "thread_settings_applied") => {
            let ts = l.payload.get("thread_settings")?;
            let p = ts.get("model_provider_id").and_then(Value::as_str).filter(|p| !p.is_empty()).unwrap_or(provider);
            model_ref(p, str_at(ts, "model"))
        }
        _ => None,
    }
}

/// The thread item of an `event_msg/item_completed` line or an exec `item.completed` event.
fn completed_item(l: &Line) -> Option<&Value> {
    match (l.kind.as_str(), str_at(&l.payload, "type")) {
        ("event_msg", "item_completed") | ("item.completed", _) => l.payload.get("item"),
        _ => None,
    }
}

/// The extension's trace (DESIGN §3) synthesized from a Codex rollout (DESIGN §18.2):
///
/// * `session_start` — `codex:<id>`, `cwd`, repo via [`repo_of`], the first model named by
///   `turn_context`/`thread_settings_applied` (provider = `model_provider`), `mode:
///   "import"`, agent `main` (`sub` for reviewer/subagent threads), `omp_version` =
///   `codex <cli_version>`, no hooks; `loaded_entries` = the rematch entries among the skills
///   Codex listed to the model ([`CodexSession::offered_skills`], resolved by
///   `rematch_entry_of`) and `staging_dirs` = their directories.
/// * human input (`UserMessage` items, else `event_msg/user_message`, else response-item
///   user messages) → `input` with the live prelabel rules; ambient context, heartbeats and
///   system frames are dropped; idle unless the previous turn was aborted or a turn already
///   running receives a second input (steer); question answers are idle `question_reply`s.
///   Preceded by `agent_end` for the run it ends.
/// * tool items (`CommandExecution` → `bash`, `FileChange` → `apply_patch`, `McpToolCall` →
///   `mcp__<server>__<tool>`, `ImageView` → `view_image`, web search → `web_search`), else
///   `function_call`/`custom_tool_call` paired with their output by `call_id` → `tool_result`;
///   `is_error` from status/exit code; `skill_read` from a read of `skills/<name>/SKILL.md`.
/// * assistant message → `turn_end` after the tool results that follow it (phase
///   `final_answer` → `stop`, `commentary` → `toolUse`), with the usage recorded since the
///   previous `turn_end`, preceded by `model_change` when the model changed.
/// * `task_complete` / `turn_aborted` → `agent_end` (yielded / not); a final
///   `session_shutdown` with the extension's flag reasons.
pub fn trace_records_of(s: &CodexSession) -> Vec<TraceRecord> {
    let start_t = iso_ms(&s.timestamp).or_else(|| s.lines.iter().find_map(|l| l.t)).unwrap_or(0);
    let repo = repo_of(Path::new(&s.cwd)).map(|p| p.to_string_lossy().into_owned());
    let first_model = s.lines.iter().find_map(|l| model_of_line(l, &s.provider));
    let items_mode = s.lines.iter().any(|l| completed_item(l).is_some());
    let has_user_events = s.lines.iter().any(|l| l.kind == "event_msg" && str_at(&l.payload, "type") == "user_message");
    let has_usage_records = s.lines.iter().any(|l| l.kind == "token_usage_record");
    let (repo_remote, subpath) = crate::identity::session_place(repo.as_deref(), &s.cwd);
    let (mut loaded, mut visible, mut staging_dirs): (Vec<LoadedEntry>, Vec<String>, Vec<String>) = Default::default();
    for md in s.offered_skills() {
        let Some((entry, loads)) = rematch_entry_of(&md, start_t) else { continue };
        if !visible.contains(&entry.id) {
            visible.push(entry.id.clone());
        }
        if loads && !loaded.iter().any(|l| l.id == entry.id) {
            loaded.push(entry);
            staging_dirs.push(md.parent().unwrap_or(&md).to_string_lossy().into_owned());
        }
    }

    let mut w = Synth {
        out: Vec::new(),
        turn: None,
        usage: None,
        model: first_model.clone(),
        last_model: first_model.clone(),
        turns_since_agent_end: 0,
        last_stop: None,
        turn_open: false,
        turn_inputs: 0,
        last_input: None,
        last_deliverable: None,
    };
    w.out.push(TraceRecord::SessionStart {
        seq: 0,
        t: start_t,
        session_id: s.id.clone(),
        cwd: s.cwd.clone(),
        branch_hint: basename(repo.as_deref().unwrap_or(&s.cwd)),
        repo,
        model: first_model,
        mode: "import".into(),
        agent: Some(s.agent()),
        omp_version: if s.cli_version.is_empty() { "codex".into() } else { format!("codex {}", s.cli_version) },
        loaded_entries: loaded,
        hooks_loaded: Vec::new(),
        staging_dirs,
        repo_remote,
        subpath,
        head_sha: None,
        visible_entries: visible,
        instructions_exposed: Vec::new(),
        exposure_view: None,
    });

    let mut calls: HashMap<&str, &Value> = HashMap::new();
    let mut last_t = start_t;
    for l in &s.lines {
        let t = l.t.unwrap_or(last_t);
        last_t = t;
        if let Some(m) = model_of_line(l, &s.provider) {
            w.model = Some(m);
            continue;
        }
        let p = &l.payload;
        let ptype = str_at(p, "type");
        match (l.kind.as_str(), ptype) {
            ("event_msg", "task_started") | ("turn.started", _) => {
                w.turn_open = true;
                w.turn_inputs = 0;
            }
            ("event_msg", "task_complete") | ("turn.completed", _) => {
                if let Some(u) = p.get("usage").filter(|_| l.kind == "turn.completed") {
                    add_usage(&mut w.usage, usage_of(u));
                }
                w.task_end("stop", t);
            }
            ("event_msg", "turn_aborted") | ("turn.failed", _) => w.task_end("aborted", t),
            ("token_usage_record", _) => {
                if let Some(u) = p.get("usage") {
                    add_usage(&mut w.usage, usage_of(u));
                }
            }
            ("event_msg", "token_count") if !has_usage_records => {
                if let Some(u) = p.pointer("/info/last_token_usage") {
                    add_usage(&mut w.usage, usage_of(u));
                }
            }
            _ if items_mode => {
                let Some(item) = completed_item(l) else {
                    continue;
                };
                match item_kind(item).as_str() {
                    "usermessage" => w.user(&output_text(item.get("content")), t),
                    "agentmessage" => w.assistant(item_text(item), str_at(item, "phase"), t),
                    _ => {
                        if let Some(r) = tool_of_item(item) {
                            w.tool(r, t);
                        }
                    }
                }
            }
            ("event_msg", "user_message") => w.user(str_at(p, "message"), t),
            ("response_item", "message") => match str_at(p, "role") {
                "user" if !has_user_events => w.user(&content_text_blocks(p.get("content")), t),
                "assistant" => w.assistant(content_text_blocks(p.get("content")), str_at(p, "phase"), t),
                _ => {}
            },
            ("response_item", "function_call" | "custom_tool_call") => {
                if let Some(id) = p.get("call_id").and_then(Value::as_str) {
                    calls.insert(id, p);
                }
            }
            ("response_item", "function_call_output" | "custom_tool_call_output") => {
                let call = p.get("call_id").and_then(Value::as_str).and_then(|id| calls.get(id).copied());
                w.tool(tool_of_call(call, p), t);
            }
            _ => {}
        }
    }
    w.flush_turn();
    w.agent_end(last_t);
    let reasons = flag_reasons(&w.out);
    w.out.push(TraceRecord::SessionShutdown { seq: w.seq(), t: last_t, flagged: !reasons.is_empty(), reasons, files_changed: Vec::new() });
    w.out
}

/// Text blocks of a response-item message (`input_text` / `output_text`), joined.
fn content_text_blocks(content: Option<&Value>) -> String {
    match content {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| matches!(str_at(b, "type"), "input_text" | "output_text" | "text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        other => content_text(other),
    }
}

#[cfg(test)]
mod tests {
    #![cfg_attr(not(unix), allow(dead_code, unused_imports))] // helpers of Unix-only tests
    use super::*;

    fn line(ts: &str, kind: &str, payload: Value) -> String {
        format!("{}\n", json!({"timestamp": format!("2026-10-20T10:{ts}.000Z"), "type": kind, "payload": payload}))
    }

    fn item(ts: &str, item: Value) -> String {
        line(ts, "event_msg", json!({"type": "item_completed", "thread_id": "t", "turn_id": "x", "item": item}))
    }

    fn user(ts: &str, text: &str) -> String {
        item(ts, json!({"type": "UserMessage", "id": format!("u{ts}"), "content": [{"type": "text", "text": text, "text_elements": []}]}))
    }

    fn agent(ts: &str, text: &str, phase: &str) -> String {
        item(ts, json!({"type": "AgentMessage", "id": format!("m{ts}"), "content": [{"type": "Text", "text": text}], "phase": phase}))
    }

    fn cmd(ts: &str, script: &str, exit: i64, read: Option<&str>) -> String {
        let parsed = match read {
            Some(p) => json!([{"type": "read", "cmd": script, "name": "SKILL.md", "path": p}]),
            None => json!([{"type": "unknown", "cmd": script}]),
        };
        item(ts, json!({"type": "CommandExecution", "id": format!("exec-{ts}"), "command": ["/bin/zsh", "-lc", script],
            "cwd": "file:///repo", "parsed_cmd": parsed, "status": if exit == 0 { "completed" } else { "failed" },
            "aggregated_output": format!("out of {script}"), "exit_code": exit}))
    }

    fn ev(ts: &str, ty: &str) -> String {
        line(ts, "event_msg", json!({"type": ty, "turn_id": "x", "reason": "interrupted"}))
    }

    const ID: &str = "01a150dc-b64a-73f1-aa01-6dce7592ea06";

    fn meta(cwd: &Path, source: Value) -> String {
        line("00:00", "session_meta", json!({"id": ID, "session_id": ID, "timestamp": "2026-10-20T10:00:00.000Z", "cwd": cwd,
            "originator": "Codex Desktop", "cli_version": "0.163.0", "source": source, "thread_source": "user",
            "model_provider": "openai", "parent_thread_id": null}))
    }

    /// Items-mode rollout: ambient-wrapped request + `$skill` mention, skill read, a failing
    /// test run, usage, a file change, a revert redirect, an aborted turn followed by an
    /// input, a heartbeat, a question answer and a steer inside a running turn.
    fn items_rollout(repo: &Path) -> String {
        let file = repo.join("src/ledger.rs");
        [
            meta(repo, json!("vscode")),
            ev("00:01", "task_started"),
            line("00:01", "turn_context", json!({"turn_id": "t1", "cwd": repo, "model": "gpt-6.1-sol", "effort": "high"})),
            user("00:02", "\n<in-app-browser-context source=\"ambient-ui-state\">\nThe user has a tab open.\n</in-app-browser-context>\n\n## My request:\nAdd a reconcile function to the ledger and use $ledger-tests"),
            agent("00:03", "I'll read the skill first.", "commentary"),
            cmd("00:04", "sed -n '1,200p' .agents/skills/ledger-tests/SKILL.md", 0, Some(".agents/skills/ledger-tests/SKILL.md")),
            cmd("00:05", "cargo test -p ledger", 101, None),
            line("00:05", "token_usage_record", json!({"usage": {"input_tokens": 1000, "cached_input_tokens": 400, "cache_write_input_tokens": 0, "output_tokens": 50, "total_tokens": 1050}})),
            item("00:06", json!({"type": "FileChange", "id": "fc1", "status": "completed",
                "changes": {format!("file://{}", file.display()): {"type": "update", "unified_diff": "@@"}}})),
            agent("00:07", "Added reconcile() to the ledger.", "final_answer"),
            line("00:07", "token_usage_record", json!({"usage": {"input_tokens": 1200, "cached_input_tokens": 1000, "output_tokens": 30, "total_tokens": 1230}})),
            ev("00:08", "task_complete"),
            ev("00:10", "task_started"),
            user("00:10", "No, that's wrong: revert the change to the ledger"),
            item("00:11", json!({"type": "McpToolCall", "id": "call_1", "server": "cua_repl", "tool": "js", "arguments": {"code": "1"},
                "status": "failed", "result": {"content": [{"type": "text", "text": "timed out"}], "isError": true}})),
            ev("00:12", "turn_aborted"),
            user("00:13", "<heartbeat>\n<automation_id>a</automation_id>\n</heartbeat>"),
            ev("00:14", "task_started"),
            user("00:14", "Keep the reconcile function but rename it"),
            user("00:15", "<send_user_message_question_reply>\n[{\"questionItemId\":\"q\",\"question\":\"Which name?\",\"answer\":\"Call it settle\"}]\n</send_user_message_question_reply>"),
            user("00:16", "also update the docs while you are at it"),
            line("00:16", "turn_context", json!({"turn_id": "t3", "cwd": repo, "model": "gpt-6-astra"})),
            agent("00:17", "Renamed to settle().", "final_answer"),
            ev("00:18", "task_complete"),
        ]
        .concat()
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::paths::canonicalize(tmp.path()).unwrap().join("payments");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        (tmp, repo)
    }

    fn inputs(recs: &[TraceRecord]) -> Vec<(String, String, bool, String)> {
        recs.iter()
            .filter_map(|r| match r {
                TraceRecord::Input { text, prelabel, idle, source, .. } => {
                    Some((text.clone(), prelabel.clone(), *idle, source.clone()))
                }
                _ => None,
            })
            .collect()
    }

    // POSIX-only: the fixture writes `file://` URLs from native paths.
    #[cfg(unix)]
    #[test]
    fn items_rollout_maps_to_the_trace_schema() {
        let (_t, repo) = repo();
        let path = repo.join(format!("rollout-2026-10-20T10-00-00-{ID}.jsonl"));
        let s = parse_session(&items_rollout(&repo), Some(&path)).unwrap();
        assert_eq!(s.id, format!("codex:{ID}"));
        assert!(!s.is_subagent());
        let recs = trace_records_of(&s);
        for (i, r) in recs.iter().enumerate() {
            assert_eq!(r.seq() as usize, i, "seq = index");
        }
        let TraceRecord::SessionStart { session_id, repo: r, model, mode, agent, omp_version, .. } = &recs[0] else {
            panic!("{:?}", recs[0])
        };
        assert_eq!(session_id, &format!("codex:{ID}"));
        assert_eq!(r.as_deref(), Some(repo.to_str().unwrap()));
        assert_eq!(model.as_ref().map(|m| (m.provider.as_str(), m.id.as_str())), Some(("openai", "gpt-6.1-sol")));
        assert_eq!((mode.as_str(), agent.as_ref().unwrap().kind.as_str(), omp_version.as_str()), ("import", "main", "codex 0.163.0"));

        let ins = inputs(&recs);
        let got: Vec<(&str, &str, bool, &str)> =
            ins.iter().map(|(t, p, i, s)| (t.as_str(), p.as_str(), *i, s.as_str())).collect();
        assert_eq!(
            got,
            vec![
                ("Add a reconcile function to the ledger and use $ledger-tests", "none", true, ""),
                ("No, that's wrong: revert the change to the ledger", "revert", true, ""),
                ("Keep the reconcile function but rename it", "interrupt", false, ""),
                ("Call it settle", "none", true, "question_reply"),
                ("also update the docs while you are at it", "interrupt", false, ""),
            ],
            "ambient blocks stripped, heartbeat dropped, abort and steer are interrupts"
        );
        let TraceRecord::Input { skills_invoked, after_deliverable, .. } =
            recs.iter().find(|r| matches!(r, TraceRecord::Input { .. })).unwrap()
        else {
            unreachable!()
        };
        assert_eq!(skills_invoked, &vec!["ledger-tests".to_string()]);
        assert_eq!(after_deliverable, &None);

        let tools: Vec<(&str, bool, Option<&str>, Option<&str>)> = recs
            .iter()
            .filter_map(|r| match r {
                TraceRecord::ToolResult { tool, is_error, skill_read, path, .. } => {
                    Some((tool.as_str(), *is_error, skill_read.as_deref(), path.as_deref()))
                }
                _ => None,
            })
            .collect();
        let file = repo.join("src/ledger.rs");
        assert_eq!(
            tools,
            vec![
                ("bash", false, Some("ledger-tests"), Some(".agents/skills/ledger-tests/SKILL.md")),
                ("bash", true, None, None),
                ("apply_patch", false, None, Some(file.to_str().unwrap())),
                ("mcp__cua_repl__js", true, None, None),
            ]
        );
        let TraceRecord::ToolResult { input, content_head, .. } =
            recs.iter().find(|r| matches!(r, TraceRecord::ToolResult { .. })).unwrap()
        else {
            unreachable!()
        };
        assert_eq!(input["command"], "sed -n '1,200p' .agents/skills/ledger-tests/SKILL.md");
        assert_eq!(input["cwd"], "/repo");
        assert!(content_head.starts_with("out of sed"));

        let turns: Vec<String> = recs
            .iter()
            .filter_map(|r| match r {
                TraceRecord::TurnEnd { model, stop_reason, usage: Some(u), .. } => Some(format!(
                    "{} {} {}/{}/{}",
                    model.as_ref().map_or("-", |m| m.id.as_str()),
                    stop_reason.as_deref().unwrap_or("-"),
                    u.input,
                    u.cache_read,
                    u.output
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            turns,
            vec!["gpt-6.1-sol toolUse 600/400/50", "gpt-6.1-sol stop 200/1000/30", "gpt-6-astra stop 0/0/0"],
            "usage since the previous turn_end; the commentary turn ends after its tool results"
        );
        let pos = |ev: &str| recs.iter().position(|r| r.ev() == ev).unwrap();
        assert!(pos("turn_end") > recs.iter().position(|r| matches!(r, TraceRecord::ToolResult { is_error: true, .. })).unwrap());
        assert!(recs.iter().any(|r| matches!(r, TraceRecord::ModelChange { to: Some(m), .. } if m.id == "gpt-6-astra")));
        let ends: Vec<bool> = recs
            .iter()
            .filter_map(|r| match r {
                TraceRecord::AgentEnd { yielded, .. } => Some(*yielded),
                _ => None,
            })
            .collect();
        assert_eq!(ends, vec![true, true], "the aborted turn had no assistant message");
        let TraceRecord::SessionShutdown { reasons, flagged, .. } = recs.last().unwrap() else { panic!() };
        assert!(*flagged);
        assert_eq!(reasons, &vec!["redirect:revert".to_string(), "redirect:interrupt".to_string()]);

        let row = session_record(&s);
        assert_eq!((row.id.as_str(), row.source.as_str()), (format!("codex:{ID}").as_str(), "import"));
        assert_eq!(row.source_path.as_deref(), Some(path.to_str().unwrap()));
        assert_eq!(row.model.as_ref().map(|m| m.id.as_str()), Some("gpt-6.1-sol"));
        assert_eq!(row.skill_reads, vec!["ledger-tests".to_string()]);
        assert!(row.files_touched.contains(&file.to_string_lossy().into_owned()), "{:?}", row.files_touched);
        assert_eq!((row.tool_calls, row.tool_errors, row.turns, row.takeovers), (4, 2, 3, 2));
        assert_eq!((row.tokens_in, row.tokens_out), (2200, 80));
        assert_eq!(row.redirects.len(), 3);
        assert!(row.flagged);
        assert_eq!(row.branch, "payments");
    }

    /// Without thread items: `response_item`s carry the conversation (environment context
    /// dropped, `exec_command` exit codes, `apply_patch` paths, error cluster).
    #[test]
    fn response_items_fallback_and_error_cluster() {
        let (_t, repo) = repo();
        let call = |ts: &str, id: &str, cmd: &str| {
            line(ts, "response_item", json!({"type": "function_call", "name": "exec_command", "call_id": id,
                "arguments": json!({"cmd": cmd, "yield_time_ms": 1000}).to_string()}))
        };
        let out = |ts: &str, id: &str, code: i64| {
            line(ts, "response_item", json!({"type": "function_call_output", "call_id": id,
                "output": format!("Chunk ID: 1\nWall time: 0.1 seconds\nProcess exited with code {code}\nOutput:\nerror[E0425]")}))
        };
        let msg = |ts: &str, role: &str, text: &str| {
            let ty = if role == "assistant" { "output_text" } else { "input_text" };
            line(ts, "response_item", json!({"type": "message", "role": role, "content": [{"type": ty, "text": text}]}))
        };
        let text = [
            meta(&repo, json!("exec")),
            line("00:01", "turn_context", json!({"cwd": repo, "model": "gpt-6.1-sol"})),
            msg("00:01", "developer", "You are Codex"),
            msg("00:01", "user", "<environment_context>\n  <cwd>/x</cwd>\n</environment_context>"),
            msg("00:02", "user", "Fix the build of the payments crate"),
            call("00:03", "c1", "cargo build"),
            out("00:04", "c1", 101),
            call("00:05", "c2", "cargo build --offline"),
            out("00:06", "c2", 101),
            line("00:07", "response_item", json!({"type": "custom_tool_call", "name": "apply_patch", "call_id": "c3",
                "input": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n*** End Patch"})),
            line("00:07", "response_item", json!({"type": "custom_tool_call_output", "call_id": "c3", "output": "Success"})),
            call("00:08", "c4", "cat ~/.agents/skills/rust-build/SKILL.md"),
            out("00:09", "c4", 1),
            msg("00:10", "assistant", "The build still fails."),
        ]
        .concat();
        let s = parse_session(&text, None).unwrap();
        let recs = trace_records_of(&s);
        assert_eq!(inputs(&recs).iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), vec!["Fix the build of the payments crate"]);
        let errs: Vec<(String, bool)> = recs
            .iter()
            .filter_map(|r| match r {
                TraceRecord::ToolResult { tool, is_error, .. } => Some((tool.clone(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            errs,
            vec![("bash".into(), true), ("bash".into(), true), ("apply_patch".into(), false), ("bash".into(), true)]
        );
        let TraceRecord::SessionShutdown { reasons, flagged, .. } = recs.last().unwrap() else { panic!() };
        assert!(*flagged);
        assert_eq!(reasons, &vec!["tool_error_cluster".to_string()]);
        let row = session_record(&s);
        assert!(row.files_touched.contains(&"src/lib.rs".to_string()), "{:?}", row.files_touched);
        assert_eq!(row.skill_reads, vec!["rust-build".to_string()]);
        assert_eq!(row.error_clusters, 1);
        assert_eq!(row.id, format!("codex:{ID}"));
    }

    #[test]
    fn subagent_threads_and_exec_json_stream() {
        let (_t, repo) = repo();
        let s = parse_session(&meta(&repo, json!({"subagent": {"other": "guardian"}})), None).unwrap();
        assert!(s.is_subagent());
        let TraceRecord::SessionStart { agent, .. } = &trace_records_of(&s)[0] else { panic!() };
        assert_eq!(agent.as_ref().map(|a| (a.kind.as_str(), a.name.as_str())), Some(("sub", "guardian")));

        // `codex exec --json`: flat events, snake_case items, usage on turn.completed.
        let stream = [
            json!({"type": "thread.started", "thread_id": "0199a213-81c0-7800-8aa1-bbab2a035a53"}),
            json!({"type": "turn.started"}),
            json!({"type": "item.completed", "item": {"id": "item_1", "type": "command_execution", "command": "bash -lc 'cargo test'",
                "aggregated_output": "test result: FAILED", "exit_code": 101, "status": "failed"}}),
            json!({"type": "item.completed", "item": {"id": "item_2", "type": "file_change", "status": "completed",
                "changes": [{"path": "src/lib.rs", "kind": "update"}]}}),
            json!({"type": "item.completed", "item": {"id": "item_3", "type": "agent_message", "text": "Fixed it."}}),
            json!({"type": "turn.completed", "usage": {"input_tokens": 500, "cached_input_tokens": 100, "output_tokens": 40}}),
        ]
        .iter()
        .map(|v| format!("{v}\n"))
        .collect::<String>();
        let s = parse_session(&stream, None).unwrap();
        assert_eq!(s.id, "codex:0199a213-81c0-7800-8aa1-bbab2a035a53");
        let recs = trace_records_of(&s);
        let evs: Vec<&str> = recs.iter().map(TraceRecord::ev).collect();
        assert_eq!(evs, vec!["session_start", "tool_result", "tool_result", "turn_end", "agent_end", "session_shutdown"]);
        let TraceRecord::TurnEnd { usage, text_head, .. } = &recs[3] else { panic!() };
        assert_eq!((usage.as_ref().unwrap().input, usage.as_ref().unwrap().cache_read), (400, 100));
        assert_eq!(text_head, "Fixed it.");
        assert!(matches!(&recs[1], TraceRecord::ToolResult { is_error: true, .. }));
        assert!(matches!(&recs[2], TraceRecord::ToolResult { path: Some(p), .. } if p == "src/lib.rs"));
    }

    /// The `## Skills` list Codex gave the model → rematch entries: links in the shared
    /// skills dir to a provisional (staging) and a kept entry dir, each with `rematch.json`, and
    /// neither a foreign skill, a hook's router (Codex never runs it) nor an entry created after
    /// the session started.
    #[cfg(unix)]
    #[test]
    fn loaded_entries_from_the_offered_skill_list() {
        let (_t, repo) = repo();
        let base = repo.parent().unwrap().to_path_buf();
        let staged = base.join("rematch/staging/payments/ledger-tests");
        let newer = base.join("rematch/staging/payments/too-new");
        let kept = base.join("omp/agent/skills/kept-one");
        let hook = base.join("rematch/staging/shared/stuck-detector");
        let links = base.join("rematch/skills");
        let skills = base.join("home/.agents/skills");
        for d in [&staged, &newer, &kept, &hook, &links, &skills.join("mine")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let started = iso_ms("2026-10-20T10:00:00.000Z").unwrap();
        std::fs::write(staged.join("rematch.json"), json!({"id": "e-prov", "created_at": started - 1, "version": 2, "content_sha256": "ab"}).to_string()).unwrap();
        std::fs::write(newer.join("rematch.json"), json!({"id": "e-new", "created_at": started + 1}).to_string()).unwrap();
        std::fs::write(kept.join("rematch.json"), json!({"id": "e-kept", "created_at": 0}).to_string()).unwrap();
        std::fs::write(hook.join("rematch.json"), json!({"id": "e-hook", "kind": "hook", "created_at": 0}).to_string()).unwrap();
        std::os::unix::fs::symlink(&staged, links.join("ledger-tests")).unwrap();
        std::os::unix::fs::symlink(&newer, links.join("too-new")).unwrap();
        std::os::unix::fs::symlink(&kept, links.join("kept-one")).unwrap();
        std::os::unix::fs::symlink(&hook, links.join("stuck-detector")).unwrap();
        std::os::unix::fs::symlink(&links, skills.join("rematch")).unwrap();
        let body = format!(
            "## Skills\nA skill is …\n### Skill roots\n- `r0` = `{}`\n- `r1` = `/opt/codex/.system`\n### Available skills\n\
             - ledger-tests: Use when testing the ledger. (file: r0/rematch/ledger-tests/SKILL.md)\n\
             - too-new: Use when x. (file: r0/rematch/too-new/SKILL.md)\n\
             - kept-one: Use when y. (file: r0/rematch/kept-one/SKILL.md)\n\
             - stuck-detector: Use when stuck. (file: r0/rematch/stuck-detector/SKILL.md)\n\
             - mine: Use when z. (file: r0/mine/SKILL.md)\n\
             - imagegen: Generate images. (file: r1/imagegen/SKILL.md)\n",
            skills.display()
        );
        let text = [
            meta(&repo, json!("vscode")),
            line("00:00", "world_state", json!({"full": true, "state": {"host_skills": {"body": body}, "skills": {"includeInstructions": true}}})),
            line("00:01", "response_item", json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": body}]})),
            user("00:02", "Add the ledger reconciliation"),
        ]
        .concat();
        let s = parse_session(&text, None).unwrap();
        assert_eq!(s.offered_skills().len(), 6, "{:?}", s.offered_skills());
        assert_eq!(s.offered_skills()[5], PathBuf::from("/opt/codex/.system/imagegen/SKILL.md"));
        let TraceRecord::SessionStart { loaded_entries, staging_dirs, visible_entries, .. } = &trace_records_of(&s)[0] else { panic!() };
        let prov = LoadedEntry { id: "e-prov".into(), version: Some(2), sha: Some("ab".into()) };
        assert_eq!(loaded_entries, &vec![prov, LoadedEntry::id("e-kept")]);
        assert_eq!(visible_entries, &vec!["e-prov".to_string(), "e-kept".to_string(), "e-hook".to_string()], "the hook router is visible, not loaded");
        assert_eq!(staging_dirs[0], skills.join("rematch/ledger-tests").to_string_lossy());
        let r = session_record(&s);
        assert_eq!(r.loaded_entries, vec!["e-prov".to_string(), "e-kept".to_string()]);
        assert_eq!((r.loaded_versions.get("e-prov"), r.loaded_entry_shas.clone()), (Some(&2), vec!["ab".to_string()]));
    }

    #[test]
    fn scan_find_and_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("2026/10/20");
        std::fs::create_dir_all(&day).unwrap();
        let f = day.join(format!("rollout-2026-10-20T10-00-00-{ID}.jsonl"));
        std::fs::write(&f, "").unwrap();
        std::fs::write(day.join("notes.jsonl"), "").unwrap();
        assert_eq!(scan_sessions_dir(tmp.path()).unwrap(), vec![f.clone()]);
        assert_eq!(find_session_file(tmp.path(), &format!("codex:{ID}")), Some(f.clone()));
        assert_eq!(find_session_file(tmp.path(), "codex:"), None);
        assert_eq!(id_from_filename(&f).as_deref(), Some(ID));
        assert!(parse_session("not json\n", None).is_err());
        assert_eq!(file_path("file:///a%20b/c"), "/a b/c");
        assert_eq!(file_path("src/x"), "src/x");
        assert_eq!(file_path("file:///C:/Users/u/repo"), if cfg!(windows) { "C:/Users/u/repo" } else { "/C:/Users/u/repo" });
        assert_eq!(skill_in_command("cat \"/u/.agents/skills/x-y/SKILL.md\" | head"), Some("x-y".into()));
        assert_eq!(skills_invoked("use $foo-bar and $HOME then [$baz](skill://baz)"), vec!["foo-bar", "baz"]);
    }

    /// `REMATCH_REAL_HISTORY=1`: every rollout under `~/.codex/sessions` (read-only) parses
    /// and yields a well-formed trace.
    #[test]
    fn real_history_smoke() {
        if std::env::var("REMATCH_REAL_HISTORY").as_deref() != Ok("1") {
            return;
        }
        let Some(dir) = default_sessions_dir().filter(|d| d.is_dir()) else {
            eprintln!("no Codex sessions dir");
            return;
        };
        let files = scan_sessions_dir(&dir).unwrap();
        let (mut flagged, mut subs, mut inputs_n, mut tools_n, mut skills) = (0, 0, 0, 0, 0);
        let mut labels: std::collections::BTreeMap<String, u32> = Default::default();
        for f in &files {
            let s = read_session(f).unwrap_or_else(|e| panic!("{}: {e:#}", f.display()));
            assert!(s.id.starts_with(ID_PREFIX), "{}", f.display());
            let recs = trace_records_of(&s);
            assert!(matches!(recs.first(), Some(TraceRecord::SessionStart { .. })));
            assert!(matches!(recs.last(), Some(TraceRecord::SessionShutdown { .. })));
            let row = session_record(&s);
            flagged += row.flagged as u32;
            subs += s.is_subagent() as u32;
            inputs_n += recs.iter().filter(|r| r.ev() == "input").count();
            for r in recs.iter().filter(|_| !s.is_subagent()) {
                if let TraceRecord::Input { prelabel, .. } = r {
                    *labels.entry(prelabel.clone()).or_default() += 1;
                }
            }
            tools_n += row.tool_calls;
            skills += row.skill_reads.len();
        }
        eprintln!(
            "codex real history: {} files, {subs} subagent, {flagged} flagged, {inputs_n} inputs, {tools_n} tool results, {skills} skill reads; main-session prelabels {labels:?}",
            files.len()
        );
    }
}
