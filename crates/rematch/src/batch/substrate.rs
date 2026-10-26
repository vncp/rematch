//! Rollout substrate (SPEC "Isolation for batch mode", DESIGN §0/§11.2): each rollout
//! gets a fresh worktree of the task repo, a candidate skills directory and a `--config`
//! overlay that turns every omp skill provider off except that directory and disables
//! memory. The agent dir is not swapped (auth lives there).
//!
//! Workspace layout: `runs/<task>/<seed>/<variant>/{wt/, skills/<name>/…, overlay.yml,
//! events.jsonl}`.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rematch_core::features::head;
use rematch_core::ledger::{sha256_hex, EntryFilter};
use rematch_core::library::{list_for_match, validate_name, REMATCH_JSON};
use rematch_core::outcome::canonical_json;
use rematch_core::trace::{skill_from_read_path, AgentInfo, TraceRecord, Usage};
use rematch_core::types::{EntryId, ModelRef, Status};
use serde_json::Value;

use super::tasks::{is_git_repo, Task};
use crate::app::AppCtx;
use crate::omp;
use crate::proc;

/// omp skill providers (`omp config list | grep skills.enable`) switched off in rollouts.
pub const SKILL_PROVIDER_KEYS: &[&str] = &[
    "enableCodexUser",
    "enableClaudeUser",
    "enableClaudeProject",
    "enablePiUser",
    "enablePiProject",
    "enableAgentsUser",
    "enableAgentsProject",
];

/// Chars kept from tool output / assistant text in converted trace records (DESIGN §3).
const CONTENT_HEAD: usize = 1024;
const TEXT_HEAD: usize = 2048;
const INPUT_STRING_CHARS: usize = 2048;

/// One harness entry as files (paths relative to the entry dir).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    /// Ledger id for library entries; `None` for candidates.
    pub id: Option<EntryId>,
    pub files: BTreeMap<String, String>,
}

/// The current harness for a task repo: every kept/pinned entry plus the provisional
/// entries an interactive session in the repo would load (`library::list_for_match`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BaseLibrary {
    pub entries: Vec<TreeEntry>,
}

/// Harness under test: the base library with candidate entries on top (a candidate
/// replaces a base entry of the same name).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CandidateTree {
    pub entries: Vec<TreeEntry>,
    pub base: BaseLibrary,
}

impl BaseLibrary {
    pub fn load(ctx: &AppCtx, repo: &Path) -> Result<BaseLibrary> {
        let git_repo = is_git_repo(repo).then_some(repo);
        let m = list_for_match(&ctx.ledger, &ctx.paths, git_repo, repo)?;
        let mut entries = Vec::new();
        for me in &m.entries {
            let Some(e) = ctx.ledger.get_entry(&me.id)? else { continue };
            if !matches!(e.status, Status::Provisional | Status::Kept | Status::Pinned) || !e.path.is_dir() {
                continue;
            }
            entries.push(TreeEntry { name: e.name.clone(), id: Some(e.id.clone()), files: read_entry_files(&e.path)? });
        }
        Ok(BaseLibrary { entries })
    }

    /// The base without the given entries (retired by the next-run rule).
    pub fn without(&self, ids: &[EntryId]) -> BaseLibrary {
        BaseLibrary {
            entries: self
                .entries
                .iter()
                .filter(|e| e.id.as_ref().is_none_or(|id| !ids.contains(id)))
                .cloned()
                .collect(),
        }
    }

    pub fn ids(&self) -> Vec<EntryId> {
        self.entries.iter().filter_map(|e| e.id.clone()).collect()
    }
}

impl CandidateTree {
    pub fn reference(base: &BaseLibrary) -> CandidateTree {
        CandidateTree { entries: Vec::new(), base: base.clone() }
    }

    /// Entries as loaded: base entries not shadowed by a candidate, then candidates.
    pub fn effective(&self) -> Vec<&TreeEntry> {
        let shadowed = |n: &str| self.entries.iter().any(|c| c.name == n);
        self.base.entries.iter().filter(|b| !shadowed(&b.name)).chain(self.entries.iter()).collect()
    }
}

