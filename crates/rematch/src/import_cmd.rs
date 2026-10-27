//! `import --history [--harness omp|claude-code|codex|pi|opencode|all] [--sessions <dir>]`
//! (session history → ledger: omp session JSONL, Claude Code, Codex and Pi transcripts, the
//! OpenCode session store) and `import --forge <path>` (compound-forge skills → provisional
//! brownfield entries, learnings → provisional lessons). Both are idempotent. Also the lookup of a
//! ledger session's trace records (the lake's events file, else the live trace, else synthesized
//! from its harness transcript) shared by `improve` and `reflect --history`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Args;
use rematch_core::features::{rank_stems, stems_of_text, MAX_PREDICATE_STEMS};
use rematch_core::import::{
    apply_trace_signals, extract_outcomes, read_session, scan_sessions_dir, session_id_from_filename, session_record,
    trace_records_of,
};
use rematch_core::ledger::{EntryFilter, SessionFilter};
use rematch_core::{import_claude, import_codex, import_opencode, import_pi};
use rematch_core::library::{self, validate_name, REMATCH_JSON};
use rematch_core::outcome::{minhash, outcome_tokens, MINHASH_K};
use rematch_core::package::{slug, SkillDoc};
use rematch_core::predicate;
use rematch_core::trace::{self, TraceRecord};
use rematch_core::types::{Edit, Entry, Family, Kind, Origin, SessionRecord, Status};
use serde_json::{json, Value};

use crate::app::{new_entry, AppCtx, NewEntry};
use crate::lake_read::Source;

pub const FORGE_BRANCH: &str = "forge";

/// Whose session history `import --history` reads.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryHarness {
    /// omp: `<agent dir>/sessions` plus every profile's sessions dir.
    Omp,
    /// Claude Code: `~/.claude/projects` (`$CLAUDE_CONFIG_DIR/projects`).
    ClaudeCode,
    /// Codex: `~/.codex/sessions` (`$CODEX_HOME/sessions`).
    Codex,
    /// Pi: `~/.pi/agent/sessions` (`$PI_CODING_AGENT_SESSION_DIR`, `$PI_CODING_AGENT_DIR/sessions`).
    Pi,
    /// OpenCode: its session store `opencode.db` (`$OPENCODE_DB`, `$XDG_DATA_HOME/opencode`).
    #[value(name = "opencode")]
    OpenCode,
    /// Every harness whose default history dir exists.
    All,
}

#[derive(Args, Debug, Clone)]
pub struct ImportArgs {
    /// Import session history into the ledger.
    #[arg(long)]
    pub history: bool,
    /// Which harness's history: omp, claude-code, codex, pi, opencode, or all (default: each
    /// harness's default dir that exists).
    #[arg(long, value_enum, default_value_t = HistoryHarness::All)]
    pub harness: HistoryHarness,
    /// Sessions directory of the selected harness (OpenCode: its `opencode.db` or the dir
    /// holding it; with `--harness all`: an omp sessions dir).
    #[arg(long, value_name = "DIR")]
    pub sessions: Option<PathBuf>,
    /// Skip a named profile during `--history` import (repeatable; merged with `policy.json` `skip_profiles`).
    #[arg(long = "skip-profile", value_name = "NAME")]
    pub skip_profiles: Vec<String>,
    /// compound-forge checkout: skills/* → provisional brownfield entries, learnings/*.md → lessons.
    #[arg(long, value_name = "PATH")]
    pub forge: Option<PathBuf>,
    /// With --forge: copy each skill into rematch's own staging instead of registering it in place,
    /// so the forge checkout and plugin can be removed afterwards. Scripts are staged non-executable.
    #[arg(long, requires = "forge")]
    pub migrate: bool,
}

