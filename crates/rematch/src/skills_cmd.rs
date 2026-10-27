//! `skills sync --cwd <dir> [--harness omp|claude-code|codex|pi|opencode]` (DESIGN §16 "Shared
//! skills"): `REMATCH_ROOT/skills/` is the one skill directory every harness loads — omp through
//! `skills.customDirectories`, Claude Code through the plugin's `skills/` symlink, Codex
//! through `~/.agents/skills/rematch`, Pi through the Pi package's `resources_discover`, OpenCode
//! through its plugin. The link rules live in
//! [`rematch_core::library::sync_shared_skills`] (kept sources include every
//! `profile/skills/<name>/` of the profile, DESIGN §20); this module adds the cwd, the
//! per-harness reach checks, the migration from the per-harness directories of older versions,
//! and the Claude Code plugin's `agents/` copies ([`sync_plugin_agents`], DESIGN §18.1).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use rematch_core::import::repo_of;
use rematch_core::ledger::{sha256_hex, EntryFilter};
use rematch_core::library::{self, SkillsSync, AGENT_MD};
use rematch_core::package::SkillDoc;
use rematch_core::types::{EntryId, Kind, Status};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app::AppCtx;

#[derive(Subcommand, Debug, Clone)]
pub enum SkillsCmd {
    /// Link the skills that load for --cwd into REMATCH_ROOT/skills (adds matching provisional
    /// entries, keeps every kept/pinned one, prunes links of entries no longer loadable).
    Sync(SyncArgs),
}

