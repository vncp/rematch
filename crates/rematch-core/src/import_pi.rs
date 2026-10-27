//! Historical importer for Pi (`@earendil-works/pi-coding-agent`, DESIGN §18.3): session JSONL
//! (`<pi agent dir>/sessions/--<cwd>--/<ts>_<id>.jsonl`, or a flat `PI_CODING_AGENT_SESSION_DIR`)
//! → the trace the Pi adapter writes live (`adapters/pi/index.ts`), synthesized after the fact.
//!
//! Format facts relied on (Pi 1.4.0 `docs/session-format.md`, `core/session-manager.js`,
//! `core/agent-session.js`): line 1 is the `session` header (`version` 1–3, `id`, `cwd`,
//! `timestamp`, `parentSession` for a fork/clone); entries form a tree through `id`/`parentId`
//! (version 1 files are linear) and the active branch ends at the last entry, as in omp's format
//! ([`crate::import::parse_session`] reads both). A `/fork` or `/clone` copies the branch up to
//! the fork point into the new file with the original entry timestamps, so entries older than
//! the header are the parent's prefix: they set the context (previous input, deliverable, model)
//! but are not this session's evidence. `/skill:<name> <args>` is stored expanded as
//! `<skill name="<name>" location="…">…</skill>\n\n<args>`; it is traced as the typed
//! `/skill:<name> <args>`. `system` messages carry the prompt sections; their
//! `<available_skills>` list names the `SKILL.md` locations the model was offered. Pi has no
//! subagents: every session is a main session. The file records no input source, so a user
//! message an extension sent (`source: "extension"` live, which the adapter skips) cannot be told
//! from a typed one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;
use serde_json::Value;

use crate::features::{head, strip_selector};
use crate::import::{
    args_hash, content_bytes, content_text, entry_ms, is_injected_frame, iso_ms, message, model_ref, parse_session, repo_of,
    tool_calls, truncate_tool_input, SessionHeader, TRACE_CONTENT_HEAD_CHARS, TRACE_DELIVERABLE_CHARS, TRACE_INPUT_TEXT_CHARS,
    TRACE_TOOL_INPUT_BYTES,
};
use crate::redirect::prelabel;
use crate::trace::{
    basename, flag_reasons, fold_trace, push_unique, skill_from_read_path, skills_in_text, AgentInfo, LoadedEntry, TraceRecord,
    Usage,
};
use crate::types::{ModelRef, SessionRecord, UnixMs};

/// Ledger/trace id prefix of Pi sessions: `pi:<id>`; a session resumed after its trace was
/// finished continues in segment `pi:<id>:r<n>` (the live adapter's rule).
pub const ID_PREFIX: &str = "pi:";

/// `pi:<id>` with every character outside `[A-Za-z0-9._-]` replaced by `_` (Pi accepts custom
/// `--session-id`s; ids become file names). The live adapter applies the same rule.
pub fn session_id(raw: &str) -> String {
    let clean: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
    format!("{ID_PREFIX}{clean}")
}

/// The Pi session id behind a ledger id: `pi:<id>` or a resumed segment `pi:<id>:r<n>`.
pub fn base_id(ledger_id: &str) -> Option<&str> {
    let raw = ledger_id.strip_prefix(ID_PREFIX)?;
    let raw = match raw.rsplit_once(":r") {
        Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => raw,
    };
    (!raw.is_empty()).then_some(raw)
}

/// Pi's agent dir as Pi resolves it (`config.js` `getAgentDir`): `REMATCH_PI_AGENT_DIR`
/// (exported by the adapter to the binaries it runs), else `PI_CODING_AGENT_DIR`, else
/// `~/.pi/agent`. omp reads the same `PI_CODING_AGENT_*` names, so a value inside omp's config
/// root (`omp_config_root`, `~/.omp`) is omp's and is not taken for Pi's.
pub fn default_agent_dir(omp_config_root: &Path) -> Option<PathBuf> {
    agent_dir_from(|k| std::env::var(k).ok(), omp_config_root)
}

