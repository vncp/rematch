//! The extension's own trace (DESIGN §3): `traces/<session_id>.jsonl`, one record per line,
//! `{"seq","t","ev",...}`. Parsing is tolerant: unknown events and malformed known events
//! become [`TraceRecord::Other`], unparseable lines are skipped. [`fold_trace`] derives the
//! ledger's [`SessionRecord`]; [`find_moment`] and [`triage`] drive reflection.

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::family::family_of_ref;
use crate::features::{exts_of, head, stems_of_session, strip_selector};
use crate::redirect::{class_name, mark_class, PRELABEL_INTERRUPT, PRELABEL_NONE, PRELABEL_REVERT};
use crate::types::{EditorScope, ModelRef, Moment, Redirect, RedirectClass, SessionRecord, UnixMs};

/// Tool-error cluster: ≥ `ERROR_CLUSTER_MIN` errors in a window of `ERROR_CLUSTER_WINDOW`
/// consecutive tool results.
pub const ERROR_CLUSTER_WINDOW: usize = 6;
pub const ERROR_CLUSTER_MIN: usize = 3;

/// Chars of user text kept on a [`Redirect`].
pub const REDIRECT_TEXT_CHARS: usize = 1024;
/// Chars of the preceding deliverable kept on a [`Redirect`].
pub const REDIRECT_DELIVERABLE_CHARS: usize = 2048;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub name: String,
}

/// omp's assistant `usage`, verbatim (camelCase on the wire).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub cost: Value,
}

impl Usage {
    /// Prompt-side tokens: fresh input plus cache reads and writes.
    pub fn tokens_in(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

/// One trace line. Field names match DESIGN §3 exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum TraceRecord {
    SessionStart {
        seq: u32,
        #[serde(default)]
        t: UnixMs,
        #[serde(default)]
        session_id: String,
        #[serde(default)]
        cwd: String,
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        branch_hint: String,
        #[serde(default)]
        model: Option<ModelRef>,
        #[serde(default)]
        mode: String,
        #[serde(default)]
        agent: Option<AgentInfo>,
        #[serde(default)]
        omp_version: String,
        #[serde(default)]
        loaded_entries: Vec<String>,
        #[serde(default)]
        hooks_loaded: Vec<String>,
        #[serde(default)]
        staging_dirs: Vec<String>,
    },
    Input {
        seq: u32,
        #[serde(default)]
        t: UnixMs,
        #[serde(default)]
        text: String,
        #[serde(default)]
        source: String,
        #[serde(default = "yes")]
        idle: bool,
        #[serde(default)]
        after_deliverable: Option<String>,
        #[serde(default = "none_label")]
        prelabel: String,
        #[serde(default)]
        skills_invoked: Vec<String>,
    },
    ToolResult {
        seq: u32,
        #[serde(default)]
        t: UnixMs,
        #[serde(default)]
        tool: String,
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        input: Value,
        #[serde(default)]
        args_hash: String,
        #[serde(default)]
        result_bytes: u64,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        error_class: Option<String>,
        #[serde(default)]
        content_head: String,
        #[serde(default)]
        skill_read: Option<String>,
        #[serde(default)]
        path: Option<String>,
    },
    ToolBlocked {
        seq: u32,
        #[serde(default)]
        t: UnixMs,
        #[serde(default)]
        tool: String,
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        hook_id: String,
        #[serde(default)]
        reason: String,
    },
    TurnEnd {
        seq: u32,
        #[serde(default)]
        t: UnixMs,
        #[serde(default)]
        model: Option<ModelRef>,
        #[serde(default)]
        usage: Option<Usage>,
        #[serde(default)]
        stop_reason: Option<String>,
        #[serde(default)]
        text_head: String,
    },
    AgentEnd {
        seq: u32,
        #[serde(default)]
        t: UnixMs,
        #[serde(default)]
        turns: u32,
        #[serde(default)]
        is_terminal: bool,
        #[serde(default)]
        yielded: bool,
    },
    ModelChange {
        seq: u32,
        #[serde(default)]
        t: UnixMs,
        #[serde(default)]
        from: Option<ModelRef>,
        #[serde(default)]
        to: Option<ModelRef>,
        #[serde(default)]
        reason: Option<String>,
    },
    Mark {
        seq: u32,
        #[serde(default)]
        t: UnixMs,
        class: RedirectClass,
    },
    SessionShutdown {
        seq: u32,
        #[serde(default)]
        t: UnixMs,
        #[serde(default)]
        reasons: Vec<String>,
        #[serde(default)]
        flagged: bool,
    },
    /// Unknown `ev`, or a known `ev` whose fields did not parse. Kept verbatim.
    #[serde(untagged)]
    Other(Value),
}

fn yes() -> bool {
    true
}
fn none_label() -> String {
    PRELABEL_NONE.to_string()
}

impl TraceRecord {
    pub fn seq(&self) -> u32 {
        match self {
            Self::SessionStart { seq, .. }
            | Self::Input { seq, .. }
            | Self::ToolResult { seq, .. }
            | Self::ToolBlocked { seq, .. }
            | Self::TurnEnd { seq, .. }
            | Self::AgentEnd { seq, .. }
            | Self::ModelChange { seq, .. }
            | Self::Mark { seq, .. }
            | Self::SessionShutdown { seq, .. } => *seq,
            Self::Other(v) => v.get("seq").and_then(Value::as_u64).unwrap_or(0) as u32,
        }
    }