#[derive(Args, Debug, Clone, Default)]
pub struct SyncArgs {
    /// The harness asking. `claude-code` also links the plugin's `skills/` and copies the
    /// agent entries into its `agents/` (plugin: $CLAUDE_PLUGIN_ROOT, else the
    /// `~/.claude/skills/rematch` install); `codex` reports whether Codex reaches the shared dir.
    #[arg(long, value_enum)]
    pub harness: Option<SyncHarness>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncHarness {
    Omp,
    ClaudeCode,
    Codex,
    /// Pi: the rematch Pi package returns the shared dir from `resources_discover`.
    Pi,
    /// OpenCode: the shared sync only; the rematch OpenCode plugin delivers the dir.
    #[value(name = "opencode")]
    OpenCode,
}

/// Per-harness link directories of older versions, removed after the first shared sync.
const LEGACY_LINK_DIRS: &[&str] = &["claude-skills", "codex-skills"];
/// Marker of the kept-entry copies older versions wrote to `~/.agents/skills/<name>/`.
const LEGACY_CODEX_MARKER: &str = ".rematch-codex.json";

pub fn cmd_skills(ctx: &AppCtx, cmd: SkillsCmd) -> Result<Value> {
    match cmd {
        SkillsCmd::Sync(a) => sync(ctx, a.harness),
    }
}

/// The shared sync for `cwd` (matched as its repo, else itself), the removal of staging links
/// of provisional agents that are not approved (`library::unlink_unapproved_agents`), then the
/// removal of the per-harness link dirs older versions kept (`claude-skills/`, `codex-skills/`).
/// Returns the sync and the legacy dirs removed.
pub fn sync_cwd(ctx: &AppCtx, cwd: &Path) -> Result<(SkillsSync, Vec<PathBuf>)> {
    let repo = repo_of(cwd);
    let mut place = rematch_core::predicate::Place::locate(repo.as_deref(), cwd);
    place.domain = rematch_core::domain::place_domain(&ctx.paths, &place, &library::branch_for(repo.as_deref(), cwd));
    let s = library::sync_shared_skills(&ctx.ledger, &ctx.paths, Some(&place))?;
    for name in library::unlink_unapproved_agents(&ctx.ledger, &ctx.paths)? {
        eprintln!("rematch: provisional agent {name} unlinked: its AGENT.md is not approved");
    }
    let mut removed = Vec::new();
    for name in LEGACY_LINK_DIRS {
        let dir = ctx.paths.root.join(name);
        if !dir.is_dir() {
            continue;
        }
        for ent in std::fs::read_dir(&dir)? {
            let p = ent?.path();
            if std::fs::symlink_metadata(&p)?.file_type().is_symlink() {
                library::unlink(&p).with_context(|| format!("remove {}", p.display()))?;
            }
        }
        // Anything but links is not ours: the directory then stays.
        if std::fs::remove_dir(&dir).is_ok() {
            removed.push(dir);
        }
    }
    Ok((s, removed))
}

/// `<plugin root>/skills` → `REMATCH_ROOT/skills`, created or re-pointed (the repo cannot
/// ship an absolute link). `true` when it changed. A real directory there is the user's.
pub fn ensure_plugin_link(ctx: &AppCtx, plugin_root: &Path) -> Result<bool> {
    if !plugin_root.join(".claude-plugin").is_dir() {
        return Ok(false);
    }
    let link = plugin_root.join("skills");
    let target = &ctx.paths.skills_shared;
    match std::fs::symlink_metadata(&link) {
        Ok(md) if md.file_type().is_symlink() => {
            if std::fs::read_link(&link).is_ok_and(|cur| &cur == target) {
                return Ok(false);
            }
        }
        Ok(_) => return Ok(false),
        Err(_) => {}
    }
    std::fs::create_dir_all(target)?;
    library::relink(target, &link)?;
    Ok(true)
}

/// The file in the plugin's `agents/` that lists the agent copies [`sync_plugin_agents`]
/// wrote (`{"<name>.md": {"id", "sha"}}`); only those are ever replaced or pruned.
pub const PLUGIN_AGENTS_MARKER: &str = ".rematch";

/// The Claude Code plugin directory: `$CLAUDE_PLUGIN_ROOT` (set in hooks, and exported by the
/// plugin's `scripts/rematch.sh` for its commands), else the documented install
/// `<config dir>/skills/rematch` (`$CLAUDE_CONFIG_DIR`, else `~/.claude`). `None` unless it holds
/// `.claude-plugin/`.
pub fn claude_plugin_root() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_PLUGIN_ROOT")
        .filter(|r| !r.is_empty())
        .map(PathBuf::from)
        .or_else(|| rematch_core::import_claude::config_dir().map(|d| d.join("skills").join("rematch")))
        .filter(|r| r.join(".claude-plugin").is_dir())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AgentCopy {
    id: EntryId,
    sha: String,
}

/// What [`sync_plugin_agents`] did to `<plugin>/agents/`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AgentsSync {
    pub dir: PathBuf,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub pruned: Vec<String>,
    /// Agent files of these names that are not rematch's copies (or were edited since): left
    /// alone, and the entry of that name is not copied.
    pub untouched: Vec<String>,
    /// Entries whose copy was in place before this sync: what a Claude Code session that
    /// discovered its agents before its `SessionStart` hook sees.
    pub present: Vec<EntryId>,
    /// Copies after the sync.
    pub total: usize,
}

