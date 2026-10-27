//! Place-scoped delivery through a repo's own native discovery roots, for hosts that read one
//! user-level directory in every session (DESIGN §16 "Scoped exposure"): Codex scans
//! `<repo>/.agents/skills` (recursively, following directory links), Claude Code reads
//! `<repo>/.claude/agents/*.md` (real files). A repo-local entry here reaches sessions in that
//! repo only — no global directory is pruned per cwd, and two repos never write each other's.
//!
//! Rematch writes only what it can take back: a link `<repo>/.agents/skills/rematch-trials →
//! REMATCH_ROOT/views/<key>/trials` and copies `<repo>/.claude/agents/<name>.md`, each listed in
//! `REMATCH_ROOT/project-links.json` and ignored through the repo's local `info/exclude` (never
//! `.gitignore`, never a tracked file). An existing path that is not ours is never touched (the
//! item is withheld and reported). Repos without a git dir get nothing (no ignore file to keep
//! the write out of the user's status). `off` and uninstall remove every listed item that is
//! still what rematch wrote ([`withdraw_all`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ledger::sha256_hex;
use crate::paths::RematchPaths;
use crate::types::EntryId;

/// `REMATCH_ROOT/<this>`: every repo-local item rematch wrote.
pub const REGISTRY: &str = "project-links.json";
/// `<repo>/<this>`: Codex's repo skill root entry for the repo's trials.
pub const CODEX_TRIALS: &str = ".agents/skills/rematch-trials";
/// `<repo>/<this>/<name>.md`: Claude Code's project agents.
pub const CLAUDE_AGENTS: &str = ".claude/agents";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Item {
    /// A directory link to a view's `trials/`.
    Link { target: PathBuf },
    /// A file copy: its sha256 when written (an edited copy is the user's).
    Copy { sha: String, entry: EntryId },
}

type Registry = BTreeMap<PathBuf, Item>;

fn registry_path(paths: &RematchPaths) -> PathBuf {
    paths.root.join(REGISTRY)
}

fn read_registry(paths: &RematchPaths) -> Registry {
    std::fs::read_to_string(registry_path(paths)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn write_registry(paths: &RematchPaths, r: &Registry) -> Result<()> {
    let p = registry_path(paths);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(r)?).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &p).with_context(|| format!("rename {}", p.display()))
}

/// `path` still is what rematch wrote there.
fn ours(path: &Path, item: &Item) -> bool {
    match item {
        Item::Link { target } => std::fs::read_link(path).is_ok_and(|t| &t == target),
        Item::Copy { sha, .. } => std::fs::read(path).is_ok_and(|b| &sha256_hex(&b) == sha),
    }
}

fn remove(path: &Path, item: &Item) -> Result<bool> {
    if !ours(path, item) {
        return Ok(false);
    }
    crate::library::unlink(path).with_context(|| format!("remove {}", path.display()))?;
    Ok(true)
}

/// The repo's local ignore file: `<git dir>/info/exclude`, the common dir's for a worktree
/// (`.git` is a `gitdir:` file there). `None` outside a git repo.
pub fn exclude_file(repo: &Path) -> Option<PathBuf> {
    let dot = repo.join(".git");
    let git = if dot.is_dir() {
        dot
    } else {
        let text = std::fs::read_to_string(&dot).ok()?;
        let dir = repo.join(text.trim().strip_prefix("gitdir:")?.trim());
        match std::fs::read_to_string(dir.join("commondir")) {
            Ok(c) => dir.join(c.trim()),
            Err(_) => dir,
        }
    };
    git.is_dir().then(|| git.join("info").join("exclude"))
}

/// Append `/<rel>` to the repo's local ignore file unless a line already says it.
fn ignore(repo: &Path, rel: &str) -> Result<()> {
    let Some(f) = exclude_file(repo) else { return Ok(()) };
    let line = format!("/{rel}");
    let text = std::fs::read_to_string(&f).unwrap_or_default();
    if text.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    std::fs::create_dir_all(f.parent().expect("exclude has a parent"))?;
    let sep = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
    std::fs::write(&f, format!("{text}{sep}# rematch: repo-local trial delivery (removed by `rematch off`)\n{line}\n"))
        .with_context(|| format!("write {}", f.display()))
}