    pub fn t(&self) -> UnixMs {
        match self {
            Self::SessionStart { t, .. }
            | Self::Input { t, .. }
            | Self::ToolResult { t, .. }
            | Self::ToolBlocked { t, .. }
            | Self::TurnEnd { t, .. }
            | Self::AgentEnd { t, .. }
            | Self::ModelChange { t, .. }
            | Self::Mark { t, .. }
            | Self::SessionShutdown { t, .. } => *t,
            Self::Other(v) => v.get("t").and_then(Value::as_i64).unwrap_or(0),
        }
    }

    pub fn ev(&self) -> &str {
        match self {
            Self::SessionStart { .. } => "session_start",
            Self::Input { .. } => "input",
            Self::ToolResult { .. } => "tool_result",
            Self::ToolBlocked { .. } => "tool_blocked",
            Self::TurnEnd { .. } => "turn_end",
            Self::AgentEnd { .. } => "agent_end",
            Self::ModelChange { .. } => "model_change",
            Self::Mark { .. } => "mark",
            Self::SessionShutdown { .. } => "session_shutdown",
            Self::Other(v) => v.get("ev").and_then(Value::as_str).unwrap_or(""),
        }
    }

    /// Parses one line; `None` for blank or non-JSON lines (e.g. a torn last write).
    pub fn parse_line(line: &str) -> Option<TraceRecord> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        serde_json::from_str(line).ok()
    }
}

/// Reads a trace file. IO errors fail; bad lines are skipped.
pub fn read_trace(path: &Path) -> Result<Vec<TraceRecord>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("read trace {}", path.display()))?;
    Ok(parse_trace(&text))
}

pub fn parse_trace(text: &str) -> Vec<TraceRecord> {
    text.lines().filter_map(TraceRecord::parse_line).collect()
}

// ---- shared derivations (also used by the importer) ----

static SKILL_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|\s)/skill:([A-Za-z0-9][A-Za-z0-9_.-]*)").expect("static pattern")
});
static SKILL_FILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|/)skills/([^/]+)/SKILL\.md$").expect("static pattern"));

/// Skill named by a `read` path: `skill://<name>[/...]` or `.../skills/<name>/SKILL.md`,
/// read selectors allowed.
pub fn skill_from_read_path(path: &str) -> Option<String> {
    let p = strip_selector(path);
    if let Some(rest) = p.strip_prefix("skill://") {
        let name = rest.split('/').next().unwrap_or("");
        return (!name.is_empty()).then(|| name.to_string());
    }
    SKILL_FILE_RE.captures(p).map(|c| c[1].to_string())
}

/// `/skill:<name>` tokens in user text, in order.
pub fn skills_in_text(text: &str) -> Vec<String> {
    SKILL_TOKEN_RE
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect()
}

/// File paths a tool input names: `path`, `file_path`, `paths[]`. URLs (`scheme://`) are
/// excluded and read selectors stripped.
pub fn paths_in_tool_input(input: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["path", "file_path"] {
        if let Some(p) = input.get(key).and_then(Value::as_str) {
            push_file(&mut out, p);
        }
    }
    if let Some(ps) = input.get("paths").and_then(Value::as_array) {
        for p in ps.iter().filter_map(Value::as_str) {
            push_file(&mut out, p);
        }
    }
    out
}

static PATCH_FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)^\["?([^\]\n"]+?)"?#[0-9A-Za-z]{4}\]\s*$|^\*\*\* (?:Add|Update|Delete) File: (.+?)\s*$|^\*\*\* Move to: (.+?)\s*$|^MV\s+"?([^"\n]+?)"?\s*$"#,
    )
    .expect("static pattern")
});