/// Files of an entry dir, minus `rematch.json` (bookkeeping, never loaded).
pub fn read_entry_files(dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d).with_context(|| format!("read {}", d.display()))? {
            let p = ent?.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let rel = p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            if rel == REMATCH_JSON {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                out.insert(rel, text);
            }
        }
    }
    Ok(out)
}

fn safe_rel(rel: &str) -> Result<PathBuf> {
    let p = PathBuf::from(rel);
    if rel.is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("entry file path must be relative without traversal: {rel:?}");
    }
    Ok(p)
}

/// Write an entry's files under `dir` (created).
pub fn write_entry_files(dir: &Path, files: &BTreeMap<String, String>) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    for (rel, content) in files {
        let p = dir.join(safe_rel(rel)?);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, content).with_context(|| format!("write {}", p.display()))?;
    }
    Ok(())
}

/// A materialized rollout workspace.
#[derive(Debug, Clone)]
pub struct Install {
    /// `runs/<task>/<seed>/<variant>/`
    pub dir: PathBuf,
    pub worktree: PathBuf,
    pub skills_dir: PathBuf,
    pub overlay: PathBuf,
    pub events: PathBuf,
    /// The task repo the worktree came from.
    pub repo: PathBuf,
    /// `git worktree add` (else a plain copy).
    pub git: bool,
    /// Library entry ids loaded by this harness.
    pub loaded: Vec<EntryId>,
    /// Entry names in the skills dir.
    pub skills: Vec<String>,
}

pub trait Substrate {
    fn materialize(&self, tree: &CandidateTree, dest: &Path) -> Result<Install>;
    fn rollout_cmd(&self, install: &Install, task: &Task, seed: u64) -> Result<omp::ToolsCall>;
    fn read_trajectory(&self, path: &Path) -> Result<Vec<TraceRecord>>;
    /// Remove the worktree registration and the workspace dir.
    fn cleanup(&self, install: &Install) -> Result<()>;
}

pub struct OmpSubstrate {
    pub repo: PathBuf,
    pub solver: ModelRef,
}

/// The overlay YAML: all skill providers off, `customDirectories = [skills_dir]`,
/// memory off (`memory.backend`, `memories.enabled`). Strings are double-quoted (JSON
/// escapes are valid YAML) so `off` stays a string under YAML 1.1 readers too.
pub fn overlay_yaml(skills_dir: &Path) -> Result<String> {
    let mut y = String::from("skills:\n  enabled: true\n");
    for k in SKILL_PROVIDER_KEYS {
        y.push_str(&format!("  {k}: false\n"));
    }
    y.push_str("  customDirectories:\n");
    y.push_str(&format!("    - {}\n", serde_json::to_string(&skills_dir.to_string_lossy())?));
    y.push_str("memory:\n  backend: \"off\"\nmemories:\n  enabled: false\n");
    Ok(y)
}

fn git(repo: &Path, args: &[&std::ffi::OsStr]) -> Result<proc::Bounded> {
    let mut c = Command::new("git");
    c.arg("-C").arg(repo).args(args);
    let b = proc::run_bounded(c, Duration::from_secs(120))?;
    if b.code != Some(0) {
        bail!("git {:?} failed: {}", args, b.stderr.trim());
    }
    Ok(b)
}

impl Substrate for OmpSubstrate {
    fn materialize(&self, tree: &CandidateTree, dest: &Path) -> Result<Install> {
        if dest.exists() {
            std::fs::remove_dir_all(dest).with_context(|| format!("clear {}", dest.display()))?;
        }
        std::fs::create_dir_all(dest)?;
        let worktree = dest.join("wt");
        let git = is_git_repo(&self.repo);
        if git {
            git_worktree_add(&self.repo, &worktree)?;
        } else {
            let mut c = Command::new("cp");
            c.arg("-R").arg(&self.repo).arg(&worktree);
            let b = proc::run_bounded(c, Duration::from_secs(300))?;
            if b.code != Some(0) {
                bail!("copy {} failed: {}", self.repo.display(), b.stderr.trim());
            }
        }
        let skills_dir = dest.join("skills");
        std::fs::create_dir_all(&skills_dir)?;
        let mut loaded = Vec::new();
        let mut skills = Vec::new();
        for e in tree.effective() {
            validate_name(&e.name)?;
            write_entry_files(&skills_dir.join(&e.name), &e.files)?;
            skills.push(e.name.clone());
            if let Some(id) = &e.id {
                loaded.push(id.clone());
            }
        }
        let overlay = dest.join("overlay.yml");
        std::fs::write(&overlay, overlay_yaml(&skills_dir)?)?;
        Ok(Install {
            dir: dest.to_path_buf(),
            worktree,
            skills_dir,
            overlay,
            events: dest.join("events.jsonl"),
            repo: self.repo.clone(),
            git,
            loaded,
            skills,
        })
    }