/// Pi's sessions directory (`main.js` `ENV_SESSION_DIR`): `REMATCH_PI_SESSION_DIR` (exported
/// by the adapter) or `PI_CODING_AGENT_SESSION_DIR` (outside omp's config root), else
/// `<agent dir>/sessions` ([`default_agent_dir`]).
pub fn default_sessions_dir(omp_config_root: &Path) -> Option<PathBuf> {
    sessions_dir_from(|k| std::env::var(k).ok(), omp_config_root)
}

struct Env<F: Fn(&str) -> Option<String>> {
    get: F,
}

impl<F: Fn(&str) -> Option<String>> Env<F> {
    fn non_empty(&self, k: &str) -> Option<String> {
        (self.get)(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    }

    /// Pi's `os.homedir()`: `$HOME`, else `%USERPROFILE%` on Windows.
    fn home(&self) -> Option<PathBuf> {
        crate::paths::home_from(&self.get)
    }

    fn path(&self, k: &str) -> Option<PathBuf> {
        let v = self.non_empty(k)?;
        Some(match (v.strip_prefix("~/"), self.home()) {
            (Some(rest), Some(h)) => h.join(rest),
            _ => PathBuf::from(v),
        })
    }

    /// A `PI_CODING_AGENT_*` value that is not omp's.
    fn pi_own(&self, k: &str, omp_config_root: &Path) -> Option<PathBuf> {
        self.path(k).filter(|p| !p.starts_with(omp_config_root))
    }
}

pub fn agent_dir_from(get: impl Fn(&str) -> Option<String>, omp_config_root: &Path) -> Option<PathBuf> {
    let env = Env { get };
    env.path("REMATCH_PI_AGENT_DIR")
        .or_else(|| env.pi_own("PI_CODING_AGENT_DIR", omp_config_root))
        .or_else(|| env.home().map(|h| h.join(".pi").join("agent")))
}

pub fn sessions_dir_from(get: impl Fn(&str) -> Option<String>, omp_config_root: &Path) -> Option<PathBuf> {
    let env = Env { get };
    if let Some(d) = env.path("REMATCH_PI_SESSION_DIR").or_else(|| env.pi_own("PI_CODING_AGENT_SESSION_DIR", omp_config_root)) {
        return Some(d);
    }
    agent_dir_from(env.get, omp_config_root).map(|a| a.join("sessions"))
}

/// Every `*.jsonl` under `dir` (recursively) whose first line is a Pi `session` header, sorted by path.
pub fn scan_sessions_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
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
    paths.retain(|p| is_session_file(p));
    paths.sort();
    Ok(paths)
}

fn is_session_file(p: &Path) -> bool {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(p) else { return false };
    let mut first = String::new();
    if std::io::BufReader::new(f).read_line(&mut first).is_err() {
        return false;
    }
    serde_json::from_str::<Value>(first.trim()).is_ok_and(|v| v.get("type").and_then(Value::as_str) == Some("session"))
}

/// The session file of a ledger id (`pi:<id>[:r<n>]`) under `dir`: `<ts>_<id>.jsonl`, matched on
/// the sanitized id.
pub fn find_session_file(dir: &Path, ledger_id: &str) -> Option<PathBuf> {
    let raw = base_id(ledger_id)?;
    scan_sessions_dir(dir).ok()?.into_iter().find(|p| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.split_once('_'))
            .is_some_and(|(_, id)| session_id(id).strip_prefix(ID_PREFIX) == Some(raw))
    })
}

/// A parsed Pi session: header, active branch (root → leaf), the display name (`session_info`).
#[derive(Debug, Clone, Default)]
pub struct PiSession {
    pub path: Option<PathBuf>,
    pub header: SessionHeader,
    pub entries: Vec<Value>,
    pub name: Option<String>,
}

impl PiSession {
    /// `pi:<header id>` (the file name's id when the header has none).
    pub fn id(&self) -> String {
        if self.header.id.is_empty() {
            String::new()
        } else {
            session_id(&self.header.id)
        }
    }

    /// Pi has no subagents (its forks are main sessions with a `parentSession`).
    pub fn is_subagent(&self) -> bool {
        false
    }

    fn start(&self) -> UnixMs {
        iso_ms(&self.header.timestamp).or_else(|| self.entries.first().and_then(entry_ms)).unwrap_or(0)
    }

