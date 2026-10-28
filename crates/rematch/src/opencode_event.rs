//! The OpenCode adapter's entry points (DESIGN §18, `adapters/opencode`). The plugin forwards
//! OpenCode's own hook and event payloads, unchanged and in host order, as one JSON envelope
//! on stdin; every mapping to the DESIGN §3 trace lives here and in
//! `rematch_core::import_opencode`, which the history importer shares.
//!
//! * `trace-event --harness opencode` — `{pid, directory, version, skills_paths, events: [...]}`
//!   (or one event object). Event `kind`s: `load` (sync the shared skills dir for the
//!   directory, report the approved hooks the plugin runs), `prompt` (`chat.message`: the user
//!   message and its parts, `command` when `command.execute.before` preceded it), `tool` (a
//!   tool part in a final state, from `message.part.updated`), `turn` (an assistant message
//!   with `time.completed` or an error, from `message.updated`, plus its text), `idle`
//!   (`session.idle`: the run ended), `retrieve` (`experimental.chat.system.transform`: an
//!   approved `retrieve` hook's units for the next request, recorded as `retrieval`),
//!   `reconcile`, `shutdown` (the plugin's `dispose`), `digest`.
//! * `hook-check --harness opencode` — `tool.execute.before`: the approved `block`/`stuck`
//!   hooks matched for the directory. A rejection is traced as `tool_blocked`; the plugin
//!   throws the reason, which OpenCode records as the tool part's error.
//!
//! **Lifecycle.** OpenCode has no session-end event: `session.idle` ends a run, and a session
//! can be continued at any time, from the same process or another. A session is traced in
//! *segments*: `oc:<id>`, then `oc:<id>:r2`, … Each OpenCode process that sends events for
//! a session registers its pid as an owner of the open segment
//! (`REMATCH_ROOT/opencode/open/<segment>.json`). A segment is finalized (`agent_end` of an
//! open run, `session_shutdown`, `pending/<segment>.json` when flagged) when its last owner
//! disposes the plugin (`shutdown`), when `reconcile` finds every owner gone (a crashed or
//! killed process), or when `reconcile` finds it quiet for [`QUIET_MS`] with no run going.
//! An event for a session whose latest segment is finalized opens the next segment: a
//! finished trace is never appended to, so `count` (which folds a finished trace once) never
//! misses resumed events and never folds a segment twice. Within a segment an idle session
//! taking a new prompt is one more run of the same trace.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rematch_core::features::head;
use rematch_core::import::{args_hash, repo_of, TRACE_INPUT_TEXT_CHARS};
use rematch_core::import_opencode::{self as oc, UserText};
use rematch_core::library::{self, MatchResult};
use rematch_core::trace::{flag_reasons, AgentInfo, LoadedEntry, RetrievedUnit, TraceRecord};
use rematch_core::types::{
    HookMode, Kind, ModelRef, DEFAULT_CHECK_BUDGET_BYTES, DEFAULT_CHECK_TIMEOUT_SECS,
    DEFAULT_RETRIEVE_BUDGET_BYTES, DEFAULT_RETRIEVE_MIN_OVERLAP, DEFAULT_STUCK_REPEATS,
};
use serde_json::{json, Value};

use crate::app::AppCtx;
use crate::trace_event::{files_changed, parse_hook_module, str_of, trailing_run, HookModule, TraceEventArgs, TraceFile};

/// A segment whose owners are alive but that saw no record for this long, with no run
/// going, is finalized by `reconcile`; the session's next event opens a new segment.
pub const QUIET_MS: i64 = 6 * 60 * 60 * 1000;
/// Name of the adapter's package (`adapters/opencode/package.json`), checked by `doctor`.
pub const PACKAGE_NAME: &str = "@vncp/rematch-opencode";

// ---- payload ----

/// What every event of one envelope shares.
struct Env {
    pid: u32,
    directory: Option<PathBuf>,
    version: String,
    /// `skills.paths` the plugin added to OpenCode's config (the shared dir when it loaded).
    skills_paths: Vec<String>,
}

impl Env {
    fn of(payload: &Value) -> Env {
        Env {
            pid: payload.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
            directory: Some(str_of(payload, "directory")).filter(|d| !d.is_empty()).map(PathBuf::from),
            version: str_of(payload, "version").to_string(),
            skills_paths: payload["skills_paths"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect(),
        }
    }

    fn cwd(&self, ctx: &AppCtx, ev: &Value) -> PathBuf {
        match str_of(ev, "directory") {
            "" => self.directory.clone().unwrap_or_else(|| ctx.cwd.clone()),
            d => PathBuf::from(d),
        }
    }

    fn version_of(&self, ev: &Value) -> String {
        match str_of(ev, "version") {
            "" => self.version.clone(),
            v => v.to_string(),
        }
    }
}

/// OpenCode session ids (`ses_<base62>`) become file names: nothing else is accepted.
fn native_of(ev: &Value) -> Result<String> {
    let id = str_of(ev, "session");
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')) {
        bail!("unusable OpenCode session id {id:?}");
    }
    Ok(id.to_string())
}

fn is_subagent(ev: &Value) -> bool {
    !str_of(ev, "parent").is_empty()
}

// ---- what loads ----

/// What a session in `cwd` loads under OpenCode.
pub struct Loaded {
    pub m: MatchResult,
    /// Approved, matching hook modules the plugin runs: `block`/`stuck` (through
    /// `hook-check`), `truncate` (`experimental.chat.messages.transform`) and `retrieve`
    /// (`experimental.chat.system.transform`, through a `retrieve` event).
    pub hooks: Vec<HookModule>,
    /// `session_start.loaded_entries`: predicate hits minus lessons and agents (not delivered
    /// to OpenCode), minus hooks that do not run and `retrieve` hooks (credited through
    /// `retrieval` records, as in omp); versioned.
    pub loaded_entries: Vec<LoadedEntry>,
}

