//! The Claude Code adapter's entry points (DESIGN §18). The plugin's hook scripts forward each
//! hook payload (one JSON object on stdin) unchanged; the schema stays here.
//!
//! * `trace-event --harness claude-code` — `SessionStart`, `UserPromptSubmit`, `PostToolUse`,
//!   `PostToolUseFailure`, `Stop`, `StopFailure`, `SessionEnd` → DESIGN §3 records appended to
//!   `traces/cc:<session_id>.jsonl`; `SessionStart` also matches the cwd and links the
//!   matching staging entries into `REMATCH_ROOT/claude-skills/` ([`sync_claude_skills`]);
//!   `UserPromptSubmit` runs an approved `retrieve` hook and returns its units as
//!   `additionalContext`; `SessionEnd` writes `session_shutdown` and, when flagged,
//!   `pending/<sid>.json`.
//! * `hook-check --harness claude-code` — `PreToolUse`: the approved `block`/`stuck` hooks
//!   matched for the cwd, evaluated against the tool input (the `stuck` run read back from
//!   the trace). A rejection is traced as `tool_blocked` and returned as exit 2 + reason.
//!
//! Each hook is a fresh process, so the per-session state the omp extension keeps in memory
//! (previous input, deliverable, error window, open run, call history) is re-read from the
//! trace file, which is locked while a record is appended. `--hook-output` speaks Claude
//! Code's hook protocol instead of printing the envelope; with it every error exits 0, so
//! rematch never fails a hook except by an intentional block.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use rematch_core::features::head;
use rematch_core::import::{args_hash, is_injected_frame, repo_of, TRACE_DELIVERABLE_CHARS, TRACE_INPUT_TEXT_CHARS};
use rematch_core::import_claude;
use rematch_core::library::{self, MatchResult, SKILL_MD};
use rematch_core::trace::{self, flag_reasons, AgentInfo, RetrievedUnit, TraceRecord};
use rematch_core::types::{HookMode, Kind, ModelRef, Status, DEFAULT_RETRIEVE_BUDGET_BYTES, DEFAULT_RETRIEVE_MIN_OVERLAP, DEFAULT_STUCK_REPEATS};
use serde_json::{json, Value};

use crate::app::AppCtx;
use crate::envelope::Envelope;

/// Directory under `REMATCH_ROOT` that the plugin's `skills/` symlink points at.
pub const CLAUDE_SKILLS_DIR: &str = "claude-skills";
/// Bytes of the transcript tail read to recover a run's turns.
const TRANSCRIPT_TAIL_BYTES: u64 = 8 * 1024 * 1024;
/// Tool results whose paths join the retrieval query (the extension's `retrievalQuery`).
const RETRIEVAL_QUERY_PATHS: usize = 10;

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    ClaudeCode,
}

#[derive(Args, Debug, Clone)]
pub struct TraceEventArgs {
    /// Harness whose hook payload arrives on stdin.
    #[arg(long, value_enum)]
    pub harness: Harness,
    /// Session id (default: `cc:<payload session_id>`).
    #[arg(long)]
    pub session: Option<String>,
    /// Speak the harness hook protocol: hook JSON on stdout, a block reason on stderr with
    /// exit 2; any error exits 0. Without it the result is the usual envelope.
    #[arg(long)]
    pub hook_output: bool,
}

pub fn claude_skills_dir(ctx: &AppCtx) -> PathBuf {
    ctx.paths.root.join(CLAUDE_SKILLS_DIR)
}

fn read_payload() -> Result<Value> {
    let text = std::io::read_to_string(std::io::stdin()).context("read hook payload from stdin")?;
    serde_json::from_str(&text).context("hook payload is not JSON")
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// What the hook process prints and exits with under `--hook-output`.
#[derive(Debug, Default)]
struct HookReply {
    stdout: Option<Value>,
    stderr: Option<String>,
    exit: i32,
}

impl HookReply {
    fn json(&self) -> Value {
        json!({"stdout": self.stdout, "stderr": self.stderr, "exit": self.exit})
    }
}

/// Prints an envelope of `trace-event`/`hook-check` in hook-protocol form and returns the
/// exit code: `data.hook.stdout` (one JSON line) and `data.hook.stderr`, exit `data.hook.exit`.
/// A failed command logs to stderr and exits 0: rematch never fails a hook.
pub fn emit_hook(env: &Envelope) -> i32 {
    if !env.ok {
        eprintln!("rematch {}: {}", env.operation, env.error.as_deref().unwrap_or("error"));
        return 0;
    }
    let hook = env.data.as_ref().and_then(|d| d.get("hook")).cloned().unwrap_or(Value::Null);
    if let Some(out) = hook.get("stdout").filter(|v| !v.is_null()) {
        println!("{out}");
    }
    if let Some(err) = hook.get("stderr").and_then(Value::as_str) {
        eprintln!("{err}");
    }
    match hook.get("exit").and_then(Value::as_i64) {
        Some(2) => 2,
        _ => 0,
    }
}

/// `cc:<payload session_id>` unless `--session` names one. Ids become file names.
fn session_of(a: &TraceEventArgs, payload: &Value) -> Result<String> {
    let sid = match &a.session {
        Some(s) => s.clone(),
        None => {
            let raw = str_of(payload, "session_id");
            if raw.is_empty() {
                bail!("hook payload has no session_id");
            }
            import_claude::session_id(raw)
        }
    };
    if sid.is_empty() || sid.starts_with('.') || !sid.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-')) {
        bail!("unusable session id {sid:?}");
    }
    Ok(sid)
}