fn read_agents_marker(dir: &Path) -> BTreeMap<String, AgentCopy> {
    std::fs::read_to_string(dir.join(PLUGIN_AGENTS_MARKER)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// The marker's copies that are still on disk: `(file, entry id)`.
pub fn plugin_agent_copies(plugin_root: &Path) -> Vec<(PathBuf, EntryId)> {
    let dir = plugin_root.join("agents");
    read_agents_marker(&dir).into_iter().map(|(f, c)| (dir.join(f), c.id)).filter(|(p, _)| p.is_file()).collect()
}

/// omp tool names → Claude Code's (an omp `tools` list in an `AGENT.md`).
const CLAUDE_TOOLS: &[(&str, &str)] = &[
    ("read", "Read"),
    ("bash", "Bash"),
    ("edit", "Edit"),
    ("write", "Write"),
    ("grep", "Grep"),
    ("glob", "Glob"),
    ("find", "Glob"),
    ("web_search", "WebSearch"),
    ("fetch", "WebFetch"),
    ("task", "Agent"),
    ("todo_write", "TodoWrite"),
    ("notebook", "NotebookEdit"),
];

/// An omp `AGENT.md` as a Claude Code agent file: `name`, `description` and the body as they
/// are; `tools` with omp names mapped (unknown ones dropped; none left = the field is dropped
/// and the agent inherits the session's tools); `model` only when Claude Code reads it (an
/// alias `sonnet|opus|haiku|inherit` or a `claude-…` id; omp roles such as `@smol` are
/// dropped, so the agent runs on the session's model). Other omp keys are dropped.
pub fn claude_agent_file(agent_md: &str) -> Result<String> {
    use serde_yaml::Value as Y;
    let doc = SkillDoc::parse(agent_md)?;
    let mut fm = serde_yaml::Mapping::new();
    for key in ["name", "description"] {
        if let Some(v) = doc.frontmatter.get(key) {
            fm.insert(Y::String(key.into()), v.clone());
        }
    }
    let tools: Vec<&str> = match doc.frontmatter.get("tools") {
        Some(Y::String(s)) => s.split(',').map(str::trim).collect(),
        Some(Y::Sequence(xs)) => xs.iter().filter_map(Y::as_str).map(str::trim).collect(),
        _ => Vec::new(),
    };
    let mut mapped: Vec<&str> = Vec::new();
    for t in tools {
        let to = CLAUDE_TOOLS.iter().find(|(omp, _)| omp.eq_ignore_ascii_case(t)).map(|(_, cc)| *cc);
        if let Some(cc) = to.filter(|cc| !mapped.contains(cc)) {
            mapped.push(cc);
        }
    }
    if !mapped.is_empty() {
        fm.insert(Y::String("tools".into()), Y::String(mapped.join(", ")));
    }
    if let Some(m) = doc.get_str("model").map(str::trim).filter(|m| ["sonnet", "opus", "haiku", "inherit"].contains(m) || m.starts_with("claude-")) {
        fm.insert(Y::String("model".into()), Y::String(m.into()));
    }
    Ok(SkillDoc { frontmatter: fm, body: doc.body }.render())
}

/// The agent files Claude Code should load: every approved provisional agent
/// ([`library::agent_approved`]; omp offers each through `REMATCH_ROOT/agents/` in every
/// session) and every kept/pinned one, as `<name>.md` → `(id, Claude Code file)`. A
/// provisional agent wins over a kept one of its name: it is the one being measured.
fn wanted_agents(ctx: &AppCtx) -> Result<BTreeMap<String, (EntryId, String)>> {
    let mut out = BTreeMap::new();
    for status in [Status::Kept, Status::Pinned, Status::Provisional] {
        for e in ctx.ledger.list_entries(&EntryFilter { status: Some(status), ..Default::default() })? {
            if e.kind != Kind::Agent
                || !library::load_entry(&e.path).is_ok_and(|x| x.id == e.id)
                || (status == Status::Provisional && !library::agent_approved(&e))
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(e.path.join(AGENT_MD)) else { continue };
            match claude_agent_file(&text) {
                Ok(body) => {
                    out.insert(format!("{}.md", e.name), (e.id, body));
                }
                Err(err) => eprintln!("rematch: agent {} not copied: {err:#}", e.name),
            }
        }
    }
    Ok(out)
}

/// Copies the agent entries into `<plugin_root>/agents/` as real files: Claude Code reads a
/// plugin's agents only from real files (a symlinked agent file is not listed, DESIGN §18.1),
/// so the links omp reads in `REMATCH_ROOT/agents/` cannot serve it. The marker
/// [`PLUGIN_AGENTS_MARKER`] names the copies; a copy is rewritten or pruned only while its
/// content is what the sync wrote, and a file of the same name that is not a copy is never
/// touched. `None` when `plugin_root` is not a plugin directory or its `agents/` is a link.
pub fn sync_plugin_agents(ctx: &AppCtx, plugin_root: &Path) -> Result<Option<AgentsSync>> {
    if !plugin_root.join(".claude-plugin").is_dir() {
        return Ok(None);
    }
    let dir = plugin_root.join("agents");
    if std::fs::symlink_metadata(&dir).is_ok_and(|m| !m.is_dir()) {
        return Ok(None);
    }
    let before = read_agents_marker(&dir);
    let wanted = wanted_agents(ctx)?;
    let current_sha = |file: &str| std::fs::read(dir.join(file)).ok().map(|b| sha256_hex(&b));
    let mut s = AgentsSync { dir: dir.clone(), ..Default::default() };
    s.present = before.iter().filter(|(f, _)| dir.join(f).is_file()).map(|(_, c)| c.id.clone()).collect();
    let mut after: BTreeMap<String, AgentCopy> = BTreeMap::new();
    for (file, (id, body)) in &wanted {
        let sha = sha256_hex(body.as_bytes());
        let on_disk = current_sha(file);
        let ours = before.get(file).is_some_and(|c| on_disk.as_ref() == Some(&c.sha));
        match on_disk {
            Some(cur) if cur == sha && before.contains_key(file) => {}
            Some(_) if !ours => {
                s.untouched.push(file.clone());
                continue;
            }
            existing => {
                std::fs::create_dir_all(&dir)?;
                let p = dir.join(file);
                std::fs::write(&p, body).with_context(|| format!("write {}", p.display()))?;
                let list = if existing.is_some() { &mut s.updated } else { &mut s.added };
                list.push(file.clone());
            }
        }
        after.insert(file.clone(), AgentCopy { id: id.clone(), sha });
    }
    for (file, copy) in &before {
        if after.contains_key(file) {
            continue;
        }
        match current_sha(file) {
            Some(cur) if cur == copy.sha => {
                std::fs::remove_file(dir.join(file)).with_context(|| format!("remove {}", dir.join(file).display()))?;
                s.pruned.push(file.clone());
            }
            // Edited since it was copied: the user's now (and still listed as untouched).
            Some(_) => s.untouched.push(file.clone()),
            None => s.pruned.push(file.clone()),
        }
    }
    s.total = after.len();
    let marker = dir.join(PLUGIN_AGENTS_MARKER);
    if after.is_empty() {
        if marker.exists() {
            std::fs::remove_file(&marker)?;
        }
        // Only an empty directory goes: anything else in it is the user's.
        let _ = std::fs::remove_dir(&dir);
    } else if after != before {
        std::fs::write(&marker, serde_json::to_string_pretty(&after)?).with_context(|| format!("write {}", marker.display()))?;
    }
    s.untouched.sort();
    s.untouched.dedup();
    Ok(Some(s))
}

/// Codex's user skill root, `$HOME/.agents/skills` (developers.openai.com/codex/skills).
fn codex_user_skills() -> Result<PathBuf> {
    let home = rematch_core::paths::home_dir().ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(home.join(".agents").join("skills"))
}

/// Kept-entry copies older versions wrote under `~/.agents/skills/` (directories holding
/// [`LEGACY_CODEX_MARKER`]); Codex now reads kept entries through the shared dir, so
/// `skills sync --harness codex` and `uninstall` remove them.
pub fn legacy_codex_copies() -> Vec<PathBuf> {
    let Ok(dir) = codex_user_skills() else { return Vec::new() };
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join(LEGACY_CODEX_MARKER).is_file())
        .collect();
    out.sort();
    out
}

/// Codex reaches the shared dir through one link in a skill root: it scans user and repo
/// roots recursively (depth ≤ 6) and follows directory symlinks (DESIGN §18.2.1), so
/// `<root>/rematch/<name>/SKILL.md` is found. A link older versions pointed at
/// `REMATCH_ROOT/codex-skills` is re-pointed here; their kept copies are removed. Returns
/// `(reached_from, migrated)`.
fn codex(ctx: &AppCtx, repo: Option<&Path>) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let user = codex_user_skills()?;
    let candidates: Vec<PathBuf> =
        std::iter::once(user.join("rematch")).chain(repo.map(|r| r.join(".agents").join("skills").join("rematch"))).collect();
    let mut migrated = Vec::new();
    let legacy = ctx.paths.root.join("codex-skills");
    for link in &candidates {
        if std::fs::read_link(link).is_ok_and(|t| t == legacy) {
            library::relink(&ctx.paths.skills_shared, link)?;
            migrated.push(link.clone());
        }
    }
    for copy in legacy_codex_copies() {
        std::fs::remove_dir_all(&copy).with_context(|| format!("remove {}", copy.display()))?;
        migrated.push(copy);
    }
    let shared = rematch_core::paths::canonicalize(&ctx.paths.skills_shared)?;
    let reached = candidates.into_iter().filter(|p| rematch_core::paths::canonicalize(p).is_ok_and(|c| c == shared)).collect();
    Ok((reached, migrated))
}