/// Files named inside edit payloads: hashline `[PATH#TAG]` headers and `MV DEST`, and
/// apply_patch `*** Add|Update|Delete File:` / `*** Move to:` lines.
pub fn paths_in_patch_text(text: &str) -> Vec<String> {
    PATCH_FILE_RE
        .captures_iter(text)
        .filter_map(|c| {
            (1..=4)
                .find_map(|i| c.get(i))
                .map(|m| m.as_str().to_string())
        })
        .collect()
}

/// Files a tool call touches: [`paths_in_tool_input`] plus, for `edit`/`apply_patch`, the
/// paths inside the patch payload (`input` or `patch`).
pub fn tool_input_files(tool: &str, input: &Value) -> Vec<String> {
    let mut out = paths_in_tool_input(input);
    if matches!(tool, "edit" | "apply_patch") {
        for key in ["input", "patch"] {
            if let Some(text) = input.get(key).and_then(Value::as_str) {
                for p in paths_in_patch_text(text) {
                    push_file(&mut out, &p);
                }
            }
        }
    }
    out
}

pub(crate) fn push_file(out: &mut Vec<String>, raw: &str) {
    let p = strip_selector(raw);
    if p.is_empty() || p.contains("://") {
        return;
    }
    out.push(p.to_string());
}

pub(crate) fn push_unique(out: &mut Vec<String>, seen: &mut HashSet<String>, item: String) {
    if seen.insert(item.clone()) {
        out.push(item);
    }
}

/// Seqs at which a tool-error cluster starts: walking results in order, the last
/// [`ERROR_CLUSTER_WINDOW`] results (fewer at the start) are inspected; entering the
/// "≥ [`ERROR_CLUSTER_MIN`] errors" state opens a cluster at that result, leaving it allows
/// the next one.
pub fn error_cluster_seqs(results: &[(u32, bool)]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut active = false;
    for i in 0..results.len() {
        let lo = (i + 1).saturating_sub(ERROR_CLUSTER_WINDOW);
        let errors = results[lo..=i].iter().filter(|(_, e)| *e).count();
        if errors >= ERROR_CLUSTER_MIN {
            if !active {
                out.push(results[i].0);
                active = true;
            }
        } else {
            active = false;
        }
    }
    out
}