/// The payload's `cwd` (it follows `cd` and worktrees), else the command's `--cwd`.
fn cwd_of(ctx: &AppCtx, payload: &Value) -> PathBuf {
    match str_of(payload, "cwd") {
        "" => ctx.cwd.clone(),
        c => PathBuf::from(c),
    }
}

// ---- the trace file ----

/// The session's trace, locked (advisory `flock`) for the life of the value: hooks of one
/// session run concurrently (parallel tool calls).
struct TraceFile {
    path: PathBuf,
    file: std::fs::File,
    records: Vec<TraceRecord>,
    now: i64,
}

impl TraceFile {
    fn open(ctx: &AppCtx, sid: &str) -> Result<TraceFile> {
        std::fs::create_dir_all(&ctx.paths.traces)?;
        let path = ctx.paths.traces.join(format!("{sid}.jsonl"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.lock().with_context(|| format!("lock {}", path.display()))?;
        let records = trace::read_trace(&path)?;
        Ok(TraceFile { path, file, records, now: ctx.now })
    }

    fn seq(&self) -> u32 {
        self.records.iter().map(|r| r.seq() + 1).max().unwrap_or(0)
    }

    fn push(&mut self, rec: TraceRecord) -> Result<()> {
        let mut line = serde_json::to_string(&rec)?;
        line.push('\n');
        self.file.write_all(line.as_bytes()).with_context(|| format!("append to {}", self.path.display()))?;
        self.records.push(rec);
        Ok(())
    }

    fn started(&self) -> bool {
        self.records.iter().any(|r| matches!(r, TraceRecord::SessionStart { .. }))
    }

    fn last_index(&self, pred: impl Fn(&TraceRecord) -> bool) -> Option<usize> {
        self.records.iter().rposition(pred)
    }

    /// Records after the last `agent_end` (the run in progress, if any).
    fn open_run(&self) -> &[TraceRecord] {
        let from = self.last_index(|r| matches!(r, TraceRecord::AgentEnd { .. })).map_or(0, |i| i + 1);
        &self.records[from..]
    }

    fn run_active(&self) -> bool {
        self.open_run().iter().any(|r| matches!(r, TraceRecord::Input { .. } | TraceRecord::ToolResult { .. } | TraceRecord::TurnEnd { .. }))
    }

    fn last_model(&self) -> Option<ModelRef> {
        self.records.iter().rev().find_map(|r| match r {
            TraceRecord::TurnEnd { model: Some(m), .. } | TraceRecord::SessionStart { model: Some(m), .. } => Some(m.clone()),
            TraceRecord::ModelChange { to: Some(m), .. } => Some(m.clone()),
            _ => None,
        })
    }
}

// ---- what loads: match, hook modules, skill links ----

/// An approved hook module (`hooks_dir/<id>.ts`, written by `library::hook_module` on
/// approval): the data the user approved, read back.
#[derive(Debug, Clone, PartialEq)]
pub struct HookModule {
    pub id: String,
    pub mode: HookMode,
    pub tool: String,
    pub field: String,
    pub regex: String,
    pub message: String,
    pub params: Value,
}

/// Parses the generated module's `export default { key: <json>, … };` lines.
pub fn parse_hook_module(text: &str) -> Result<HookModule> {
    let mut fields = serde_json::Map::new();
    let body = text.split_once("export default {").map(|(_, b)| b).ok_or_else(|| anyhow!("no default export"))?;
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with('}') {
            break;
        }
        let Some((key, raw)) = line.split_once(": ") else { continue };
        let raw = raw.strip_suffix(',').unwrap_or(raw);
        fields.insert(key.trim().to_string(), serde_json::from_str(raw).with_context(|| format!("hook field {key}"))?);
    }
    let s = |k: &str| fields.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let mode: HookMode = serde_json::from_value(fields.get("mode").cloned().unwrap_or(Value::Null)).context("hook mode")?;
    let m = HookModule {
        id: s("id"),
        mode,
        tool: s("tool"),
        field: s("field"),
        regex: s("regex"),
        message: s("message"),
        params: fields.get("params").cloned().unwrap_or(Value::Null),
    };
    if m.id.is_empty() || m.tool.is_empty() {
        bail!("hook module names no id or tool");
    }
    Ok(m)
}

impl HookModule {
    fn param(&self, key: &str, default: u32) -> u32 {
        self.params.get(key).and_then(Value::as_u64).map_or(default, |v| v as u32)
    }