    fn rollout_cmd(&self, install: &Install, task: &Task, _seed: u64) -> Result<omp::ToolsCall> {
        Ok(omp::ToolsCall {
            model: self.solver.clone(),
            cwd: install.worktree.clone(),
            prompt: task.prompt.clone(),
            tools: task.tools.clone(),
            max_time: task.max_time(),
            config_overlay: Some(install.overlay.clone()),
            extra_args: Vec::new(),
        })
    }

    fn read_trajectory(&self, path: &Path) -> Result<Vec<TraceRecord>> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let events: Vec<Value> =
            text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect();
        Ok(events_to_trace(&events, &self.repo))
    }

    fn cleanup(&self, install: &Install) -> Result<()> {
        if install.git && install.worktree.exists() {
            let _ = git(&install.repo, &["worktree".as_ref(), "remove".as_ref(), "--force".as_ref(), install.worktree.as_os_str()]);
        }
        if install.dir.exists() {
            std::fs::remove_dir_all(&install.dir).with_context(|| format!("remove {}", install.dir.display()))?;
        }
        if install.git {
            let _ = git(&install.repo, &["worktree".as_ref(), "prune".as_ref()]);
        }
        Ok(())
    }
}

fn git_worktree_add(repo: &Path, wt: &Path) -> Result<()> {
    git(repo, &["worktree".as_ref(), "add".as_ref(), "--detach".as_ref(), wt.as_os_str(), "HEAD".as_ref()])
        .map(|_| ())
        .with_context(|| format!("git worktree add in {}", repo.display()))
}

/// Text of an omp message `content` (string or blocks).
fn content_text(content: Option<&Value>) -> String {
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

fn truncate_strings(v: &Value) -> Value {
    match v {
        Value::String(s) => {
            let n = s.chars().count();
            if n > INPUT_STRING_CHARS {
                Value::String(format!("{}…[+{}]", head(s, INPUT_STRING_CHARS), n - INPUT_STRING_CHARS))
            } else {
                v.clone()
            }
        }
        Value::Array(a) => Value::Array(a.iter().map(truncate_strings).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), truncate_strings(v))).collect()),
        other => other.clone(),
    }
}

fn iso_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