    /// Entries a `/fork` or `/clone` copied from the parent: older than this file's header.
    fn inherited(&self, e: &Value) -> bool {
        self.header.parent_session.is_some() && entry_ms(e).is_some_and(|t| t < self.start())
    }

    /// Absolute `SKILL.md` locations of every `<available_skills>` list in the session's own
    /// system messages (prompt sections, compaction checkpoints), first-seen order.
    pub fn offered_skills(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut scan = |m: &Value| {
            let mut texts = vec![content_text(m.get("content"))];
            if let Some(sections) = m.get("sections").and_then(Value::as_object) {
                texts.extend(sections.values().filter_map(Value::as_str).map(str::to_string));
            }
            for text in texts {
                for c in SKILL_LOCATION_RE.captures_iter(&text) {
                    let p = PathBuf::from(unescape_xml(&c[1]));
                    if p.is_absolute() && !out.contains(&p) {
                        out.push(p);
                    }
                }
            }
        };
        for e in &self.entries {
            match (message(e), e.get("type").and_then(Value::as_str)) {
                (Some(("system", m)), _) => scan(m),
                (_, Some("compaction")) => {
                    if let Some(m) = e.get("systemMessage") {
                        scan(m);
                    }
                }
                _ => {}
            }
        }
        out
    }
}

static SKILL_LOCATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<location>([^<\n]+SKILL\.md)</location>").expect("static pattern"));
static SKILL_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)^<skill name="([^"]+)" location="[^"]*">\n.*?\n</skill>(?:\n\n(.*))?$"#).expect("static pattern")
});

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// A stored user text as typed: an expanded skill command back to `/skill:<name> <args>`.
fn typed_text(stored: &str) -> String {
    match SKILL_BLOCK_RE.captures(stored) {
        Some(c) => match c.get(2).map(|a| a.as_str().trim()).filter(|a| !a.is_empty()) {
            Some(args) => format!("/skill:{} {args}", &c[1]),
            None => format!("/skill:{}", &c[1]),
        },
        None => stored.to_string(),
    }
}

pub fn read_session(path: &Path) -> Result<PiSession> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read session {}", path.display()))?;
    parse_pi_session(&text, Some(path))
}

pub fn parse_pi_session(text: &str, path: Option<&Path>) -> Result<PiSession> {
    let s = parse_session(text, path)?;
    let mut header = s.header;
    if header.id.is_empty() {
        // `<ts>_<id>.jsonl`; Pi timestamps hold no `_`.
        header.id = path
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .and_then(|s| s.split_once('_'))
            .map(|(_, id)| id.to_string())
            .unwrap_or_default();
    }
    let name = s
        .entries
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("session_info"))
        .and_then(|e| e.get("name").and_then(Value::as_str))
        .map(str::to_string);
    Ok(PiSession { path: s.path, header, entries: s.entries, name })
}

/// Ledger row: the fold of [`trace_records_of`], source `import`, the session file as `source_path`.
pub fn session_record(s: &PiSession) -> SessionRecord {
    let mut r = fold_trace(&trace_records_of(s), &s.id());
    r.source = "import".into();
    r.source_path = s.path.as_ref().map(|p| p.to_string_lossy().into_owned());
    r
}

struct Synth<'a> {
    out: Vec<TraceRecord>,
    /// Assistant message whose `turn_end` is due once its tool results are written, with the
    /// latest time seen for that turn.
    turn: Option<(&'a Value, UnixMs)>,
    turns_since_agent_end: u32,
    last_stop: Option<&'a str>,
    /// The entry before a user message was a tool result: the message was steered into a run.
    after_tool_result: bool,
    last_model: Option<ModelRef>,
    last_input: Option<String>,
    last_deliverable: Option<String>,
}

impl<'a> Synth<'a> {
    fn seq(&self) -> u32 {
        self.out.len() as u32
    }