    fn applies_to(&self, tool: &str) -> bool {
        self.tool == "*" || self.tool.eq_ignore_ascii_case(tool)
    }
}

/// What a session in `cwd` loads under Claude Code.
pub struct Loaded {
    pub m: MatchResult,
    /// Approved, matching hook modules this adapter runs (`block`, `stuck`, `retrieve`);
    /// `truncate` cannot run in Claude Code (no hook rewrites past tool results).
    pub hooks: Vec<HookModule>,
    /// `session_start.loaded_entries`: predicate hits, minus lessons, agents (not loaded by
    /// this adapter), and hooks that are not run or that credit through `retrieval` records.
    pub loaded_entries: Vec<String>,
}

pub fn load(ctx: &AppCtx, cwd: &Path) -> Result<Loaded> {
    let repo = repo_of(cwd);
    let m = library::list_for_match(&ctx.ledger, &ctx.paths, repo.as_deref(), cwd)?;
    let mut hooks = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for file in &m.hooks {
        match std::fs::read_to_string(file).map_err(anyhow::Error::from).and_then(|t| parse_hook_module(&t)) {
            Ok(h) if h.mode == HookMode::Truncate => skipped.push(h.id),
            Ok(h) if h.mode == HookMode::Block && regex::Regex::new(&h.regex).is_err() => {
                eprintln!("rematch: hook {} skipped: bad regex", h.id);
                skipped.push(h.id);
            }
            Ok(h) => hooks.push(h),
            Err(e) => eprintln!("rematch: hook {} skipped: {e:#}", file.display()),
        }
    }
    let loaded_entries = m
        .entries
        .iter()
        .filter(|e| e.predicate_hit && !matches!(e.kind, Kind::Lesson | Kind::Agent))
        .filter(|e| !skipped.contains(&e.id))
        .filter(|e| !hooks.iter().any(|h| h.id == e.id && h.mode == HookMode::Retrieve))
        .filter(|e| e.kind != Kind::Hook || hooks.iter().any(|h| h.id == e.id))
        .map(|e| e.id.clone())
        .collect();
    Ok(Loaded { m, hooks, loaded_entries })
}

/// The skill links of `REMATCH_ROOT/claude-skills/` after a sync.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkillSync {
    pub dir: PathBuf,
    /// `name → target`.
    pub linked: BTreeMap<String, PathBuf>,
    pub changed: bool,
}