/// omp `--mode json` events → rematch trace records (DESIGN §3) so `fold_trace`,
/// `excerpt_around` and the editor see a rollout like an interactive session:
/// `session` → `session_start`; `message_end` user → `input`, assistant → `turn_end`,
/// toolResult → `tool_result` (input recovered from the assistant's toolCall block);
/// `agent_end` → `agent_end`. Every occurrence of the rollout's cwd (the throwaway
/// worktree) is rewritten to the task repo.
pub fn events_to_trace(events: &[Value], repo: &Path) -> Vec<TraceRecord> {
    let repo_s = repo.to_string_lossy().to_string();
    let wt = events
        .iter()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("session"))
        .and_then(|e| e.get("cwd").and_then(Value::as_str))
        .map(str::to_string);
    let rewrite = |v: &Value| -> Value {
        match &wt {
            Some(w) if !w.is_empty() => {
                let s = serde_json::to_string(v).unwrap_or_default();
                let w_json = serde_json::to_string(w).unwrap_or_default();
                let r_json = serde_json::to_string(&repo_s).unwrap_or_default();
                // Replace inside JSON string bodies (quotes stripped from both sides).
                let s = s.replace(&w_json[1..w_json.len() - 1], &r_json[1..r_json.len() - 1]);
                serde_json::from_str(&s).unwrap_or_else(|_| v.clone())
            }
            _ => v.clone(),
        }
    };
    let mut out = Vec::new();
    let mut seq: u32 = 0;
    let mut t: i64 = 0;
    let mut call_args: BTreeMap<String, Value> = BTreeMap::new();
    let mut turns = 0u32;
    let next = |seq: &mut u32| {
        let s = *seq;
        *seq += 1;
        s
    };
    for raw in events {
        let ev = rewrite(raw);
        let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let msg = ev.get("message");
        if let Some(ts) = msg.and_then(|m| m.get("timestamp")).and_then(Value::as_i64) {
            t = ts;
        }
        match ty {
            "session" => {
                if let Some(ts) = ev.get("timestamp").and_then(Value::as_str).and_then(iso_ms) {
                    t = ts;
                }
                let branch_hint = Path::new(&repo_s).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                out.push(TraceRecord::SessionStart {
                    seq: next(&mut seq),
                    t,
                    session_id: ev.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                    cwd: repo_s.clone(),
                    repo: is_git_repo(repo).then(|| repo_s.clone()),
                    branch_hint,
                    model: None,
                    mode: "batch".into(),
                    agent: Some(AgentInfo { kind: "batch".into(), name: "rematch".into() }),
                    omp_version: String::new(),
                    loaded_entries: Vec::new(),
                    hooks_loaded: Vec::new(),
                    staging_dirs: Vec::new(),
                    repo_remote: None,
                    subpath: None,
                    head_sha: None,
                    visible_entries: Vec::new(),
                });
            }
            "tool_execution_start" => {
                if let (Some(id), Some(args)) = (ev.get("toolCallId").and_then(Value::as_str), ev.get("args")) {
                    call_args.entry(id.to_string()).or_insert_with(|| args.clone());
                }
            }
            "message_end" => {
                let Some(m) = msg else { continue };
                match m.get("role").and_then(Value::as_str).unwrap_or("") {
                    "user" => out.push(TraceRecord::Input {
                        seq: next(&mut seq),
                        t,
                        text: head(&content_text(m.get("content")), 16 * 1024),
                        source: "batch".into(),
                        idle: true,
                        after_deliverable: None,
                        prelabel: "none".into(),
                        skills_invoked: Vec::new(),
                    }),
                    "assistant" => {
                        if let Some(blocks) = m.get("content").and_then(Value::as_array) {
                            for b in blocks.iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("toolCall")) {
                                if let (Some(id), Some(args)) = (b.get("id").and_then(Value::as_str), b.get("arguments")) {
                                    call_args.insert(id.to_string(), args.clone());
                                }
                            }
                        }
                        turns += 1;
                        let model = match (m.get("provider").and_then(Value::as_str), m.get("model").and_then(Value::as_str)) {
                            (Some(p), Some(id)) => Some(ModelRef { provider: p.into(), id: id.into() }),
                            _ => None,
                        };
                        out.push(TraceRecord::TurnEnd {
                            seq: next(&mut seq),
                            t,
                            model,
                            usage: m.get("usage").and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok()),
                            stop_reason: m.get("stopReason").and_then(Value::as_str).map(str::to_string),
                            text_head: head(&content_text(m.get("content")), TEXT_HEAD),
                        });
                    }
                    "toolResult" => {
                        let call_id = m.get("toolCallId").and_then(Value::as_str).unwrap_or_default().to_string();
                        let tool = m.get("toolName").and_then(Value::as_str).unwrap_or_default().to_string();
                        let full = call_args.get(&call_id).cloned().unwrap_or(Value::Null);
                        let text = content_text(m.get("content"));
                        let is_error = m.get("isError").and_then(Value::as_bool).unwrap_or(false);
                        let path = ["path", "file_path"]
                            .iter()
                            .find_map(|k| full.get(*k).and_then(Value::as_str))
                            .map(str::to_string);
                        let skill_read =
                            if tool == "read" { path.as_deref().and_then(skill_from_read_path) } else { None };
                        out.push(TraceRecord::ToolResult {
                            seq: next(&mut seq),
                            t,
                            tool,
                            call_id,
                            input: truncate_strings(&full),
                            args_hash: sha256_hex(canonical_json(&full).as_bytes()),
                            result_bytes: text.len() as u64,
                            is_error,
                            error_class: is_error.then(|| "error".to_string()),
                            content_head: head(&text, CONTENT_HEAD),
                            skill_read,
                            path,
                        });
                    }
                    _ => {}
                }
            }
            "agent_end" => out.push(TraceRecord::AgentEnd { seq: next(&mut seq), t, turns, is_terminal: true, yielded: false }),
            _ => {}
        }
    }
    out
}