pub fn load(ctx: &AppCtx, cwd: &Path) -> Result<Loaded> {
    let repo = repo_of(cwd);
    let m = library::list_for_match(&ctx.ledger, &ctx.paths, repo.as_deref(), cwd)?;
    let mut hooks = Vec::new();
    for file in &m.hooks {
        match std::fs::read_to_string(file).map_err(anyhow::Error::from).and_then(|t| parse_hook_module(&t)) {
            Ok(h) if h.mode == HookMode::Block && regex::Regex::new(&h.regex).is_err() => eprintln!("rematch: hook {} skipped: bad regex", h.id),
            Ok(h) => hooks.push(h),
            Err(e) => eprintln!("rematch: hook {} skipped: {e:#}", file.display()),
        }
    }
    let loaded_entries = m
        .entries
        .iter()
        .filter(|e| e.predicate_hit && !matches!(e.kind, Kind::Lesson | Kind::Agent))
        .filter(|e| e.kind != Kind::Hook || hooks.iter().any(|h| h.id == e.id && h.mode != HookMode::Retrieve))
        .map(|e| LoadedEntry { id: e.id.clone(), version: Some(e.version), sha: e.sha.clone() })
        .collect();
    Ok(Loaded { m, hooks, loaded_entries })
}

/// `dir` is among the `skills.paths` the plugin gave OpenCode.
fn delivered(env: &Env, dir: &Path) -> bool {
    let canon = rematch_core::paths::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    env.skills_paths.iter().any(|p| Path::new(p) == dir || rematch_core::paths::canonicalize(p).is_ok_and(|c| c == canon))
}

/// Skill directories OpenCode was given for this process: the place's view (the plugin puts
/// it into `skills.paths`, so provisional entries of this place load), else only the kept
/// library (an older plugin or a manual `skills.paths`: no provisional entry reached the
/// model). `None` when neither.
fn delivered_sync<'a>(ctx: &AppCtx, env: &Env, synced: &'a crate::skills_cmd::CwdSync) -> Option<&'a rematch_core::library::SkillsSync> {
    if synced.view.dirs.first().is_some_and(|d| delivered(env, d)) {
        Some(&synced.view)
    } else if delivered(env, &ctx.paths.skills_shared) {
        Some(&synced.shared)
    } else {
        None
    }
}

/// The rematch entry a successful OpenCode skill load or `read` opened. A `read` of a
/// `…/<name>/SKILL.md` resolves through the `rematch.json` beside it. The `skill` tool loads by
/// name: credited only through a skills dir OpenCode was given (this directory's view, else the
/// kept library) and only when no other skill root OpenCode reads holds a skill of that name
/// (another `skills.paths` entry, `.opencode/skill(s)` or `.claude/skills` from the directory up,
/// the user's `~/.config/opencode/skill(s)` and `~/.claude/skills`): which one OpenCode picked
/// is then unknown, so a same-named native skill earns rematch nothing.
fn skill_entry_of(ctx: &AppCtx, env: &Env, cwd: &Path, name: &str, path: Option<&str>) -> Option<String> {
    if let Some(p) = path.filter(|p| p.ends_with("SKILL.md")) {
        return crate::skills_cmd::entry_of_skill_path(p);
    }
    let synced = crate::skills_cmd::sync_cwd(ctx, cwd).ok()?;
    let ours = delivered_sync(ctx, env, &synced)?.dirs.first()?.join(name);
    let id = rematch_core::library::load_entry(&ours).ok()?.id;
    let home = rematch_core::paths::home_dir();
    let mut roots: Vec<PathBuf> = env
        .skills_paths
        .iter()
        .map(PathBuf::from)
        .filter(|p| p != &ctx.paths.skills_shared && !p.starts_with(rematch_core::library::views_dir(&ctx.paths)))
        .collect();
    for a in cwd.ancestors() {
        roots.extend([".opencode/skill", ".opencode/skills", ".claude/skills"].map(|d| a.join(d)));
    }
    if let Some(h) = home {
        roots.extend([".config/opencode/skill", ".config/opencode/skills", ".claude/skills"].map(|d| h.join(d)));
    }
    let native = roots.iter().any(|r| r.join(name).join("SKILL.md").is_file() && rematch_core::library::load_entry(&r.join(name)).map_or(true, |e| e.id != id));
    (!native).then_some(id)
}

// ---- segments ----

fn open_dir(ctx: &AppCtx) -> PathBuf {
    ctx.paths.root.join("opencode").join("open")
}

fn owner_path(ctx: &AppCtx, sid: &str) -> PathBuf {
    open_dir(ctx).join(format!("{}.json", rematch_core::paths::id_file_stem(sid)))
}

fn read_owners(p: &Path) -> Vec<u32> {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| v["pids"].as_array().into_iter().flatten().filter_map(Value::as_u64).map(|p| p as u32).collect())
        .unwrap_or_default()
}

fn write_owners(p: &Path, native: &str, pids: &[u32]) -> Result<()> {
    std::fs::create_dir_all(p.parent().expect("owner file has a parent"))?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(&json!({"session": native, "pids": pids}))?).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, p).with_context(|| format!("rename {}", p.display()))
}

/// A `session_shutdown` after the last `session_start` (`count`'s rule).
fn finished(records: &[TraceRecord]) -> bool {
    let Some(start) = records.iter().rposition(|r| matches!(r, TraceRecord::SessionStart { .. })) else { return false };
    records[start..].iter().any(|r| matches!(r, TraceRecord::SessionShutdown { .. }))
}