/// Makes `REMATCH_ROOT/claude-skills/` hold exactly one symlink per skill this cwd loads:
/// every `<name>/SKILL.md` of the contributed staging branch dirs (what omp loads through
/// `skills.customDirectories`) and every kept/pinned entry's package. Staging stays where it
/// is; Claude Code sees it through the plugin's `skills/` symlink to this dir. Only
/// symlinks are removed. With the kill switch on, every link goes.
pub fn sync_claude_skills(ctx: &AppCtx, m: &MatchResult) -> Result<SkillSync> {
    let dir = claude_skills_dir(ctx);
    let mut want: BTreeMap<String, PathBuf> = BTreeMap::new();
    if !m.off {
        for d in &m.skill_dirs {
            let mut kids: Vec<PathBuf> = std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()).collect();
            kids.sort();
            for k in kids {
                let Some(name) = k.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
                if k.join(SKILL_MD).is_file() {
                    want.entry(name).or_insert(k);
                }
            }
        }
        for me in m.entries.iter().filter(|e| matches!(e.status, Status::Kept | Status::Pinned)) {
            let Some(e) = ctx.ledger.get_entry(&me.id)? else { continue };
            if matches!(e.kind, Kind::Lesson | Kind::Agent) {
                continue;
            }
            let path = if e.path.join(SKILL_MD).is_file() { e.path.clone() } else { ctx.paths.skills_dir().join(&e.name) };
            if path.join(SKILL_MD).is_file() {
                want.entry(e.name.clone()).or_insert(path);
            }
        }
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut changed = false;
    for ent in std::fs::read_dir(&dir)?.flatten() {
        let p = ent.path();
        if !std::fs::symlink_metadata(&p).is_ok_and(|md| md.file_type().is_symlink()) {
            continue;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        if want.get(&name).is_some_and(|t| std::fs::read_link(&p).is_ok_and(|cur| &cur == t)) {
            continue;
        }
        std::fs::remove_file(&p).with_context(|| format!("unlink {}", p.display()))?;
        changed = true;
    }
    for (name, target) in &want {
        let link = dir.join(name);
        if std::fs::symlink_metadata(&link).is_ok() {
            continue;
        }
        symlink_dir(target, &link)?;
        changed = true;
    }
    Ok(SkillSync { dir, linked: want, changed })
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).with_context(|| format!("link {} → {}", link.display(), target.display()))
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> Result<()> {
    std::os::windows::fs::symlink_dir(target, link).with_context(|| format!("link {} → {}", link.display(), target.display()))
}

/// `<plugin root>/skills` → `REMATCH_ROOT/claude-skills`, created when absent (the repo
/// cannot ship an absolute link). `true` when created or repointed.
fn ensure_plugin_link(ctx: &AppCtx, plugin_root: &Path) -> Result<bool> {
    if !plugin_root.join(".claude-plugin").is_dir() {
        return Ok(false);
    }
    let link = plugin_root.join("skills");
    let target = claude_skills_dir(ctx);
    match std::fs::symlink_metadata(&link) {
        Ok(md) if md.file_type().is_symlink() => {
            if std::fs::read_link(&link).is_ok_and(|cur| cur == target) {
                return Ok(false);
            }
            std::fs::remove_file(&link)?;
        }
        // A real directory is the user's: leave it.
        Ok(_) => return Ok(false),
        Err(_) => {}
    }
    std::fs::create_dir_all(&target)?;
    symlink_dir(&target, &link)?;
    Ok(true)
}

// ---- trace-event ----

pub fn cmd_trace_event(ctx: &AppCtx, a: TraceEventArgs) -> Result<Value> {
    let payload = read_payload()?;
    trace_event(ctx, &a, &payload)
}

pub fn trace_event(ctx: &AppCtx, a: &TraceEventArgs, payload: &Value) -> Result<Value> {
    let event = str_of(payload, "hook_event_name").to_string();
    if event == "PreToolUse" {
        return hook_check(ctx, a, payload);
    }
    if ctx.paths.is_off() {
        // Staging must not load while off: drop the links, write nothing.
        if event == "SessionStart" {
            sync_claude_skills(ctx, &MatchResult { off: true, ..Default::default() })?;
        }
        return Ok(json!({"event": event, "off": true, "records": [], "hook": HookReply::default().json()}));
    }
    let sid = session_of(a, payload)?;
    let cwd = cwd_of(ctx, payload);
    let subagent = payload.get("agent_id").is_some_and(|v| !v.is_null());
    let mut reply = HookReply::default();
    let mut tf = TraceFile::open(ctx, &sid)?;
    let before = tf.records.len();
    let mut extra = json!({});
    match event.as_str() {
        "SessionStart" => {
            let source = str_of(payload, "source");
            let mut reload = false;
            if let Some(root) = std::env::var_os("CLAUDE_PLUGIN_ROOT").filter(|r| !r.is_empty()) {
                reload |= ensure_plugin_link(ctx, Path::new(&root))?;
            }
            // Compaction continues the session: its skills refresh, its trace does not restart.
            if source != "compact" || !tf.started() {
                reload |= start(ctx, &mut tf, &sid, &cwd, payload, &mut extra)?;
            } else {
                reload |= sync_claude_skills(ctx, &load(ctx, &cwd)?.m)?.changed;
            }
            if reload {
                reply.stdout = Some(json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "reloadSkills": true}}));
            }
        }
        "UserPromptSubmit" if !subagent => {
            ensure_started(ctx, &mut tf, &sid, &cwd, payload)?;
            prompt(ctx, &mut tf, &cwd, payload, &mut reply)?;
        }
        "PostToolUse" | "PostToolUseFailure" if !subagent => {
            ensure_started(ctx, &mut tf, &sid, &cwd, payload)?;
            let failed = event == "PostToolUseFailure";
            let content = if failed { payload.get("error").cloned().unwrap_or(Value::Null) } else { payload.get("tool_response").cloned().unwrap_or(Value::Null) };
            let seq = tf.seq();
            let rec = import_claude::tool_result_record(
                seq,
                tf.now,
                str_of(payload, "tool_name"),
                str_of(payload, "tool_use_id"),
                payload.get("tool_input").unwrap_or(&Value::Null),
                &content,
                failed,
            );
            tf.push(rec)?;
        }
        "Stop" | "StopFailure" if !subagent => {
            ensure_started(ctx, &mut tf, &sid, &cwd, payload)?;
            close_run(&mut tf, payload, event == "Stop")?;
        }
        "SessionEnd" if tf.started() => {
            if tf.run_active() {
                close_run(&mut tf, payload, false)?;
            }
            let reasons = flag_reasons(&tf.records);
            let flagged = !reasons.is_empty();
            let seq = tf.seq();
            tf.push(TraceRecord::SessionShutdown { seq, t: tf.now, reasons: reasons.clone(), flagged })?;
            if flagged {
                std::fs::create_dir_all(&ctx.paths.pending)?;
                let p = ctx.paths.pending.join(format!("{sid}.json"));
                let body = json!({"session_id": sid, "flagged_at": ctx.now, "reasons": reasons, "trace": tf.path, "source": "claude-code"});
                std::fs::write(&p, serde_json::to_string(&body)?).with_context(|| format!("write {}", p.display()))?;
                extra["pending"] = json!(p);
            }
        }
        _ => {}
    }
    let written: Vec<&str> = tf.records[before..].iter().map(TraceRecord::ev).collect();
    let mut out = json!({"event": event, "session": sid, "trace": tf.path, "records": written, "hook": reply.json()});
    if let (Some(o), Value::Object(x)) = (out.as_object_mut(), extra) {
        o.extend(x);
    }
    Ok(out)
}