pub fn cmd_import(ctx: &AppCtx, a: ImportArgs) -> Result<Value> {
    if !a.history && a.forge.is_none() {
        return Err(crate::envelope::Usage("import needs --history and/or --forge <path>".into()).into());
    }
    let mut out = json!({});
    let mut lines = Vec::new();
    if a.history {
        let selected = |h: HistoryHarness| a.harness == h || (a.harness == HistoryHarness::All && (h == HistoryHarness::Omp || a.sessions.is_none()));
        let all = a.harness == HistoryHarness::All;
        let mut imported = 0;
        let mut omp_dirs: Vec<PathBuf> = Vec::new();
        if selected(HistoryHarness::Omp) && (!all || a.sessions.is_some() || omp_history_present(ctx)) {
            let h = match &a.sessions {
                Some(dir) => import_history(ctx, dir)?,
                None => import_history_all_profiles(ctx, &a.skip_profiles)?,
            };
            omp_dirs.extend(h.get("profiles").and_then(Value::as_object).into_iter().flatten().filter_map(|(_, p)| p["dir"].as_str().map(PathBuf::from)));
            omp_dirs.extend(h["dir"].as_str().map(PathBuf::from));
            lines.push(format!(
                "history: {} files, {} new, {} updated, {} kept from traces, {} failed; {} sessions with skill reads, {} redirects, {} outcomes",
                h["files"], h["new"], h["updated"], h["skipped_traced"], h["failed"], h["with_skill_reads"], h["redirects"], h["outcomes"]
            ));
            if let Some(profiles) = h.get("profiles").and_then(Value::as_object) {
                let summary: Vec<String> = profiles
                    .iter()
                    .map(|(name, p)| format!("{name}: {} ({} new)", p["files"], p["new"]))
                    .collect();
                let skipped: Vec<&str> = h
                    .get("skipped_profiles")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if !summary.is_empty() || !skipped.is_empty() {
                    let skip_note = if skipped.is_empty() {
                        String::new()
                    } else {
                        format!(" (skipped: {})", skipped.join(", "))
                    };
                    lines.push(format!("profiles: {}{skip_note}", summary.join(", ")));
                }
            }
            out["history"] = h;
            imported += 1;
        }
        let harnesses = [
            (Transcripts::ClaudeCode, "history_claude_code"),
            (Transcripts::Codex, "history_codex"),
            (Transcripts::Pi, "history_pi"),
            (Transcripts::OpenCode, "history_opencode"),
        ];
        for (h, key) in harnesses {
            if !selected(h.history_harness()) {
                continue;
            }
            let name = h.name();
            let found = match &a.sessions {
                Some(d) if h == Transcripts::OpenCode => Some(import_opencode::store_of(d)),
                Some(d) => Some(d.clone()),
                None => h.default_dir(ctx),
            };
            let exists = |p: &Path| if h == Transcripts::OpenCode { p.is_file() } else { p.is_dir() };
            let dir = match (found, all) {
                (Some(d), _) if exists(&d) => d,
                (_, true) => continue,
                (d, false) => bail!(
                    "{name} sessions {} does not exist",
                    d.map(|d| d.display().to_string()).unwrap_or_else(|| "(no home directory)".into())
                ),
            };
            // omp reads the same `PI_CODING_AGENT_DIR`: a Pi dir that is an omp sessions dir
            // imported above would import omp's sessions a second time under `pi:` ids.
            if all && h == Transcripts::Pi && omp_dirs.iter().any(|o| same_dir(o, &dir)) {
                lines.push(format!("pi history: {} is an omp sessions dir, skipped", dir.display()));
                continue;
            }
            let hist = import_harness_history(ctx, h, &dir)?;
            lines.push(format!(
                "{name} history: {} files ({} subagent), {} new, {} updated, {} kept from traces, {} failed; {} sessions with skill reads, {} redirects",
                hist["files"], hist["subagent_files"], hist["new"], hist["updated"], hist["skipped_traced"], hist["failed"], hist["with_skill_reads"], hist["redirects"]
            ));
            out[key] = hist;
            imported += 1;
        }
        if imported == 0 {
            bail!("no session history found (omp sessions, ~/.claude/projects, ~/.codex/sessions, ~/.pi/agent/sessions, opencode.db)");
        }
    }
    if let Some(forge) = &a.forge {
        let f = import_forge(ctx, forge, a.migrate)?;
        lines.push(format!(
            "forge: {} skills registered ({} existing), {} lessons registered ({} existing)",
            f["skills_registered"], f["skills_existing"], f["lessons_registered"], f["lessons_existing"]
        ));
        out["forge"] = f;
    }
    out["lines"] = json!(lines);
    Ok(out)
}

#[derive(Default)]
struct HistoryStats {
    files: usize,
    new: u32,
    updated: u32,
    skipped: u32,
    failed: u32,
    subagents: u32,
    skill_sessions: u32,
    skills: std::collections::BTreeMap<String, u32>,
    redirects: u32,
    outcomes: u32,
    /// Clean sessions that loaded entries, whose synthesized trace was written for `count`.
    traces: u32,
    errors: Vec<String>,
}

impl HistoryStats {
    fn merge(&mut self, other: &HistoryStats) {
        self.files += other.files;
        self.new += other.new;
        self.updated += other.updated;
        self.skipped += other.skipped;
        self.failed += other.failed;
        self.subagents += other.subagents;
        self.skill_sessions += other.skill_sessions;
        for (k, v) in &other.skills {
            *self.skills.entry(k.clone()).or_default() += v;
        }
        self.redirects += other.redirects;
        self.outcomes += other.outcomes;
        self.traces += other.traces;
        for e in &other.errors {
            if self.errors.len() < 10 {
                self.errors.push(e.clone());
            }
        }
    }