    fn flush_turn(&mut self) {
        let Some((m, t)) = self.turn.take() else { return };
        let model = model_ref(m);
        if let (Some(to), Some(from)) = (&model, &self.last_model) {
            if to != from {
                self.out.push(TraceRecord::ModelChange { seq: self.seq(), t, from: Some(from.clone()), to: Some(to.clone()), reason: Some("turn".into()) });
            }
        }
        if model.is_some() {
            self.last_model = model.clone();
        }
        let usage = m.get("usage").and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok()).unwrap_or_default();
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

/// The Pi adapter's trace (DESIGN §3, §18.3) synthesized from a session's active branch:
///
/// * `session_start` — `pi:<id>`, header cwd, repo via [`repo_of`], model of the first own
///   assistant message (else the first `model_change`), `mode: "import"`, agent `main`,
///   `omp_version: "pi"` (the file records no runtime version), `loaded_entries` = the rematch
///   entries among the offered skills ([`PiSession::offered_skills`], hooks/agents/lessons only
///   visible), `staging_dirs` = their dirs.
/// * user message → `input` with the live prelabel rules: `interrupt` only when it arrived after
///   a tool result that was not an abort (steered into a running run; the file records no queue
///   mode); a prompt after an Esc abort is idle, as live; slash/`!`/`%`
///   lines other than `/rematch mark` and `/skill:` dropped, injected frames dropped, an expanded
///   skill block traced as `/skill:<name> <args>`. Preceded by `agent_end` for the run it ends.
/// * `toolResult` → `tool_result` paired with its `toolCall` by id; `skill_read` only for a
///   successful `read` of a `SKILL.md`.
/// * assistant message → `turn_end` after its tool results (`model_change` first on a change).
/// * a final `agent_end` and `session_shutdown` with the adapter's flag reasons.
///
/// Entries a fork copied from its parent update the context only.
pub fn trace_records_of(s: &PiSession) -> Vec<TraceRecord> {
    let header = &s.header;
    let start_t = s.start();
    let repo = repo_of(Path::new(&header.cwd)).map(|p| p.to_string_lossy().into_owned());
    let (repo_remote, subpath) = crate::identity::session_place(repo.as_deref(), &header.cwd);
    let own = |e: &&Value| !s.inherited(e);
    let model = s
        .entries
        .iter()
        .filter(own)
        .filter_map(message)
        .find(|(role, _)| *role == "assistant")
        .and_then(|(_, m)| model_ref(m))
        .or_else(|| {
            s.entries.iter().filter(own).find(|e| e.get("type").and_then(Value::as_str) == Some("model_change")).and_then(|e| {
                Some(ModelRef { provider: e.get("provider")?.as_str()?.to_string(), id: e.get("modelId")?.as_str()?.to_string() })
            })
        });
    let (mut loaded, mut visible, mut staging_dirs): (Vec<LoadedEntry>, Vec<String>, Vec<String>) = Default::default();
    for md in s.offered_skills() {
        let Some((entry, loads)) = crate::import_codex::rematch_entry_of(&md, start_t) else { continue };
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
        turns_since_agent_end: 0,
        last_stop: None,
        after_tool_result: false,
        last_model: model.clone(),
        last_input: None,
        last_deliverable: None,
    };
    w.out.push(TraceRecord::SessionStart {
        seq: 0,
        t: start_t,
        session_id: s.id(),
        cwd: header.cwd.clone(),
        branch_hint: basename(repo.as_deref().unwrap_or(&header.cwd)),
        repo,
        model,
        mode: "import".into(),
        agent: Some(AgentInfo { kind: "main".into(), name: "main".into() }),
        omp_version: "pi".into(),
        loaded_entries: loaded,
        hooks_loaded: Vec::new(),
        staging_dirs,
        repo_remote,
        subpath,
        head_sha: None,
        visible_entries: visible,
    });

    let mut calls: HashMap<&str, &Value> = HashMap::new();
    let mut last_t = start_t;
    for e in &s.entries {
        let inherited = s.inherited(e);
        let t = entry_ms(e).unwrap_or(last_t).max(if inherited { 0 } else { start_t });
        // A steer is a user message right after a tool result inside one run; a compaction, branch
        // summary, extension message or `!` shell run in between ends that reading.
        let boundary = matches!(e.get("type").and_then(Value::as_str), Some("compaction" | "branch_summary" | "custom_message"))
            || message(e).is_some_and(|(r, _)| matches!(r, "bashExecution" | "custom" | "branchSummary" | "compactionSummary"));
        if boundary {
            w.after_tool_result = false;
        }
        let Some((role, m)) = message(e) else { continue };
        if inherited {
            // The parent's prefix: context for the first own input, never evidence.
            match role {
                "user" => w.last_input = Some(typed_text(&content_text(m.get("content")))),
                "assistant" => {
                    let text = content_text(m.get("content"));
                    if !text.trim().is_empty() {
                        w.last_deliverable = Some(text);
                    }
                    if let Some(mr) = model_ref(m) {
                        w.last_model = Some(mr);
                    }
                }
                _ => {}
            }
            continue;
        }
        last_t = t;
        match role {
            "user" => {
                w.flush_turn();
                let steered = w.after_tool_result;
                w.after_tool_result = false;
                if !steered {
                    w.agent_end(t);
                }
                let stored = content_text(m.get("content"));
                if is_injected_frame(&stored) {
                    continue;
                }
                let text = typed_text(&stored);
                let idle = !steered;
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
                    after_deliverable: w.last_deliverable.as_deref().map(|d| head(d, TRACE_DELIVERABLE_CHARS)),
                    prelabel: label.to_string(),
                    skills_invoked: skills,
                });
                w.last_input = Some(text);
            }
            "assistant" => {
                w.flush_turn();
                w.after_tool_result = false;
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
                let raw_path = input.get("path").or_else(|| input.get("file_path")).and_then(Value::as_str);
                let is_error = m.get("isError").and_then(Value::as_bool).unwrap_or(false);
                w.out.push(TraceRecord::ToolResult {
                    seq: w.seq(),
                    t,
                    skill_read: (tool == "read" && !is_error).then(|| raw_path.and_then(skill_from_read_path)).flatten(),
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
                // An Esc during a tool ends its result as aborted: the run is over, the next
                // prompt is no steer.
                w.after_tool_result = !(is_error && content_text(m.get("content")).to_ascii_lowercase().contains("aborted"));
                if let Some((_, turn_t)) = &mut w.turn {
                    *turn_t = (*turn_t).max(t);
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SID: &str = "019a0000-aaaa-7bbb-8ccc-000000000001";

    fn line(v: Value) -> String {
        format!("{v}\n")
    }

    fn fixture(cwd: &str, skill_md: &str) -> String {
        let mut s = String::new();
        s += &line(json!({"type":"session","version":3,"id":SID,"timestamp":"2026-10-01T10:00:00.000Z","cwd":cwd}));
        let skills = format!("<available_skills>\n  <skill>\n    <name>pdf-tools</name>\n    <description>d</description>\n    <location>{skill_md}</location>\n  </skill>\n</available_skills>");
        s += &line(json!({"type":"message","id":"s0","parentId":null,"timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"system","content":"","sections":{"skills":skills},"timestamp":1}}));
        s += &line(json!({"type":"message","id":"u1","parentId":"s0","timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"user","content":"add a parser for the config file","timestamp":2}}));
        s += &line(json!({"type":"message","id":"a1","parentId":"u1","timestamp":"2026-10-01T10:00:03.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-x","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15},"stopReason":"toolUse","content":[{"type":"toolCall","id":"c1","name":"read","arguments":{"path":skill_md}}],"timestamp":3}}));
        s += &line(json!({"type":"message","id":"r1","parentId":"a1","timestamp":"2026-10-01T10:00:04.000Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"read","isError":false,"content":[{"type":"text","text":"# pdf"}],"timestamp":4}}));
        s += &line(json!({"type":"message","id":"u2","parentId":"r1","timestamp":"2026-10-01T10:00:05.000Z","message":{"role":"user","content":"no, use serde instead","timestamp":5}}));
        s += &line(json!({"type":"message","id":"a2","parentId":"u2","timestamp":"2026-10-01T10:00:06.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-x","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15},"stopReason":"stop","content":[{"type":"text","text":"done"}],"timestamp":6}}));
        // An abandoned branch: never imported.
        s += &line(json!({"type":"message","id":"x1","parentId":"a1","timestamp":"2026-10-01T10:00:07.000Z","message":{"role":"user","content":"abandoned","timestamp":7}}));
        s += &line(json!({"type":"message","id":"u3","parentId":"a2","timestamp":"2026-10-01T10:00:08.000Z","message":{"role":"user","content":"<skill name=\"pdf-tools\" location=\"/x/SKILL.md\">\nReferences are relative to /x.\n\nbody\n</skill>\n\nextract report.pdf","timestamp":8}}));
        s += &line(json!({"type":"message","id":"a3","parentId":"u3","timestamp":"2026-10-01T10:00:09.000Z","message":{"role":"assistant","provider":"openai","model":"gpt-y","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2},"stopReason":"stop","content":[{"type":"text","text":"extracted"}],"timestamp":9}}));
        s
    }

    #[test]
    fn ids_and_segments() {
        assert_eq!(session_id("abc-1"), "pi:abc-1");
        assert_eq!(session_id("a/b c"), "pi:a_b_c");
        assert_eq!(base_id("pi:abc"), Some("abc"));
        assert_eq!(base_id("pi:abc:r2"), Some("abc"));
        assert_eq!(base_id("pi:abc:rx"), Some("abc:rx"));
        assert_eq!(base_id("cc:abc"), None);
    }

    #[test]
    fn sessions_dir_resolution_keeps_omp_dirs_out() {
        let omp = Path::new("/h/.omp");
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        assert_eq!(sessions_dir_from(env(&[("HOME", "/h")]), omp), Some(PathBuf::from("/h/.pi/agent/sessions")));
        assert_eq!(sessions_dir_from(env(&[("HOME", "/h"), ("PI_CODING_AGENT_DIR", "/p/agent")]), omp), Some(PathBuf::from("/p/agent/sessions")));
        assert_eq!(
            sessions_dir_from(env(&[("HOME", "/h"), ("PI_CODING_AGENT_DIR", "/h/.omp/agent")]), omp),
            Some(PathBuf::from("/h/.pi/agent/sessions")),
            "omp's agent dir is not Pi's"
        );
        assert_eq!(
            sessions_dir_from(env(&[("HOME", "/h"), ("PI_CODING_AGENT_DIR", "/h/.omp/agent"), ("REMATCH_PI_AGENT_DIR", "~/pi")]), omp),
            Some(PathBuf::from("/h/pi/sessions"))
        );
        assert_eq!(sessions_dir_from(env(&[("HOME", "/h"), ("PI_CODING_AGENT_SESSION_DIR", "/s")]), omp), Some(PathBuf::from("/s")));
    }

    #[test]
    fn synthesizes_the_live_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = tmp.path().join("staging/repo/pdf-tools");
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("SKILL.md"), "---\nname: pdf-tools\n---\n").unwrap();
        std::fs::write(entry.join("rematch.json"), json!({"id":"e-pdf","created_at":0,"version":3,"content_sha256":"cafe"}).to_string()).unwrap();
        let shared = tmp.path().join("skills");
        std::fs::create_dir_all(&shared).unwrap();
        crate::library::symlink_dir(&entry, &shared.join("pdf-tools")).unwrap();
        let md = shared.join("pdf-tools/SKILL.md");
        let s = parse_pi_session(&fixture("/nowhere/proj", md.to_str().unwrap()), None).unwrap();
        assert_eq!(s.id(), format!("pi:{SID}"));
        let recs = trace_records_of(&s);
        let evs: Vec<&str> = recs.iter().map(TraceRecord::ev).collect();
        assert_eq!(
            evs,
            ["session_start", "input", "tool_result", "turn_end", "input", "turn_end", "agent_end", "input", "model_change", "turn_end", "agent_end", "session_shutdown"]
        );
        let TraceRecord::SessionStart { loaded_entries, omp_version, mode, .. } = &recs[0] else { panic!() };
        assert_eq!(loaded_entries, &vec![LoadedEntry { id: "e-pdf".into(), version: Some(3), sha: Some("cafe".into()) }]);
        assert_eq!((omp_version.as_str(), mode.as_str()), ("pi", "import"));
        let TraceRecord::ToolResult { skill_read, .. } = &recs[2] else { panic!() };
        assert_eq!(skill_read.as_deref(), Some("pdf-tools"));
        // A message after a tool result was steered into the running run: a takeover.
        let TraceRecord::Input { prelabel, idle, .. } = &recs[4] else { panic!() };
        assert_eq!((prelabel.as_str(), *idle), ("interrupt", false));
        let TraceRecord::Input { text, skills_invoked, prelabel, .. } = &recs[7] else { panic!() };
        assert_eq!(text, "/skill:pdf-tools extract report.pdf");
        assert_eq!((skills_invoked.clone(), prelabel.as_str()), (vec!["pdf-tools".to_string()], "none"));
        let TraceRecord::SessionShutdown { flagged, reasons, .. } = recs.last().unwrap() else { panic!() };
        assert!(*flagged && reasons == &vec!["redirect:interrupt".to_string()]);
        let row = session_record(&s);
        assert_eq!((row.id.as_str(), row.source.as_str()), (format!("pi:{SID}").as_str(), "import"));
        assert_eq!(row.loaded_entries, vec!["e-pdf".to_string()]);
        assert!(row.skill_reads.contains(&"pdf-tools".to_string()));
    }

    #[test]
    fn a_fork_prefix_is_context_not_evidence_and_failed_reads_load_nothing() {
        let mut s = String::new();
        s += &line(json!({"type":"session","version":3,"id":"fork1","timestamp":"2026-10-02T10:00:00.000Z","cwd":"/nowhere","parentSession":"/gone/parent.jsonl"}));
        s += &line(json!({"type":"message","id":"u1","parentId":null,"timestamp":"2026-10-01T10:00:02.000Z","message":{"role":"user","content":"wrong, revert that","timestamp":2}}));
        s += &line(json!({"type":"message","id":"a1","parentId":"u1","timestamp":"2026-10-01T10:00:03.000Z","message":{"role":"assistant","provider":"p","model":"m","usage":{},"stopReason":"stop","content":[{"type":"text","text":"reverted"}],"timestamp":3}}));
        s += &line(json!({"type":"message","id":"u2","parentId":"a1","timestamp":"2026-10-02T10:00:05.000Z","message":{"role":"user","content":"now add the tests","timestamp":5}}));
        s += &line(json!({"type":"message","id":"a2","parentId":"u2","timestamp":"2026-10-02T10:00:06.000Z","message":{"role":"assistant","provider":"p","model":"m","usage":{},"stopReason":"toolUse","content":[{"type":"toolCall","id":"c1","name":"read","arguments":{"path":"/x/skills/foo/SKILL.md"}}],"timestamp":6}}));
        s += &line(json!({"type":"message","id":"r1","parentId":"a2","timestamp":"2026-10-02T10:00:07.000Z","message":{"role":"toolResult","toolCallId":"c1","toolName":"read","isError":true,"content":[{"type":"text","text":"ENOENT"}],"timestamp":7}}));
        let p = parse_pi_session(&s, None).unwrap();
        let recs = trace_records_of(&p);
        let inputs: Vec<(&str, &str, Option<&str>)> = recs
            .iter()
            .filter_map(|r| match r {
                TraceRecord::Input { text, prelabel, after_deliverable, .. } => Some((text.as_str(), prelabel.as_str(), after_deliverable.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(inputs, vec![("now add the tests", "none", Some("reverted"))], "the copied prefix sets context only");
        let row = session_record(&p);
        assert!(!row.flagged && row.skill_reads.is_empty(), "{row:?}");
        assert!(!p.is_subagent());
    }

    #[test]
    fn an_aborted_run_then_an_ordinary_prompt_is_no_takeover() {
        // What Pi 1.4.0 writes for Esc during a tool (observed): an aborted error result and an
        // empty `error` assistant message; and a bare `aborted` assistant for Esc while streaming.
        let mut s = String::new();
        s += &line(json!({"type":"session","version":3,"id":"ab1","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/nowhere"}));
        let entries = [
            json!({"role":"user","content":[{"type":"text","text":"add the reconciliation command"}],"timestamp":1}),
            json!({"role":"assistant","provider":"p","model":"m","usage":{},"stopReason":"toolUse","content":[{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"sleep 5"}}],"timestamp":2}),
            json!({"role":"toolResult","toolCallId":"c1","toolName":"bash","isError":true,"content":[{"type":"text","text":"Command aborted"}],"timestamp":3}),
            json!({"role":"assistant","provider":"p","model":"m","usage":{},"stopReason":"error","errorMessage":"This operation was aborted","content":[],"timestamp":4}),
            json!({"role":"user","content":[{"type":"text","text":"now write the changelog entry"}],"timestamp":5}),
            json!({"role":"assistant","provider":"p","model":"m","usage":{},"stopReason":"aborted","content":[{"type":"text","text":"Writ"}],"timestamp":6}),
            json!({"role":"user","content":[{"type":"text","text":"also bump the crate version"}],"timestamp":7}),
        ];
        let mut parent = Value::Null;
        for (i, m) in entries.iter().enumerate() {
            let id = format!("e{i}");
            s += &line(json!({"type":"message","id":id,"parentId":parent,"timestamp":format!("2026-10-01T10:00:{:02}.000Z", i + 1),"message":m}));
            parent = json!(id);
        }
        let recs = trace_records_of(&parse_pi_session(&s, None).unwrap());
        let inputs: Vec<(&str, bool)> = recs
            .iter()
            .filter_map(|r| match r {
                TraceRecord::Input { prelabel, idle, .. } => Some((prelabel.as_str(), *idle)),
                _ => None,
            })
            .collect();
        assert_eq!(inputs, vec![("none", true), ("none", true), ("none", true)]);
        let TraceRecord::SessionShutdown { flagged, .. } = recs.last().unwrap() else { panic!() };
        assert!(!flagged);
    }

    #[test]
    fn boundaries_after_a_tool_result_are_no_steer() {
        // A run cut off after a tool result (crash), then a `!` shell run, a compaction or an
        // extension message before the next prompt: idle prompts, not takeovers.
        let call = |id: &str| json!({"type":"message","message":{"role":"assistant","provider":"p","model":"m","usage":{},"stopReason":"toolUse","content":[{"type":"toolCall","id":id,"name":"bash","arguments":{"command":"ls"}}]}});
        let res = |id: &str| json!({"type":"message","message":{"role":"toolResult","toolCallId":id,"toolName":"bash","isError":false,"content":[{"type":"text","text":"ok"}]}});
        let user = |text: &str| json!({"type":"message","message":{"role":"user","content":text}});
        let entries = [
            user("list the migrations"),
            call("c1"),
            res("c1"),
            json!({"type":"message","message":{"role":"bashExecution","command":"git status","output":"clean","exitCode":0,"cancelled":false,"truncated":false}}),
            user("now write the changelog entry"),
            call("c2"),
            res("c2"),
            json!({"type":"compaction","summary":"s","firstKeptEntryId":"e6","tokensBefore":1}),
            user("then bump the crate version"),
            call("c3"),
            res("c3"),
            json!({"type":"custom_message","customType":"x","content":"note","display":true}),
            user("finally tag the release"),
            call("c4"),
            res("c4"),
            user("use the vault instead"),
        ];
        let mut s = line(json!({"type":"session","version":3,"id":"b1","timestamp":"2026-10-01T10:00:00.000Z","cwd":"/nowhere"}));
        let mut parent = Value::Null;
        for (i, mut e) in entries.into_iter().enumerate() {
            let id = format!("e{i}");
            e["id"] = json!(id);
            e["parentId"] = parent;
            e["timestamp"] = json!(format!("2026-10-01T10:00:{:02}.000Z", i + 1));
            s += &line(e);
            parent = json!(id);
        }
        let recs = trace_records_of(&parse_pi_session(&s, None).unwrap());
        let labels: Vec<&str> = recs
            .iter()
            .filter_map(|r| match r {
                TraceRecord::Input { prelabel, .. } => Some(prelabel.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["none", "none", "none", "none", "interrupt"], "only the last prompt was steered into a run");
    }

    #[test]
    fn scan_and_find() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("--w--");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(format!("2026-10-01T10-00-00-000Z_{SID}.jsonl"));
        std::fs::write(&f, fixture("/w", "/x/SKILL.md")).unwrap();
        std::fs::write(dir.join("notes.jsonl"), "{\"type\":\"other\"}\n").unwrap();
        assert_eq!(scan_sessions_dir(tmp.path()).unwrap(), vec![f.clone()]);
        assert_eq!(find_session_file(tmp.path(), &format!("pi:{SID}")), Some(f.clone()));
        assert_eq!(find_session_file(tmp.path(), &format!("pi:{SID}:r3")), Some(f));
        assert_eq!(find_session_file(tmp.path(), "pi:other"), None);
    }
}