/// `session_start` after matching `cwd` and syncing the skill links; `true` when the links
/// changed (the hook asks Claude Code to re-scan skills).
fn start(ctx: &AppCtx, tf: &mut TraceFile, sid: &str, cwd: &Path, payload: &Value, extra: &mut Value) -> Result<bool> {
    let loaded = load(ctx, cwd)?;
    let sync = sync_claude_skills(ctx, &loaded.m)?;
    let repo = repo_of(cwd).map(|p| p.to_string_lossy().into_owned());
    let cwd_s = cwd.to_string_lossy().into_owned();
    let seq = tf.seq();
    tf.push(TraceRecord::SessionStart {
        seq,
        t: tf.now,
        session_id: sid.to_string(),
        branch_hint: Path::new(repo.as_deref().unwrap_or(&cwd_s)).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        cwd: cwd_s,
        repo,
        model: payload.get("model").and_then(Value::as_str).and_then(import_claude::model_ref),
        mode: match str_of(payload, "source") {
            "" => "claude-code".to_string(),
            s => format!("claude-code:{s}"),
        },
        agent: Some(match str_of(payload, "agent_type") {
            "" => AgentInfo { kind: "main".into(), name: "main".into() },
            t => AgentInfo { kind: "main".into(), name: t.to_string() },
        }),
        omp_version: "claude-code".into(),
        loaded_entries: loaded.loaded_entries.clone(),
        hooks_loaded: loaded.hooks.iter().map(|h| h.id.clone()).collect(),
        staging_dirs: loaded.m.skill_dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect(),
    })?;
    extra["skills"] = json!(sync.linked.keys().collect::<Vec<_>>());
    extra["loaded_entries"] = json!(loaded.loaded_entries);
    Ok(sync.changed)
}

/// A trace whose `SessionStart` hook never ran (plugin enabled mid-session, hook timeout)
/// starts at its first traced event.
fn ensure_started(ctx: &AppCtx, tf: &mut TraceFile, sid: &str, cwd: &Path, payload: &Value) -> Result<()> {
    if !tf.started() {
        start(ctx, tf, sid, cwd, payload, &mut json!({}))?;
    }
    Ok(())
}

/// `UserPromptSubmit`: closes an aborted run, writes `input` (prelabel from the trace's
/// previous input and run state), then runs an approved `retrieve` hook.
fn prompt(ctx: &AppCtx, tf: &mut TraceFile, cwd: &Path, payload: &Value, reply: &mut HookReply) -> Result<()> {
    let text = str_of(payload, "prompt").to_string();
    // Background-task reports, scheduled tasks and cross-session messages also arrive here.
    if text.trim().is_empty() || is_injected_frame(&text) {
        return Ok(());
    }
    // A prompt while the previous run never stopped (Esc, or typed while Claude worked) is an
    // interrupt, as an omp input while the agent is busy.
    let idle = !tf.open_run().iter().any(|r| matches!(r, TraceRecord::Input { .. }));
    if !idle {
        close_run(tf, payload, false)?;
    }
    let prev = tf.records.iter().rev().find_map(|r| match r {
        TraceRecord::Input { text, .. } => Some(text.clone()),
        _ => None,
    });
    let deliverable = tf.records.iter().rev().find_map(|r| match r {
        TraceRecord::TurnEnd { text_head, .. } if !text_head.trim().is_empty() => Some(text_head.clone()),
        _ => None,
    });
    let seq = tf.seq();
    if let Some(rec) = import_claude::input_record(seq, tf.now, &text, idle, prev.as_deref(), deliverable.as_deref(), "") {
        tf.push(rec)?;
    }
    retrieve(ctx, tf, cwd, &text, reply)
}