/// Ids of provisional entries waiting on this task's next run.
pub fn pending_for_task(ctx: &AppCtx, task_id: &str) -> Result<Vec<rematch_core::Entry>> {
    Ok(ctx
        .ledger
        .list_entries(&EntryFilter { status: Some(Status::Provisional), ..Default::default() })?
        .into_iter()
        .filter(|e| e.pending_next_run)
        .filter(|e| matches!(&e.origin, rematch_core::Origin::Task { task_id: t } if t == task_id))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_overlay_disables_providers_and_memory() {
        let y = overlay_yaml(Path::new("/tmp/x/skills")).unwrap();
        let v: serde_yaml::Value = serde_yaml::from_str(&y).unwrap();
        for k in SKILL_PROVIDER_KEYS {
            assert_eq!(v["skills"][*k].as_bool(), Some(false), "{k}\n{y}");
        }
        assert_eq!(v["skills"]["enabled"].as_bool(), Some(true));
        assert_eq!(v["skills"]["customDirectories"][0].as_str(), Some("/tmp/x/skills"));
        assert_eq!(v["memory"]["backend"].as_str(), Some("off"));
        assert!(y.contains("'off'") || y.contains("\"off\""), "off must be quoted for YAML 1.1 readers:\n{y}");
        assert_eq!(v["memories"]["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn batch_events_convert_to_foldable_trace() {
        let text = include_str!("../../tests/fixtures/omp-tools-stream.jsonl");
        let events: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let repo = Path::new("/w/demo-repo");
        let recs = events_to_trace(&events, repo);
        let evs: Vec<&str> = recs.iter().map(TraceRecord::ev).collect();
        assert_eq!(evs, ["session_start", "input", "turn_end", "tool_result", "tool_result", "turn_end", "agent_end"]);
        let s = rematch_core::trace::fold_trace(&recs, "sid");
        assert_eq!(s.tool_calls, 2);
        assert_eq!(s.tool_errors, 1);
        assert_eq!(s.turns, 2);
        assert_eq!(s.cwd, "/w/demo-repo");
        assert_eq!(s.model.as_ref().unwrap().id, "gemini-4-argon");
        assert!(s.files_touched.iter().any(|f| f.ends_with("README.md")), "{:?}", s.files_touched);
        let TraceRecord::ToolResult { input, is_error, content_head, .. } = &recs[4] else { panic!() };
        assert_eq!(input["path"], "missing.txt");
        assert!(*is_error);
        assert!(content_head.contains("not found"));
        // the worktree path never survives conversion
        let all = serde_json::to_string(&recs).unwrap();
        assert!(!all.contains("/tmp/rematch-fixture-wt"), "{all}");
    }

    #[test]
    fn batch_tree_candidates_shadow_base() {
        let f = |n: &str, id: Option<&str>| TreeEntry {
            name: n.into(),
            id: id.map(str::to_string),
            files: BTreeMap::from([("SKILL.md".to_string(), n.to_string())]),
        };
        let base = BaseLibrary { entries: vec![f("a", Some("1")), f("b", Some("2"))] };
        let t = CandidateTree { entries: vec![f("b", None), f("c", None)], base: base.clone() };
        let names: Vec<_> = t.effective().iter().map(|e| (e.name.clone(), e.id.clone())).collect();
        assert_eq!(names, vec![("a".into(), Some("1".into())), ("b".into(), None), ("c".into(), None)]);
        assert_eq!(base.without(&["1".into()]).ids(), vec!["2".to_string()]);
    }
}