/// The open segment of a session, started when needed, with `pid` registered as an owner.
fn open_segment(ctx: &AppCtx, env: &Env, ev: &Value, native: &str, model: Option<ModelRef>, extra: &mut Value) -> Result<(String, TraceFile)> {
    let mut n = 1;
    while ctx.paths.trace_file(&oc::segment_id(native, n + 1)).exists() {
        n += 1;
    }
    let mut sid = oc::segment_id(native, n);
    let mut tf = TraceFile::open(ctx, &sid)?;
    if tf.started() && finished(&tf.records) {
        drop(tf);
        n += 1;
        sid = oc::segment_id(native, n);
        tf = TraceFile::open(ctx, &sid)?;
    }
    if !tf.started() {
        let mode = if n == 1 { "opencode" } else { "opencode:resume" };
        start(ctx, env, ev, &mut tf, &sid, mode, model, extra)?;
    }
    let owners = owner_path(ctx, &sid);
    let mut pids = read_owners(&owners);
    if !pids.contains(&env.pid) || !owners.exists() {
        pids.retain(|p| *p != env.pid);
        pids.push(env.pid);
        write_owners(&owners, native, &pids)?;
    }
    Ok((sid, tf))
}

/// `session_start` after matching the directory and syncing the kept library and its view.
#[allow(clippy::too_many_arguments)]
fn start(ctx: &AppCtx, env: &Env, ev: &Value, tf: &mut TraceFile, sid: &str, mode: &str, model: Option<ModelRef>, extra: &mut Value) -> Result<()> {
    let cwd = env.cwd(ctx, ev);
    let loaded = load(ctx, &cwd)?;
    let synced = crate::skills_cmd::sync_cwd(ctx, &cwd)?;
    let sync = delivered_sync(ctx, env, &synced);
    // Only entries behind a directory OpenCode was given reached the model (hooks run anyway).
    let loaded_entries: Vec<LoadedEntry> = loaded
        .loaded_entries
        .iter()
        .filter(|e| loaded.hooks.iter().any(|h| h.id == e.id) || sync.is_some_and(|s| s.visible.contains(&e.id)))
        .cloned()
        .collect();
    let repo_dir = repo_of(&cwd);
    let id = repo_dir.as_deref().map(|r| rematch_core::identity::identify(r, &cwd)).unwrap_or_default();
    let repo = repo_dir.map(|p| p.to_string_lossy().into_owned());
    let cwd_s = cwd.to_string_lossy().into_owned();
    let version = env.version_of(ev);
    let seq = tf.seq();
    tf.push(TraceRecord::SessionStart {
        seq,
        t: tf.now,
        session_id: sid.to_string(),
        branch_hint: Path::new(repo.as_deref().unwrap_or(&cwd_s)).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        cwd: cwd_s,
        repo,
        model,
        mode: mode.to_string(),
        agent: Some(AgentInfo { kind: "main".into(), name: match str_of(ev, "agent") {
            "" => "main".to_string(),
            a => a.to_string(),
        } }),
        omp_version: if version.is_empty() { "opencode".into() } else { format!("opencode {version}") },
        loaded_entries: loaded_entries.clone(),
        hooks_loaded: loaded.hooks.iter().map(|h| h.id.clone()).collect(),
        staging_dirs: sync.map(|s| s.dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect()).unwrap_or_default(),
        repo_remote: id.remote,
        subpath: id.subpath,
        head_sha: id.head_sha,
        visible_entries: sync.map(|s| s.visible.clone()).unwrap_or_default(),
        instructions_exposed: Vec::new(),
        exposure_view: sync.and_then(|s| s.view.clone()),
    })?;
    extra["started"] = json!(sid);
    extra["loaded_entries"] = json!(loaded_entries);
    Ok(())
}

/// Writes `agent_end` for the open run: its turns, `yielded` when its last turn yielded.
fn end_run(tf: &mut TraceFile, t: i64, yielded_override: Option<bool>) -> Result<()> {
    if !tf.run_active() {
        return Ok(());
    }
    let run = tf.open_run();
    let turns = run.iter().filter(|r| matches!(r, TraceRecord::TurnEnd { .. })).count() as u32;
    let last_stop = run.iter().rev().find_map(|r| match r {
        TraceRecord::TurnEnd { stop_reason, .. } => Some(stop_reason.clone()),
        _ => None,
    });
    let yielded = yielded_override.unwrap_or_else(|| oc::yielded(last_stop.flatten().as_deref()));
    let seq = tf.seq();
    tf.push(TraceRecord::AgentEnd { seq, t, turns, is_terminal: true, yielded })
}

/// Closes a segment: the open run, `session_shutdown` (flag reasons, changed files), the
/// pending file when flagged; the owner file goes.
fn finalize(ctx: &AppCtx, sid: &str, tf: &mut TraceFile) -> Result<Value> {
    let now = tf.now;
    end_run(tf, now, None)?;
    let reasons = flag_reasons(&tf.records);
    let flagged = !reasons.is_empty();
    let files_changed = files_changed(&tf.records);
    let seq = tf.seq();
    tf.push(TraceRecord::SessionShutdown { seq, t: now, reasons: reasons.clone(), flagged, files_changed })?;
    let mut pending = Value::Null;
    if flagged {
        std::fs::create_dir_all(&ctx.paths.pending)?;
        let p = ctx.paths.pending_file(sid);
        let body = json!({"session_id": sid, "flagged_at": ctx.now, "reasons": reasons, "trace": tf.path, "source": "opencode"});
        std::fs::write(&p, serde_json::to_string(&body)?).with_context(|| format!("write {}", p.display()))?;
        pending = json!(p);
    }
    let _ = std::fs::remove_file(owner_path(ctx, sid));
    Ok(json!({"session": sid, "flagged": flagged, "reasons": reasons, "pending": pending}))
}