/// What [`sync_repo`] did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RepoSync {
    pub written: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    /// `(what, why)` not delivered: not a git repo, or a path there that is not rematch's.
    pub withheld: Vec<(String, String)>,
}

/// What a repo should carry: the Codex trials link (when `trials` is `Some`) and the Claude
/// Code agent copies (`<name>.md` → `(entry, file text)`).
#[derive(Debug, Clone, Default)]
pub struct RepoWant {
    pub trials: Option<PathBuf>,
    pub claude_agents: Option<BTreeMap<String, (EntryId, String)>>,
}

/// Bring `repo` in line with `want` for the parts `want` names (`None` parts are left as they
/// are): write missing items, re-point or rewrite ours, remove ours no longer wanted.
pub fn sync_repo(paths: &RematchPaths, repo: &Path, want: &RepoWant) -> Result<RepoSync> {
    let mut out = RepoSync::default();
    let mut reg = read_registry(paths);
    if exclude_file(repo).is_none() {
        if want.trials.is_some() || want.claude_agents.as_ref().is_some_and(|a| !a.is_empty()) {
            out.withheld.push((repo.display().to_string(), "not a git repo: no local ignore file to keep repo-local trials out of your status".into()));
        }
        return Ok(out);
    }
    if let Some(target) = &want.trials {
        let link = repo.join(CODEX_TRIALS);
        let item = Item::Link { target: target.clone() };
        match (std::fs::symlink_metadata(&link), reg.get(&link)) {
            (Ok(_), Some(old)) if !ours(&link, old) => out.withheld.push((CODEX_TRIALS.into(), format!("{} is not rematch's", link.display()))),
            (Ok(_), None) => out.withheld.push((CODEX_TRIALS.into(), format!("{} exists and is not rematch's", link.display()))),
            _ if ours(&link, &item) => {}
            _ => {
                std::fs::create_dir_all(link.parent().expect("link has a parent"))?;
                ignore(repo, CODEX_TRIALS)?;
                crate::library::relink(target, &link)?;
                reg.insert(link.clone(), item);
                out.written.push(link);
            }
        }
    }
    if let Some(agents) = &want.claude_agents {
        let dir = repo.join(CLAUDE_AGENTS);
        for (file, (entry, text)) in agents {
            let p = dir.join(file);
            let item = Item::Copy { sha: sha256_hex(text.as_bytes()), entry: entry.clone() };
            if ours(&p, &item) {
                continue;
            }
            let mine = reg.get(&p).is_some_and(|old| ours(&p, old));
            if p.exists() && !mine {
                out.withheld.push((format!("{CLAUDE_AGENTS}/{file}"), format!("{} is not rematch's", p.display())));
                continue;
            }
            std::fs::create_dir_all(&dir)?;
            ignore(repo, &format!("{CLAUDE_AGENTS}/{file}"))?;
            std::fs::write(&p, text).with_context(|| format!("write {}", p.display()))?;
            reg.insert(p.clone(), item);
            out.written.push(p);
        }
        let stale: Vec<PathBuf> = reg
            .iter()
            .filter(|(p, i)| matches!(i, Item::Copy { .. }) && p.parent() == Some(dir.as_path()))
            .filter(|(p, _)| !agents.contains_key(&p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()))
            .map(|(p, _)| p.clone())
            .collect();
        for p in stale {
            let item = reg.remove(&p).expect("listed");
            if remove(&p, &item)? {
                out.removed.push(p);
            }
        }
    }
    write_registry(paths, &reg)?;
    Ok(out)
}

/// Remove every listed item that is still rematch's (off, uninstall); edited or replaced ones
/// are left to the user and dropped from the list. Returns the paths removed.
pub fn withdraw_all(paths: &RematchPaths) -> Result<Vec<PathBuf>> {
    let reg = read_registry(paths);
    let mut out = Vec::new();
    for (p, item) in &reg {
        if remove(p, item)? {
            out.push(p.clone());
        }
    }
    if !reg.is_empty() {
        write_registry(paths, &Registry::new())?;
    }
    Ok(out)
}