    fn to_json(&self, dir: &Path) -> Value {
        json!({
            "dir": dir,
            "files": self.files,
            "new": self.new,
            "updated": self.updated,
            "skipped_traced": self.skipped,
            "failed": self.failed,
            "subagent_files": self.subagents,
            "with_skill_reads": self.skill_sessions,
            "skills_read": self.skills,
            "redirects": self.redirects,
            "outcomes": self.outcomes,
            "traces_written": self.traces,
            "errors": self.errors,
        })
    }
}

fn import_dir_stats(ctx: &AppCtx, dir: &Path) -> Result<HistoryStats> {
    let files = scan_sessions_dir(dir)?;
    let mut st = HistoryStats { files: files.len(), ..Default::default() };
    let policy = ctx.domain_policy()?;
    for f in &files {
        let session = match read_session(&f.path) {
            Ok(s) => s,
            Err(e) => {
                st.failed += 1;
                if st.errors.len() < 10 {
                    st.errors.push(format!("{}: {e:#}", f.path.display()));
                }
                continue;
            }
        };
        let mut rec = session_record(&session);
        rec.domain = rematch_core::domain::session_domain(&rec, &policy);
        if rec.id.is_empty() {
            st.failed += 1;
            continue;
        }
        if f.parent_id.is_some() {
            st.subagents += 1;
        }
        match ctx.ledger.get_session(&rec.id)? {
            // Our own trace of the session is richer (loaded entries, prelabels): keep it.
            Some(existing) if existing.source == "trace" => {
                st.skipped += 1;
                continue;
            }
            Some(_) => st.updated += 1,
            None => st.new += 1,
        }
        if !rec.skill_reads.is_empty() {
            st.skill_sessions += 1;
            for s in &rec.skill_reads {
                *st.skills.entry(s.clone()).or_default() += 1;
            }
        }
        st.redirects += rec.redirects.len() as u32;
        ctx.ledger.upsert_session(&rec)?;
        for o in extract_outcomes(&session, &rec) {
            ctx.ledger.insert_outcome(&o)?;
            ctx.ledger.put_minhash(&o.id, &minhash(&outcome_tokens(&o), MINHASH_K))?;
            st.outcomes += 1;
        }
    }
    eprintln!("rematch import: {} session files from {}", files.len(), dir.display());
    Ok(st)
}

pub fn import_history(ctx: &AppCtx, dir: &Path) -> Result<Value> {
    if !dir.is_dir() {
        bail!("sessions dir {} does not exist", dir.display());
    }
    Ok(import_dir_stats(ctx, dir)?.to_json(dir))
}

/// Import the default agent dir (`ctx.paths.agent_dir` when default, plus `default_agent_dir()`)
/// and every `<config_root>/profiles/<name>/agent/sessions` except profiles in `skip_profiles`
/// or `REMATCH_ROOT/policy.json` `skip_profiles`.
pub fn import_history_all_profiles(ctx: &AppCtx, cli_skip: &[String]) -> Result<Value> {
    let policy = crate::scope::export::Policy::load(&ctx.paths.root).unwrap_or_default();
    let mut skip_set: std::collections::BTreeSet<String> = policy
        .skip_profiles
        .iter()
        .chain(cli_skip.iter())
        .filter_map(|s| rematch_core::paths::normalize_profile_name(s).or_else(|| {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }))
        .collect();

    let default_agent = if ctx.paths.profile.is_none() {
        ctx.paths.agent_dir.clone()
    } else {
        ctx.paths.default_agent_dir()
    };
    let default_sessions = default_agent.join("sessions");

    let all_profiles = ctx.paths.list_profiles();
    let mut skipped_profiles = Vec::new();
    let mut targets: Vec<(String, PathBuf)> = Vec::new();

    if skip_set.remove("default") {
        skipped_profiles.push("default".to_string());
    } else if default_sessions.is_dir() {
        targets.push(("default".to_string(), default_sessions.clone()));
    }

    for prof in &all_profiles {
        if skip_set.contains(prof) {
            skipped_profiles.push(prof.clone());
            continue;
        }
        let sdir = rematch_core::paths::RematchPaths::profile_agent_dir(&ctx.paths.config_root, prof).join("sessions");
        if sdir.is_dir() {
            targets.push((prof.clone(), sdir));
        }
    }
    for s in skip_set {
        if !skipped_profiles.contains(&s) {
            skipped_profiles.push(s);
        }
    }

    let active_sessions = ctx.paths.agent_dir.join("sessions");
    if targets.is_empty() && !active_sessions.is_dir() && all_profiles.is_empty() {
        bail!("sessions dir {} does not exist", active_sessions.display());
    }

    let mut total = HistoryStats::default();
    let mut per_profile = serde_json::Map::new();
    for (name, sdir) in &targets {
        let st = import_dir_stats(ctx, sdir)?;
        total.merge(&st);
        per_profile.insert(name.clone(), st.to_json(sdir));
    }

    let mut out = total.to_json(&active_sessions);
    out["profiles"] = Value::Object(per_profile);
    out["skipped_profiles"] = json!(skipped_profiles);
    Ok(out)
}