/// Derives the ledger row for a finished (or in-progress) trace.
pub fn fold_trace(records: &[TraceRecord], session_id: &str) -> SessionRecord {
    let mut s = SessionRecord {
        id: session_id.to_string(),
        source: "trace".to_string(),
        ..Default::default()
    };
    s.started_at = records.first().map(TraceRecord::t).unwrap_or(0);
    s.ended_at = records.last().map(TraceRecord::t);

    let mut skills_seen = HashSet::new();
    let mut files_seen = HashSet::new();
    let mut inputs: Vec<String> = Vec::new();
    let mut results: Vec<(u32, bool)> = Vec::new();
    let mut last_deliverable = String::new();
    let mut shutdown_flagged = false;

    for rec in records {
        match rec {
            TraceRecord::SessionStart {
                t,
                cwd,
                repo,
                branch_hint,
                model,
                loaded_entries,
                ..
            } => {
                s.started_at = *t;
                s.cwd = cwd.clone();
                s.repo = repo.clone().filter(|r| !r.is_empty());
                s.branch = branch_hint.clone();
                if let Some(m) = model {
                    s.family = Some(family_of_ref(m));
                    s.model = Some(m.clone());
                }
                for e in loaded_entries {
                    if !s.loaded_entries.contains(e) {
                        s.loaded_entries.push(e.clone());
                    }
                }
            }
            TraceRecord::Input {
                seq,
                t,
                text,
                after_deliverable,
                prelabel,
                skills_invoked,
                ..
            } => {
                inputs.push(text.clone());
                for name in skills_invoked.iter().cloned().chain(skills_in_text(text)) {
                    push_unique(&mut s.skill_reads, &mut skills_seen, name);
                }
                if prelabel == PRELABEL_INTERRUPT {
                    s.takeovers += 1;
                }
                if prelabel != PRELABEL_NONE && !prelabel.is_empty() {
                    let marked = mark_class(prelabel);
                    s.redirects.push(Redirect {
                        session_id: session_id.to_string(),
                        seq: *seq,
                        prelabel: prelabel.clone(),
                        class: marked,
                        confidence: marked.map(|_| 1.0),
                        text_head: head(text, REDIRECT_TEXT_CHARS),
                        deliverable_head: head(
                            after_deliverable.as_deref().unwrap_or(&last_deliverable),
                            REDIRECT_DELIVERABLE_CHARS,
                        ),
                        t: *t,
                    });
                }
            }
            TraceRecord::ToolResult {
                seq,
                tool,
                input,
                is_error,
                skill_read,
                path,
                ..
            } => {
                s.tool_calls += 1;
                if *is_error {
                    s.tool_errors += 1;
                }
                results.push((*seq, *is_error));
                let skill = skill_read.clone().or_else(|| {
                    (tool == "read")
                        .then(|| {
                            input
                                .get("path")
                                .and_then(Value::as_str)
                                .and_then(skill_from_read_path)
                        })
                        .flatten()
                });
                if let Some(name) = skill {
                    push_unique(&mut s.skill_reads, &mut skills_seen, name);
                }
                let mut files = Vec::new();
                if let Some(p) = path {
                    push_file(&mut files, p);
                }
                files.extend(tool_input_files(tool, input));
                for f in files {
                    push_unique(&mut s.files_touched, &mut files_seen, f);
                }
            }
            TraceRecord::TurnEnd {
                model,
                usage,
                text_head,
                ..
            } => {
                s.turns += 1;
                if let Some(u) = usage {
                    s.tokens_in += u.tokens_in();
                    s.tokens_out += u.output;
                }
                if s.model.is_none() {
                    if let Some(m) = model {
                        s.family = Some(family_of_ref(m));
                        s.model = Some(m.clone());
                    }
                }
                last_deliverable = text_head.clone();
            }
            TraceRecord::Mark { seq, t, class } => {
                let label = format!("mark:{}", class_name(*class));
                // `/evolve mark` typed as input already produced this redirect.
                let duplicate = s.redirects.last().is_some_and(|r| {
                    r.prelabel == label && last_input_seq(records, *seq) == Some(r.seq)
                });
                if !duplicate {
                    s.redirects.push(Redirect {
                        session_id: session_id.to_string(),
                        seq: *seq,
                        prelabel: label,
                        class: Some(*class),
                        confidence: Some(1.0),
                        text_head: String::new(),
                        deliverable_head: head(&last_deliverable, REDIRECT_DELIVERABLE_CHARS),
                        t: *t,
                    });
                }
            }
            TraceRecord::SessionShutdown { flagged, .. } => shutdown_flagged |= *flagged,
            TraceRecord::ToolBlocked { .. }
            | TraceRecord::AgentEnd { .. }
            | TraceRecord::ModelChange { .. } => {}
            TraceRecord::Other(_) => {}
        }
    }

    if s.branch.is_empty() {
        s.branch = basename(s.repo.as_deref().unwrap_or(&s.cwd));
    }
    s.error_cluster_seqs = error_cluster_seqs(&results);
    s.error_clusters = s.error_cluster_seqs.len() as u32;
    s.exts = exts_of(&s.files_touched);
    s.stems = stems_of_session(&inputs, &s.files_touched);
    s.flagged = shutdown_flagged || !s.redirects.is_empty() || s.error_clusters > 0;
    s
}

fn last_input_seq(records: &[TraceRecord], before: u32) -> Option<u32> {
    records
        .iter()
        .filter(|r| matches!(r, TraceRecord::Input { .. }) && r.seq() < before)
        .map(TraceRecord::seq)
        .max()
}