fn sync(ctx: &AppCtx, harness: Option<SyncHarness>) -> Result<Value> {
    let repo = repo_of(&ctx.cwd);
    let (s, mut migrated) = sync_cwd(ctx, &ctx.cwd)?;
    let dir = &ctx.paths.skills_shared;
    let place = repo.as_deref().unwrap_or(&ctx.cwd).display().to_string();
    let mut lines = vec![if s.off {
        format!("rematch is off: {} holds no links", dir.display())
    } else {
        format!("{}: {} skill link(s) after syncing {place}", dir.display(), s.total)
    }];
    if !s.added.is_empty() {
        lines.push(format!("added: {}", s.added.join(", ")));
    }
    if !s.pruned.is_empty() {
        lines.push(format!("pruned: {}", s.pruned.join(", ")));
    }
    if !s.untouched.is_empty() {
        lines.push(format!("not links, left alone: {}", s.untouched.join(", ")));
    }
    let mut out = serde_json::to_value(&s)?;
    out["cwd"] = json!(ctx.cwd);
    out["repo"] = json!(repo);
    // Kept skills linked from the profile (this machine's keeps, other machines', adopted ones).
    out["profile_sources"] = json!(rematch_core::profile::kept_sources(&ctx.paths).len());
    match harness {
        Some(SyncHarness::ClaudeCode) => match claude_plugin_root() {
            Some(root) => {
                let changed = ensure_plugin_link(ctx, &root)?;
                out["plugin_link"] = json!({"path": root.join("skills"), "changed": changed});
                if let Some(a) = sync_plugin_agents(ctx, &root)? {
                    lines.push(format!("{}: {} agent cop{}", a.dir.display(), a.total, if a.total == 1 { "y" } else { "ies" }));
                    for (what, names) in [("agents added", &a.added), ("agents updated", &a.updated), ("agents pruned", &a.pruned), ("agent files not ours, left alone", &a.untouched)] {
                        if !names.is_empty() {
                            lines.push(format!("{what}: {}", names.join(", ")));
                        }
                    }
                    out["agents"] = serde_json::to_value(&a)?;
                }
                lines.push("Claude Code sees links added after its skill scan from its next session (or SessionStart re-scan), and agents copied after its start from its next session".into());
            }
            None => lines.push(
                "no Claude Code plugin found ($CLAUDE_PLUGIN_ROOT, ~/.claude/skills/rematch): the plugin's skills/ link and agents/ copies were not synced".into(),
            ),
        },
        Some(SyncHarness::Codex) => {
            let (reached, codex_migrated) = codex(ctx, repo.as_deref())?;
            migrated.extend(codex_migrated);
            if reached.is_empty() {
                let user = codex_user_skills()?;
                lines.push(format!(
                    "Codex does not see {} yet: mkdir -p {} && ln -s {} {}",
                    dir.display(),
                    user.display(),
                    dir.display(),
                    user.join("rematch").display()
                ));
            }
            lines.push("Codex has no per-session gating: every link is visible in every Codex session; counting still needs a predicate hit".into());
            out["reached_from"] = json!(reached);
        }
        Some(SyncHarness::Pi) => lines.push(
            "Pi loads the shared dir through the rematch Pi package (`resources_discover`), at session start and `/reload`".into(),
        ),
        Some(SyncHarness::Omp | SyncHarness::OpenCode) | None => {}
    }
    for m in &migrated {
        lines.push(format!("migrated: removed or re-pointed {}", m.display()));
    }
    out["harness"] = json!(harness.map(|h| match h {
        SyncHarness::Omp => "omp",
        SyncHarness::ClaudeCode => "claude-code",
        SyncHarness::Codex => "codex",
        SyncHarness::Pi => "pi",
        SyncHarness::OpenCode => "opencode",
    }));
    out["migrated"] = json!(migrated);
    out["lines"] = json!(lines);
    Ok(out)
}