/// Whether any omp sessions dir exists (active, default, or a profile's).
fn omp_history_present(ctx: &AppCtx) -> bool {
    ctx.paths.agent_dir.join("sessions").is_dir()
        || ctx.paths.default_agent_dir().join("sessions").is_dir()
        || ctx
            .paths
            .list_profiles()
            .iter()
            .any(|p| rematch_core::paths::RematchPaths::profile_agent_dir(&ctx.paths.config_root, p).join("sessions").is_dir())
}

/// Whether two paths name the same directory (by real path when both exist).
fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// `import --history` of a Claude Code, Codex or Pi sessions dir, or of the OpenCode store:
/// each transcript's trace is synthesized and folded into its ledger row. Outcomes are derived
/// from omp tool receipts only, so none are extracted here. A clean main session that loaded
/// rematch entries (Codex and Pi: skills listed to the model, DESIGN §18.2.2, §18.3) gets its
/// synthesized trace written to `traces/<sid>.jsonl`, so `count` folds it into those entries'
/// counters; flagged ones reach the counters through `reflect --history`, which skips sessions
/// that already have a trace.
fn import_harness_history(ctx: &AppCtx, h: Transcripts, dir: &Path) -> Result<Value> {
    let path_label = |p: &PathBuf| p.display().to_string();
    let st = match h {
        Transcripts::ClaudeCode => import_harness_stats(
            ctx,
            import_claude::scan_sessions_dir(dir)?,
            path_label,
            |p: &PathBuf| import_claude::read_session(p),
            import_claude::session_record,
            import_claude::ClaudeSession::is_subagent,
            import_claude::trace_records_of,
        )?,
        Transcripts::Codex => import_harness_stats(
            ctx,
            import_codex::scan_sessions_dir(dir)?,
            path_label,
            |p: &PathBuf| import_codex::read_session(p),
            import_codex::session_record,
            import_codex::CodexSession::is_subagent,
            import_codex::trace_records_of,
        )?,
        Transcripts::Pi => import_harness_stats(
            ctx,
            import_pi::scan_sessions_dir(dir)?,
            path_label,
            |p: &PathBuf| import_pi::read_session(p),
            import_pi::session_record,
            import_pi::PiSession::is_subagent,
            import_pi::trace_records_of,
        )?,
        Transcripts::OpenCode => import_harness_stats(
            ctx,
            import_opencode::scan_store(dir)?,
            |r: &import_opencode::SessionRef| r.to_string(),
            import_opencode::read_session,
            import_opencode::session_record,
            import_opencode::OpenCodeSession::is_subagent,
            import_opencode::trace_records_of,
        )?,
    };
    eprintln!("rematch import: {} {} sessions from {}", st.files, h.name(), dir.display());
    Ok(st.to_json(dir))
}