pub(crate) fn basename(p: &str) -> String {
    Path::new(p.trim_end_matches('/'))
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Records around `seq` for the editor: `before` records preceding the first record with
/// seq ≥ `seq`, that record, and `after` following ones. Empty when `seq` is past the end.
pub fn excerpt_around(
    records: &[TraceRecord],
    seq: u32,
    before: usize,
    after: usize,
) -> Vec<TraceRecord> {
    let Some(i) = records.iter().position(|r| r.seq() >= seq) else {
        return Vec::new();
    };
    let lo = i.saturating_sub(before);
    let hi = (i + after + 1).min(records.len());
    records[lo..hi].to_vec()
}

/// The first failure moment: earliest of a redirect the classifier did not dismiss (class
/// `None`), an explicit mark, or the start of a tool-error cluster.
pub fn find_moment(session: &SessionRecord) -> Option<Moment> {
    let redirect = session
        .redirects
        .iter()
        .filter(|r| mark_class(&r.prelabel).is_some() || r.class != Some(RedirectClass::None))
        .map(|r| r.seq);
    let cluster = session.error_cluster_seqs.iter().copied();
    redirect.chain(cluster).min().map(|seq| Moment {
        session_id: session.id.clone(),
        seq,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriageKind {
    /// Procedural failure (how the agent worked): proposals use the invariant editor.
    Process,
    /// Factual/domain complaint: logged; proposals, if any, use the domain editor.
    Content,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Triage {
    pub kind: TriageKind,
    pub scope: EditorScope,
    pub moment: Option<Moment>,
    /// Short machine-readable reason: `error_cluster`, `interrupt`, `revert`,
    /// `procedural_text`, `content_text`, `no_moment`.
    pub reason: String,
}

/// Words that make a redirect about how the agent worked (tooling, verification, steps).
pub const PROCESS_TEXT_PATTERN: &str = r"(?i)\b(?:tests?|testing|cargo|build|builds|compile[sd]?|lint|linter|clippy|fmt|format(?:ter)?|verify|verified|verification|check(?:ed)?|run|ran|running|command|commands|tool|tools|script|scripts|step|steps|first|before|after|skip(?:ped)?|forgot|workflow|process|procedure|commit|push|git|branch|ci|debug|logs?|retry|loop|stuck|read|grep|search|plan|order|install|worktree|diff|edit|patch)\b";

static PROCESS_TEXT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(PROCESS_TEXT_PATTERN).expect("static pattern"));

/// Triage of the first failure (SPEC loop step 2). Process when the moment is a tool-error
/// cluster, a takeover (`interrupt`), a `revert`, or a redirect whose text references
/// tooling/verification/steps ([`PROCESS_TEXT_PATTERN`]); otherwise content.
pub fn triage(session: &SessionRecord, records: &[TraceRecord]) -> Triage {
    let process = |moment, reason: &str| Triage {
        kind: TriageKind::Process,
        scope: EditorScope::Invariant,
        moment,
        reason: reason.to_string(),
    };
    let content = |moment, reason: &str| Triage {
        kind: TriageKind::Content,
        scope: EditorScope::Domain,
        moment,
        reason: reason.to_string(),
    };
    let Some(moment) = find_moment(session) else {
        return content(None, "no_moment");
    };
    if session.error_cluster_seqs.contains(&moment.seq) {
        return process(Some(moment), "error_cluster");
    }
    let redirect = session.redirects.iter().find(|r| r.seq == moment.seq);
    let prelabel = redirect.map(|r| r.prelabel.as_str()).unwrap_or("");
    if prelabel == PRELABEL_INTERRUPT {
        return process(Some(moment), "interrupt");
    }
    if prelabel == PRELABEL_REVERT {
        return process(Some(moment), "revert");
    }
    // Text of the redirect; a bare `mark` record falls back to the input it labels.
    let mut text = redirect.map(|r| r.text_head.clone()).unwrap_or_default();
    if text.is_empty() {
        if let Some(TraceRecord::Input { text: t, .. }) = records
            .iter()
            .filter(|r| matches!(r, TraceRecord::Input { .. }) && r.seq() < moment.seq)
            .max_by_key(|r| r.seq())
        {
            text = t.clone();
        }
    }
    if PROCESS_TEXT_RE.is_match(&text) {
        process(Some(moment), "procedural_text")
    } else {
        content(Some(moment), "content_text")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE: &str = r#"
{"seq":0,"t":1000,"ev":"session_start","session_id":"S1","cwd":"/w/repo/sub","repo":"/w/repo","branch_hint":"repo","model":{"provider":"anthropic","id":"claude-opus-5-5"},"mode":"interactive","agent":{"kind":"main","name":"main"},"omp_version":"18.12.6","loaded_entries":["e1","e2"],"hooks_loaded":[],"staging_dirs":["/s/repo"]}
{"seq":1,"t":1001,"ev":"input","text":"Fix the order book parser /skill:quant-options","source":"interactive","idle":true,"after_deliverable":null,"prelabel":"none","skills_invoked":["quant-options"]}
{"seq":2,"t":1002,"ev":"tool_result","tool":"read","call_id":"c1","input":{"path":"skill://bellwether-dev/units/a.md:1-20"},"args_hash":"h","result_bytes":10,"is_error":false,"error_class":null,"content_head":"","skill_read":null,"path":null}
{"seq":3,"t":1003,"ev":"tool_result","tool":"read","call_id":"c2","input":{"path":"/w/repo/src/order_book.rs:10-40"},"args_hash":"h","result_bytes":10,"is_error":false,"error_class":null,"content_head":"","skill_read":null,"path":"/w/repo/src/order_book.rs"}
{"seq":4,"t":1004,"ev":"tool_result","tool":"edit","call_id":"c3","input":{"file_path":"/w/repo/src/lib.rs"},"args_hash":"h","result_bytes":10,"is_error":true,"error_class":"error","content_head":"","skill_read":null,"path":null}
{"seq":5,"t":1005,"ev":"turn_end","model":{"provider":"anthropic","id":"claude-opus-5-5"},"usage":{"input":10,"output":5,"cacheRead":100,"cacheWrite":3,"totalTokens":118,"cost":{"total":0.1}},"stop_reason":"stop","text_head":"Fixed the parser."}
{"seq":6,"t":1006,"ev":"brand_new_event","x":1}
{"seq":7,"t":1007,"ev":"input","text":"No, the spread is computed from the wrong side of the book","source":"interactive","idle":true,"after_deliverable":"Fixed the parser.","prelabel":"negation","skills_invoked":[]}
{"seq":8,"t":1008,"ev":"tool_result","tool":"bash","call_id":"c4","input":{"command":"cargo test"},"args_hash":"h","result_bytes":10,"is_error":true,"error_class":"error","content_head":"","skill_read":null,"path":null}
{"seq":9,"t":1009,"ev":"tool_result","tool":"bash","call_id":"c5","input":{"command":"cargo test"},"args_hash":"h","result_bytes":10,"is_error":true,"error_class":"error","content_head":"","skill_read":null,"path":null}
not json at all
{"seq":10,"t":1010,"ev":"turn_end","usage":{"input":1,"output":2},"stop_reason":"stop","text_head":"Done."}
{"seq":11,"t":1011,"ev":"input","text":"stop","source":"interactive","idle":false,"after_deliverable":null,"prelabel":"interrupt","skills_invoked":[]}
{"seq":12,"t":1012,"ev":"mark","class":"preference"}
{"seq":13,"t":1013,"ev":"session_shutdown","reasons":["redirect:negation"],"flagged":true}
"#;

    #[test]
    fn parse_is_tolerant() {
        let recs = parse_trace(TRACE);
        assert_eq!(recs.len(), 14, "bad line skipped, unknown kept");
        assert!(matches!(&recs[6], TraceRecord::Other(v) if v["x"] == 1));
        assert_eq!(
            (recs[6].seq(), recs[6].t(), recs[6].ev()),
            (6, 1006, "brand_new_event")
        );
        // Known ev with a broken field degrades to Other instead of failing.
        let bad =
            TraceRecord::parse_line(r#"{"seq":3,"t":1,"ev":"mark","class":"bogus"}"#).unwrap();
        assert!(matches!(bad, TraceRecord::Other(_)));
        assert_eq!(bad.ev(), "mark");
        // Round-trip keeps the wire shape.
        let line = serde_json::to_string(&recs[12]).unwrap();
        assert_eq!(
            line,
            r#"{"ev":"mark","seq":12,"t":1012,"class":"preference"}"#
        );
        assert_eq!(
            serde_json::to_string(&recs[6]).unwrap(),
            r#"{"ev":"brand_new_event","seq":6,"t":1006,"x":1}"#
        );
    }

    #[test]
    fn read_trace_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("S1.jsonl");
        std::fs::write(&p, TRACE).unwrap();
        assert_eq!(read_trace(&p).unwrap().len(), 14);
        assert!(read_trace(&dir.path().join("missing.jsonl")).is_err());
    }

    #[test]
    fn fold_derives_session_record() {
        let s = fold_trace(&parse_trace(TRACE), "S1");
        assert_eq!(s.id, "S1");
        assert_eq!(s.source, "trace");
        assert_eq!(
            (s.cwd.as_str(), s.repo.as_deref(), s.branch.as_str()),
            ("/w/repo/sub", Some("/w/repo"), "repo")
        );
        assert_eq!((s.started_at, s.ended_at), (1000, Some(1013)));
        assert_eq!(s.family.as_ref().unwrap().0, "anthropic/claude-opus");
        assert_eq!(s.turns, 2);
        assert_eq!((s.tokens_in, s.tokens_out), (114, 7));
        assert_eq!((s.tool_calls, s.tool_errors), (5, 3));
        assert_eq!(s.skill_reads, vec!["quant-options", "bellwether-dev"]);
        assert_eq!(
            s.files_touched,
            vec!["/w/repo/src/order_book.rs", "/w/repo/src/lib.rs"]
        );
        assert_eq!(s.exts, vec!["rs"]);
        assert!(
            s.stems.contains(&"spread".to_string()),
            "stems from inputs: {:?}",
            s.stems
        );
        assert!(s.stems.contains(&"book".to_string()));
        assert_eq!(s.loaded_entries, vec!["e1", "e2"]);
        assert_eq!(s.takeovers, 1);
        // 3 errors within the last 6 tool results at seq 9.
        assert_eq!(
            (s.error_clusters, s.error_cluster_seqs.clone()),
            (1, vec![9])
        );
        let labels: Vec<(u32, &str)> = s
            .redirects
            .iter()
            .map(|r| (r.seq, r.prelabel.as_str()))
            .collect();
        assert_eq!(
            labels,
            vec![(7, "negation"), (11, "interrupt"), (12, "mark:preference")]
        );
        assert_eq!(s.redirects[0].deliverable_head, "Fixed the parser.");
        assert_eq!(s.redirects[2].class, Some(RedirectClass::Preference));
        assert_eq!(s.redirects[2].deliverable_head, "Done.");
        assert!(s.flagged);
    }

    #[test]
    fn mark_typed_as_input_is_not_double_counted() {
        let t = r#"{"seq":0,"t":1,"ev":"input","text":"/evolve mark scope","idle":true,"prelabel":"mark:scope"}
{"seq":1,"t":2,"ev":"mark","class":"scope"}
{"seq":2,"t":3,"ev":"mark","class":"correction"}"#;
        let s = fold_trace(&parse_trace(t), "S");
        let labels: Vec<(u32, &str)> = s
            .redirects
            .iter()
            .map(|r| (r.seq, r.prelabel.as_str()))
            .collect();
        assert_eq!(labels, vec![(0, "mark:scope"), (2, "mark:correction")]);
    }

    #[test]
    fn clean_session_is_not_flagged() {
        let t = r#"{"seq":0,"t":5,"ev":"input","text":"hello","idle":true,"prelabel":"none"}
{"seq":1,"t":6,"ev":"turn_end","usage":{"input":1,"output":1},"text_head":"hi"}"#;
        let s = fold_trace(&parse_trace(t), "S");
        assert!(!s.flagged && s.redirects.is_empty() && s.error_clusters == 0);
        assert_eq!(s.branch, "", "no session_start: no cwd to name a branch");
        assert_eq!(fold_trace(&[], "E").started_at, 0);
    }

    #[test]
    fn error_cluster_window_boundaries() {
        let r = |errs: &[bool]| -> Vec<(u32, bool)> {
            errs.iter()
                .enumerate()
                .map(|(i, e)| (i as u32, *e))
                .collect()
        };
        // 2 errors in 6 → none; 3 spread over exactly 6 → one at the third error.
        assert!(error_cluster_seqs(&r(&[true, false, false, false, false, true])).is_empty());
        assert_eq!(
            error_cluster_seqs(&r(&[true, false, false, false, true, true])),
            vec![5]
        );
        // Spread over 7 → none.
        assert!(error_cluster_seqs(&r(&[true, false, false, false, false, true, true])).is_empty());
        // Short sessions count: 3 of 3.
        assert_eq!(error_cluster_seqs(&r(&[true, true, true])), vec![2]);
        // A sustained run is one cluster; recovery then a new burst is a second.
        let mut v = vec![true; 5];
        v.extend([false; 6]);
        v.extend([true; 3]);
        assert_eq!(error_cluster_seqs(&r(&v)), vec![2, 13]);
    }

    #[test]
    fn skill_and_path_extraction() {
        assert_eq!(
            skill_from_read_path("skill://forge-ux:raw").as_deref(),
            Some("forge-ux")
        );
        assert_eq!(
            skill_from_read_path("/h/.omp/agent/skills/qo-proof/SKILL.md:1-50").as_deref(),
            Some("qo-proof")
        );
        assert_eq!(
            skill_from_read_path("skills/x/SKILL.md").as_deref(),
            Some("x")
        );
        assert_eq!(skill_from_read_path("/h/skills/x/README.md"), None);
        assert_eq!(skill_from_read_path("skill://"), None);
        assert_eq!(
            skills_in_text("/skill:a then /skill:b-c.d, not x/skill:z"),
            vec!["a", "b-c.d"]
        );
        let input = serde_json::json!({"path": "a.rs:1-2", "paths": ["b.md", "omp://x.md"], "file_path": "c.toml"});
        assert_eq!(paths_in_tool_input(&input), vec!["a.rs", "c.toml", "b.md"]);
        let edit = serde_json::json!({"input": "[src/a.rs#1A2B]\nPUT 1.=1:\n+[x#y]\n[\"my dir/b.ts\"#00ff]\nMV src/c.rs"});
        assert_eq!(
            tool_input_files("edit", &edit),
            vec!["src/a.rs", "my dir/b.ts", "src/c.rs"]
        );
        let patch = serde_json::json!({"input": "*** Begin Patch\n*** Update File: x/y.py\n*** Move to: x/z.py\n*** Add File: n.md\n*** End Patch"});
        assert_eq!(
            tool_input_files("apply_patch", &patch),
            vec!["x/y.py", "x/z.py", "n.md"]
        );
        assert!(tool_input_files("bash", &serde_json::json!({"input": "[a.rs#1A2B]"})).is_empty());
    }

    #[test]
    fn excerpt_window() {
        let recs = parse_trace(TRACE);
        let ex = excerpt_around(&recs, 7, 2, 1);
        assert_eq!(
            ex.iter().map(TraceRecord::seq).collect::<Vec<_>>(),
            vec![5, 6, 7, 8]
        );
        let start = excerpt_around(&recs, 0, 5, 0);
        assert_eq!(start.len(), 1);
        assert!(excerpt_around(&recs, 99, 2, 2).is_empty());
    }

    fn redirect(seq: u32, prelabel: &str, class: Option<RedirectClass>, text: &str) -> Redirect {
        Redirect {
            session_id: "S".into(),
            seq,
            prelabel: prelabel.into(),
            class,
            confidence: None,
            text_head: text.into(),
            deliverable_head: String::new(),
            t: 0,
        }
    }

    #[test]
    fn moment_is_first_signal_and_skips_dismissed_redirects() {
        let mut s = SessionRecord {
            id: "S".into(),
            ..Default::default()
        };
        assert_eq!(find_moment(&s), None);
        s.redirects = vec![
            redirect(4, "reprompt", Some(RedirectClass::None), "again"),
            redirect(9, "negation", None, "no"),
        ];
        s.error_cluster_seqs = vec![7];
        assert_eq!(
            find_moment(&s),
            Some(Moment {
                session_id: "S".into(),
                seq: 7
            })
        );
        s.redirects
            .push(redirect(2, "mark:scope", Some(RedirectClass::Scope), ""));
        assert_eq!(find_moment(&s).unwrap().seq, 2);
    }

    #[test]
    fn triage_process_vs_content() {
        let mut s = SessionRecord {
            id: "S".into(),
            ..Default::default()
        };
        assert_eq!(triage(&s, &[]).kind, TriageKind::Content);
        assert_eq!(triage(&s, &[]).reason, "no_moment");

        s.error_cluster_seqs = vec![3];
        let t = triage(&s, &[]);
        assert_eq!(
            (t.kind, t.scope, t.reason.as_str()),
            (TriageKind::Process, EditorScope::Invariant, "error_cluster")
        );

        s.error_cluster_seqs.clear();
        s.redirects = vec![redirect(5, "interrupt", None, "wait")];
        assert_eq!(triage(&s, &[]).reason, "interrupt");
        s.redirects = vec![redirect(5, "revert", None, "put it back")];
        assert_eq!(triage(&s, &[]).kind, TriageKind::Process);

        s.redirects = vec![redirect(
            5,
            "negation",
            Some(RedirectClass::Correction),
            "No, you didn't run the tests before saying done",
        )];
        assert_eq!(triage(&s, &[]).reason, "procedural_text");
        s.redirects = vec![redirect(
            5,
            "negation",
            Some(RedirectClass::Correction),
            "No, the strike should be 450 not 445",
        )];
        let t = triage(&s, &[]);
        assert_eq!(
            (t.kind, t.scope, t.reason.as_str()),
            (TriageKind::Content, EditorScope::Domain, "content_text")
        );
        assert_eq!(t.moment.unwrap().seq, 5);

        // A bare mark borrows the text of the input it follows.
        s.redirects = vec![redirect(
            9,
            "mark:correction",
            Some(RedirectClass::Correction),
            "",
        )];
        let recs = parse_trace(
            r#"{"seq":8,"t":1,"ev":"input","text":"you skipped the lint step","prelabel":"none"}
{"seq":9,"t":2,"ev":"mark","class":"correction"}"#,
        );
        assert_eq!(triage(&s, &recs).reason, "procedural_text");
    }
}