/// Open segments (owner files), `(segment id, owner file)`.
fn open_segments(ctx: &AppCtx) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(open_dir(ctx)) else { return Vec::new() };
    let mut v: Vec<(String, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| Some((rematch_core::paths::id_from_file_stem(p.file_stem()?.to_str()?).into_owned(), p)))
        .collect();
    v.sort();
    v
}

/// `reconcile`: finalizes segments whose owners are all gone, or that went quiet.
/// `shutdown`: `pid` leaves every segment it owns; those it was the last owner of end.
fn settle(ctx: &AppCtx, leaving: Option<u32>) -> Result<Vec<Value>> {
    let mut done = Vec::new();
    for (sid, owner) in open_segments(ctx) {
        let pids = read_owners(&owner);
        if leaving.is_some_and(|p| !pids.contains(&p)) {
            continue;
        }
        let alive: Vec<u32> = pids.iter().copied().filter(|p| Some(*p) != leaving && crate::jobs::pid_alive(i64::from(*p))).collect();
        let mut tf = TraceFile::open(ctx, &sid)?;
        if !tf.started() || finished(&tf.records) {
            let _ = std::fs::remove_file(&owner);
            continue;
        }
        let last = tf.records.iter().map(TraceRecord::t).max().unwrap_or(0);
        let quiet = leaving.is_none() && !tf.run_active() && ctx.now - last > QUIET_MS;
        if alive.is_empty() || quiet {
            let mut r = finalize(ctx, &sid, &mut tf)?;
            r["why"] = json!(if quiet && !alive.is_empty() { "quiet" } else if leaving.is_some() { "shutdown" } else { "owners gone" });
            done.push(r);
        } else if alive != pids {
            write_owners(&owner, str_of(&serde_json::from_str::<Value>(&std::fs::read_to_string(&owner).unwrap_or_default()).unwrap_or(Value::Null), "session"), &alive)?;
        }
    }
    Ok(done)
}

// ---- trace-event ----

pub fn trace_event(ctx: &AppCtx, a: &TraceEventArgs, payload: &Value) -> Result<Value> {
    if a.hook_output {
        bail!("--hook-output is Claude Code's hook protocol; the OpenCode plugin reads the envelope");
    }
    if a.session.is_some() {
        bail!("--session does not apply to OpenCode: each event names its session");
    }
    let env = Env::of(payload);
    let events: Vec<&Value> = match payload.get("events").and_then(Value::as_array) {
        Some(list) => list.iter().collect(),
        None => vec![payload],
    };
    let mut results = Vec::new();
    for ev in events {
        let kind = str_of(ev, "kind");
        // One bad event never drops the others of its envelope.
        let r = event(ctx, &env, ev).unwrap_or_else(|e| json!({"kind": kind, "error": format!("{e:#}")}));
        results.push(r);
    }
    Ok(json!({"events": results}))
}

fn event(ctx: &AppCtx, env: &Env, ev: &Value) -> Result<Value> {
    let kind = str_of(ev, "kind");
    if ctx.paths.is_off() {
        // Nothing loads or is written while off; the sync empties the shared dir and views.
        if kind == "load" {
            crate::skills_cmd::sync_cwd(ctx, &env.cwd(ctx, ev))?;
        }
        return Ok(json!({"kind": kind, "off": true}));
    }
    match kind {
        "load" => {
            let cwd = env.cwd(ctx, ev);
            let synced = crate::skills_cmd::sync_cwd(ctx, &cwd)?;
            let sync = &synced.view;
            let loaded = load(ctx, &cwd)?;
            let hooks: Vec<Value> = loaded.hooks.iter().map(|h| json!({"id": h.id, "mode": h.mode, "tool": h.tool, "params": h.params})).collect();
            // `dirs`: this place's view (kept library + this place's provisional entries), the
            // one directory the plugin puts into `skills.paths` for this process.
            Ok(json!({"kind": kind, "skills_dir": ctx.paths.skills_shared, "view": sync.view, "dirs": sync.dirs, "skills": sync.links.keys().collect::<Vec<_>>(), "hooks": hooks}))
        }
        "reconcile" => Ok(json!({"kind": kind, "finalized": settle(ctx, None)?})),
        "shutdown" => Ok(json!({"kind": kind, "finalized": settle(ctx, Some(env.pid))?})),
        "digest" => Ok(json!({"kind": kind, "digest": digest(ctx)?})),
        "prompt" | "tool" | "turn" | "idle" | "retrieve" | "check" | "fragments" => session_event(ctx, env, ev, kind),
        other => bail!("unknown OpenCode event kind {other:?}"),
    }
}