// Every test here builds symlinks (unprivileged only on Unix).
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::app::{new_entry, NewEntry};
    use rematch_core::ledger::Ledger;
    use rematch_core::paths::RematchPaths;
    use rematch_core::types::{Edit, Entry, Family, Kind, Origin, Predicate, Status};

    fn ctx(dir: &Path, cwd: &Path) -> AppCtx {
        let paths = RematchPaths::with_roots(dir.join("rematch"), dir.join("agent"));
        paths.ensure_dirs().unwrap();
        let ledger = Ledger::open(&paths.ledger_file).unwrap();
        AppCtx { paths, ledger, now: rematch_core::ledger::now_ms(), cwd: cwd.to_path_buf(), text: false }
    }

    fn stage(c: &AppCtx, name: &str, repo: &Path) -> Entry {
        let e = new_entry(
            NewEntry {
                kind: Kind::Skill,
                name: name.into(),
                branch: "payments".into(),
                status: Status::Provisional,
                family: Family("openai/gpt".into()),
                origin: Origin::Description,
                predicate: Predicate { repo_glob: repo.to_string_lossy().into_owned(), ..Default::default() },
                trigger_line: "Use when testing.".into(),
                path: PathBuf::new(),
            },
            c.now,
        )
        .unwrap();
        let body = format!("---\nname: {name}\ndescription: Use when testing.\n---\n\nUse when testing.\n");
        library::stage(&c.ledger, &c.paths, &e, &[Edit::Write { path: library::SKILL_MD.into(), content: body }]).unwrap()
    }

    #[test]
    fn legacy_link_dirs_go_after_the_first_sync() {
        let t = tempfile::tempdir().unwrap();
        let base = rematch_core::paths::canonicalize(t.path()).unwrap();
        let repo = base.join("payments");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let c = ctx(&base, &repo);
        let e = stage(&c, "ledger-tests", &repo);
        let (claude, codex) = (c.paths.root.join("claude-skills"), c.paths.root.join("codex-skills"));
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(codex.join("kept-by-user")).unwrap();
        std::os::unix::fs::symlink(&e.path, claude.join("ledger-tests")).unwrap();
        std::os::unix::fs::symlink(&e.path, codex.join("ledger-tests")).unwrap();

        let (s, removed) = sync_cwd(&c, &repo).unwrap();
        assert_eq!(s.added, vec!["ledger-tests".to_string()]);
        assert_eq!(removed, vec![claude.clone()], "a dir holding more than links stays");
        assert!(!claude.exists() && codex.join("kept-by-user").is_dir() && !codex.join("ledger-tests").exists());
        assert_eq!(std::fs::read_link(c.paths.skills_shared.join("ledger-tests")).unwrap(), e.path);
    }

    #[test]
    fn plugin_link_points_at_the_shared_dir() {
        let t = tempfile::tempdir().unwrap();
        let base = rematch_core::paths::canonicalize(t.path()).unwrap();
        let c = ctx(&base, &base);
        let plugin = base.join("plugin");
        std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
        std::os::unix::fs::symlink(c.paths.root.join("claude-skills"), plugin.join("skills")).unwrap();
        assert!(ensure_plugin_link(&c, &plugin).unwrap(), "an old link is re-pointed");
        assert_eq!(std::fs::read_link(plugin.join("skills")).unwrap(), c.paths.skills_shared);
        assert!(!ensure_plugin_link(&c, &plugin).unwrap());
        let other = base.join("other-plugin");
        std::fs::create_dir_all(other.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(other.join("skills")).unwrap();
        assert!(!ensure_plugin_link(&c, &other).unwrap(), "a real skills/ dir is the user's");
    }

    #[test]
    fn claude_agent_files_keep_what_claude_code_reads() {
        let omp = "---\nname: log-triage\ndescription: Use for a long build log.\nmodel: \"@smol\"\ntools: [read, grep, eval, Read]\nthinking: low\nrematch:\n  id: a1\n---\n\nRead the log.\n";
        let cc = claude_agent_file(omp).unwrap();
        assert_eq!(cc, "---\nname: log-triage\ndescription: Use for a long build log.\ntools: Read, Grep\n---\n\nRead the log.\n");
        let cc = claude_agent_file("---\nname: x\ndescription: d\nmodel: haiku\ntools: eval\n---\nb\n").unwrap();
        assert_eq!(cc, "---\nname: x\ndescription: d\nmodel: haiku\n---\nb\n", "no known tool: the session's tools");
    }
}