/// The approved `retrieve` hook: units of the loaded packages overlapping the prompt plus the
/// paths of the last tool results, as `additionalContext` — only when they differ from the
/// units last injected (an injected context stays in Claude Code's conversation).
fn retrieve(ctx: &AppCtx, tf: &mut TraceFile, cwd: &Path, text: &str, reply: &mut HookReply) -> Result<()> {
    let loaded = load(ctx, cwd)?;
    let hooks: Vec<&HookModule> = loaded.hooks.iter().filter(|h| h.mode == HookMode::Retrieve).collect();
    let Some(hook) = hooks.first() else { return Ok(()) };
    let mut paths: Vec<String> = Vec::new();
    for r in tf.records.iter().rev().filter(|r| matches!(r, TraceRecord::ToolResult { .. })).take(RETRIEVAL_QUERY_PATHS) {
        if let TraceRecord::ToolResult { path: Some(p), .. } = r {
            if !paths.contains(p) {
                paths.push(p.clone());
            }
        }
    }
    paths.reverse();
    let query = head(&[vec![text.to_string()], paths].concat().join("\n"), TRACE_INPUT_TEXT_CHARS);
    let candidates = crate::retrieve::candidates(ctx, cwd, &[])?;
    let r = crate::retrieve::rank(
        &candidates,
        &query,
        hook.param("budget_bytes", DEFAULT_RETRIEVE_BUDGET_BYTES) as usize,
        hook.param("min_overlap", DEFAULT_RETRIEVE_MIN_OVERLAP),
    );
    if r.units.is_empty() {
        return Ok(());
    }
    let units: Vec<RetrievedUnit> =
        r.units.iter().map(|u| RetrievedUnit { entry_id: u.entry_id.clone(), path: u.path.clone(), bytes: u.text.len() as u64 }).collect();
    let last = tf.records.iter().rev().find_map(|r| match r {
        TraceRecord::Retrieval { units, .. } => Some(units.clone()),
        _ => None,
    });
    let same = last.is_some_and(|l| l.iter().map(|u| (&u.entry_id, &u.path)).eq(units.iter().map(|u| (&u.entry_id, &u.path))));
    if same {
        return Ok(());
    }
    let seq = tf.seq();
    let bytes = units.iter().map(|u| u.bytes).sum();
    tf.push(TraceRecord::Retrieval { seq, t: tf.now, entry_ids: hooks.iter().map(|h| h.id.clone()).collect(), units, bytes })?;
    let body: Vec<String> =
        r.units.iter().map(|u| format!("<unit skill=\"{}\" path=\"{}\">\n{}\n</unit>", u.name, u.path, u.text.trim_end())).collect();
    let context = format!(
        "[rematch] Units of skills loaded in this session that match this request, retrieved by the harness (the whole skill: the `rematch:<name>` skill).\n\n{}",
        body.join("\n\n")
    );
    reply.stdout = Some(json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": context}}));
    Ok(())
}

/// Ends the open run: one `turn_end` per API response the transcript holds after the last
/// recorded turn or input (with `model_change` when the model changed), else one
/// `turn_end` carrying `last_assistant_message`; then `agent_end`. `yielded` for a `Stop`.
fn close_run(tf: &mut TraceFile, payload: &Value, yielded: bool) -> Result<()> {
    if !tf.run_active() {
        return Ok(());
    }
    let cutoff = tf
        .records
        .iter()
        .filter_map(|r| match r {
            TraceRecord::Input { t, .. } | TraceRecord::TurnEnd { t, .. } | TraceRecord::AgentEnd { t, .. } => Some(*t),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let transcript = str_of(payload, "transcript_path");
    let responses = match transcript {
        "" => Vec::new(),
        p => match import_claude::read_session_tail(Path::new(p), TRANSCRIPT_TAIL_BYTES) {
            Ok(s) => import_claude::responses_after(&s, cutoff),
            Err(e) => {
                eprintln!("rematch: transcript {p} unreadable: {e:#}");
                Vec::new()
            }
        },
    };
    let mut turns = 0u32;
    let mut last_model = tf.last_model();
    for r in responses {
        if let (Some(to), Some(from)) = (&r.model, &last_model) {
            if to != from {
                let seq = tf.seq();
                tf.push(TraceRecord::ModelChange { seq, t: r.t, from: Some(from.clone()), to: Some(to.clone()), reason: Some("turn".into()) })?;
            }
        }
        if r.model.is_some() {
            last_model = r.model.clone();
        }
        let seq = tf.seq();
        tf.push(TraceRecord::TurnEnd {
            seq,
            t: r.t,
            model: r.model,
            usage: Some(r.usage.unwrap_or_default()),
            stop_reason: r.stop_reason,
            text_head: head(&r.text, TRACE_DELIVERABLE_CHARS),
        })?;
        turns += 1;
    }
    let last_message = str_of(payload, "last_assistant_message");
    if turns == 0 && yielded {
        let seq = tf.seq();
        tf.push(TraceRecord::TurnEnd {
            seq,
            t: tf.now,
            model: None,
            usage: None,
            stop_reason: Some("end_turn".into()),
            text_head: head(last_message, TRACE_DELIVERABLE_CHARS),
        })?;
        turns = 1;
    }
    let seq = tf.seq();
    tf.push(TraceRecord::AgentEnd { seq, t: tf.now, turns, is_terminal: true, yielded })?;
    Ok(())
}

// ---- hook-check ----

pub fn cmd_hook_check(ctx: &AppCtx, a: TraceEventArgs) -> Result<Value> {
    let payload = read_payload()?;
    hook_check(ctx, &a, &payload)
}

/// Identical calls (tool + args hash) right before now, newest first: `tool_result`s and the
/// `tool_blocked`s of `stuck` hooks count (a call `stuck` rejected stays in the run); a call a
/// `block` rule rejected never ran and is skipped, as is every other record.
pub fn trailing_run(records: &[TraceRecord], tool: &str, hash: &str, stuck_ids: &[&str]) -> u32 {
    let mut n = 0;
    for r in records.iter().rev() {
        let (t, h) = match r {
            TraceRecord::ToolResult { tool, args_hash, .. } => (tool, args_hash),
            TraceRecord::ToolBlocked { tool, args_hash, hook_id, .. } if stuck_ids.contains(&hook_id.as_str()) => (tool, args_hash),
            _ => continue,
        };
        if t != tool || h != hash {
            break;
        }
        n += 1;
    }
    n
}

/// `input[field]`, with omp's `path` and Claude Code's `file_path` standing in for each other.
fn field_value<'a>(input: &'a Value, field: &str) -> Option<&'a str> {
    let alias = match field {
        "path" => "file_path",
        "file_path" => "path",
        _ => field,
    };
    input.get(field).or_else(|| input.get(alias)).and_then(Value::as_str)
}

pub fn hook_check(ctx: &AppCtx, a: &TraceEventArgs, payload: &Value) -> Result<Value> {
    let allow = |why: &str| json!({"event": "PreToolUse", "block": false, "why": why, "hook": HookReply::default().json()});
    if ctx.paths.is_off() {
        return Ok(allow("off"));
    }
    let tool = import_claude::tool_name(str_of(payload, "tool_name"));
    let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let cwd = cwd_of(ctx, payload);
    let loaded = load(ctx, &cwd)?;
    let call_hooks: Vec<&HookModule> =
        loaded.hooks.iter().filter(|h| matches!(h.mode, HookMode::Block | HookMode::Stuck) && h.applies_to(&tool)).collect();
    if call_hooks.is_empty() {
        return Ok(allow("no matching block/stuck hook"));
    }
    let subagent = payload.get("agent_id").is_some_and(|v| !v.is_null());
    let sid = session_of(a, payload)?;
    let hash = args_hash(&input);
    let trace_path = ctx.paths.traces.join(format!("{sid}.jsonl"));
    let records = if subagent || !trace_path.is_file() { Vec::new() } else { trace::read_trace(&trace_path)? };
    let stuck_ids: Vec<&str> = loaded.hooks.iter().filter(|h| h.mode == HookMode::Stuck).map(|h| h.id.as_str()).collect();
    let mut blocked: Option<(&HookModule, String)> = None;
    for h in &call_hooks {
        let reason = match h.mode {
            HookMode::Block => {
                let re = regex::Regex::new(&h.regex)?;
                field_value(&input, &h.field).is_some_and(|v| re.is_match(v)).then(|| h.message.clone())
            }
            // Subagents have no trace, so no call history.
            HookMode::Stuck if !subagent => {
                (trailing_run(&records, &tool, &hash, &stuck_ids) >= h.param("repeats", DEFAULT_STUCK_REPEATS)).then(|| h.message.clone())
            }
            _ => None,
        };
        if let Some(r) = reason {
            blocked = Some((h, r));
            break;
        }
    }
    let Some((hook, reason)) = blocked else { return Ok(allow("passed")) };
    if !subagent {
        let mut tf = TraceFile::open(ctx, &sid)?;
        ensure_started(ctx, &mut tf, &sid, &cwd, payload)?;
        let seq = tf.seq();
        tf.push(TraceRecord::ToolBlocked {
            seq,
            t: tf.now,
            tool: tool.clone(),
            call_id: str_of(payload, "tool_use_id").to_string(),
            hook_id: hook.id.clone(),
            reason: reason.clone(),
            args_hash: hash,
        })?;
    }
    let reply = HookReply { stdout: None, stderr: Some(reason.clone()), exit: 2 };
    Ok(json!({"event": "PreToolUse", "block": true, "hook_id": hook.id, "reason": reason, "hook": reply.json()}))
}

/// `match --harness claude-code`: the link sync for the command's cwd.
pub fn match_claude(ctx: &AppCtx, m: &MatchResult) -> Result<Value> {
    let s = sync_claude_skills(ctx, m)?;
    Ok(json!({"dir": s.dir, "linked": s.linked, "changed": s.changed}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(id: &str, mode: &str, tool: &str, field: &str, regex: &str, params: &str) -> String {
        format!(
            "// Generated by rematch for entry x ({id}).\nexport default {{\n  id: {id:?},\n  mode: {mode:?},\n  tool: {tool:?},\n  field: {field:?},\n  regex: {regex:?},\n  message: \"stop: use the safe path\",\n  pass_condition: \"p\",\n  params: {params},\n}};\n"
        )
    }

    #[test]
    fn parses_generated_modules() {
        let h = parse_hook_module(&module("e1", "block", "bash", "command", r"(?i)rm\s+-rf", r#"{"source":"rm\\s+-rf","flags":"i"}"#)).unwrap();
        assert_eq!((h.id.as_str(), h.mode, h.tool.as_str(), h.field.as_str()), ("e1", HookMode::Block, "bash", "command"));
        assert_eq!(h.regex, r"(?i)rm\s+-rf");
        assert_eq!(h.message, "stop: use the safe path");
        let s = parse_hook_module(&module("e2", "stuck", "*", "", "", r#"{"repeats":2}"#)).unwrap();
        assert_eq!(s.param("repeats", 3), 2);
        assert!(s.applies_to("bash"));
        assert!(parse_hook_module("export default {\n  mode: \"block\",\n};").is_err());
    }

    #[test]
    fn library_module_round_trips() {
        use rematch_core::types::{Family, HookRule, Origin, Predicate};
        let mut e = crate::app::new_entry(
            crate::app::NewEntry {
                kind: Kind::Hook,
                name: "no-force-push".into(),
                branch: "b".into(),
                status: Status::Provisional,
                family: Family("anthropic/claude-opus".into()),
                origin: Origin::Description,
                predicate: Predicate::default(),
                trigger_line: "t".into(),
                path: PathBuf::from("/x"),
            },
            0,
        )
        .unwrap();
        e.hook = Some(HookRule::block("bash", "command", r"(?i)git\s+push\s+--force", "Use --force-with-lease.", "no force push"));
        let text = library::hook_module(&e).unwrap();
        let h = parse_hook_module(&text).unwrap();
        assert_eq!((h.id.as_str(), h.mode, h.regex.as_str()), (e.id.as_str(), HookMode::Block, r"(?i)git\s+push\s+--force"));
        assert_eq!(h.message, "Use --force-with-lease.");
    }

    #[test]
    fn stuck_run_reads_the_trace() {
        let rec = |seq: u32, tool: &str, hash: &str| TraceRecord::ToolResult {
            seq,
            t: 0,
            tool: tool.into(),
            call_id: String::new(),
            input: Value::Null,
            args_hash: hash.into(),
            result_bytes: 0,
            is_error: true,
            error_class: None,
            content_head: String::new(),
            skill_read: None,
            path: None,
        };
        let blocked = |seq: u32, hook: &str, hash: &str| TraceRecord::ToolBlocked {
            seq,
            t: 0,
            tool: "bash".into(),
            call_id: String::new(),
            hook_id: hook.into(),
            reason: String::new(),
            args_hash: hash.into(),
        };
        let recs = vec![
            rec(0, "bash", "a"),
            rec(1, "bash", "h"),
            TraceRecord::TurnEnd { seq: 2, t: 0, model: None, usage: None, stop_reason: None, text_head: String::new() },
            rec(3, "bash", "h"),
            blocked(4, "blk", "h"),
            blocked(5, "stk", "h"),
        ];
        assert_eq!(trailing_run(&recs, "bash", "h", &["stk"]), 3);
        assert_eq!(trailing_run(&recs, "bash", "x", &["stk"]), 0);
        assert_eq!(trailing_run(&recs[..2], "bash", "h", &[]), 1);
    }

    #[test]
    fn path_fields_alias() {
        let input = json!({"file_path": "/etc/passwd"});
        assert_eq!(field_value(&input, "path"), Some("/etc/passwd"));
        assert_eq!(field_value(&json!({"path": "/a"}), "file_path"), Some("/a"));
        assert_eq!(field_value(&input, "command"), None);
    }
}