fn session_event(ctx: &AppCtx, env: &Env, ev: &Value, kind: &str) -> Result<Value> {
    let native = native_of(ev)?;
    // Subagent sessions are not traced (their prompt is the parent agent's); the plugin
    // never sends them, this guards the boundary.
    if is_subagent(ev) {
        return Ok(json!({"kind": kind, "skipped": "subagent"}));
    }
    let model = match kind {
        "prompt" => oc::model_of(&ev["message"]),
        "turn" => oc::model_of(&ev["info"]),
        _ => None,
    };
    let mut extra = json!({});
    let (sid, mut tf) = open_segment(ctx, env, ev, &native, model, &mut extra)?;
    let before = tf.records.len();
    match kind {
        "prompt" => prompt(&mut tf, ev)?,
        "tool" => {
            let part = &ev["part"];
            let call = str_of(part, "callID");
            let seen = tf.records.iter().any(|r| matches!(r, TraceRecord::ToolResult { call_id, .. } if call_id == call));
            let seq = tf.seq();
            if let (false, Some(mut rec)) = (seen && !call.is_empty(), oc::tool_result_record(seq, part)) {
                if let TraceRecord::ToolResult { skill_read: Some(name), path, is_error: false, skill_entry, .. } = &mut rec {
                    *skill_entry = skill_entry_of(ctx, env, &env.cwd(ctx, ev), name, path.as_deref());
                }
                tf.push(rec)?;
            }
        }
        "fragments" => {
            // The plugin put these approved fragments into this request's system prompt.
            let fragments: Vec<rematch_core::trace::ExposedInstruction> = serde_json::from_value(ev["fragments"].clone()).unwrap_or_default();
            let sent = |f: &rematch_core::trace::ExposedInstruction| {
                tf.records.iter().any(|r| matches!(r, TraceRecord::FragmentsSent { fragments: x, .. } if x.iter().any(|y| y.id == f.id && y.sha == f.sha)))
            };
            let new: Vec<_> = fragments.into_iter().filter(|f| !f.sha.is_empty() && !sent(f)).map(|f| rematch_core::trace::ExposedInstruction { via: "opencode:system.transform".into(), ..f }).collect();
            if !new.is_empty() {
                let seq = tf.seq();
                let bytes = ev.get("bytes").and_then(Value::as_u64).unwrap_or(0);
                tf.push(TraceRecord::FragmentsSent { seq, t: tf.now, host: "opencode".into(), fragments: new, bytes })?;
            }
        }
        "turn" => {
            let seq = tf.seq();
            if let Some(TraceRecord::TurnEnd { t, model, usage, stop_reason, text_head, .. }) = oc::turn_record(seq, &ev["info"], str_of(ev, "text")) {
                let seen = tf.records.iter().any(|r| matches!(r, TraceRecord::TurnEnd { t: t0, model: m0, .. } if *t0 == t && *m0 == model));
                if !seen {
                    let last = tf.last_model();
                    if let (Some(to), Some(from)) = (&model, &last) {
                        if to != from {
                            let seq = tf.seq();
                            tf.push(TraceRecord::ModelChange { seq, t, from: Some(from.clone()), to: Some(to.clone()), reason: Some("turn".into()) })?;
                        }
                    }
                    let seq = tf.seq();
                    tf.push(TraceRecord::TurnEnd { seq, t, model, usage, stop_reason, text_head })?;
                }
            }
        }
        "idle" => {
            let t = ev.get("t").and_then(Value::as_i64).unwrap_or(tf.now);
            end_run(&mut tf, t, None)?;
        }
        "retrieve" => {
            // Before the session's prompt reached the trace there is nothing to query: the
            // plugin asks again on the next request instead of caching "no units".
            let has_input = tf.records.iter().any(|r| matches!(r, TraceRecord::Input { .. }));
            extra["has_input"] = json!(has_input);
            // Units only from packages OpenCode was given (the view or kept library in `skills.paths`).
            if has_input && (delivered(env, &ctx.paths.skills_shared) || env.skills_paths.iter().any(|p| Path::new(p).starts_with(rematch_core::library::views_dir(&ctx.paths)))) {
                let cwd = env.cwd(ctx, ev);
                if let Some(context) = retrieve(ctx, &mut tf, &cwd)? {
                    extra["context"] = json!(context);
                }
            }
        }
        "check" => {
            let cwd = env.cwd(ctx, ev);
            let files: Vec<PathBuf> = ev["files"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|p| if Path::new(p).is_absolute() { PathBuf::from(p) } else { cwd.join(p) })
                .collect();
            if !files.is_empty() {
                if let Some(out) = run_check_event(ctx, &mut tf, &cwd, &files)? {
                    extra = out;
                }
            }
        }
        _ => unreachable!("session_event kinds"),
    }
    let written: Vec<&str> = tf.records[before..].iter().map(TraceRecord::ev).collect();
    let mut out = json!({"kind": kind, "session": sid, "trace": tf.path, "records": written});
    if let (Some(o), Value::Object(x)) = (out.as_object_mut(), extra) {
        o.extend(x);
    }
    Ok(out)
}

/// `chat.message`: the user message's typed words → `input` (a takeover closes the open run
/// first, as in the Claude Code adapter). A command expansion is not a prompt: only
/// `/rematch-mark <class>` becomes the mark input; other commands are dropped as in omp.
fn prompt(tf: &mut TraceFile, ev: &Value) -> Result<()> {
    let parts: Vec<Value> = ev["parts"].as_array().cloned().unwrap_or_default();
    let text = match ev.get("command").filter(|c| c.is_object()) {
        Some(c) => {
            let name = str_of(c, "name");
            let args = str_of(c, "arguments").trim();
            if name == "rematch-mark" {
                UserText::Command(format!("/rematch-mark {args}"))
            } else {
                return Ok(());
            }
        }
        None => oc::user_prompt(&parts),
    };
    if text == UserText::Ignored {
        return Ok(());
    }
    let t = ev.pointer("/message/time/created").and_then(Value::as_i64).unwrap_or(tf.now);
    if tf.records.iter().any(|r| matches!(r, TraceRecord::Input { t: t0, .. } if *t0 == t)) {
        return Ok(());
    }
    let idle = !tf.run_active();
    let prev = tf.records.iter().rev().find_map(|r| match r {
        TraceRecord::Input { text, .. } => Some(text.clone()),
        _ => None,
    });
    let deliverable = tf.records.iter().rev().find_map(|r| match r {
        TraceRecord::TurnEnd { text_head, .. } if !text_head.trim().is_empty() => Some(text_head.clone()),
        _ => None,
    });
    // A slash line the user typed is a command (omp's rule): nothing to trace, no takeover.
    if oc::prompt_record(0, t, &text, idle, prev.as_deref(), deliverable.as_deref()).is_none() {
        return Ok(());
    }
    if !idle {
        end_run(tf, t, Some(false))?;
    }
    let seq = tf.seq();
    if let Some(rec) = oc::prompt_record(seq, t, &text, idle, prev.as_deref(), deliverable.as_deref()) {
        tf.push(rec)?;
    }
    Ok(())
}