/// Remove listed agent copies whose entry no longer may load (`keep(entry)` false): a retired,
/// kept or unapproved agent leaves every repo at its next status change, not only the repo of
/// the next session.
pub fn prune_copies(paths: &RematchPaths, keep: impl Fn(&str) -> bool) -> Result<Vec<PathBuf>> {
    let mut reg = read_registry(paths);
    let gone: Vec<PathBuf> = reg.iter().filter(|(_, i)| matches!(i, Item::Copy { entry, .. } if !keep(entry))).map(|(p, _)| p.clone()).collect();
    let mut out = Vec::new();
    for p in gone {
        let item = reg.remove(&p).expect("listed");
        if remove(&p, &item)? {
            out.push(p);
        }
    }
    if !out.is_empty() {
        write_registry(paths, &reg)?;
    }
    Ok(out)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn env() -> (tempfile::TempDir, RematchPaths) {
        let t = tempfile::tempdir().unwrap();
        let paths = RematchPaths::with_roots(t.path().join("rematch"), t.path().join("agent"));
        paths.ensure_dirs().unwrap();
        (t, paths)
    }

    #[test]
    fn repo_local_items_are_ignored_scoped_and_withdrawn() {
        let (t, paths) = env();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(repo.join(".git/info")).unwrap();
        let trials = paths.root.join("views/repo-x/trials");
        std::fs::create_dir_all(&trials).unwrap();
        let agents: BTreeMap<String, (EntryId, String)> = [("triage.md".to_string(), ("e1".to_string(), "---\nname: triage\ndescription: d\n---\nbody\n".to_string()))].into();
        let s = sync_repo(&paths, &repo, &RepoWant { trials: Some(trials.clone()), claude_agents: Some(agents.clone()) }).unwrap();
        assert_eq!(s.written.len(), 2, "{s:?}");
        assert_eq!(std::fs::read_link(repo.join(CODEX_TRIALS)).unwrap(), trials);
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("/.agents/skills/rematch-trials") && exclude.contains("/.claude/agents/triage.md"));
        assert!(sync_repo(&paths, &repo, &RepoWant { trials: Some(trials.clone()), claude_agents: Some(agents) }).unwrap().written.is_empty(), "idempotent");

        // an authored agent of the same file name is never overwritten
        let other = t.path().join("other");
        std::fs::create_dir_all(other.join(".git")).unwrap();
        std::fs::create_dir_all(other.join(CLAUDE_AGENTS)).unwrap();
        std::fs::write(other.join(CLAUDE_AGENTS).join("triage.md"), "mine").unwrap();
        let agents: BTreeMap<String, (EntryId, String)> = [("triage.md".to_string(), ("e1".to_string(), "x".to_string()))].into();
        let s = sync_repo(&paths, &other, &RepoWant { trials: None, claude_agents: Some(agents) }).unwrap();
        assert!(s.written.is_empty() && s.withheld.len() == 1, "{s:?}");
        assert_eq!(std::fs::read_to_string(other.join(CLAUDE_AGENTS).join("triage.md")).unwrap(), "mine");

        // not a git repo: nothing written, said why
        let plain = t.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let s = sync_repo(&paths, &plain, &RepoWant { trials: Some(trials.clone()), claude_agents: None }).unwrap();
        assert!(s.written.is_empty() && s.withheld[0].1.contains("not a git repo"));

        // an agent that may no longer load leaves every repo; off withdraws the rest
        assert_eq!(prune_copies(&paths, |_| false).unwrap(), vec![repo.join(CLAUDE_AGENTS).join("triage.md")]);
        assert_eq!(withdraw_all(&paths).unwrap(), vec![repo.join(CODEX_TRIALS)]);
        assert!(std::fs::symlink_metadata(repo.join(CODEX_TRIALS)).is_err());
    }
}
