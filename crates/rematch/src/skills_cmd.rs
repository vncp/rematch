//! `skills sync --cwd <dir> [--harness omp|claude-code|codex]` (DESIGN §16 "Shared skills"):
//! `REMATCH_ROOT/skills/` is the one skill directory every harness loads — omp through
//! `skills.customDirectories`, Claude Code through the plugin's `skills/` symlink, Codex
//! through `~/.agents/skills/rematch`. The link rules live in
//! [`rematch_core::library::sync_shared_skills`]; this module adds the cwd, the per-harness
//! reach checks and the migration from the per-harness directories of older versions.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use rematch_core::import::repo_of;
use rematch_core::library::{self, SkillsSync};
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
    /// The harness asking. `claude-code` also links the plugin's `skills/` when
    /// $CLAUDE_PLUGIN_ROOT is set; `codex` reports whether Codex reaches the shared dir.
    #[arg(long, value_enum)]
    pub harness: Option<SyncHarness>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncHarness {
    Omp,
    ClaudeCode,
    Codex,
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

/// The shared sync for `cwd` (matched as its repo, else itself), then the removal of the
/// per-harness link dirs older versions kept (`claude-skills/`, `codex-skills/`). Returns the
/// sync and the legacy dirs removed.
pub fn sync_cwd(ctx: &AppCtx, cwd: &Path) -> Result<(SkillsSync, Vec<PathBuf>)> {
    let repo = repo_of(cwd);
    let s = library::sync_shared_skills(&ctx.ledger, &ctx.paths, Some(repo.as_deref().unwrap_or(cwd)))?;
    let mut removed = Vec::new();
    for name in LEGACY_LINK_DIRS {
        let dir = ctx.paths.root.join(name);
        if !dir.is_dir() {
            continue;
        }
        for ent in std::fs::read_dir(&dir)? {
            let p = ent?.path();
            if std::fs::symlink_metadata(&p)?.file_type().is_symlink() {
                std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
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

/// Codex's user skill root, `$HOME/.agents/skills` (developers.openai.com/codex/skills).
fn codex_user_skills() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".agents").join("skills"))
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
    let shared = std::fs::canonicalize(&ctx.paths.skills_shared)?;
    let reached = candidates.into_iter().filter(|p| std::fs::canonicalize(p).is_ok_and(|c| c == shared)).collect();
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
    match harness {
        Some(SyncHarness::ClaudeCode) => {
            if let Some(root) = std::env::var_os("CLAUDE_PLUGIN_ROOT").filter(|r| !r.is_empty()) {
                let root = PathBuf::from(root);
                let changed = ensure_plugin_link(ctx, &root)?;
                out["plugin_link"] = json!({"path": root.join("skills"), "changed": changed});
            }
            lines.push("Claude Code sees links added after its skill scan from its next session (or SessionStart re-scan)".into());
        }
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
        Some(SyncHarness::Omp) | None => {}
    }
    for m in &migrated {
        lines.push(format!("migrated: removed or re-pointed {}", m.display()));
    }
    out["harness"] = json!(harness.map(|h| match h {
        SyncHarness::Omp => "omp",
        SyncHarness::ClaudeCode => "claude-code",
        SyncHarness::Codex => "codex",
    }));
    out["migrated"] = json!(migrated);
    out["lines"] = json!(lines);
    Ok(out)
}

#[cfg(test)]
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
                predicate: Predicate { repo_glob: repo.to_string_lossy().into_owned(), file_exts: vec![], stems: vec![] },
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
        let base = t.path().canonicalize().unwrap();
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
        let base = t.path().canonicalize().unwrap();
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
}