/// Characters of the toast (OpenCode toasts are one short notice; `/rematch-digest` shows all).
const TOAST_CHARS: usize = 900;

/// The session-start digest (`rematch digest`, which advances `digest.last`): its lines, every
/// action waiting for the user (`digest.actions`: listed every session until decided, each with
/// its exact approve/decline/inspect commands, never its proposed text), and a short toast.
fn digest(ctx: &AppCtx) -> Result<Value> {
    let d = crate::digest::cmd_digest(ctx)?;
    let lines: Vec<String> = d["lines"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
    let actions: Vec<Value> = d["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["id"].is_string())
        .map(|a| json!({"id": a["id"], "kind": a["kind"], "name": a["name"], "state": a["state"], "line": crate::trace_event::action_line(a, "/rematch-")}))
        .collect();
    // Actions first: they wait for the user; the lines are news.
    let mut toast = String::new();
    if !actions.is_empty() {
        toast.push_str(&format!("{} action(s) waiting for your review (low-risk items apply automatically):\n", actions.len()));
    }
    for a in actions.iter().take(3) {
        toast.push_str(&format!("{}\n", a["line"].as_str().unwrap_or_default()));
    }
    if actions.len() > 3 {
        toast.push_str(&format!("+{} more actions (/rematch-digest)\n", actions.len() - 3));
    }
    for l in lines.iter().take(3) {
        toast.push_str(&format!("{l}\n"));
    }
    if lines.len() > 3 {
        toast.push_str(&format!("+{} more (/rematch-digest)\n", lines.len() - 3));
    }
    let toast = rematch_core::features::head(toast.trim_end(), TOAST_CHARS);
    Ok(json!({"lines": lines, "actions": actions, "toast": if toast.is_empty() { Value::Null } else { json!(toast) }}))
}

/// Tool results whose paths join the retrieval query (the extension's `retrievalQuery`).
const RETRIEVAL_QUERY_PATHS: usize = 10;

/// An approved `retrieve` hook, run from `experimental.chat.system.transform` before a model
/// request: units of the packages this session loaded that overlap the last prompt plus the
/// paths of the last tool results (`retrieve::rank`, the module's budget and min overlap).
/// Returns the system text for this request only (OpenCode does not persist it); a
/// `retrieval` record crediting that hook is written when the unit set differs from the last
/// one, so counting attributes the added context to the hook and the packages the units came
/// from. With several approved `retrieve` hooks the first (match order) runs and is credited.
fn retrieve(ctx: &AppCtx, tf: &mut TraceFile, cwd: &Path) -> Result<Option<String>> {
    let loaded = load(ctx, cwd)?;
    let Some(hook) = loaded.hooks.iter().find(|h| h.mode == HookMode::Retrieve) else { return Ok(None) };
    let Some(text) = tf.records.iter().rev().find_map(|r| match r {
        TraceRecord::Input { text, .. } => Some(text.clone()),
        _ => None,
    }) else {
        return Ok(None);
    };
    let mut paths: Vec<String> = Vec::new();
    for r in tf.records.iter().rev().filter(|r| matches!(r, TraceRecord::ToolResult { .. })).take(RETRIEVAL_QUERY_PATHS) {
        if let TraceRecord::ToolResult { path: Some(p), .. } = r {
            if !paths.contains(p) {
                paths.push(p.clone());
            }
        }
    }
    paths.reverse();
    let query = head(&[vec![text], paths].concat().join("\n"), TRACE_INPUT_TEXT_CHARS);
    let candidates = crate::retrieve::candidates(ctx, cwd, &[])?;
    let r = crate::retrieve::rank(
        &candidates,
        &query,
        hook.param("budget_bytes", DEFAULT_RETRIEVE_BUDGET_BYTES) as usize,
        hook.param("min_overlap", DEFAULT_RETRIEVE_MIN_OVERLAP),
    );
    if r.units.is_empty() {
        return Ok(None);
    }
    let units: Vec<RetrievedUnit> =
        r.units.iter().map(|u| RetrievedUnit { entry_id: u.entry_id.clone(), path: u.path.clone(), bytes: u.text.len() as u64 }).collect();
    let last = tf.records.iter().rev().find_map(|r| match r {
        TraceRecord::Retrieval { units, .. } => Some(units.clone()),
        _ => None,
    });
    let same = last.is_some_and(|l| l.iter().map(|u| (&u.entry_id, &u.path)).eq(units.iter().map(|u| (&u.entry_id, &u.path))));
    if !same {
        let seq = tf.seq();
        let bytes = units.iter().map(|u| u.bytes).sum();
        tf.push(TraceRecord::Retrieval { seq, t: tf.now, entry_ids: vec![hook.id.clone()], units, bytes })?;
    }
    let body: Vec<String> =
        r.units.iter().map(|u| format!("<unit skill=\"{}\" path=\"{}\">\n{}\n</unit>", u.name, u.path, u.text.trim_end())).collect();
    Ok(Some(format!(
        "[rematch] Units of skills loaded in this session that match the current request, retrieved by the harness (the whole skill: the `skill` tool).\n\n{}",
        body.join("\n\n")
    )))
}

/// An active `check` hook under OpenCode (`check` event): runs `rematch check` scoped to `files`,
/// records `check_ran` in the trace, and returns `{context, ok, errors, check_kind, duration_ms}`
/// (`None` when nothing ran or nothing is worth inserting).
fn run_check_event(ctx: &AppCtx, tf: &mut TraceFile, cwd: &Path, files: &[PathBuf]) -> Result<Option<Value>> {
    let loaded = load(ctx, cwd)?;
    let Some(hook) = loaded.hooks.iter().find(|h| h.mode == HookMode::Check) else {
        return Ok(None);
    };
    if !hook.applies_to("edit") && !hook.applies_to("*") {
        return Ok(None);
    }
    let repo_top = rematch_core::check::find_repo_top(cwd);
    let Some(spec) = rematch_core::check::detect_for_files(&repo_top, files) else {
        return Ok(None);
    };
    let timeout_secs = hook.param("timeout_secs", DEFAULT_CHECK_TIMEOUT_SECS).max(1);
    let budget_bytes = hook.param("budget_bytes", DEFAULT_CHECK_BUDGET_BYTES) as usize;
    let rep = crate::check::run_with_impact(
        &spec,
        files,
        std::time::Duration::from_secs(u64::from(timeout_secs)),
        budget_bytes,
    );
    let errors = rep.diagnostics.iter().filter(|d| d.severity == "error").count();
    let Some(rendered) = rep.rendered.clone().filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    crate::check::push_check_ran(tf, &hook.id, &spec, &rep, files)?;
    Ok(Some(json!({"context": rendered, "ok": rep.ok, "errors": errors, "check_kind": spec.kind.as_str(), "duration_ms": rep.duration_ms})))
}

// ---- hook-check ----

/// `input[field]`; omp's `path`, Claude Code's `file_path` and OpenCode's `filePath` stand in
/// for each other.
fn field_of<'a>(input: &'a Value, field: &str) -> Option<&'a str> {
    const PATHS: [&str; 3] = ["path", "file_path", "filePath"];
    if let Some(v) = input.get(field).and_then(Value::as_str) {
        return Some(v);
    }
    if PATHS.contains(&field) {
        return PATHS.iter().find_map(|k| input.get(*k).and_then(Value::as_str));
    }
    None
}