fn import_harness_stats<U, S>(
    ctx: &AppCtx,
    units: Vec<U>,
    label: impl Fn(&U) -> String,
    read: impl Fn(&U) -> Result<S>,
    record: impl Fn(&S) -> SessionRecord,
    is_subagent: impl Fn(&S) -> bool,
    trace: impl Fn(&S) -> Vec<TraceRecord>,
) -> Result<HistoryStats> {
    let mut st = HistoryStats { files: units.len(), ..Default::default() };
    let policy = ctx.domain_policy()?;
    for f in &units {
        let session = match read(f) {
            Ok(s) => s,
            Err(e) => {
                st.failed += 1;
                if st.errors.len() < 10 {
                    st.errors.push(format!("{}: {e:#}", label(f)));
                }
                continue;
            }
        };
        let mut rec = record(&session);
        rec.domain = rematch_core::domain::session_domain(&rec, &policy);
        if rec.id.is_empty() {
            st.failed += 1;
            continue;
        }
        let subagent = is_subagent(&session);
        if subagent {
            st.subagents += 1;
        }
        let existing = ctx.ledger.get_session(&rec.id)?;
        let counted = existing.as_ref().is_some_and(|s| s.counted);
        match existing {
            // The adapter's own trace of the session is richer (loaded entries): keep it.
            Some(existing) if existing.source == "trace" => {
                st.skipped += 1;
                continue;
            }
            Some(_) => st.updated += 1,
            None => st.new += 1,
        }
        if !rec.skill_reads.is_empty() {
            st.skill_sessions += 1;
            for s in &rec.skill_reads {
                *st.skills.entry(s.clone()).or_default() += 1;
            }
        }
        st.redirects += rec.redirects.len() as u32;
        ctx.ledger.upsert_session(&rec)?;
        let trace_path = ctx.paths.trace_file(&rec.id);
        if !subagent && !rec.flagged && !counted && !rec.loaded_entries.is_empty() && !trace_path.exists() {
            let mut body = String::new();
            for r in &trace(&session) {
                body.push_str(&serde_json::to_string(r)?);
                body.push('\n');
            }
            std::fs::create_dir_all(&ctx.paths.traces)?;
            let tmp = trace_path.with_extension("jsonl.tmp");
            std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
            std::fs::rename(&tmp, &trace_path).with_context(|| format!("rename to {}", trace_path.display()))?;
            st.traces += 1;
        }
    }
    Ok(st)
}
/// The omp session file of a ledger session: the path recorded at import, else
/// `<agent dir>/sessions/<slug of its cwd>/*_<id>.jsonl` (across the active, default, and
/// profile agent dirs), else any `*_<id>.jsonl` under those sessions dirs. Subagent files
/// (`<Name>.jsonl`) are found only through the recorded path.
pub fn locate_session_file(ctx: &AppCtx, s: &SessionRecord) -> Result<Option<PathBuf>> {
    if let Some(p) = s.source_path.as_deref().map(PathBuf::from).filter(|p| p.is_file()) {
        return Ok(Some(p));
    }
    let is_it = |p: &Path| {
        p.extension().is_some_and(|e| e == "jsonl") && session_id_from_filename(p).as_deref() == Some(s.id.as_str())
    };
    let mut session_dirs = vec![ctx.paths.agent_dir.join("sessions")];
    let default_sessions = ctx.paths.default_agent_dir().join("sessions");
    if !session_dirs.contains(&default_sessions) {
        session_dirs.push(default_sessions);
    }
    for prof in ctx.paths.list_profiles() {
        let pdir = rematch_core::paths::RematchPaths::profile_agent_dir(&ctx.paths.config_root, &prof).join("sessions");
        if !session_dirs.contains(&pdir) {
            session_dirs.push(pdir);
        }
    }
    let slugs = cwd_slugs(&s.cwd);
    for dir in &session_dirs {
        if !dir.is_dir() {
            continue;
        }
        for slug in &slugs {
            for ent in std::fs::read_dir(dir.join(slug)).into_iter().flatten().flatten() {
                let p = ent.path();
                if is_it(&p) {
                    return Ok(Some(p));
                }
            }
        }
    }
    for dir in &session_dirs {
        if !dir.is_dir() {
            continue;
        }
        if let Some(found) = scan_sessions_dir(dir)?.into_iter().map(|f| f.path).find(|p| is_it(p)) {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// omp's session directory names for a cwd: `-<path under $HOME, / → ->` inside the home
/// directory, `--<absolute path, / → ->--` outside it.
fn cwd_slugs(cwd: &str) -> Vec<String> {
    let cwd = Path::new(cwd);
    let mut out = Vec::new();
    if let Some(home) = rematch_core::paths::home_dir() {
        if let Ok(rel) = cwd.strip_prefix(&home) {
            out.push(format!("-{}", rel.to_string_lossy().replace('/', "-")));
        }
    }
    out.push(format!("--{}--", cwd.to_string_lossy().trim_start_matches('/').replace('/', "-")));
    out
}

/// Harnesses whose sessions carry an id prefix (`cc:`, `codex:`, `pi:`, `oc:`); unprefixed ids
/// are omp's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transcripts {
    ClaudeCode,
    Codex,
    Pi,
    OpenCode,
}

impl Transcripts {
    pub fn of_id(id: &str) -> Option<Transcripts> {
        if id.starts_with(import_claude::ID_PREFIX) {
            Some(Transcripts::ClaudeCode)
        } else if id.starts_with(import_codex::ID_PREFIX) {
            Some(Transcripts::Codex)
        } else if id.starts_with(import_pi::ID_PREFIX) {
            Some(Transcripts::Pi)
        } else if id.starts_with(import_opencode::ID_PREFIX) {
            Some(Transcripts::OpenCode)
        } else {
            None
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Transcripts::ClaudeCode => "claude-code",
            Transcripts::Codex => "codex",
            Transcripts::Pi => "pi",
            Transcripts::OpenCode => "opencode",
        }
    }

    fn history_harness(self) -> HistoryHarness {
        match self {
            Transcripts::ClaudeCode => HistoryHarness::ClaudeCode,
            Transcripts::Codex => HistoryHarness::Codex,
            Transcripts::Pi => HistoryHarness::Pi,
            Transcripts::OpenCode => HistoryHarness::OpenCode,
        }
    }

    /// The harness's default history location (OpenCode: its store file).
    fn default_dir(self, ctx: &AppCtx) -> Option<PathBuf> {
        match self {
            Transcripts::ClaudeCode => import_claude::default_sessions_dir(),
            Transcripts::Codex => import_codex::default_sessions_dir(),
            Transcripts::Pi => import_pi::default_sessions_dir(&ctx.paths.config_root),
            Transcripts::OpenCode => import_opencode::default_store(),
        }
    }

    /// The transcript file of a ledger session: the path recorded at import, else the
    /// harness's lookup under its default sessions dir (not OpenCode: [`import_opencode::locate`]).
    fn locate(self, ctx: &AppCtx, s: &SessionRecord) -> Option<PathBuf> {
        if let Some(p) = s.source_path.as_deref().map(PathBuf::from).filter(|p| p.is_file()) {
            return Some(p);
        }
        let dir = self.default_dir(ctx).filter(|d| d.is_dir())?;
        match self {
            Transcripts::ClaudeCode => import_claude::find_session_file(&dir, &s.id),
            Transcripts::Codex => import_codex::find_session_file(&dir, &s.id),
            Transcripts::Pi => import_pi::find_session_file(&dir, &s.id),
            Transcripts::OpenCode => None,
        }
    }
}

/// A ledger session's harness transcript, read back: the trace synthesized from it and
/// whether it is a subagent's (the adapters trace main sessions only). `path` is the session
/// file (OpenCode: its store).
pub struct Transcript {
    pub path: PathBuf,
    pub records: Vec<TraceRecord>,
    pub subagent: bool,
}

/// The transcript of a ledger session from the harness its id names (omp, Claude Code,
/// Codex, Pi, OpenCode). `Ok(None)` when none is found; `Err` when it is unreadable. A Pi
/// segment (`pi:<id>:r<n>`) without its live trace reads the whole Pi session.
pub fn read_transcript(ctx: &AppCtx, s: &SessionRecord) -> Result<Option<Transcript>> {
    let Some(h) = Transcripts::of_id(&s.id) else {
        let Some(path) = locate_session_file(ctx, s)? else { return Ok(None) };
        let omp = read_session(&path)?;
        return Ok(Some(Transcript { subagent: omp.header.parent_session.is_some(), records: trace_records_of(&omp), path }));
    };
    if h == Transcripts::OpenCode {
        let Some(r) = import_opencode::locate(s.source_path.as_deref().map(Path::new), &s.id) else { return Ok(None) };
        let c = import_opencode::read_session(&r)?;
        return Ok(Some(Transcript { subagent: c.is_subagent(), records: import_opencode::trace_records_of(&c), path: r.db }));
    }
    let Some(path) = h.locate(ctx, s) else { return Ok(None) };
    Ok(Some(match h {
        Transcripts::ClaudeCode => {
            let c = import_claude::read_session(&path)?;
            Transcript { subagent: c.is_subagent(), records: import_claude::trace_records_of(&c), path }
        }
        Transcripts::Codex => {
            let c = import_codex::read_session(&path)?;
            Transcript { subagent: c.is_subagent(), records: import_codex::trace_records_of(&c), path }
        }
        Transcripts::Pi => {
            let p = import_pi::read_session(&path)?;
            Transcript { subagent: false, records: import_pi::trace_records_of(&p), path }
        }
        Transcripts::OpenCode => unreachable!("handled above"),
    }))
}

/// Trace records of a ledger session: its live trace when present, else the trace
/// synthesized from its harness transcript ([`read_transcript`]). `None` when neither exists.
pub fn records_for_session(ctx: &AppCtx, s: &SessionRecord) -> Result<Option<Vec<TraceRecord>>> {
    let tp = ctx.paths.trace_file(&s.id);
    if tp.is_file() {
        return Ok(Some(trace::read_trace(&tp)?));
    }
    Ok(read_transcript(ctx, s)?.map(|t| t.records))
}

/// Trace records of a ledger session for a reader (ADR 001 L4): the lake's
/// `events/<harness>/<day>/<session>.parquet` by path when there is one, else
/// [`records_for_session`]. With the source used.
pub fn session_records(ctx: &AppCtx, s: &SessionRecord) -> Result<Option<(Vec<TraceRecord>, Source)>> {
    if let Some(records) = crate::lake_read::lake_events(ctx, s) {
        return Ok(Some((records, Source::Lake)));
    }
    Ok(records_for_session(ctx, s)?.map(|r| (r, Source::Ledger)))
}

/// [`session_records`] plus the session row whose redirect and error-cluster seqs address
/// those records: an imported row's signals are re-folded from the records (rows imported
/// before seqs addressed the synthesized trace), keeping matching classifier labels.
pub fn session_with_records(ctx: &AppCtx, s: &SessionRecord) -> Result<Option<(SessionRecord, Vec<TraceRecord>, Source)>> {
    let Some((records, src)) = session_records(ctx, s)? else {
        return Ok(None);
    };
    let mut row = s.clone();
    if s.source == "import" {
        apply_trace_signals(&mut row, &trace::fold_trace(&records, &s.id));
    }
    Ok(Some((row, records, src)))
}

/// Session stem corpus for tf-idf ranking.
pub fn stem_corpus(ctx: &AppCtx) -> Result<Vec<Vec<String>>> {
    Ok(ctx.ledger.list_sessions(&SessionFilter::default())?.into_iter().map(|s| s.stems).collect())
}

/// Trigger text of a skill: description plus every `Use when …` line of the body.
pub fn trigger_text(doc: &SkillDoc) -> (String, String) {
    let desc = doc.get_str("description").unwrap_or("").trim().to_string();
    let use_when: Vec<&str> = doc
        .body
        .lines()
        .map(str::trim)
        .filter(|l| l.to_ascii_lowercase().trim_start_matches(['*', '-', ' ', '_']).starts_with("use when"))
        .collect();
    let trigger_line = use_when.first().map(|s| s.to_string()).unwrap_or_else(|| desc.clone());
    (format!("{desc}\n{}", use_when.join("\n")), trigger_line)
}

/// Ranked predicate stems of a text against the ledger corpus (≤ 10).
pub fn ranked_text_stems(text: &str, corpus: &[Vec<String>]) -> Vec<String> {
    let mut r = rank_stems(&stems_of_text(text), corpus);
    r.truncate(MAX_PREDICATE_STEMS);
    r
}

fn existing_import(ctx: &AppCtx, name: &str, source: &str) -> Result<Option<Entry>> {
    Ok(ctx
        .ledger
        .list_entries(&EntryFilter { name: Some(name.into()), ..Default::default() })?
        .into_iter()
        .find(|e| matches!(&e.origin, Origin::Import { source: s } if s == source)))
}

/// Every regular file under `dir` as `Edit::Write`s with paths relative to `dir`. Non-UTF-8
/// files are skipped (skills are text); symlinks are not followed.
fn read_tree(dir: &Path) -> Result<Vec<Edit>> {
    fn walk(root: &Path, cur: &Path, out: &mut Vec<Edit>) -> Result<()> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(cur)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for p in entries {
            let meta = std::fs::symlink_metadata(&p)?;
            if meta.is_dir() {
                walk(root, &p, out)?;
            } else if meta.is_file() {
                let rel = p.strip_prefix(root)?.to_string_lossy().to_string();
                if rel == REMATCH_JSON {
                    continue;
                }
                match String::from_utf8(std::fs::read(&p)?) {
                    Ok(content) => out.push(Edit::Write { path: rel, content }),
                    Err(_) => eprintln!("rematch import: skipping non-text file {}", p.display()),
                }
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    Ok(out)
}

pub fn import_forge(ctx: &AppCtx, forge: &Path, migrate: bool) -> Result<Value> {
    let forge = crate::app::absolute(forge)?;
    let skills_dir = forge.join("skills");
    if !skills_dir.is_dir() {
        bail!("{} has no skills/ directory", forge.display());
    }
    let corpus = stem_corpus(ctx)?;
    let family = Family("unknown/unknown".into());
    let mut registered = Vec::new();
    let mut existing = Vec::new();
    let mut skipped = Vec::new();
    let mut dirs: Vec<PathBuf> =
        std::fs::read_dir(&skills_dir)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.join("SKILL.md").is_file()).collect();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        if validate_name(&name).is_err() {
            skipped.push(json!({"name": name, "reason": "invalid name"}));
            continue;
        }
        if migrate && name == "compound-forge" {
            skipped.push(json!({"name": name, "reason": "meta-skill for the forge tool itself; not migrated"}));
            continue;
        }
        if let Some(e) = existing_import(ctx, &name, "forge")? {
            let in_place = !e.path.starts_with(&ctx.paths.staging);
            if !(migrate && in_place) {
                existing.push(json!({"id": e.id, "name": name}));
                continue;
            }
            // Registered in place earlier; move the registration into our own staging.
            let files = read_tree(&dir)?;
            let mut e = e;
            e.status = Status::Provisional;
            let staged = library::stage(&ctx.ledger, &ctx.paths, &e, &files)?;
            registered.push(json!({"id": staged.id, "name": name, "stems": staged.predicate.stems, "path": staged.path, "migrated_from": dir, "files": files.len()}));
            continue;
        }
        let md = std::fs::read_to_string(dir.join("SKILL.md"))?;
        let doc = SkillDoc::parse(&md).unwrap_or(SkillDoc { body: md.clone(), ..Default::default() });
        let (text, trigger_line) = trigger_text(&doc);
        let stems = ranked_text_stems(&format!("{name} {text}"), &corpus);
        let pred = predicate::derive(None, &[], &stems, None);
        let mut e = new_entry(
            NewEntry {
                kind: Kind::Skill,
                name: name.clone(),
                branch: FORGE_BRANCH.into(),
                status: Status::Provisional,
                family: family.clone(),
                origin: Origin::Import { source: "forge".into() },
                predicate: pred,
                trigger_line,
                path: dir.clone(),
            },
            ctx.now,
        )?;
        e.status_since = Some(ctx.now);
        if migrate {
            // Copy the whole skill tree into staging/<forge>/<name>/ (SPEC: stage beside, never
            // over; scripts land mode 0644 until approved). The checkout is never written to.
            let files = read_tree(&dir)?;
            let staged = library::stage(&ctx.ledger, &ctx.paths, &e, &files)?;
            registered.push(json!({"id": staged.id, "name": name, "stems": staged.predicate.stems, "path": staged.path, "migrated_from": dir, "files": files.len()}));
        } else {
            // The skill already loads natively through the forge plugin: register only,
            // never copy or write into the forge checkout.
            e.content_sha256 = Some(library::content_sha256(&dir)?);
            ctx.ledger.upsert_entry(&e)?;
            registered.push(json!({"id": e.id, "name": name, "stems": e.predicate.stems, "path": dir}));
        }
    }

    let mut lessons = Vec::new();
    let mut lessons_existing = Vec::new();
    let learn = forge.join("learnings");
    if learn.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&learn)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "md") && p.file_name().is_some_and(|n| n != "README.md"))
            .collect();
        files.sort();
        for f in files {
            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let name = format!("lesson-{}", slug(stem));
            if let Some(e) = existing_import(ctx, &name, "forge")? {
                lessons_existing.push(json!({"id": e.id, "name": name}));
                continue;
            }
            let bytes = std::fs::read(&f).with_context(|| format!("read {}", f.display()))?;
            let sha = ctx.ledger.put_blob(&bytes)?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let doc = SkillDoc::parse(&text).unwrap_or(SkillDoc { body: text.clone(), ..Default::default() });
            let title = doc
                .get_str("title")
                .map(str::to_string)
                .or_else(|| doc.body.lines().find(|l| l.starts_with("# ")).map(|l| l.trim_start_matches("# ").trim().to_string()))
                .unwrap_or_else(|| stem.replace('-', " "));
            let stems = ranked_text_stems(&format!("{title}\n{}", doc.body), &corpus);
            let dir = ctx.paths.root.join("lessons").join(FORGE_BRANCH).join(&name);
            std::fs::create_dir_all(&dir)?;
            let mut e = new_entry(
                NewEntry {
                    kind: Kind::Lesson,
                    name: name.clone(),
                    branch: FORGE_BRANCH.into(),
                    status: Status::Provisional,
                    family: family.clone(),
                    origin: Origin::Import { source: "forge".into() },
                    predicate: predicate::derive(None, &[], &stems, None),
                    trigger_line: title.clone(),
                    path: dir.clone(),
                },
                ctx.now,
            )?;
            e.status_since = Some(ctx.now);
            std::fs::write(dir.join("LESSON.md"), &bytes)?;
            e.content_sha256 = Some(library::content_sha256(&dir)?);
            let mut ev = serde_json::to_value(&e)?;
            ev["evidence_sha256"] = json!(sha);
            ev["source_path"] = json!(f);
            std::fs::write(dir.join(REMATCH_JSON), serde_json::to_string_pretty(&ev)?)?;
            ctx.ledger.upsert_entry(&e)?;
            lessons.push(json!({"id": e.id, "name": name, "evidence_sha256": sha, "source": f}));
        }
    }
    Ok(json!({
        "forge": forge,
        "skills_registered": registered.len(),
        "skills_existing": existing.len(),
        "lessons_registered": lessons.len(),
        "lessons_existing": lessons_existing.len(),
        "skills": registered,
        "lessons": lessons,
        "skipped": skipped,
        "eval_suites": "ignored (kept as files for reference)",
    }))
}