/// `tool.execute.before`: `{pid, directory, session, parent, tool, call_id, args}`.
pub fn hook_check(ctx: &AppCtx, a: &TraceEventArgs, payload: &Value) -> Result<Value> {
    if a.hook_output {
        bail!("--hook-output is Claude Code's hook protocol; the OpenCode plugin reads the envelope");
    }
    let allow = |why: &str| json!({"block": false, "why": why});
    if ctx.paths.is_off() {
        return Ok(allow("off"));
    }
    let env = Env::of(payload);
    let tool = oc::tool_name(str_of(payload, "tool"));
    let input = payload.get("args").cloned().unwrap_or(Value::Null);
    let cwd = env.cwd(ctx, payload);
    let loaded = load(ctx, &cwd)?;
    let call_hooks: Vec<&HookModule> =
        loaded.hooks.iter().filter(|h| matches!(h.mode, HookMode::Block | HookMode::Stuck) && h.applies_to(&tool)).collect();
    if call_hooks.is_empty() {
        return Ok(allow("no matching block/stuck hook"));
    }
    let subagent = is_subagent(payload);
    let native = native_of(payload)?;
    let hash = args_hash(&input);
    let stuck_ids: Vec<&str> = loaded.hooks.iter().filter(|h| h.mode == HookMode::Stuck).map(|h| h.id.as_str()).collect();
    // The main session's open segment (its call history); subagents have no trace.
    let mut seg = if subagent { None } else { Some(open_segment(ctx, &env, payload, &native, None, &mut json!({}))?) };
    let empty = Vec::new();
    let records = seg.as_ref().map_or(&empty, |(_, tf)| &tf.records);
    let mut blocked: Option<(&HookModule, String)> = None;
    for h in &call_hooks {
        let reason = match h.mode {
            HookMode::Block => {
                let re = regex::Regex::new(&h.regex)?;
                field_of(&input, &h.field).is_some_and(|v| re.is_match(v)).then(|| h.message.clone())
            }
            HookMode::Stuck if !subagent => {
                (trailing_run(records, &tool, &hash, &stuck_ids) >= h.param("repeats", DEFAULT_STUCK_REPEATS)).then(|| h.message.clone())
            }
            _ => None,
        };
        if let Some(r) = reason {
            blocked = Some((h, r));
            break;
        }
    }
    let Some((hook, reason)) = blocked else { return Ok(allow("passed")) };
    if let Some((_, tf)) = seg.as_mut() {
        let seq = tf.seq();
        tf.push(TraceRecord::ToolBlocked {
            seq,
            t: tf.now,
            tool: tool.clone(),
            call_id: str_of(payload, "call_id").to_string(),
            hook_id: hook.id.clone(),
            reason: reason.clone(),
            args_hash: hash,
        })?;
    }
    Ok(json!({"block": true, "hook_id": hook.id, "reason": reason}))
}

// ---- doctor ----

/// OpenCode's global config dir: `$OPENCODE_CONFIG_DIR`, else `$XDG_CONFIG_HOME/opencode`,
/// else `~/.config/opencode`.
fn config_dir() -> Option<PathBuf> {
    let non_empty = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    non_empty("OPENCODE_CONFIG_DIR")
        .or_else(|| non_empty("XDG_CONFIG_HOME").map(|d| d.join("opencode")))
        .or_else(|| rematch_core::paths::home_dir().map(|h| h.join(".config").join("opencode")))
}

/// The adapter package a plugin spec names: a directory (or a file in one) whose
/// `package.json` is [`PACKAGE_NAME`].
fn adapter_dir(spec: &str, base: &Path) -> Option<PathBuf> {
    let raw = spec.strip_prefix("file://").unwrap_or(spec);
    let p = if Path::new(raw).is_absolute() { PathBuf::from(raw) } else if raw.starts_with('.') { base.join(raw) } else { return None };
    let dir = if p.is_dir() { p } else { p.parent()?.to_path_buf() };
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    (v["name"] == PACKAGE_NAME).then(|| rematch_core::paths::canonicalize(&dir).unwrap_or(dir))
}

/// `doctor`'s opencode block: the plugin declared in OpenCode's global config
/// (`opencode.json`/`opencode.jsonc`/`config.json` `plugin` entries, or `$OPENCODE_CONFIG`)
/// or linked into `<config dir>/plugin(s)/`, and `opencode` on `PATH` (`$OPENCODE_BIN`).
pub fn doctor_harness(ctx: &AppCtx) -> (String, Value, Vec<String>, bool) {
    let Some(dir) = config_dir() else {
        return ("opencode: not set up (HOME is not set)".into(), json!({"config_dir": null}), vec![], false);
    };
    let mut files: Vec<PathBuf> = ["opencode.json", "opencode.jsonc", "config.json"].iter().map(|f| dir.join(f)).collect();
    if let Some(c) = std::env::var_os("OPENCODE_CONFIG").filter(|v| !v.is_empty()) {
        files.push(PathBuf::from(c));
    }
    let quoted = regex::Regex::new(r#""((?:file://)?[./~][^"]*)""#).expect("static regex");
    let mut found: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut dangling: Vec<String> = Vec::new();
    for f in files.iter().filter(|f| f.is_file()) {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        let base = f.parent().unwrap_or(&dir);
        for c in quoted.captures_iter(&text) {
            let spec = &c[1];
            if !spec.contains("opencode") && !spec.contains("rematch") {
                continue;
            }
            match adapter_dir(spec, base) {
                Some(d) => found.push((f.clone(), d)),
                None if spec.contains("adapters/opencode") => dangling.push(format!("{}: {spec}", f.display())),
                None => {}
            }
        }
    }
    for sub in ["plugin", "plugins"] {
        for name in ["rematch.ts", "rematch.js"] {
            let p = dir.join(sub).join(name);
            if let Some(d) = rematch_core::paths::canonicalize(&p).ok().and_then(|t| adapter_dir(&t.to_string_lossy(), &dir)) {
                found.push((p, d));
            }
        }
    }
    let bin = std::env::var_os("OPENCODE_BIN").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| crate::omp::which("opencode"));
    if found.is_empty() && dangling.is_empty() {
        return (
            format!("opencode: not set up (no plugin entry naming adapters/opencode in {})", dir.display()),
            json!({"config_dir": dir, "plugin": null, "opencode": bin}),
            vec![],
            false,
        );
    }
    let problems: Vec<String> = dangling.iter().map(|d| format!("plugin entry does not resolve to the rematch adapter: {d}")).collect();
    let target = found.first().map(|(_, d)| d.clone());
    let line = format!(
        "opencode: {} · {} → {} · skills via skills.paths += {} · {}",
        if problems.is_empty() { "ok" } else { "problem" },
        found.first().map(|(f, _)| f.display().to_string()).unwrap_or_else(|| dir.display().to_string()),
        target.as_deref().map(|t| t.display().to_string()).unwrap_or_else(|| "dangling".into()),
        ctx.paths.skills_shared.display(),
        match &bin {
            Some(b) => format!("opencode at {}", b.display()),
            None => "opencode not on PATH".into(),
        }
    );
    (line, json!({"config_dir": dir, "plugin": target, "declared_in": found.first().map(|(f, _)| f), "opencode": bin}), problems, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_fields_alias() {
        let input = json!({"filePath": "/etc/passwd"});
        assert_eq!(field_of(&input, "path"), Some("/etc/passwd"));
        assert_eq!(field_of(&input, "file_path"), Some("/etc/passwd"));
        assert_eq!(field_of(&json!({"command": "ls"}), "command"), Some("ls"));
        assert_eq!(field_of(&input, "command"), None);
    }

    #[test]
    fn finished_needs_a_shutdown_after_the_last_start() {
        let start = |seq| TraceRecord::SessionStart {
            seq,
            t: 0,
            session_id: String::new(),
            cwd: String::new(),
            repo: None,
            branch_hint: String::new(),
            model: None,
            mode: String::new(),
            agent: None,
            omp_version: String::new(),
            loaded_entries: vec![],
            hooks_loaded: vec![],
            staging_dirs: vec![],
            repo_remote: None,
            subpath: None,
            head_sha: None,
            visible_entries: vec![],
            instructions_exposed: vec![],
            exposure_view: None,
        };
        let shut = |seq| TraceRecord::SessionShutdown { seq, t: 0, reasons: vec![], flagged: false, files_changed: vec![] };
        assert!(!finished(&[]));
        assert!(finished(&[start(0), shut(1)]));
        assert!(!finished(&[start(0), shut(1), start(2)]));
    }
}
